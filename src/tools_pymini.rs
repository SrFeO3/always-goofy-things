//! Sandboxed Python interpreter tool: mini_python_interpreter.
//!
//! Runs LLM-supplied Python in the in-process Monty interpreter. The sandbox
//! gets no network, environment, or process authority; its only host
//! capability is one mount of the workspace root at `/work`, confined by a
//! directory descriptor so no path can leave it. Time, recursion,
//! suspensions, memory, and output are bounded per run; a crash or a memory
//! balloon can still take the host process down.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::time::Duration;

use monty::{MontyRun, RunProgress};
use monty_fs::{Mount, MountCallOutcome, MountMode, MountTable};
use monty_types::{
    CollectedStreams, CompileOptions, ExcType, ExtFunctionResult, MontyException, OsFunctionCall,
    OsPolicy, PrintStream, PrintWriter, ProcessTime, ResourceLimits, ResourceTracker,
    SandboxTimeZone, SleepMode,
};
use serde::{Deserialize, Serialize};

/// Host-side policy for one run. The defaults are fixed host policy; the
/// LLM cannot change them.
#[derive(Debug, Clone)]
pub(crate) struct MiniPolicy {
    /// Execution-time budget for the run (default 10 s).
    pub feed_duration: Duration,
    /// Python-level recursion depth ceiling (default 200).
    pub max_recursion_depth: usize,
    /// Maximum host-serviced suspensions (default 4096, counted host-side).
    pub max_suspensions: usize,
    /// Soft memory limit. Without `monty-alloc` it drives the VM pre-checks
    /// on predictable allocations and container growth; it is not a hard
    /// ceiling.
    pub max_memory: Option<usize>,
    /// Mount aggregate memory budget (default 64 MiB).
    pub mount_memory_limit: u64,
    /// Cap on bytes written through the mount per run (default 32 MiB).
    pub write_bytes_limit: u64,
    /// Combined stdout+stderr cap, tail kept (default: `--max-tool-output-bytes`).
    pub output_cap: usize,
}

impl Default for MiniPolicy {
    fn default() -> Self {
        Self {
            feed_duration: Duration::from_secs(10),
            max_recursion_depth: 200,
            max_suspensions: 4096,
            max_memory: Some(256 * 1024 * 1024),
            mount_memory_limit: 64 * 1024 * 1024,
            write_bytes_limit: 32 * 1024 * 1024,
            output_cap: crate::tools_process::tool_limits().max_output_bytes,
        }
    }
}

/// Source byte-length cap.
pub(crate) const MAX_CODE_BYTES: usize = 64 * 1024;

/// Sandbox cwd (virtual) and mount point.
const MOUNT_PATH: &str = "/work";

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

/// `description` shown to the LLM for the default (`ask` / `rw`) capability.
pub(crate) const TOOL_DESCRIPTION_RW: &str = "Execute a self-contained Python 3.14-subset program in a lightweight sandbox. Combine many operations into one script with loops and conditionals instead of making many small tool calls. The workspace root is the current directory: file paths are relative to the workspace root (e.g. open('data.csv'), Path('artifacts/report.json').write_text(data)). Do not start with '/' or '../', and paths outside the workspace do not exist. Use print() for the results you need; the value of the last expression is discarded. There is no network and no environment variables. The only built-in modules are json, re, math, collections, datetime, itertools, functools, pathlib, asyncio, dataclasses, base64, binascii, copy, os, random, time, typing, sys, and unicodedata. No other standard-library modules (csv, hashlib, struct, io, uuid, sqlite3) or third-party packages (numpy, pandas) exist.";

/// `description` shown when the session runs read-only (`--mini-python-auto-confirm=ro`):
/// the path examples only read, and writes are announced as denied.
pub(crate) const TOOL_DESCRIPTION_RO: &str = "Execute a self-contained Python 3.14-subset program in a lightweight sandbox. Combine many operations into one script with loops and conditionals instead of making many small tool calls. The workspace root is the current directory: file paths are relative to the workspace root (e.g. open('data.csv'), Path('src/main.rs').read_text()). Do not start with '/' or '../', and paths outside the workspace do not exist. Use print() for the results you need; the value of the last expression is discarded. There is no network and no environment variables. The only built-in modules are json, re, math, collections, datetime, itertools, functools, pathlib, asyncio, dataclasses, base64, binascii, copy, os, random, time, typing, sys, and unicodedata. No other standard-library modules (csv, hashlib, struct, io, uuid, sqlite3) or third-party packages (numpy, pandas) exist. File writes are denied in this session.";

pub(crate) const CODE_PARAM_DESCRIPTION: &str = "A self-contained Python program. Use paths relative to the workspace root for files and print() the results you need. State does not persist between calls.";

