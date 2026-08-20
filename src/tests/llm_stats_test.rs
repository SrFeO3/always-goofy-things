//! Tests for `src/llm_stats.rs`: resource aggregation and token-line display.

use super::*;
use crate::compat_provider::LlmProvider;
use crate::model::{CompletionTokensDetails, PromptTokensDetails};

/// Build a call record plus its tool-call count for aggregation tests.
#[allow(clippy::too_many_arguments)]
fn rec(
    model: &str,
    phase: &str,
    prompt: u32,
    cached: u32,
    write: u32,
    completion: u32,
    reasoning: u32,
    status: CallStatus,
    latency: u128,
    tool_calls: usize,
) -> (LlmCallRecord, usize) {
    let usage = Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        prompt_tokens_details: (cached > 0 || write > 0).then_some(PromptTokensDetails {
            cached_tokens: cached,
            cache_creation_tokens: write,
            audio_tokens: 0,
        }),
        completion_tokens_details: (reasoning > 0).then_some(CompletionTokensDetails {
            reasoning_tokens: reasoning,
        }),
    };
    (
        LlmCallRecord {
            timestamp: chrono::Utc::now(),
            model: model.to_string(),
            provider: LlmProvider::OpenAi,
            phase: phase.to_string(),
            usage,
            latency_ms: latency,
            ttft_ms: 10,
            request_bytes: 1,
            response_bytes: 2,
            retry_count: 0,
            status,
        },
        tool_calls,
    )
}

#[test]
fn test_accumulate_separates_cache_write_and_reasoning() {
    let mut t = ModelTotals::default();
    let (r, tc) = rec(
        "gpt-4o",
        "main",
        1000,
        200,
        100,
        300,
        50,
        CallStatus::Ok,
        100,
        2,
    );
    t.accumulate(&r, tc);

    assert_eq!(t.in_normal, 700); // 1000 - 200 cached - 100 cache_write
    assert_eq!(t.in_cached, 200);
    assert_eq!(t.in_cache_write, 100);
    assert_eq!(t.out_normal, 250); // 300 - 50 reasoning
    assert_eq!(t.out_reasoning, 50);
    assert_eq!(t.calls, 1);
    assert_eq!(t.tool_calls, 2);
    assert_eq!(t.llm_ms_total, 100);
    assert_eq!(t.llm_ms_min, 100);
    assert_eq!(t.llm_ms_max, 100);
    assert_eq!(t.ttft_ms_total, 10);
}

#[test]
fn test_metrics_aggregates_by_model_and_counts_statuses() {
    let mut m = Metrics::default();
    let (r1, tc1) = rec(
        "gpt-4o",
        "main",
        1000,
        200,
        100,
        300,
        50,
        CallStatus::Ok,
        100,
        2,
    );
    let (r2, tc2) = rec(
        "gemma4:12b",
        "todo:task:1",
        50,
        0,
        0,
        0,
        0,
        CallStatus::Empty,
        20,
        0,
    );
    let (mut r3, tc3) = rec(
        "gpt-4o",
        "todo:task:2",
        0,
        0,
        0,
        0,
        0,
        CallStatus::Ok,
        150,
        0,
    );
    r3.status = CallStatus::HttpError;
    m.record_call(r1, tc1);
    m.record_call(r2, tc2);
    m.record_call(r3, tc3);

    // Session totals.
    assert_eq!(m.totals.calls, 3);
    assert_eq!(m.totals.empties, 1);
    assert_eq!(m.totals.errors, 1);
    // 700 (gpt-4o call) + 50 (gemma empty call: its prompt still billed).
    assert_eq!(m.totals.in_normal, 750);
    assert_eq!(m.totals.in_cached, 200);
    assert_eq!(m.totals.in_cache_write, 100);
    assert_eq!(m.totals.out_normal, 250);
    assert_eq!(m.totals.out_reasoning, 50);
    assert_eq!(m.totals.tool_calls, 2);
    assert_eq!(m.totals.llm_ms_total, 270);
    assert_eq!(m.totals.llm_ms_min, 20);
    assert_eq!(m.totals.llm_ms_max, 150);
    assert_eq!(m.totals.ttft_ms_total, 30);

    // Per-model.
    assert_eq!(m.by_model.len(), 2);
    let gpt = &m.by_model["gpt-4o"];
    assert_eq!(gpt.calls, 2);
    assert_eq!(gpt.errors, 1);
    assert_eq!(gpt.in_normal, 700);
    let local = &m.by_model["gemma4:12b"];
    assert_eq!(local.calls, 1);
    assert_eq!(local.empties, 1);

    // Call log holds all three records in order.
    assert_eq!(m.calls.len(), 3);
    assert_eq!(m.calls[0].phase, "main");
    assert_eq!(m.calls[1].phase, "todo:task:1");
}

