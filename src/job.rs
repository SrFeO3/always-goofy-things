//! Job runner (todo-refine Phase 1, Outer owner).
//!
//! Runs one task at a time: enumerate, fresh session, verify, record.
//! Retries failed tasks fresh; warnings proceed; interruptions stop resumable.

use anyhow::Result;

use crate::reasoning::LoopCtx;
use crate::session::{SessionOutcome, SessionSpec, run_session};

/// One unit of work: what to do and how completion is mechanically judged.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub id: String,          // Reopen key. Unique within the plan
    pub description: String, // Work text for the LLM
    pub verify: Vec<Check>,  // Completion conditions. All true = done
}

/// Completion condition. Mechanical judge (see `Check::parse` for the DSL).
#[derive(Debug, Clone)]
pub(crate) enum Check {
    FileExists(String),           // path exists
    FileNonEmpty(String),         // exists and non-empty
    FileContains(String, String), // (path, needle)
    SqlEmpty(String),             // zero rows = success. KB use
}

impl Check {
    /// Parse one `verify` string: `exists <path>` / `nonempty <path>` /
    /// `contains <path>:<needle>` (split at the first `:`) / `sql <query>`.
    pub(crate) fn parse(s: &str) -> Result<Check> {
        let t = s.trim();
        if let Some(p) = t.strip_prefix("exists ") {
            let p = p.trim();
            if p.is_empty() {
                anyhow::bail!("bad check {:?}: `exists` needs a path", s);
            }
            return Ok(Check::FileExists(p.to_string()));
        }
        if let Some(p) = t.strip_prefix("nonempty ") {
            let p = p.trim();
            if p.is_empty() {
                anyhow::bail!("bad check {:?}: `nonempty` needs a path", s);
            }
            return Ok(Check::FileNonEmpty(p.to_string()));
        }
        if let Some(r) = t.strip_prefix("contains ") {
            let Some(i) = r.find(':') else {
                anyhow::bail!("bad check {:?}: `contains` needs `<path>:<needle>`", s);
            };
            let (path, needle) = (r[..i].trim(), r[i + 1..].to_string());
            if path.is_empty() || needle.is_empty() {
                anyhow::bail!("bad check {:?}: `contains` needs `<path>:<needle>`", s);
            }
            return Ok(Check::FileContains(path.to_string(), needle));
        }
        if let Some(q) = t.strip_prefix("sql ") {
            let q = q.trim();
            if q.is_empty() {
                anyhow::bail!("bad check {:?}: `sql` needs a query", s);
            }
            return Ok(Check::SqlEmpty(q.to_string()));
        }
        anyhow::bail!(
            "bad check {:?}: expected `exists` / `nonempty` / `contains` / `sql`",
            s
        )
    }

    /// Evaluate against the workspace (and `sql_probe` for `SqlEmpty`).
    /// Probe errors become `Fail`, never abort: syntax faults must surface
    /// as retry-then-failed, not as a runner crash.
    pub(crate) fn eval(&self, sql_probe: &dyn Fn(&str) -> Result<bool>) -> VerifyResult {
        match self {
            Check::FileExists(p) => {
                if std::path::Path::new(p).exists() {
                    VerifyResult::Pass
                } else {
                    VerifyResult::Fail {
                        reason: format!("missing {}", p),
                    }
                }
            }
            Check::FileNonEmpty(p) => match std::fs::metadata(p) {
                Ok(m) if m.len() > 0 => VerifyResult::Pass,
                Ok(_) => VerifyResult::Fail {
                    reason: format!("empty {}", p),
                },
                Err(e) => VerifyResult::Fail {
                    reason: format!("unreadable {}: {}", p, e),
                },
            },
            Check::FileContains(p, needle) => match std::fs::read_to_string(p) {
                Ok(c) if c.contains(needle) => VerifyResult::Pass,
                Ok(_) => VerifyResult::Fail {
                    reason: format!("{} lacks {:?}", p, needle),
                },
                Err(e) => VerifyResult::Fail {
                    reason: format!("unreadable {}: {}", p, e),
                },
            },
            Check::SqlEmpty(q) => match sql_probe(q) {
                Ok(true) => VerifyResult::Pass,
                Ok(false) => VerifyResult::Fail {
                    reason: "sql returned rows".to_string(),
                },
                Err(e) => VerifyResult::Fail {
                    reason: format!("sql error: {:#}", e),
                },
            },
        }
    }
}