/// The `description` for the current capability: writes are denied in `ro`
/// sessions and the tool definition must not promise them.
pub(crate) fn tool_description(write: bool) -> &'static str {
    if write {
        TOOL_DESCRIPTION_RW
    } else {
        TOOL_DESCRIPTION_RO
    }
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MiniPythonRequest {
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MiniPythonStatus {
    Completed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum MiniPythonErrorCode {
    InvalidInput,
    Unavailable,
    Timeout,
    ResourceLimit,
    InternalError,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MiniPythonError {
    pub code: MiniPythonErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl MiniPythonError {
    pub(crate) fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            code: MiniPythonErrorCode::InvalidInput,
            message: message.into(),
            retryable: true,
        }
    }

    pub(crate) fn unavailable() -> Self {
        Self {
            code: MiniPythonErrorCode::Unavailable,
            message: "The workspace could not be opened for the sandbox.".to_string(),
            retryable: false,
        }
    }

    pub(crate) fn timeout() -> Self {
        Self {
            code: MiniPythonErrorCode::Timeout,
            message: "The script exceeded the execution-time budget.".to_string(),
            retryable: false,
        }
    }

    pub(crate) fn resource_limit() -> Self {
        Self {
            code: MiniPythonErrorCode::ResourceLimit,
            message: "The script exceeded the sandbox operation budget.".to_string(),
            retryable: true,
        }
    }

    pub(crate) fn internal() -> Self {
        Self {
            code: MiniPythonErrorCode::InternalError,
            message: "The interpreter failed unexpectedly.".to_string(),
            retryable: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct StreamTruncation {
    pub stdout: bool,
    pub stderr: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodeOutput {
    pub stdout: String,
    pub stderr: String,
    pub truncated: StreamTruncation,
}

impl CodeOutput {
    fn empty() -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            truncated: StreamTruncation {
                stdout: false,
                stderr: false,
            },
        }
    }

    fn has_content(&self) -> bool {
        !self.stdout.is_empty() || !self.stderr.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MiniPythonResponse {
    pub status: MiniPythonStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_output: Option<CodeOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<MiniPythonError>,
}

impl MiniPythonResponse {
    fn completed(code_output: CodeOutput) -> Self {
        Self {
            status: MiniPythonStatus::Completed,
            code_output: Some(code_output),
            error: None,
        }
    }

    fn error(error: MiniPythonError, code_output: Option<CodeOutput>) -> Self {
        Self {
            status: MiniPythonStatus::Error,
            code_output,
            error: Some(error),
        }
    }

    /// Internal-error envelope for dispatch-layer bugs (e.g. a mini python
    /// call executed without an approval decision).
    pub(crate) fn internal_error_response() -> Self {
        Self::error(MiniPythonError::internal(), None)
    }
}

/// How the run loop ended.
enum RunOutcome {
    Completed,
    /// A sandbox-level exception: returned as `completed` with the traceback
    /// appended to stderr.
    Exception(MontyException),
    /// The execution-time budget fired (uncatchable `TimeoutError`).
    Timeout,
    /// The host-serviced suspension budget fired.
    ResourceLimit,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Execute one `mini_python_interpreter` call. `write` selects the mount mode
/// (`ReadWrite` / `ReadOnly`) and comes from the per-call approval decision,
/// never from the request.
pub(crate) async fn execute(args: &serde_json::Value, write: bool) -> MiniPythonResponse {
    let request: MiniPythonRequest = match serde_json::from_value(args.clone()) {
        Ok(request) => request,
        Err(_) => {
            return MiniPythonResponse::error(
                MiniPythonError::invalid_input(
                    "The arguments do not match the tool schema: 'code' is required and must be a string.",
                ),
                None,
            );
        }
    };
    if let Some(error) = validate(&request) {
        return MiniPythonResponse::error(error, None);
    }

    let root = crate::tools::workspace_root().clone();
    let policy = MiniPolicy::default();
    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            run(&request.code, &root, write, &policy)
        }))
        .unwrap_or_else(|_| MiniPythonResponse::error(MiniPythonError::internal(), None))
    })
    .await
    .unwrap_or_else(|_| MiniPythonResponse::error(MiniPythonError::internal(), None))
}

/// Validate the request against the host policy. Returns the error
/// envelope to send on failure.
fn validate(request: &MiniPythonRequest) -> Option<MiniPythonError> {
    if request.code.as_bytes().len() > MAX_CODE_BYTES {
        return Some(MiniPythonError::invalid_input(format!(
            "The program is {} bytes; the limit is {MAX_CODE_BYTES} bytes.",
            request.code.as_bytes().len()
        )));
    }
    if request.code.contains('\0') {
        return Some(MiniPythonError::invalid_input(
            "The program contains a NUL byte.",
        ));
    }
    None
}

/// Synchronous core: build the sandbox, run `code` to completion, and
/// normalize the result. Testable directly with a temporary workspace.
pub(crate) fn run(code: &str, root: &Path, write: bool, policy: &MiniPolicy) -> MiniPythonResponse {
    // 1. Workspace mount: the only host capability granted. Writes fail
    //    structurally on a ReadOnly mount; paths outside /work are
    //    unreachable in both modes.
    let mut table = MountTable::new();
    let mode = if write {
        MountMode::ReadWrite
    } else {
        MountMode::ReadOnly
    };
    let mount = match Mount::new(MOUNT_PATH, root, mode, Some(policy.write_bytes_limit)) {
        Ok(mount) => mount.with_memory_usage_limit(policy.mount_memory_limit),
        Err(_) => return MiniPythonResponse::error(MiniPythonError::unavailable(), None),
    };
    if table.push_mount(mount).is_err() {
        return MiniPythonResponse::error(MiniPythonError::unavailable(), None);
    }

    // 2. OsPolicy: system wall clock pinned to UTC, sleeps return immediately,
    //    process time hidden, random seeded from OS entropy. The zone is set
    //    explicitly rather than inherited from the default so a monty upgrade
    //    cannot silently move the sandbox to the host's local zone.
    let os_policy = OsPolicy {
        sleep: SleepMode::Zero,
        process_time: ProcessTime::Zero,
        timezone: SandboxTimeZone::utc(),
        ..OsPolicy::default()
    };

    // 3. Resource limits. `max_suspensions` is only recorded here; the host
    //    counts and enforces it in the drive loop.
    let mut limits = ResourceLimits::default()
        .max_feed_duration(policy.feed_duration)
        .max_recursion_depth(policy.max_recursion_depth)
        .max_suspensions(policy.max_suspensions)
        .max_total_sleep(Duration::ZERO);
    if let Some(memory) = policy.max_memory {
        limits = limits.max_memory(memory);
    }

    let mut runner = match MontyRun::new(
        code.to_owned(),
        "main.py",
        Vec::new(),
        CompileOptions::default(),
    ) {
        Ok(runner) => runner.with_os_policy(os_policy),
        Err(e) => return completed_with_exception(e, CodeOutput::empty()),
    };
    runner.set_cwd(MOUNT_PATH);
    let tracker = ResourceTracker::new(limits);

    // 4. Run loop. print() output lands in the collected stream buffer (10 MiB
    //    collector cap); the configured trim runs after completion.
    let mut streams = CollectedStreams::default();
    let mut print = PrintWriter::collect_streams(&mut streams);
    let outcome = match runner.start(Vec::new(), tracker, print.reborrow()) {
        Ok(progress) => drive(progress, &mut table, &mut print, policy.max_suspensions),
        // A duration-budget TimeoutError can fire inside start (before the
        // first suspension); classify it the same way.
        Err(e) => classify_exception(e),
    };
    drop(print);

    // 5. Normalize: completed runs and sandbox exceptions share the
    //    `code_output` envelope; host-side failures use `error`.
    let code_output = finalize_output(streams, policy.output_cap);
    match outcome {
        RunOutcome::Completed => MiniPythonResponse::completed(code_output),
        RunOutcome::Exception(e) => {
            let mut code_output = code_output;
            code_output.stderr.push_str(&format!("{e}"));
            MiniPythonResponse::completed(code_output)
        }
        RunOutcome::Timeout => MiniPythonResponse::error(
            MiniPythonError::timeout(),
            code_output.has_content().then_some(code_output),
        ),
        RunOutcome::ResourceLimit => MiniPythonResponse::error(
            MiniPythonError::resource_limit(),
            code_output.has_content().then_some(code_output),
        ),
    }
}

/// Drive the suspension loop, servicing OS calls from the mount table and
/// rejecting anything else (host functions, undefined names, futures).
fn drive(
    mut progress: RunProgress,
    table: &mut MountTable,
    print: &mut PrintWriter<'_>,
    max_suspensions: usize,
) -> RunOutcome {
    let mut suspensions = 0usize;
    loop {
        match progress {
            RunProgress::Complete(_) => return RunOutcome::Completed,
            RunProgress::OsCall(call) => {
                suspensions += 1;
                if suspensions > max_suspensions {
                    return RunOutcome::ResourceLimit;
                }
                match call.resume_with(print.reborrow(), |fc| service_os_call(table, fc)) {
                    Ok(next) => progress = next,
                    Err(e) => return classify_exception(e),
                }
            }
            RunProgress::NameLookup(lookup) => {
                suspensions += 1;
                if suspensions > max_suspensions {
                    return RunOutcome::ResourceLimit;
                }
                // Undefined names raise NameError; name the subset restriction
                // so the LLM sees why third-party modules do not exist.
                let exc = MontyException::new(
                    ExcType::NameError,
                    Some(format!(
                        "name '{}' is not defined. This sandbox has no third-party packages and only a small stdlib subset; modules outside the subset do not exist.",
                        lookup.name
                    )),
                );
                match lookup.abort(exc, print.reborrow()) {
                    Ok(next) => progress = next,
                    Err(e) => return classify_exception(e),
                }
            }
            RunProgress::FunctionCall(call) => {
                suspensions += 1;
                if suspensions > max_suspensions {
                    return RunOutcome::ResourceLimit;
                }
                // No host functions are granted; reject defensively.
                let exc = MontyException::new(
                    ExcType::RuntimeError,
                    Some("host functions are not available in this sandbox".to_string()),
                );
                match call.abort(exc, print.reborrow()) {
                    Ok(next) => progress = next,
                    Err(e) => return classify_exception(e),
                }
            }
            RunProgress::ResolveFutures(futures) => {
                suspensions += 1;
                if suspensions > max_suspensions {
                    return RunOutcome::ResourceLimit;
                }
                let exc = MontyException::new(
                    ExcType::RuntimeError,
                    Some("host futures are not available in this sandbox".to_string()),
                );
                match futures.abort(exc, print.reborrow()) {
                    Ok(next) => progress = next,
                    Err(e) => return classify_exception(e),
                }
            }
        }
    }
}

/// Answer one sandbox OS call from the mount table. Calls no mount covers
/// (network, env, entropy, anything outside `/work`) get the sandbox's own
/// no-handler default: `PermissionError` naming the path for filesystem calls,
/// `RuntimeError` otherwise.
fn service_os_call(table: &mut MountTable, call: OsFunctionCall) -> ExtFunctionResult {
    match table.handle_os_call(call) {
        MountCallOutcome::Handled(Ok(value)) => ExtFunctionResult::Return(value),
        MountCallOutcome::Handled(Err(err)) => ExtFunctionResult::Error(err.into_exception()),
        MountCallOutcome::NotHandled(call) => ExtFunctionResult::Error(call.on_no_handler()),
    }
}

/// The duration budget is the one sandbox exception the host reports as
/// `TIMEOUT` instead of a traceback. A budget timeout is monty's own
/// *uncatchable* error: no traceback frames and a `time limit exceeded`
/// message. A `TimeoutError` the script raises is an ordinary exception, so it
/// keeps the `completed` + traceback contract even though the types match.
fn classify_exception(e: MontyException) -> RunOutcome {
    let is_budget_timeout = e.exc_type() == ExcType::TimeoutError
        && e.traceback().is_empty()
        && e.message()
            .is_some_and(|message| message.contains("time limit exceeded: "));
    if is_budget_timeout {
        RunOutcome::Timeout
    } else {
        RunOutcome::Exception(e)
    }
}

fn completed_with_exception(e: MontyException, mut code_output: CodeOutput) -> MiniPythonResponse {
    code_output.stderr.push_str(&format!("{e}"));
    MiniPythonResponse::completed(code_output)
}

/// Split the collected emit-ordered output back into stdout/stderr, capping
/// the combined size at `cap` bytes and keeping the tail (the most recently
/// emitted bytes), mirroring `execute_bash`'s tail preference. `cap == 0`
/// means unlimited.
fn finalize_output(streams: CollectedStreams, cap: usize) -> CodeOutput {
    let entries = streams.into_entries();
    let total: usize = entries.iter().map(|(_, text)| text.len()).sum();
    let mut drop = if cap > 0 {
        total.saturating_sub(cap)
    } else {
        0
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut truncated = StreamTruncation {
        stdout: false,
        stderr: false,
    };
    for (stream, text) in entries {
        let (kept, dropped) = drop_head(&text, drop);
        drop -= dropped;
        if dropped > 0 {
            match stream {
                PrintStream::Stdout => truncated.stdout = true,
                PrintStream::Stderr => truncated.stderr = true,
            }
        }
        match stream {
            PrintStream::Stdout => stdout.push_str(kept),
            PrintStream::Stderr => stderr.push_str(kept),
        }
    }
    CodeOutput {
        stdout,
        stderr,
        truncated,
    }
}

/// Drop up to `bytes` from the head of `text` on UTF-8 boundaries. Returns the
/// kept tail and how many bytes were actually dropped.
fn drop_head(text: &str, bytes: usize) -> (&str, usize) {
    if bytes == 0 {
        return (text, 0);
    }
    let mut dropped = 0;
    for (i, ch) in text.char_indices() {
        let len = ch.len_utf8();
        if dropped + len > bytes {
            return (&text[i..], dropped);
        }
        dropped += len;
    }
    ("", dropped)
}

#[cfg(test)]
#[path = "tests/tools_pymini_test.rs"]
mod tests;