#[test]
fn test_from_records_rebuilds_aggregates() {
    let (r1, _) = rec(
        "gpt-4o",
        "main",
        1000,
        200,
        100,
        300,
        50,
        CallStatus::Ok,
        100,
        2,
    );
    let (r2, _) = rec("gpt-4o", "main", 500, 0, 0, 100, 0, CallStatus::Ok, 50, 0);
    let m = Metrics::from_records(vec![r1, r2]);
    assert_eq!(m.totals.calls, 2);
    assert_eq!(m.calls.len(), 2);
    assert_eq!(m.totals.in_normal, 700 + 500);
    assert_eq!(m.totals.in_cached, 200);
    assert_eq!(m.by_model["gpt-4o"].calls, 2);
    // Tool counts are not stored on records, so they reset to 0 on rebuild.
    assert_eq!(m.totals.tool_calls, 0);
}

#[test]
fn test_call_log_is_capped() {
    let mut m = Metrics::default();
    for i in 0..(MAX_CALL_RECORDS + 50) {
        let (r, _) = rec("gpt-4o", "main", 1, 0, 0, 1, 0, CallStatus::Ok, 1, 0);
        let _ = i;
        m.record_call(r, 0);
    }
    assert_eq!(m.calls.len(), MAX_CALL_RECORDS);
    assert_eq!(m.totals.calls, (MAX_CALL_RECORDS + 50) as u64);
}

#[test]
fn test_fmt_helpers() {
    assert_eq!(fmt_tokens(0), "0.0K (0)");
    assert_eq!(fmt_tokens(12345), "12.3K (12345)");
    assert_eq!(fmt_ms(0), "0.0s");
    assert_eq!(fmt_ms(12300), "12.3s");
    assert_eq!(fmt_ms(61200), "1m 1.2s");
}

// --------------------------------------------------------------------------
// [Tokens] display line and its compatibility with the legacy format (no
// CacheW column; reasoning folded into Out).
// --------------------------------------------------------------------------

fn usage(prompt: u32, cached: u32, cache_write: u32, completion: u32, reasoning: u32) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        prompt_tokens_details: (cached > 0 || cache_write > 0).then_some(PromptTokensDetails {
            cached_tokens: cached,
            cache_creation_tokens: cache_write,
            audio_tokens: 0,
        }),
        completion_tokens_details: (reasoning > 0).then_some(CompletionTokensDetails {
            reasoning_tokens: reasoning,
        }),
    }
}

/// Record one call (Ok) and return the display line plus the resulting totals.
fn line_after(usage: Usage, model: &str) -> String {
    let rec = LlmCallRecord {
        timestamp: chrono::Utc::now(),
        model: model.to_string(),
        provider: LlmProvider::OpenAi,
        phase: "main".to_string(),
        usage: usage.clone(),
        latency_ms: 100,
        ttft_ms: 10,
        request_bytes: 1,
        response_bytes: 2,
        retry_count: 0,
        status: CallStatus::Ok,
    };
    let mut m = Metrics::default();
    m.record_call(rec, 0);
    format_token_line(&usage, &m.totals)
}

