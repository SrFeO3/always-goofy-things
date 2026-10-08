//! Tests for `src/job.rs`: check DSL, eval determinism, empty plan.

use super::*;
use crate::compat_provider::{LlmProvider, ProviderExtra};
use crate::compat_resilience::ToolResultFormat;
use crate::llm_stats::Metrics;
use crate::model::Settings;
use crate::session::ToolPolicy;
use crate::startup::{Config, KbAutoConfirm, MiniPythonAutoConfirm};
use crate::tools::ToolName;
use anyhow::anyhow;

// -- DSL parse --

#[test]
fn parse_four_kinds() {
    assert!(matches!(
        Check::parse("exists artifacts/a.md"),
        Ok(Check::FileExists(_))
    ));
    assert!(matches!(
        Check::parse("nonempty artifacts/a.md"),
        Ok(Check::FileNonEmpty(_))
    ));
    match Check::parse("contains artifacts/a.md:矛盾:追記") {
        Ok(Check::FileContains(p, n)) => {
            assert_eq!(p, "artifacts/a.md");
            assert_eq!(n, "矛盾:追記"); // needle keeps inner `:`
        }
        other => panic!("unexpected {:?}", other),
    }
    assert!(matches!(
        Check::parse("sql SELECT 1"),
        Ok(Check::SqlEmpty(_))
    ));
}

#[test]
fn parse_rejects_bad_forms() {
    for bad in [
        "",
        "bogus x",
        "exists ",
        "nonempty ",
        "contains no-colon-here",
        "contains :needle-only",
        "sql ",
    ] {
        assert!(Check::parse(bad).is_err(), "{:?} must fail", bad);
    }
}

// -- eval determinism (isolated temp dir) --

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "job-check-test-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst),
        tag
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn eval_file_checks_are_deterministic() {
    let dir = scratch_dir("files");
    let present = dir.join("a.md");
    let empty = dir.join("b.md");
    std::fs::write(&present, "hello 矛盾\n").unwrap();
    std::fs::write(&empty, "").unwrap();
    let missing = dir.join("nope.md");
    let no_probe = |_: &str| -> Result<bool> { unreachable!() };

    let ps = present.to_str().unwrap();
    let es = empty.to_str().unwrap();
    let ms = missing.to_str().unwrap();
    // Same state twice: identical verdicts.
    for _ in 0..2 {
        assert!(matches!(
            Check::FileExists(ps.to_string()).eval(&no_probe),
            VerifyResult::Pass
        ));
        assert!(matches!(
            Check::FileExists(ms.to_string()).eval(&no_probe),
            VerifyResult::Fail { .. }
        ));
        assert!(matches!(
            Check::FileNonEmpty(ps.to_string()).eval(&no_probe),
            VerifyResult::Pass
        ));
        assert!(matches!(
            Check::FileNonEmpty(es.to_string()).eval(&no_probe),
            VerifyResult::Fail { .. }
        ));
        assert!(matches!(
            Check::FileContains(ps.to_string(), "矛盾".to_string()).eval(&no_probe),
            VerifyResult::Pass
        ));
        assert!(matches!(
            Check::FileContains(ps.to_string(), "absent".to_string()).eval(&no_probe),
            VerifyResult::Fail { .. }
        ));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn eval_sql_delegates_to_probe() {
    let empty_probe = |_: &str| -> Result<bool> { Ok(true) };
    let rows_probe = |_: &str| -> Result<bool> { Ok(false) };
    let err_probe = |_: &str| -> Result<bool> { Err(anyhow!("syntax fault")) };
    assert!(matches!(
        Check::SqlEmpty("SELECT 1".to_string()).eval(&empty_probe),
        VerifyResult::Pass
    ));
    assert!(matches!(
        Check::SqlEmpty("SELECT 1".to_string()).eval(&rows_probe),
        VerifyResult::Fail { .. }
    ));
    // Probe faults become Fail (retry-then-failed), never a crash.
    assert!(matches!(
        Check::SqlEmpty("SELECT 1".to_string()).eval(&err_probe),
        VerifyResult::Fail { .. }
    ));
}

// -- run_job: empty plan exits via finalize without any session --

struct Empty;
struct Done;
struct NoopStore;

impl Enumerator for Empty {
    async fn next_task(&mut self, _ctx: &mut LoopCtx<'_>) -> Result<Option<Task>> {
        Ok(None)
    }
}

impl Verifier for Done {
    fn check(&self, _ctx: &LoopCtx<'_>, _task: &Task) -> Result<VerifyResult> {
        unreachable!("no tasks enumerated")
    }
    fn finalize(&self, _ctx: &LoopCtx<'_>) -> Result<JobOutcome> {
        Ok(JobOutcome {
            status: JobStatus::Completed,
            summary: "nothing to do".to_string(),
        })
    }
}

impl Store for NoopStore {
    fn record(
        &self,
        _ctx: &LoopCtx<'_>,
        _task: &Task,
        _outcome: &SessionOutcome,
        _verdict: &VerifyResult,
    ) -> Result<()> {
        Ok(())
    }
}

fn test_config() -> Config {
    Config {
        working_dir: ".".to_string(),
        todo_mode: 0,
        llm_url: "http://localhost:11434/api/chat".to_string(),
        llm_model: "test-model".to_string(),
        llm_api_key: None,
        unsafe_reflex: false,
        verbose_level: 0,
        pretty_level: 0,
        llm_rpm: 0,
        max_output_tokens: 16,
        max_reasoning_empty_responses: 0,
        session_label: "job-test".to_string(),
        provider: None,
        provider_extras: Vec::<ProviderExtra>::new(),
        tool_result_format: ToolResultFormat::JsonString,
        only_tools: Vec::<ToolName>::new(),
        query: None,
        output_file: None,
        max_reasoning_turns: 30,
        max_replan_attempts: 3,
        max_tool_output_bytes: 1024,
        tool_timeout_secs: 30,
        db_type: None,
        db_url: None,
        db_auth_key: None,
        db_timeout: 30,
        db_max_bytes: 1024,
        db_unsafe_reflex: false,
        kb_dir: None,
        kb_auto_confirm: KbAutoConfirm::Ask,
        kb_max_bytes: 1024,
        mini_python_auto_confirm: MiniPythonAutoConfirm::Ask,
        command: None,
    }
}

#[tokio::test]
async fn empty_plan_completes_without_sessions() {
    let config = test_config();
    let mut settings = Settings::from_config(&config);
    let mut metrics = Metrics::default();
    let mut ctx = LoopCtx {
        config: &config,
        provider: LlmProvider::Ollama,
        settings: &mut settings,
        metrics: &mut metrics,
        plan_guard: None,
        kb_ctx: None,
        tool_policy: ToolPolicy::Inherit,
    };
    let mut enumerator = Empty;
    let options = JobOptions { max_retries: 1 };
    let outcome = run_job(
        &mut ctx,
        &mut enumerator,
        &Done,
        &NoopStore,
        &options,
        |_: &Task| unimplemented!("no tasks enumerated"),
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, JobStatus::Completed);
    assert_eq!(outcome.summary, "nothing to do");
}
