//! Child-process execution for tools (bash / grep).
//!
//! Separates how tool child processes are run (scrubbed environment,
//! bounded output, wall-clock timeout, signal-kill reporting) from the
//! tool definitions and command policy in [`crate::tools`].

use std::collections::VecDeque;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command as TokioCommand;

/// Tool execution limits for child processes (bash / grep).
#[derive(Clone, Copy)]
pub struct ToolLimits {
    /// Hard cap (bytes) per output stream; `0` = unlimited.
    /// Excess output keeps the tail.
    pub max_output_bytes: usize,
    /// Wall-clock timeout in seconds (0 = unlimited).
    pub tool_timeout_secs: u64,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: 1_048_576,
            tool_timeout_secs: 30,
        }
    }
}

static TOOL_LIMITS: OnceLock<ToolLimits> = OnceLock::new();

/// Register tool execution limits (called at startup from Config).
pub fn set_tool_limits(limits: ToolLimits) {
    let _ = TOOL_LIMITS.set(limits);
}

fn tool_limits() -> ToolLimits {
    TOOL_LIMITS.get().copied().unwrap_or_default()
}

/// Captured result of a tool child process.
pub struct CapturedOutput {
    /// stdout, capped to `max_output_bytes` (tail kept, `0` = unlimited), lossy UTF-8.
    pub stdout: String,
    /// stderr, capped the same way.
    pub stderr: String,
    /// Exit code (1 when killed by a signal).
    pub exit_code: i32,
    /// Signal number when the process was killed by a signal.
    pub signal: Option<i32>,
    /// True when stdout was cut by the cap or its drain was aborted.
    pub stdout_truncated: bool,
    /// True when stderr was cut by the cap or its drain was aborted.
    pub stderr_truncated: bool,
    /// Bytes of real stdout not included in `stdout` (cap overflow, or bytes
    /// already buffered when the drain was aborted; a lower bound in the
    /// abort case because the pipe remainder is unreadable).
    pub stdout_omitted: u64,
}

/// Shared capture state: bounded tail buffer plus truncation stats, so a
/// timed-out drain can still surface the bytes read so far.
struct CaptureState {
    buf: VecDeque<u8>,
    truncated: bool,
    /// Bytes of real output discarded (not included in `buf`).
    omitted: u64,
}

impl CaptureState {
    fn with_capacity(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(1 << 20)),
            truncated: false,
            omitted: 0,
        }
    }
}

/// Spawn `program` with a scrubbed environment, bounded output, and a
/// wall-clock timeout. Errors are tagged with `tool` (e.g. "BASH") so the
/// LLM-facing messages keep their `[BASH_...]` prefixes.
pub async fn run_captured(program: &str, args: &[&str], tool: &str) -> Result<CapturedOutput> {
    let limits = tool_limits();
    let mut cmd = TokioCommand::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    scrub_child_env(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| {
        anyhow!(
            "[{}_EXECUTION_FAILED] Failed to spawn {}: {}",
            tool,
            program,
            e
        )
    })?;
    let out_reader = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("[{}_EXECUTION_FAILED] Failed to capture stdout", tool))?;
    let err_reader = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("[{}_EXECUTION_FAILED] Failed to capture stderr", tool))?;
    let cap = limits.max_output_bytes;
    let out_state = Arc::new(Mutex::new(CaptureState::with_capacity(cap)));
    let err_state = Arc::new(Mutex::new(CaptureState::with_capacity(cap)));
    let out_task = tokio::spawn(read_capped(out_reader, cap, out_state.clone()));
    let err_task = tokio::spawn(read_capped(err_reader, cap, err_state.clone()));

    let wait_fut = child.wait();
    let status = match limits.tool_timeout_secs {
        0 => wait_fut.await,
        secs => match tokio::time::timeout(Duration::from_secs(secs), wait_fut).await {
            Ok(st) => st,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                // Abort the readers instead of awaiting them: a background
                // grandchild may keep the pipes open, which would hang.
                out_task.abort();
                err_task.abort();
                return Err(anyhow!(
                    "[{}_TIMED_OUT] Command timed out after {} seconds.",
                    tool,
                    secs
                ));
            }
        },
    };
    let status =
        status.map_err(|e| anyhow!("[{}_EXECUTION_FAILED] Execution error: {}", tool, e))?;

    // Drain both streams in parallel so a pipe held open by a background
    // grandchild costs at most one grace period, not two.
    let (out_res, err_res) = tokio::join!(
        drain_read(out_task, DRAIN_GRACE, out_state),
        drain_read(err_task, DRAIN_GRACE, err_state),
    );
    let (stdout, stdout_truncated, stdout_omitted) =
        out_res.context(format!("[{}_EXECUTION_FAILED] stdout read failed", tool))?;
    let (stderr, stderr_truncated, _stderr_omitted) =
        err_res.context(format!("[{}_EXECUTION_FAILED] stderr read failed", tool))?;

    let (exit_code, signal) = report_status(&status);
    Ok(CapturedOutput {
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        exit_code,
        signal,
        stdout_truncated,
        stderr_truncated,
        stdout_omitted,
    })
}

