//! LLM resource usage: per-call records, aggregation and display helpers.
//!
//! Aggregates per-call records into per-model and session totals and formats
//! the `[Tokens]` line. Tool executions are counted, not recorded; the call
//! log is persisted by `persistence`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::compat_provider::LlmProvider;
use crate::model::Usage;

/// Outcome status of a single LLM call.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CallStatus {
    /// Normal response (content and/or tool calls).
    #[default]
    Ok,
    /// Empty response (no content and no tool calls), usually retried.
    Empty,
    /// HTTP-level failure (non-2xx status).
    HttpError,
}

/// One LLM API call: the atomic unit of resource accounting.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct LlmCallRecord {
    pub(crate) timestamp: chrono::DateTime<chrono::Utc>,
    pub(crate) model: String,
    pub(crate) provider: LlmProvider,
    /// Caller-supplied call context label, e.g. `"main"` / `"todo:task:3"`.
    pub(crate) call_label: String,
    pub(crate) usage: Usage,
    /// Request-send to stream-completion latency (ms).
    pub(crate) latency_ms: u128,
    /// Time to the first received chunk (ms).
    pub(crate) ttft_ms: u128,
    /// Serialized request payload bytes (`req_json.len()`).
    pub(crate) request_bytes: usize,
    /// Received chunk bytes (SSE overhead included).
    pub(crate) response_bytes: usize,
    /// Consecutive empty responses that preceded this call.
    pub(crate) retry_count: u32,
    pub(crate) status: CallStatus,
}

/// Information about one LLM request (latency / TTFT / request & response
/// bytes), captured in `reasoning::call_llm`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LlmRequestInfo {
    pub latency_ms: u128,
    pub ttft_ms: u128,
    pub request_bytes: usize,
    pub response_bytes: usize,
}

/// Per-model (or session-wide) aggregation of LLM resource usage.
#[derive(Clone, Debug, Default)]
pub(crate) struct ModelTotals {
    /// Billable input (prompt - cached - cache_write - audio).
    pub(crate) in_normal: u64,
    /// Cache-read (discounted) input.
    pub(crate) in_cached: u64,
    /// Cache-write (billed at the normal input rate).
    pub(crate) in_cache_write: u64,
    pub(crate) in_audio: u64,
    /// Normal output (completion - reasoning).
    pub(crate) out_normal: u64,
    pub(crate) out_reasoning: u64,
    pub(crate) calls: u64,
    pub(crate) errors: u64,
    pub(crate) empties: u64,
    /// Tool calls requested by the model (counted, not stored on records).
    pub(crate) tool_calls: u64,
    pub(crate) llm_ms_total: u64,
    pub(crate) llm_ms_min: u64,
    pub(crate) llm_ms_max: u64,
    pub(crate) ttft_ms_total: u64,
}

impl ModelTotals {
    /// Accumulate one call record (with its tool-call count) into this totals.
    pub(crate) fn accumulate(&mut self, rec: &LlmCallRecord, tool_call_count: usize) {
        let details = rec.usage.prompt_tokens_details.as_ref();
        let cached = details.map(|d| d.cached_tokens as u64).unwrap_or(0);
        let cache_write = details.map(|d| d.cache_creation_tokens as u64).unwrap_or(0);
        let audio = details.map(|d| d.audio_tokens as u64).unwrap_or(0);
        let prompt = rec.usage.prompt_tokens as u64;
        let completion = rec.usage.completion_tokens as u64;
        let reasoning = rec
            .usage
            .completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens as u64)
            .unwrap_or(0);

        self.in_normal += prompt.saturating_sub(cached + cache_write + audio);
        self.in_cached += cached;
        self.in_cache_write += cache_write;
        self.in_audio += audio;
        self.out_normal += completion.saturating_sub(reasoning);
        self.out_reasoning += reasoning;

        self.calls += 1;
        match rec.status {
            CallStatus::Ok => {}
            CallStatus::Empty => self.empties += 1,
            CallStatus::HttpError => self.errors += 1,
        }
        self.tool_calls += tool_call_count as u64;

        let ms = rec.latency_ms as u64;
        self.llm_ms_total += ms;
        if self.calls == 1 || ms < self.llm_ms_min {
            self.llm_ms_min = ms;
        }
        if ms > self.llm_ms_max {
            self.llm_ms_max = ms;
        }
        self.ttft_ms_total += rec.ttft_ms as u64;
    }
}

