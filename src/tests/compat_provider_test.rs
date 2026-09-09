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
        request_json(LlmProvider::OpenAiResponses),
        include_str!("fixtures/openai_responses_request.json").trim(),
        "golden mismatch for OpenAiResponses"
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
        LlmProvider::OpenAiResponses,
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
        LlmProvider::OpenAiResponses,
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
            LlmProvider::OpenAiResponses => include_str!("fixtures/openai_responses_request.json"),
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
        LlmProvider::OpenAiResponses,
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

// ------------------------------------------------------------------
// Ollama native request shaping: options, tool_name, thinking.
// ------------------------------------------------------------------

#[test]
fn test_ollama_options_contain_num_predict_only() {
    let req = sample_request(LlmProvider::Ollama);
    let val = req.to_provider_json().unwrap();
    assert_eq!(val["options"]["num_predict"], 100);
    // num_ctx is intentionally unset; the model's own default applies.
    assert!(val["options"].get("num_ctx").is_none());
}

fn tool_round_trip_request(provider: LlmProvider) -> ChatRequest {
    ChatRequest {
        provider,
        model: "gpt-4o".to_string(),
        max_output_tokens: 100,
        tools: vec![],
        stream: true,
        messages: vec![
            Message {
                role: "user".to_string(),
                content: "read it".to_string(),
                ..Default::default()
            },
            Message {
                role: "assistant".to_string(),
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: json!({ "path": "a.txt" }),
                    },
                    thought_signature: None,
                }]),
                ..Default::default()
            },
            Message {
                role: "tool".to_string(),
                content: r#"{"status":"ok"}"#.to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read_file".to_string()),
                ..Default::default()
            },
        ],
        tool_result_format: ToolResultFormat::JsonString,
        max_tokens_fallback: false,
    }
}

#[test]
fn test_ollama_tool_result_message_includes_tool_name() {
    let val = tool_round_trip_request(LlmProvider::Ollama)
        .to_provider_json()
        .unwrap();
    let msgs = val["messages"].as_array().unwrap();
    let tool_msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .expect("tool message");
    assert_eq!(tool_msg["tool_name"], "read_file");
    assert!(tool_msg["content"].is_string());
}

#[test]
fn test_tool_name_not_leaked_to_openai_payload() {
    let val = tool_round_trip_request(LlmProvider::OpenAi)
        .to_provider_json()
        .unwrap();
    let msgs = val["messages"].as_array().unwrap();
    let tool_msg = msgs
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .expect("tool message");
    assert!(tool_msg.get("tool_name").is_none());
    assert_eq!(tool_msg["tool_call_id"], "call_1");
}

#[test]
fn test_ollama_assistant_reasoning_content_renamed_to_thinking() {
    let msgs = vec![json!({
        "role": "assistant",
        "content": "",
        "reasoning_content": "thinking text"
    })];
    let result = convert_messages_for_ollama(msgs);
    let msg = &result[0];
    assert_eq!(msg["thinking"], "thinking text");
    assert!(msg.get("reasoning_content").is_none());
}

#[test]
fn test_ollama_assistant_without_reasoning_untouched() {
    let msgs = vec![json!({
        "role": "assistant",
        "content": "Sure!",
        "tool_calls": [{ "id": "call_1", "function": { "name": "read_file" } }]
    })];
    let result = convert_messages_for_ollama(msgs);
    assert_eq!(result[0]["content"], "Sure!");
    assert!(result[0].get("thinking").is_none());
    assert!(result[0].get("reasoning_content").is_none());
}

// ------------------------------------------------------------------
// OpenAI Responses API: conversation -> input items, tools, stream events
// ------------------------------------------------------------------

#[test]
fn test_openai_responses_round_trips_tool_conversation_into_input_items() {
    let msgs = vec![
        json!({ "role": "system", "content": "sys" }),
        json!({ "role": "user", "content": "list both dirs" }),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                { "id": "call_1", "type": "function", "function": { "name": "list_directory", "arguments": { "path": "a" } } },
                { "id": "call_2", "type": "function", "function": { "name": "list_directory", "arguments": { "path": "b" } } }
            ]
        }),
        json!({ "role": "tool", "tool_call_id": "call_1", "content": "{\"entries\":[]}" }),
        json!({ "role": "tool", "tool_call_id": "call_2", "content": "{\"entries\":[]}" }),
        json!({ "role": "assistant", "content": "done" }),
    ];
    let items = convert_messages_for_openai_responses(msgs);

    // system dropped, everything else becomes an input item in order:
    // user, function_call x2, function_call_output x2, assistant text
    assert_eq!(items.len(), 6);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["content"][0]["type"], "input_text");

    assert_eq!(items[1]["type"], "function_call");
    assert_eq!(items[1]["call_id"], "call_1");
    assert_eq!(items[1]["name"], "list_directory");
    assert_eq!(items[1]["arguments"], "{\"path\":\"a\"}");
    assert_eq!(items[2]["type"], "function_call");
    assert_eq!(items[2]["call_id"], "call_2");

    assert_eq!(items[3]["type"], "function_call_output");
    assert_eq!(items[3]["call_id"], "call_1");
    assert_eq!(items[3]["output"], "{\"entries\":[]}");
    assert_eq!(items[4]["type"], "function_call_output");
    assert_eq!(items[4]["call_id"], "call_2");

    assert_eq!(items[5]["role"], "assistant");
    assert_eq!(items[5]["content"][0]["type"], "output_text");
    assert_eq!(items[5]["content"][0]["text"], "done");
}

