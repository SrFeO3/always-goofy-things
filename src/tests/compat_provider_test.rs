use super::*;
use crate::model::{FunctionCall, ToolCall};
use serde_json::json;

// ------------------------------------------------------------------
// Anthropic image conversion tests
// ------------------------------------------------------------------

#[test]
fn test_anthropic_converts_image_url_to_image_source() {
    let msg = json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Describe this image"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}
        ]
    });
    let result = convert_message_for_anthropic(&msg);

    assert_eq!(result["role"], "user");
    let blocks = result["content"]
        .as_array()
        .expect("content should be array");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "iVBORw0KGgo=");
}

#[test]
fn test_anthropic_passes_text_only_user_message_unchanged() {
    let msg = json!({
        "role": "user",
        "content": "Hello, world!"
    });
    let result = convert_message_for_anthropic(&msg);
    assert_eq!(result, msg);
}

#[test]
fn test_anthropic_user_with_text_only_blocks_passes_through() {
    let msg = json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Query"},
            {"type": "text", "text": "<attached_file path=\"f.txt\">content</attached_file>"}
        ]
    });
    let result = convert_message_for_anthropic(&msg);
    assert_eq!(result["role"], "user");
    let blocks = result["content"].as_array().expect("should be array");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "text");
}

// ------------------------------------------------------------------
// Ollama image extraction tests
// ------------------------------------------------------------------

#[test]
fn test_ollama_extracts_images_and_collapses_content() {
    let msgs = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "What is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,ABC123"}}
        ]
    })];
    let result = convert_messages_for_ollama(msgs);
    let msg = &result[0];

    assert_eq!(msg["role"], "user");
    assert_eq!(msg["content"], "What is this?");
    let images = msg["images"].as_array().expect("images should be array");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0], "ABC123");
}

#[test]
fn test_ollama_text_only_blocks_collapse_to_string() {
    let msgs = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Query"},
            {"type": "text", "text": "<attached_file path=\"f.txt\">content</attached_file>"}
        ]
    })];
    let result = convert_messages_for_ollama(msgs);
    let msg = &result[0];

    assert_eq!(msg["role"], "user");
    // Two text blocks joined by newline
    assert!(msg["content"].as_str().unwrap().contains("Query"));
    assert!(msg["content"].as_str().unwrap().contains("attached_file"));
    // No images field
    assert!(msg.get("images").is_none());
}

#[test]
fn test_ollama_plain_string_unchanged() {
    let msgs = vec![json!({
        "role": "user",
        "content": "Hello"
    })];
    let result = convert_messages_for_ollama(msgs);
    // Plain string content is not an array -> function skips it
    assert_eq!(result[0]["content"], "Hello");
    assert!(result[0].get("images").is_none());
}

#[test]
fn test_ollama_non_user_messages_untouched() {
    let msgs = vec![
        json!({"role": "system", "content": "You are helpful"}),
        json!({"role": "assistant", "content": "Sure!"}),
    ];
    let result = convert_messages_for_ollama(msgs);
    assert_eq!(result[0]["content"], "You are helpful");
    assert_eq!(result[1]["content"], "Sure!");
}

// ------------------------------------------------------------------
// OpenAI document -> image_url conversion tests
// ------------------------------------------------------------------

#[test]
fn test_openai_converts_document_to_file() {
    let msgs = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Analyze this"},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "PDF123"}}
        ]
    })];
    let result = convert_messages_for_openai(msgs);
    let msg = &result[0];

    assert_eq!(msg["role"], "user");
    let blocks = msg["content"].as_array().expect("content should be array");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "file");
    assert_eq!(
        blocks[1]["file"]["file_data"],
        "data:application/pdf;base64,PDF123"
    );
}

#[test]
fn test_openai_passes_non_document_blocks_unchanged() {
    let msgs = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Query"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,ABC"}}
        ]
    })];
    let result = convert_messages_for_openai(msgs);
    let msg = &result[0];
    let blocks = msg["content"].as_array().expect("content should be array");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image_url");
    assert_eq!(blocks[1]["image_url"]["url"], "data:image/png;base64,ABC");
}

// ------------------------------------------------------------------
// Golden request-payload snapshots + measurement-junk guards
// ------------------------------------------------------------------
//
// These tests pin the provider request payloads byte-for-byte so that the
// resource-accounting feature can never change what is sent to the LLM (or
// leak measurement keys into it). `include_usage` in the OpenAI payload is a
// pre-existing request stream option, not measurement data.

