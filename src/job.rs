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
    /// Render back to the plan string form (`parse` roundtrips it).
    pub(crate) fn to_dsl(&self) -> String {
        match self {
            Check::FileExists(p) => format!("exists {}", p),
            Check::FileNonEmpty(p) => format!("nonempty {}", p),
            Check::FileContains(p, needle) => format!("contains {}:{}", p, needle),
            Check::SqlEmpty(q) => format!("sql {}", q),
        }
    }

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
    Fail {
        reason: String,
    },
    /// Proceed with a warning. Constructed only under `--features kb`
    /// (extract heading rule); allowed dead elsewhere.
    #[cfg_attr(not(feature = "kb"), allow(dead_code))]
    Warn {
        reason: String,
    },
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
    build: impl Fn(&Task) -> Result<SessionSpec>,
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
            // A failing `build` (lost chunk rows) aborts the same way.
            let outcome = run_session(ctx, build(&task)?).await?;
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
#[path = "tests/job_test.rs"]
mod tests;