#[test]
fn test_openai_responses_synthesizes_call_ids_when_missing() {
    let msgs = vec![
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{ "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" } }]
        }),
        json!({ "role": "tool", "content": "\"ok\"" }),
    ];
    let items = convert_messages_for_openai_responses(msgs);
    assert_eq!(items[0]["type"], "function_call");
    assert_eq!(items[0]["call_id"], "call_openai_responses_1");
    // Empty tool_call_id falls back to the pending call's synthesized id.
    assert_eq!(items[1]["type"], "function_call_output");
    assert_eq!(items[1]["call_id"], "call_openai_responses_1");
    assert_eq!(items[1]["output"], "\"ok\"");
}

#[test]
fn test_openai_responses_user_content_blocks_converted() {
    let msgs = vec![json!({
        "role": "user",
        "content": [
            { "type": "text", "text": "Describe" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,ABC123" } },
            { "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "PDF123" } },
            { "type": "file", "file": { "file_data": "data:application/pdf;base64,FILE456" } }
        ]
    })];
    let items = convert_messages_for_openai_responses(msgs);
    let blocks = items[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "input_text");
    assert_eq!(blocks[0]["text"], "Describe");
    assert_eq!(blocks[1]["type"], "input_image");
    assert_eq!(blocks[1]["image_url"], "data:image/png;base64,ABC123");
    assert_eq!(blocks[2]["type"], "input_file");
    assert_eq!(blocks[2]["file_data"], "PDF123");
    // data: URL prefix is stripped for Responses `file_data`.
    assert_eq!(blocks[3]["type"], "input_file");
    assert_eq!(blocks[3]["file_data"], "FILE456");
}

#[test]
fn test_openai_responses_tool_definition_flattened() {
    let tools = vec![
        json!({ "type": "function", "function": { "name": "read_file", "description": "Read a file", "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } } }),
        json!({ "type": "function", "function": { "name": "no_desc", "parameters": { "type": "object" } } }),
    ];
    let result = convert_tools_to_openai_responses(&tools);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0]["type"], "function");
    assert_eq!(result[0]["name"], "read_file");
    assert_eq!(result[0]["description"], "Read a file");
    assert_eq!(result[0]["parameters"]["type"], "object");
    assert!(result[0].get("function").is_none());
    assert!(result[1].get("description").is_none());
}

#[test]
fn test_openai_responses_stream_text_and_reasoning_events() {
    let text = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.output_text.delta",
        "delta": "Hello"
    }));
    assert_eq!(text["choices"][0]["delta"]["content"], "Hello");

    // Refusals are surfaced as text, mirroring the Chat Completions fallback.
    let refusal = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.refusal.delta",
        "delta": "I cannot help with that."
    }));
    assert_eq!(
        refusal["choices"][0]["delta"]["content"],
        "I cannot help with that."
    );

    let reasoning = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.reasoning_summary_text.delta",
        "delta": "thinking..."
    }));
    assert_eq!(
        reasoning["choices"][0]["delta"]["reasoning_content"],
        "thinking..."
    );

    let raw = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.reasoning_text.delta",
        "delta": "raw..."
    }));
    assert_eq!(raw["choices"][0]["delta"]["reasoning_content"], "raw...");

    // Irrelevant events are ignored (empty object, like other providers).
    let ignored =
        convert_openai_responses_event_to_openai_format(json!({ "type": "response.created" }));
    assert!(ignored.as_object().unwrap().is_empty());
}

#[test]
fn test_openai_responses_stream_function_call_items() {
    let added = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": { "type": "function_call", "call_id": "call_abc", "name": "read_file", "arguments": "" }
    }));
    assert_eq!(added["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    assert_eq!(
        added["choices"][0]["delta"]["tool_calls"][0]["id"],
        "call_abc"
    );
    assert_eq!(
        added["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );

    let done = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "function_call",
            "call_id": "call_abc",
            "name": "read_file",
            "arguments": "{\"path\":\"src/main.rs\"}"
        }
    }));
    assert_eq!(
        done["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"src/main.rs\"}"
    );

    // Message items (already streamed as deltas) are not re-emitted.
    let msg_done = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.output_item.done",
        "output_index": 1,
        "item": { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "full" }] }
    }));
    assert!(msg_done.as_object().unwrap().is_empty());
}

