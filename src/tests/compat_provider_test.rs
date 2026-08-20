use super::*;
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
            "phase",
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