/// Per-task verdict.
#[derive(Debug, Clone)]
pub(crate) enum VerifyResult {
    Pass,
    Fail { reason: String },
    Warn { reason: String }, // Proceed with a warning
}

/// Whole-job status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobStatus {
    Completed,
    Failed,
    Interrupted, // State kept; rerun resumes
}

/// Whole-job result.
#[derive(Debug, Clone)]
pub(crate) struct JobOutcome {
    pub status: JobStatus,
    pub summary: String, // Completion report text
}

/// Next one task (`None` = done). Dynamic replans inside before answering.
pub(crate) trait Enumerator {
    async fn next_task(&mut self, ctx: &mut LoopCtx<'_>) -> Result<Option<Task>>;
}

/// Mechanical judge: one task, then the whole job (deliverables).
pub(crate) trait Verifier {
    fn check(&self, ctx: &LoopCtx<'_>, task: &Task) -> Result<VerifyResult>;
    fn finalize(&self, ctx: &LoopCtx<'_>) -> Result<JobOutcome>;
}

/// Persisted state (reopen key = `Task.id`).
pub(crate) trait Store {
    fn record(
        &self,
        ctx: &LoopCtx<'_>,
        task: &Task,
        outcome: &SessionOutcome,
        verdict: &VerifyResult,
    ) -> Result<()>;
}

/// Runner limits (from CLI in adapters).
#[derive(Debug, Clone, Copy)]
pub(crate) struct JobOptions {
    pub max_retries: u32, // Fresh retries per task after the first attempt
}

/// Run the job loop. `Err` only on machinery failure; task outcomes
/// (pass / fail / interrupt) come back as `JobOutcome`.
pub(crate) async fn run_job<E, V, S>(
    ctx: &mut LoopCtx<'_>,
    enumerator: &mut E,
    verifier: &V,
    store: &S,
    options: &JobOptions,
    build: impl Fn(&Task) -> SessionSpec,
) -> Result<JobOutcome>
where
    E: Enumerator,
    V: Verifier,
    S: Store,
{
    loop {
        let task = match enumerator.next_task(ctx).await? {
            None => break,
            Some(t) => t,
        };
        println!("--- [Job] {}: {} ---", task.id, task.description);
        let mut retries = 0u32;
        loop {
            // Fatal machinery faults abort; task outcomes stop resumable.
            let outcome = run_session(ctx, build(&task)).await?;
            if !outcome.end_reason.is_completed() {
                // Best-effort verdict so `--status` shows the attempt;
                // the task stays pending either way.
                let verdict = verifier.check(ctx, &task).unwrap_or(VerifyResult::Fail {
                    reason: format!("interrupted: {:?}", outcome.end_reason),
                });
                store.record(ctx, &task, &outcome, &verdict)?;
                return Ok(JobOutcome {
                    status: JobStatus::Interrupted,
                    summary: format!(
                        "interrupted at {} ({:?}); rerun to resume",
                        task.id, outcome.end_reason
                    ),
                });
            }
            let verdict = verifier.check(ctx, &task)?;
            store.record(ctx, &task, &outcome, &verdict)?;
            match &verdict {
                VerifyResult::Pass => {
                    println!("[ok] {}", task.id);
                    break;
                }
                VerifyResult::Warn { reason } => {
                    println!("[warn] {}: {}", task.id, reason);
                    break;
                }
                VerifyResult::Fail { reason } if retries < options.max_retries => {
                    retries += 1;
                    println!(
                        "[retry {}/{}] {}: {}",
                        retries, options.max_retries, task.id, reason
                    );
                    continue;
                }
                VerifyResult::Fail { reason } => {
                    return Ok(JobOutcome {
                        status: JobStatus::Failed,
                        summary: format!("task {} failed: {}", task.id, reason),
                    });
                }
            }
        }
    }
    verifier.finalize(ctx)
}

#[cfg(test)]
mod tests {
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
}