#[test]
fn test_openai_responses_stream_completed_maps_usage() {
    let completed = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.completed",
        "response": {
            "id": "resp_123",
            "status": "completed",
            "usage": {
                "input_tokens": 100,
                "input_tokens_details": { "cached_tokens": 40, "cache_write_tokens": 0 },
                "output_tokens": 30,
                "output_tokens_details": { "reasoning_tokens": 12 },
                "total_tokens": 130
            }
        }
    }));
    assert_eq!(completed["usage"]["prompt_tokens"], 100);
    assert_eq!(completed["usage"]["completion_tokens"], 30);
    assert_eq!(
        completed["usage"]["prompt_tokens_details"]["cached_tokens"],
        40
    );
    assert_eq!(
        completed["usage"]["completion_tokens_details"]["reasoning_tokens"],
        12
    );
    // The terminal event also maps `response.status` to the internal
    // finish_reason (for the empty-response diagnostics).
    assert_eq!(
        completed["choices"][0]["finish_reason"],
        "completed (responses)"
    );

    // No usage payload: still emit the finish_reason, no usage object.
    let no_usage = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.completed",
        "response": { "id": "resp_123", "status": "completed" }
    }));
    assert_eq!(
        no_usage["choices"][0]["finish_reason"],
        "completed (responses)"
    );
    assert!(no_usage.get("usage").is_none());
}

#[test]
fn test_openai_responses_stream_incomplete_maps_finish_reason() {
    // `response.incomplete` is the abnormal terminal event: no usage, but the
    // incomplete_details.reason explains why generation stopped.
    let incomplete = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.incomplete",
        "response": {
            "id": "resp_123",
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" }
        }
    }));
    assert_eq!(
        incomplete["choices"][0]["finish_reason"],
        "incomplete (responses: max_output_tokens)"
    );
    assert!(incomplete.get("usage").is_none());
}

#[test]
fn test_openai_responses_accumulates_usage_through_pipeline() {
    // The converter output feeds straight into accumulate_usage.
    let completed = convert_openai_responses_event_to_openai_format(json!({
        "type": "response.completed",
        "response": {
            "usage": {
                "input_tokens": 5,
                "output_tokens": 7,
                "output_tokens_details": { "reasoning_tokens": 2 }
            }
        }
    }));
    let mut usage: Option<crate::model::Usage> = None;
    accumulate_usage(&completed, &mut usage);
    let u = usage.expect("usage captured");
    assert_eq!(u.prompt_tokens, 5);
    assert_eq!(u.completion_tokens, 7);
    assert_eq!(
        u.completion_tokens_details
            .expect("reasoning details")
            .reasoning_tokens,
        2
    );
}

// ------------------------------------------------------------------
// Anthropic: tool defs, tool results, provider detection.
// ------------------------------------------------------------------

#[test]
fn test_anthropic_tool_definition_description_optional() {
    let tools = vec![
        json!({ "type": "function", "function": { "name": "no_desc", "parameters": { "type": "object" } } }),
        json!({ "type": "function", "function": { "name": "with_desc", "description": "d", "parameters": { "type": "object" } } }),
        json!({ "type": "function", "function": { "name": "no_params" } }), // dropped
    ];
    let result = convert_tools_to_anthropic(&tools);
    assert_eq!(result.len(), 2);
    assert!(result[0].get("description").is_none());
    assert_eq!(result[0]["name"], "no_desc");
    assert_eq!(result[1]["description"], "d");
}

#[test]
fn test_anthropic_tool_result_object_stringified() {
    let msg = json!({
        "role": "tool",
        "tool_call_id": "call_1",
        "content": { "stdout": "ok", "exit_code": 0 }
    });
    let result = convert_message_for_anthropic(&msg);
    let block = &result["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["content"], r#"{"exit_code":0,"stdout":"ok"}"#);
}

#[test]
fn test_detect_provider_routes_deepseek_anthropic_endpoint() {
    assert_eq!(
        detect_provider("https://api.deepseek.com/anthropic"),
        LlmProvider::Anthropic
    );
    assert_eq!(
        detect_provider("https://api.anthropic.com/v1/messages"),
        LlmProvider::Anthropic
    );
    assert_eq!(
        detect_provider("http://localhost:11434/api/chat"),
        LlmProvider::Ollama
    );
    assert_eq!(
        detect_provider("https://api.openai.com/v1/chat/completions"),
        LlmProvider::OpenAi
    );
    assert_eq!(
        detect_provider("https://api.openai.com/v1/responses"),
        LlmProvider::OpenAiResponses
    );
    assert_eq!(
        detect_provider("https://api.deepseek.com"),
        LlmProvider::OpenAi
    );
}