/// Non-cache models keep the legacy `---` cache columns.
#[test]
fn test_format_token_line_non_cache_model() {
    let line = line_after(usage(1000, 0, 0, 300, 0), "gemma4:12b");
    assert_eq!(
        line,
        "Turn: In 1.0K (1000), Cache ---, CacheW ---, Out 0.3K (300) \
         | Total: In 1.0K (1000), Cache ---, CacheW ---, Out 0.3K (300)"
    );
}

/// Cache-write and reasoning are split out of In / Out respectively.
#[test]
fn test_format_token_line_with_cache_and_reasoning() {
    let line = line_after(usage(1500, 200, 100, 300, 100), "gpt-4o");
    // Turn: normal = 1500 - 200 - 100 = 1200; out_normal = 300 - 100 = 200.
    // Total: gross in = 1200 + 200 + 100 = 1500.
    assert_eq!(
        line,
        "Turn: In 1.2K (1200), Cache 0.2K (200), CacheW 0.1K (100), Out 0.2K (200) \
         (Reasoning 0.1K (100)) | Total: In 1.5K (1500), Cache 0.2K (200), \
         CacheW 0.1K (100), Out 0.2K (200) (Reasoning 0.1K (100))"
    );
}

/// Numeric compatibility with the legacy aggregation:
/// - legacy total In (in_normal + in_cached) == new In + Cache + CacheW + audio
/// - legacy Out (completion)                == new Out + Reasoning
/// - legacy turn In (prompt - cached)       == new turn In + CacheW (+audio)
#[test]
fn test_token_line_numbers_compat_with_legacy() {
    // No cache-write / audio: exact equality between every pair.
    let rec = LlmCallRecord {
        timestamp: chrono::Utc::now(),
        model: "gpt-4o".to_string(),
        provider: LlmProvider::OpenAi,
        phase: "main".to_string(),
        usage: usage(1000, 200, 0, 300, 50),
        latency_ms: 100,
        ttft_ms: 10,
        request_bytes: 1,
        response_bytes: 2,
        retry_count: 0,
        status: CallStatus::Ok,
    };
    let mut m = Metrics::default();
    m.record_call(rec, 0);
    let t = &m.totals;

    // Legacy total In = in_normal + in_cached == new In + Cache + CacheW + audio.
    assert_eq!(
        t.in_normal + t.in_cached,
        t.in_normal + t.in_cached + t.in_cache_write + t.in_audio
    );
    assert_eq!(t.in_cache_write, 0);
    assert_eq!(t.in_audio, 0);
    // Legacy total Out = completion == new Out + Reasoning.
    assert_eq!(300, t.out_normal + t.out_reasoning);
    assert_eq!(t.out_normal, 250);
    assert_eq!(t.out_reasoning, 50);
    // Legacy turn In = prompt - cached == new turn normal.
    assert_eq!(800, t.in_normal);

    // With cache-write present, the new model becomes a strict superset:
    // legacy In (prompt - cached) == new In normal + CacheW.
    let rec2 = LlmCallRecord {
        timestamp: chrono::Utc::now(),
        model: "gpt-4o".to_string(),
        provider: LlmProvider::OpenAi,
        phase: "main".to_string(),
        usage: usage(1000, 200, 100, 0, 0),
        latency_ms: 100,
        ttft_ms: 10,
        request_bytes: 1,
        response_bytes: 2,
        retry_count: 0,
        status: CallStatus::Ok,
    };
    let mut m2 = Metrics::default();
    m2.record_call(rec2, 0);
    let t2 = &m2.totals;
    assert_eq!(t2.in_normal, 700); // 1000 - 200 - 100
    assert_eq!(t2.in_cache_write, 100);
    // Legacy would have counted cache-write as normal input: 800 == 700 + 100.
    assert_eq!(800, t2.in_normal + t2.in_cache_write);
}
