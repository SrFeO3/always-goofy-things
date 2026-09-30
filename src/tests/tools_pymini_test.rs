//! Acceptance tests for the `mini_python_interpreter` tool.
//!
//! The sandbox core (`tools_pymini::run`) is exercised directly against
//! temporary workspaces so the tests never touch the process-wide workspace
//! root and stay parallel-safe.

use super::*;
use std::path::PathBuf;
use std::time::Duration;

/// A unique temporary workspace for one test.
fn temp_workspace(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("agt_pymini_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Run `code` in a fresh workspace with default policy (read-write).
fn run_rw(name: &str, code: &str) -> MiniPythonResponse {
    let root = temp_workspace(name);
    run(code, &root, true, &MiniPolicy::default())
}

fn completed(resp: &MiniPythonResponse) -> &CodeOutput {
    resp.code_output
        .as_ref()
        .expect("expected a completed response with code_output")
}

// ---------------------------------------------------------------------------
// Schema / input validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schema_rejects_invalid_requests() {
    // `code` is required and must be a string.
    let resp = execute(&serde_json::json!({}), true).await;
    assert_eq!(resp.status, MiniPythonStatus::Error);
    let err = resp.error.unwrap();
    assert_eq!(err.code, MiniPythonErrorCode::InvalidInput);

    let resp = execute(&serde_json::json!({ "code": 123 }), true).await;
    assert_eq!(resp.status, MiniPythonStatus::Error);

    // Unknown fields are rejected (deny_unknown_fields): no omission-default
    // may become a security boundary.
    let resp = execute(&serde_json::json!({ "code": "1", "timeout": 1 }), true).await;
    assert_eq!(resp.status, MiniPythonStatus::Error);
    let err = resp.error.unwrap();
    assert_eq!(err.code, MiniPythonErrorCode::InvalidInput);
}

#[tokio::test]
async fn source_size_and_nul_limits() {
    let oversized = format!("print({})", "x".repeat(MAX_CODE_BYTES + 1));
    let resp = execute(&serde_json::json!({ "code": oversized }), true).await;
    assert_eq!(resp.status, MiniPythonStatus::Error);
    assert_eq!(resp.error.unwrap().code, MiniPythonErrorCode::InvalidInput);

    let with_nul = "print('a\u{0}b')".to_string();
    let resp = execute(&serde_json::json!({ "code": with_nul }), true).await;
    assert_eq!(resp.status, MiniPythonStatus::Error);
    assert_eq!(resp.error.unwrap().code, MiniPythonErrorCode::InvalidInput);
}

// ---------------------------------------------------------------------------
// Basic execution and mount modes
// ---------------------------------------------------------------------------

#[test]
fn smoke_completed_prints_stdout() {
    let resp = run_rw("smoke", "print(1 + 1)");
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert_eq!(co.stdout, "2\n");
    assert_eq!(co.stderr, "");
    assert!(!co.truncated.stdout && !co.truncated.stderr);
}

#[test]
fn stdlib_subset_imports_work() {
    let resp = run_rw(
        "imports",
        "import json, math, re, collections\nprint(json.dumps({'x': math.sqrt(4)}))\nprint(re.sub(r'[0-9]+', 'N', 'a1b22'))",
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(co.stdout.contains("\"x\": 2.0"), "stdout: {}", co.stdout);
    assert!(co.stdout.contains("aNbN"), "stdout: {}", co.stdout);
}

#[test]
fn ro_mode_denies_writes_structurally() {
    let root = temp_workspace("ro");
    let resp = run(
        "open('x.txt', 'w').write('hi')",
        &root,
        false,
        &MiniPolicy::default(),
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(
        co.stderr.contains("PermissionError"),
        "expected PermissionError traceback, got: {}",
        co.stderr
    );
    assert!(
        !root.join("x.txt").exists(),
        "ro mode must not create files"
    );
}

#[test]
fn rw_writes_persist_on_host() {
    let root = temp_workspace("rw");
    let code = r#"
from pathlib import Path
open('out.txt', 'w').write('hello')
open('log.txt', 'a').write('a')
open('log.txt', 'a').write('b')
Path('artifacts').mkdir(exist_ok=True)
Path('artifacts/report.json').write_text('{}')
Path('old.tmp').write_text('x')
Path('old.tmp').unlink()
Path('out.txt').rename('renamed.txt')
print('done')
"#;
    let resp = run(code, &root, true, &MiniPolicy::default());
    assert_eq!(resp.status, MiniPythonStatus::Completed, "{:?}", resp);
    assert_eq!(
        std::fs::read_to_string(root.join("renamed.txt")).unwrap(),
        "hello"
    );
    assert_eq!(std::fs::read_to_string(root.join("log.txt")).unwrap(), "ab");
    assert_eq!(
        std::fs::read_to_string(root.join("artifacts/report.json")).unwrap(),
        "{}"
    );
    assert!(!root.join("old.tmp").exists());
    assert!(!root.join("out.txt").exists());
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

#[test]
fn write_bytes_limit_surfaces_as_sandbox_oserror() {
    let root = temp_workspace("wlimit");
    let policy = MiniPolicy {
        write_bytes_limit: 10,
        ..MiniPolicy::default()
    };
    let resp = run(
        "open('big.txt', 'w').write('x' * 100)",
        &root,
        true,
        &policy,
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(
        co.stderr.contains("OSError"),
        "expected OSError traceback, got: {}",
        co.stderr
    );
}

#[test]
fn suspension_limit_is_enforced_host_side() {
    let root = temp_workspace("susp");
    let policy = MiniPolicy {
        max_suspensions: 10,
        ..MiniPolicy::default()
    };
    let code = "from pathlib import Path\nfor i in range(20):\n    Path('nope').exists()\nprint('unreachable')";
    let resp = run(code, &root, true, &policy);
    assert_eq!(resp.status, MiniPythonStatus::Error);
    let err = resp.error.unwrap();
    assert_eq!(err.code, MiniPythonErrorCode::ResourceLimit);
    // Partial output may be attached (or absent); the guarded print must
    // never have run.
    assert!(
        resp.code_output
            .as_ref()
            .is_none_or(|co| !co.stdout.contains("unreachable"))
    );
}

#[test]
fn infinite_loop_hits_time_budget() {
    let root = temp_workspace("timeout");
    let policy = MiniPolicy {
        feed_duration: Duration::from_millis(200),
        ..MiniPolicy::default()
    };
    let resp = run("while True: pass", &root, true, &policy);
    assert_eq!(resp.status, MiniPythonStatus::Error);
    let err = resp.error.unwrap();
    assert_eq!(err.code, MiniPythonErrorCode::Timeout);
    assert!(err.message.contains("budget"), "{}", err.message);
}

/// The sandbox wall clock is pinned to UTC, so results do not vary with the
/// host's local zone. Asserted rather than left to monty's default, whose
/// change would otherwise move the sandbox silently.
#[test]
fn wall_clock_is_pinned_to_utc() {
    let root = temp_workspace("tz");
    let resp = run(
        "import time, datetime\nprint(time.timezone)\nprint(time.tzname)\nprint(datetime.datetime.now().hour == time.gmtime(time.time()).tm_hour)",
        &root,
        true,
        &MiniPolicy::default(),
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed, "{:?}", resp);
    let co = completed(&resp);
    let lines: Vec<&str> = co.stdout.lines().collect();
    assert_eq!(lines[0], "0", "UTC offset must be 0: {}", co.stdout);
    assert_eq!(
        lines[1], "('UTC', 'UTC')",
        "zone name must be UTC: {}",
        co.stdout
    );
    // Naive `datetime.now()` must read the UTC wall clock, not the host's.
    assert_eq!(
        lines[2], "True",
        "now() must be on the UTC wall clock: {}",
        co.stdout
    );
}

/// Only monty's own budget timeout maps to the host `TIMEOUT` envelope. A
/// `TimeoutError` the script raises is a sandbox exception and must keep the
/// `completed` + traceback contract, even though `ExcType` is identical.
#[test]
fn script_raised_timeout_error_stays_completed() {
    let root = temp_workspace("timeout_raised");
    let policy = MiniPolicy {
        feed_duration: Duration::from_millis(200),
        ..MiniPolicy::default()
    };
    let resp = run(
        "print('before')\nraise TimeoutError('my own timeout')",
        &root,
        true,
        &policy,
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed, "{:?}", resp);
    assert!(
        resp.error.is_none(),
        "a script exception is not a host error"
    );
    let co = completed(&resp);
    assert!(
        co.stderr.contains("TimeoutError") && co.stderr.contains("my own timeout"),
        "the script's own message must reach the LLM: {}",
        co.stderr
    );
    // The traceback (and the output printed before the raise) is preserved.
    assert!(co.stderr.contains("main.py"), "{}", co.stderr);
    assert_eq!(co.stdout, "before\n");
}

/// A budget timeout is uncatchable, so a script cannot suppress it and turn it
/// into a `completed` run. Pinned so the `TIMEOUT` envelope stays reachable.
#[test]
fn budget_timeout_cannot_be_caught_by_the_script() {
    let root = temp_workspace("timeout_uncatchable");
    let policy = MiniPolicy {
        feed_duration: Duration::from_millis(200),
        ..MiniPolicy::default()
    };
    let resp = run(
        "try:\n    while True: pass\nexcept TimeoutError:\n    print('caught')",
        &root,
        true,
        &policy,
    );
    assert_eq!(resp.status, MiniPythonStatus::Error, "{:?}", resp);
    assert_eq!(resp.error.unwrap().code, MiniPythonErrorCode::Timeout);
}

#[test]
fn infinite_recursion_is_completed_recursion_error() {
    let resp = run_rw("recursion", "def f():\n    return f()\nf()");
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(co.stderr.contains("RecursionError"), "got: {}", co.stderr);
}

#[test]
fn predictable_huge_allocation_hits_soft_memory_limit() {
    let root = temp_workspace("mem");
    let policy = MiniPolicy {
        max_memory: Some(1 << 20),
        ..MiniPolicy::default()
    };
    let resp = run(
        "x = b'x' * (2 * 1024 * 1024)\nprint('unreachable')",
        &root,
        true,
        &policy,
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(
        co.stderr.contains("MemoryError"),
        "expected MemoryError traceback, got: {}",
        co.stderr
    );
    assert!(!co.stdout.contains("unreachable"));
}

#[test]
fn growing_container_hits_soft_memory_limit() {
    let root = temp_workspace("memgrow");
    let policy = MiniPolicy {
        max_memory: Some(1 << 20),
        feed_duration: Duration::from_secs(5),
        ..MiniPolicy::default()
    };
    let resp = run(
        "a = []\nwhile True:\n    a.append(1)\nprint('unreachable')",
        &root,
        true,
        &policy,
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(
        co.stderr.contains("MemoryError"),
        "expected MemoryError traceback, got: {}",
        co.stderr
    );
}

// ---------------------------------------------------------------------------
// Output normalization
// ---------------------------------------------------------------------------

#[test]
fn oversized_output_keeps_tail_and_flags_truncation() {
    let root = temp_workspace("trunc");
    let policy = MiniPolicy {
        output_cap: 1024,
        ..MiniPolicy::default()
    };
    let resp = run("print('A' * 5000)", &root, true, &policy);
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(co.stdout.len() <= 1024, "stdout len {}", co.stdout.len());
    assert!(
        co.stdout.ends_with('\n') && co.stdout.trim_end_matches('\n').chars().all(|c| c == 'A'),
        "tail must be kept: {:?}",
        &co.stdout[..co.stdout.len().min(20)]
    );
    assert!(co.truncated.stdout, "stdout must be flagged truncated");
    assert!(!co.truncated.stderr);
}

#[test]
fn unavailable_when_workspace_cannot_be_opened() {
    let missing = temp_workspace("unavail");
    std::fs::remove_dir_all(&missing).unwrap();
    let resp = run("print(1)", &missing, true, &MiniPolicy::default());
    assert_eq!(resp.status, MiniPythonStatus::Error);
    assert_eq!(resp.error.unwrap().code, MiniPythonErrorCode::Unavailable);
}

// ---------------------------------------------------------------------------
// Security
// ---------------------------------------------------------------------------

#[test]
fn paths_outside_the_workspace_do_not_exist() {
    let root = temp_workspace("fs");
    let code = r#"
for path in ['/etc/passwd', '/tmp', '../secret']:
    try:
        open(path).read()
        print('ESCAPE:' + path)
    except Exception:
        pass
print('done')
"#;
    let resp = run(code, &root, true, &MiniPolicy::default());
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(
        !co.stdout.contains("ESCAPE"),
        "workspace escape must not succeed: {}",
        co.stdout
    );
}

#[test]
fn absolute_symlink_escape_is_refused() {
    let outside_dir = temp_workspace("outside");
    let root = temp_workspace("sym");
    let secret = outside_dir.join("secret.txt");
    std::fs::write(&secret, "TOP_SECRET").unwrap();
    // Absolute-target symlink inside the workspace: refused by the mount.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();

    let resp = run(
        "print(open('link.txt').read())",
        &root,
        true,
        &MiniPolicy::default(),
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(
        !co.stdout.contains("TOP_SECRET"),
        "absolute symlink must not be followed: {}",
        co.stdout
    );
    assert!(!co.stderr.is_empty(), "escape must raise: {}", co.stderr);
}

#[test]
fn environment_and_entropy_are_unavailable() {
    let resp = run_rw(
        "env",
        "import os\nprint(os.getenv('PATH'))\nprint(os.environ)",
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    assert!(
        !co.stdout.contains('/'),
        "getenv must not leak: {}",
        co.stdout
    );
    assert!(!co.stderr.is_empty(), "getenv must raise: {}", co.stderr);

    let resp = run_rw("entropy", "import os\nprint(os.urandom(8))");
    let co = completed(&resp);
    assert!(co.stdout.is_empty());
    assert!(!co.stderr.is_empty(), "urandom must raise: {}", co.stderr);
}

#[test]
fn host_modules_do_not_exist() {
    for module in ["socket", "subprocess", "ctypes", "pandas", "numpy", "csv"] {
        let resp = run_rw("mods", &format!("import {module}\nprint('IMPORTED')"));
        assert_eq!(resp.status, MiniPythonStatus::Completed);
        let co = completed(&resp);
        assert!(
            !co.stdout.contains("IMPORTED"),
            "import {module} must fail, stdout: {}",
            co.stdout
        );
        assert!(
            co.stderr.contains("Error"),
            "import {module} must raise, stderr: {}",
            co.stderr
        );
    }
}

#[test]
fn guard_files_are_modifiable_by_design() {
    // The sandbox bypasses tool-call guards, so todo.md is writable. This
    // test pins that intentional allowance.
    let root = temp_workspace("guard");
    std::fs::write(root.join("todo.md"), "# original\n").unwrap();
    let resp = run(
        "open('todo.md', 'w').write('# overwritten\\n')",
        &root,
        true,
        &MiniPolicy::default(),
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(root.join("todo.md")).unwrap(),
        "# overwritten\n"
    );
}

#[test]
fn host_paths_do_not_leak_into_output() {
    let root = temp_workspace("leak");
    let root_str = root.to_str().unwrap().to_string();
    let resp = run(
        "print(__file__)\nraise ValueError('boom')",
        &root,
        true,
        &MiniPolicy::default(),
    );
    assert_eq!(resp.status, MiniPythonStatus::Completed);
    let co = completed(&resp);
    // __file__ is the virtual path; tracebacks use the script name.
    assert_eq!(co.stdout, "/work/main.py\n");
    assert!(
        co.stderr.contains("File \"main.py\""),
        "traceback must use the script name: {}",
        co.stderr
    );
    assert!(
        !co.stderr.contains(&root_str),
        "host path must not leak: {}",
        co.stderr
    );
}

// ---------------------------------------------------------------------------
// Tool definition / gate
// ---------------------------------------------------------------------------

#[test]
fn description_matches_capability_and_lists_the_subset() {
    // rw: write examples, no denial sentence.
    assert!(TOOL_DESCRIPTION_RW.contains("write_text"));
    assert!(!TOOL_DESCRIPTION_RW.contains("File writes are denied"));
    // ro: read-only examples plus the denial sentence.
    assert!(!TOOL_DESCRIPTION_RO.contains("write_text"));
    assert!(TOOL_DESCRIPTION_RO.contains("File writes are denied"));
    // The module list is the complete subset plus explicit absences.
    for module in [
        "json",
        "re",
        "math",
        "collections",
        "datetime",
        "itertools",
        "functools",
        "pathlib",
        "asyncio",
        "dataclasses",
        "base64",
        "binascii",
        "copy",
        "os",
        "random",
        "time",
        "typing",
        "sys",
        "unicodedata",
    ] {
        assert!(
            TOOL_DESCRIPTION_RW.contains(module),
            "description must list module {module}"
        );
    }
    for absent in [
        "csv", "hashlib", "struct", "io", "uuid", "sqlite3", "numpy", "pandas",
    ] {
        assert!(
            TOOL_DESCRIPTION_RW.contains(absent),
            "description must name absent module {absent}"
        );
    }
}

#[test]
fn tool_definitions_carry_the_mini_tool() {
    use crate::startup::MiniPythonAutoConfirm;
    let defs = crate::tools::get_tool_definitions(None, None, MiniPythonAutoConfirm::Ask, |_| true);
    let mini = defs
        .iter()
        .find(|d| d["function"]["name"] == "mini_python_interpreter")
        .expect("mini_python_interpreter must be in the default tool list");
    assert_eq!(mini["function"]["description"], TOOL_DESCRIPTION_RW);
    assert_eq!(
        mini["function"]["parameters"]["required"],
        serde_json::json!(["code"])
    );

    // ro sessions get the read-only description (no write examples).
    let defs = crate::tools::get_tool_definitions(None, None, MiniPythonAutoConfirm::Ro, |_| true);
    let mini = defs
        .iter()
        .find(|d| d["function"]["name"] == "mini_python_interpreter")
        .unwrap();
    assert_eq!(mini["function"]["description"], TOOL_DESCRIPTION_RO);

    // The tool respects --only-tools filtering like every other tool.
    let defs = crate::tools::get_tool_definitions(None, None, MiniPythonAutoConfirm::Ask, |n| {
        n != "mini_python_interpreter"
    });
    assert!(
        defs.iter()
            .all(|d| d["function"]["name"] != "mini_python_interpreter")
    );
}

#[tokio::test]
async fn mini_python_uses_its_own_gate_only() {
    use crate::startup::MiniPythonAutoConfirm;
    use crate::tools::{ToolRunDecisionKind, confirm_execute_tool};
    let args = &serde_json::json!({ "code": "print(1)" });

    // ro: auto-approved, read-only.
    let d = confirm_execute_tool(
        "mini_python_interpreter",
        args,
        false,
        false,
        crate::startup::KbAutoConfirm::Ask,
        MiniPythonAutoConfirm::Ro,
        true,
        |_| true,
    )
    .await;
    assert!(d.proceed);
    assert_eq!(d.kind, ToolRunDecisionKind::AutoConfirm);
    assert_eq!(d.mini_python_write, Some(false));

    // rw: auto-approved, read-write.
    let d = confirm_execute_tool(
        "mini_python_interpreter",
        args,
        false,
        false,
        crate::startup::KbAutoConfirm::Ask,
        MiniPythonAutoConfirm::Rw,
        true,
        |_| true,
    )
    .await;
    assert!(d.proceed);
    assert_eq!(d.mini_python_write, Some(true));

    // ask in batch mode: denied (no stdin).
    let d = confirm_execute_tool(
        "mini_python_interpreter",
        args,
        false,
        false,
        crate::startup::KbAutoConfirm::Ask,
        MiniPythonAutoConfirm::Ask,
        true,
        |_| true,
    )
    .await;
    assert!(!d.proceed);

    // The global --unsafe-reflex must NOT approve mini python.
    let d = confirm_execute_tool(
        "mini_python_interpreter",
        args,
        true,
        false,
        crate::startup::KbAutoConfirm::Ask,
        MiniPythonAutoConfirm::Ask,
        true,
        |_| true,
    )
    .await;
    assert!(
        !d.proceed,
        "--unsafe-reflex must never approve arbitrary code"
    );

    // Non-mini tools carry no mini decision.
    let d = confirm_execute_tool(
        "calc",
        &serde_json::json!({ "expressions": ["1 + 1"] }),
        true,
        false,
        crate::startup::KbAutoConfirm::Ask,
        MiniPythonAutoConfirm::Ask,
        true,
        |_| true,
    )
    .await;
    assert!(d.proceed);
    assert_eq!(d.mini_python_write, None);
}

#[test]
fn dispatch_reports_missing_decision_as_internal_error() {
    // Executing mini python without an approval decision is an internal bug,
    // reported as such (never executed, never a guess at the mode).
    let context = crate::tools::ToolExecutionContext::new(None, None, None, 0, None, |_| true);
    let res = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(crate::tools::execute_tool(
            "mini_python_interpreter",
            &serde_json::json!({ "code": "print(1)" }),
            &context,
            None,
        ))
        .unwrap();
    assert_eq!(res["status"], "error");
    assert_eq!(res["error"]["code"], "INTERNAL_ERROR");
}

#[test]
fn compat_infers_mini_python_from_code_arg() {
    use crate::model::{FunctionCall, ToolCall};
    use crate::startup::MiniPythonAutoConfirm;

    let defs = crate::tools::get_tool_definitions(None, None, MiniPythonAutoConfirm::Ask, |_| true);
    let mut calls = vec![ToolCall {
        id: String::new(),
        tool_type: "function".to_string(),
        function: FunctionCall {
            name: String::new(),
            arguments: serde_json::json!({ "code": "print(1)" }),
        },
        thought_signature: None,
    }];
    crate::compat_resilience::post_process_tool_calls(&mut calls, &defs);
    assert_eq!(
        calls[0].function.name, "mini_python_interpreter",
        "a call with only a 'code' argument must infer mini_python_interpreter"
    );
}