/// Scrub the child environment: only an explicit allowlist is passed, so
/// secrets (LLM_API_KEY / DB_AUTH_KEY, etc.) are never inherited.
fn scrub_child_env(cmd: &mut TokioCommand) {
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    } else {
        cmd.env("PATH", "/usr/bin:/bin");
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home); // HOME neutralization is a later stage
    }
    cmd.env("LANG", "C.UTF-8");
    for key in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
}

/// Read a stream keeping at most `cap` bytes (the tail; `0` = unlimited),
/// writing into shared `state` so a timed-out drain can still surface the
/// partial output. No std/tokio equivalent keeps the tail;
/// `AsyncReadExt::take` keeps the head.
async fn read_capped<R: AsyncRead + Unpin>(
    mut r: R,
    cap: usize,
    state: Arc<Mutex<CaptureState>>,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let mut s = state.lock().unwrap();
        for &b in &chunk[..n] {
            if cap > 0 && s.buf.len() == cap {
                s.buf.pop_front();
                s.truncated = true;
                s.omitted += 1;
            }
            s.buf.push_back(b);
        }
    }
    Ok(())
}

/// Grace period for draining output after the child exits.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Drain a read task, aborting it after `grace` so a background grandchild
/// holding the pipe open cannot block the tool forever. Returns the bytes
/// read so far (partial when aborted), whether they are truncated, and the
/// omitted byte count.
async fn drain_read(
    mut task: tokio::task::JoinHandle<std::io::Result<()>>,
    grace: Duration,
    state: Arc<Mutex<CaptureState>>,
) -> std::io::Result<(Vec<u8>, bool, u64)> {
    match tokio::time::timeout(grace, &mut task).await {
        Ok(Ok(Ok(()))) => {
            let s = state.lock().unwrap();
            Ok((s.buf.iter().copied().collect(), s.truncated, s.omitted))
        }
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(e)) => Err(std::io::Error::other(e)),
        Err(_) => {
            task.abort();
            let mut s = state.lock().unwrap();
            // Surface the partial tail instead of losing it, and count those
            // bytes as omitted (the unread pipe remainder is unknowable).
            let kept: Vec<u8> = s.buf.drain(..).collect();
            s.omitted += kept.len() as u64;
            Ok((kept, true, s.omitted))
        }
    }
}

/// Split an exit status into (exit_code, signal). Signal-killed processes
/// report exit_code 1 and the signal number separately.
fn report_status(status: &std::process::ExitStatus) -> (i32, Option<i32>) {
    match status.code() {
        Some(code) => (code, None),
        None => {
            #[cfg(unix)]
            {
                (1, status.signal())
            }
            #[cfg(not(unix))]
            {
                (1, None)
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/tools_process_test.rs"]
mod tests;
