//! Fresh-session starter (todo-refine Phase 0, Inner owner).
//!
//! Wraps `run_reasoning_loop` for one fresh session: new `Session`,
//! per-session tool narrowing, and report condense. Adds no runtime entity.

use anyhow::Result;

use crate::model::{Message, Session};
use crate::reasoning::{EndReason, LoopCtx, run_reasoning_loop};
use crate::{persistence, startup, todo_guard};

/// Per-session tool narrowing, composed with `--only-tools` at every
/// `is_enabled` site (see `tool_enabled`). `Inherit` changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolPolicy {
    Inherit,
    AllowList(&'static [&'static str]),
}

impl ToolPolicy {
    /// Whether `name` passes this policy alone (compose with config outside).
    pub(crate) fn allows(self, name: &str) -> bool {
        match self {
            ToolPolicy::Inherit => true,
            ToolPolicy::AllowList(list) => list.contains(&name),
        }
    }
}

/// Effective enablement: global `--only-tools` narrowed by the session policy.
pub(crate) fn tool_enabled(config: &startup::Config, policy: ToolPolicy, name: &str) -> bool {
    config.is_tool_enabled(name) && policy.allows(name)
}

/// Final-report contract: required headings + length cap (condense retry over).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReportRule {
    pub title: &'static str,
    pub fields: &'static [&'static str],
    pub max_chars: usize,
}

/// One fresh session's inputs (transient; never persisted).
#[derive(Debug)]
pub(crate) struct SessionSpec {
    pub label: String,
    pub system: Message,
    pub instruction: String,
    pub allow_tools: ToolPolicy,
    pub report_rule: ReportRule,
    pub max_turns: Option<u32>,
}

/// One fresh session's result.
#[derive(Debug, Clone)]
pub(crate) struct SessionOutcome {
    pub end_reason: EndReason,
    pub report: Option<String>,
    /// Pre-condense text when condensing rewrote the message (the planner
    /// parses structured blocks from here).
    pub raw_report: Option<String>,
}

/// Run one fresh session: new `Session` (history 0), narrowed tools,
/// condense-on-completion. The caller's `ctx` policy is untouched; the
/// narrowing travels on a short-lived inner `LoopCtx`, as does the
/// per-session turn cap (via a cloned config, only when `Some`).
pub(crate) async fn run_session(
    ctx: &mut LoopCtx<'_>,
    spec: SessionSpec,
) -> Result<SessionOutcome> {
    let call_label = spec.label.clone();
    let mut sess = Session::new(spec.label.clone(), spec.system);
    // Move a leftover file from an earlier interrupted run aside.
    persistence::init_session(&sess.label)?;

    let overridden: Option<startup::Config> = spec.max_turns.map(|n| {
        let mut c = (*ctx.config).clone();
        c.max_reasoning_turns = n;
        c
    });
    let cfg: &startup::Config = overridden.as_ref().unwrap_or(ctx.config);
    let mut inner = LoopCtx {
        config: cfg,
        provider: ctx.provider,
        settings: &mut *ctx.settings,
        metrics: &mut *ctx.metrics,
        plan_guard: ctx.plan_guard.take(),
        kb_ctx: ctx.kb_ctx,
        tool_policy: spec.allow_tools,
    };
    let res = run_reasoning_loop(
        &mut inner,
        &mut sess,
        &call_label,
        spec.instruction,
        Vec::new(),
    )
    .await;
    match res {
        Err(e) => {
            ctx.plan_guard = inner.plan_guard.take();
            Err(e)
        }
        Ok(end_reason) => {
            // Condense runs only on completion; anything else keeps no report.
            let (report, raw_report) = if end_reason.is_completed() {
                let raw = todo_guard::last_assistant_report(&sess).map(str::to_string);
                todo_guard::llm_guard_condense_final_message(
                    &mut inner,
                    &mut sess,
                    spec.report_rule.title,
                    spec.report_rule.fields,
                    spec.report_rule.max_chars,
                )
                .await;
                let report = todo_guard::last_assistant_report(&sess).map(str::to_string);
                let raw_report = match (&raw, &report) {
                    (Some(r), Some(f)) if r != f => Some(r.clone()),
                    _ => None,
                };
                (report, raw_report)
            } else {
                (None, None)
            };
            ctx.plan_guard = inner.plan_guard.take();
            Ok(SessionOutcome {
                end_reason,
                report,
                raw_report,
            })
        }
    }
}

/// Build a budgeted prompt: `base` always fully included, then the most
/// recent reports that fit `max_chars` (chars, newest wins, chronological
/// order). Whole reports only, so no UTF-8 boundary risk. The single most
/// recent report is always kept, even over budget, to never go in silent.
pub(crate) fn assemble_instruction(base: &str, prior_reports: &[&str], max_chars: usize) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut used: usize = 0;
    for r in prior_reports.iter().rev() {
        let len = r.chars().count() + 1; // + separator newline
        if used + len > max_chars && !kept.is_empty() {
            break;
        }
        used += len;
        kept.push(r);
    }
    kept.reverse();
    if kept.is_empty() {
        return base.to_string();
    }
    format!("{}\n{}", base, kept.join("\n"))
}

#[cfg(test)]
#[path = "tests/session_test.rs"]
mod tests;