/// Deterministic request used by the golden / no-junk tests.
fn sample_request(provider: LlmProvider) -> ChatRequest {
    let messages = vec![
        Message {
            role: "system".to_string(),
            content: "You are a helpful assistant.".to_string(),
            ..Default::default()
        },
        Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
            ..Default::default()
        },
    ];
    let tools = vec![json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file",
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }
        }
    })];
    ChatRequest {
        provider,
        model: "gpt-4o".to_string(),
        max_output_tokens: 100,
        tools,
        stream: true,
        messages,
        tool_result_format: ToolResultFormat::JsonString,
        max_tokens_fallback: false,
    }
}

fn request_json(provider: LlmProvider) -> String {
    serde_json::to_string(&sample_request(provider).to_provider_json().unwrap()).unwrap()
}

/// Golden snapshots: provider payloads must match the fixtures byte-for-byte.
#[test]
fn test_golden_provider_payloads() {
    assert_eq!(
        request_json(LlmProvider::OpenAi),
        include_str!("fixtures/openai_request.json").trim(),
        "golden mismatch for OpenAi"
    );
    assert_eq!(
        request_json(LlmProvider::Ollama),
        include_str!("fixtures/ollama_request.json").trim(),
        "golden mismatch for Ollama"
    );
    assert_eq!(
        request_json(LlmProvider::Anthropic),
        include_str!("fixtures/anthropic_request.json").trim(),
        "golden mismatch for Anthropic"
    );
}

/// The request payload must never carry resource-accounting keys, and the
/// conversation `messages` array must not carry a `usage`/`metrics` key.
#[test]
fn test_request_payload_has_no_measurement_junk() {
    for provider in [
        LlmProvider::OpenAi,
        LlmProvider::Ollama,
        LlmProvider::Anthropic,
    ] {
        let val = sample_request(provider).to_provider_json().unwrap();
        let text = serde_json::to_string(&val).unwrap();
        for banned in [
            "metrics",
            "latency_ms",
            "request_bytes",
            "response_bytes",
            "ttft_ms",
            "retry_count",
            "llm_stats",
            "call_label",
            "session_id",
        ] {
            assert!(
                !text.contains(banned),
                "{:?} payload must not contain '{}': {}",
                provider,
                banned,
                text
            );
        }
        if let Some(msgs) = val.get("messages").and_then(|v| v.as_array()) {
            for m in msgs {
                assert!(
                    m.get("usage").is_none() && m.get("metrics").is_none(),
                    "{:?} message must not carry measurement keys: {}",
                    provider,
                    m
                );
            }
        }
    }
}

// ------------------------------------------------------------------
// Session ID: must never leak into any provider payload
// ------------------------------------------------------------------

/// Golden payloads must stay byte-identical even when every message carries a
/// session ID (strongest pin: the stable-ID feature cannot change or leak into
/// what is sent to the LLM).
#[test]
fn test_golden_payloads_unchanged_when_session_id_set() {
    for provider in [
        LlmProvider::OpenAi,
        LlmProvider::Ollama,
        LlmProvider::Anthropic,
    ] {
        let mut req = sample_request(provider);
        for m in &mut req.messages {
            m.session_id = "550e8400-e29b-41d4-a716-446655440000".to_string();
        }
        let text = serde_json::to_string(&req.to_provider_json().unwrap()).unwrap();
        let expected = match provider {
            LlmProvider::OpenAi => include_str!("fixtures/openai_request.json"),
            LlmProvider::Ollama => include_str!("fixtures/ollama_request.json"),
            LlmProvider::Anthropic => include_str!("fixtures/anthropic_request.json"),
        };
        assert_eq!(
            text,
            expected.trim(),
            "golden mismatch for {:?} with session_id set",
            provider
        );
    }
}

/// Explicit session ID values on messages must never appear in the provider
/// payload, for any provider (key or value).
#[test]
fn test_session_id_never_reaches_provider_payload() {
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    for provider in [
        LlmProvider::OpenAi,
        LlmProvider::Ollama,
        LlmProvider::Anthropic,
    ] {
        let mut req = sample_request(provider);
        for m in &mut req.messages {
            m.session_id = uuid.to_string();
        }
        let val = req.to_provider_json().unwrap();
        let text = serde_json::to_string(&val).unwrap();
        assert!(
            !text.contains("session_id"),
            "{:?} payload must not contain the 'session_id' key: {}",
            provider,
            text
        );
        assert!(
            !text.contains(uuid),
            "{:?} payload must not contain the session UUID value: {}",
            provider,
            text
        );
        if let Some(msgs) = val.get("messages").and_then(|v| v.as_array()) {
            for m in msgs {
                assert!(
                    m.get("session_id").is_none(),
                    "{:?} message must not carry session_id: {}",
                    provider,
                    m
                );
            }
        }
    }
}