/// Session-wide aggregation. The per-call `calls` log is capped
/// (`MAX_CALL_RECORDS`); the aggregates always span every call.
#[derive(Clone, Debug, Default)]
pub(crate) struct Metrics {
    pub(crate) calls: Vec<LlmCallRecord>,
    pub(crate) by_model: HashMap<String, ModelTotals>,
    pub(crate) totals: ModelTotals,
}

/// Cap on the in-memory per-call log (oldest records dropped).
pub(crate) const MAX_CALL_RECORDS: usize = 5000;

impl Metrics {
    /// Record one call: update per-model + session totals and append the
    /// record to the (capped) log.
    pub(crate) fn record_call(&mut self, rec: LlmCallRecord, tool_call_count: usize) {
        self.totals.accumulate(&rec, tool_call_count);
        self.by_model
            .entry(rec.model.clone())
            .or_default()
            .accumulate(&rec, tool_call_count);
        self.calls.push(rec);
        if self.calls.len() > MAX_CALL_RECORDS {
            let overflow = self.calls.len() - MAX_CALL_RECORDS;
            self.calls.drain(..overflow);
        }
    }

    /// Rebuild the aggregates from records (e.g. `load_stats`). Tool-call
    /// counts are not stored on records, so they reset to 0.
    pub(crate) fn from_records(records: Vec<LlmCallRecord>) -> Self {
        let mut m = Metrics::default();
        for rec in records {
            m.record_call(rec, 0);
        }
        m
    }
}

/// Format a token count as `12.3K (12345)`.
pub(crate) fn fmt_tokens(n: u64) -> String {
    format!("{:.1}K ({})", n as f64 / 1000.0, n)
}

/// Format a millisecond duration as `38.1s` / `1m 2.3s`.
pub(crate) fn fmt_ms(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs >= 60.0 {
        format!("{:.0}m {:.1}s", secs / 60.0, secs % 60.0)
    } else {
        format!("{:.1}s", secs)
    }
}

/// Build the `[Tokens]` display line (plain text, no ANSI) for one call.
///
/// Turn values come from the call's own `Usage`; totals come from the
/// already-accumulated session `totals` (they include the current call).
pub(crate) fn format_token_line(usage: &Usage, totals: &ModelTotals) -> String {
    let details = usage.prompt_tokens_details.as_ref();
    let cached = details.map(|d| d.cached_tokens as u64).unwrap_or(0);
    let cache_write = details.map(|d| d.cache_creation_tokens as u64).unwrap_or(0);
    let prompt = usage.prompt_tokens as u64;
    let normal = prompt.saturating_sub(cached + cache_write);
    let completion = usage.completion_tokens as u64;
    let reasoning = usage
        .completion_tokens_details
        .as_ref()
        .map(|d| d.reasoning_tokens as u64)
        .unwrap_or(0);
    let out_normal = completion.saturating_sub(reasoning);
    let cache_supported = details.is_some();

    let cache_turn = if cache_supported {
        fmt_tokens(cached)
    } else {
        "---".to_string()
    };
    let cachew_turn = if cache_supported {
        fmt_tokens(cache_write)
    } else {
        "---".to_string()
    };
    let mut turn = format!(
        "In {}, Cache {}, CacheW {}, Out {}",
        fmt_tokens(normal),
        cache_turn,
        cachew_turn,
        fmt_tokens(out_normal)
    );
    if reasoning > 0 {
        turn.push_str(&format!(" (Reasoning {})", fmt_tokens(reasoning)));
    }

    let has_cache = totals.in_cached > 0 || totals.in_cache_write > 0;
    let total_in = totals.in_normal + totals.in_cached + totals.in_cache_write + totals.in_audio;
    let cache_total = if has_cache {
        fmt_tokens(totals.in_cached)
    } else {
        "---".to_string()
    };
    let cachew_total = if has_cache {
        fmt_tokens(totals.in_cache_write)
    } else {
        "---".to_string()
    };
    let mut total = format!(
        "In {}, Cache {}, CacheW {}, Out {}",
        fmt_tokens(total_in),
        cache_total,
        cachew_total,
        fmt_tokens(totals.out_normal)
    );
    if totals.out_reasoning > 0 {
        total.push_str(&format!(
            " (Reasoning {})",
            fmt_tokens(totals.out_reasoning)
        ));
    }

    format!("Turn: {} | Total: {}", turn, total)
}

#[cfg(test)]
#[path = "tests/llm_stats_test.rs"]
mod tests;