// ------------------------------------------------------------------
// OpenAI max_completion_tokens -> max_tokens fallback
// ------------------------------------------------------------------

#[test]
fn test_openai_fallback_swaps_max_completion_tokens_for_max_tokens() {
    // Primary payload: max_completion_tokens only (golden format).
    let mut req = sample_request(LlmProvider::OpenAi);
    assert!(!req.max_tokens_fallback);
    let primary = req.to_provider_json().unwrap();
    assert_eq!(primary["max_completion_tokens"], 100);
    assert!(primary.get("max_tokens").is_none());

    // Fallback payload: max_tokens only, everything else unchanged.
    req.max_tokens_fallback = true;
    let fallback = req.to_provider_json().unwrap();
    assert!(fallback.get("max_completion_tokens").is_none());
    assert_eq!(fallback["max_tokens"], 100);
    assert_eq!(fallback["model"], primary["model"]);
    assert_eq!(fallback["messages"], primary["messages"]);
    assert_eq!(fallback["tools"], primary["tools"]);
    assert_eq!(fallback["stream"], primary["stream"]);
}

// ------------------------------------------------------------------
// Anthropic: parallel tool results must merge into one user message
// ------------------------------------------------------------------

/// Request shape: system, user, assistant (2 tool_use), tool, tool - the
/// exact sequence `run_reasoning_loop` produces for parallel tool calls.
fn parallel_tool_request() -> ChatRequest {
    let messages = vec![
        Message {
            role: "system".to_string(),
            content: "sys".to_string(),
            ..Default::default()
        },
        Message {
            role: "user".to_string(),
            content: "list both dirs".to_string(),
            ..Default::default()
        },
        Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_1".to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: "list_directory".to_string(),
                        arguments: json!({ "path": "a" }),
                    },
                    thought_signature: None,
                },
                ToolCall {
                    id: "call_2".to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: "list_directory".to_string(),
                        arguments: json!({ "path": "b" }),
                    },
                    thought_signature: None,
                },
            ]),
            ..Default::default()
        },
        Message {
            role: "tool".to_string(),
            content: json!({ "entries": [] }).to_string(),
            tool_call_id: Some("call_1".to_string()),
            ..Default::default()
        },
        Message {
            role: "tool".to_string(),
            content: json!({ "entries": [] }).to_string(),
            tool_call_id: Some("call_2".to_string()),
            ..Default::default()
        },
    ];
    ChatRequest {
        provider: LlmProvider::Anthropic,
        model: "claude-sonnet-4-5".to_string(),
        max_output_tokens: 100,
        tools: vec![json!({
            "type": "function",
            "function": {
                "name": "list_directory",
                "description": "List files",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } }
                }
            }
        })],
        stream: true,
        messages,
        tool_result_format: ToolResultFormat::JsonString,
        max_tokens_fallback: false,
    }
}

#[test]
fn test_anthropic_merges_parallel_tool_results_into_one_user_message() {
    let val = parallel_tool_request().to_provider_json().unwrap();
    let msgs = val["messages"].as_array().expect("messages array");

    // [user, assistant(tool_use x2), user(tool_result x2)] - exactly 3
    // messages with alternating roles, no consecutive users.
    assert_eq!(
        msgs.len(),
        3,
        "got: {}",
        serde_json::to_string_pretty(&val).unwrap()
    );
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[2]["role"], "user");

    // Assistant holds both tool_use blocks.
    let asst_blocks = msgs[1]["content"].as_array().expect("assistant blocks");
    assert_eq!(asst_blocks.len(), 2);
    assert_eq!(asst_blocks[0]["type"], "tool_use");
    assert_eq!(asst_blocks[0]["id"], "call_1");
    assert_eq!(asst_blocks[1]["type"], "tool_use");
    assert_eq!(asst_blocks[1]["id"], "call_2");

    // Both tool results live in ONE user message, in call order.
    let result_blocks = msgs[2]["content"].as_array().expect("tool_result blocks");
    assert_eq!(result_blocks.len(), 2);
    assert_eq!(result_blocks[0]["type"], "tool_result");
    assert_eq!(result_blocks[0]["tool_use_id"], "call_1");
    assert_eq!(result_blocks[1]["type"], "tool_result");
    assert_eq!(result_blocks[1]["tool_use_id"], "call_2");
    assert!(result_blocks[0]["content"].is_string());
}

#[test]
fn test_anthropic_single_tool_result_stays_one_user_message() {
    let mut req = parallel_tool_request();
    req.messages.truncate(4); // drop the second tool result
    let val = req.to_provider_json().unwrap();
    let msgs = val["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    let blocks = msgs[2]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "tool_result");
    assert_eq!(blocks[0]["tool_use_id"], "call_1");
}
