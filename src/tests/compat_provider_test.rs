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
    let result = extract_images_for_ollama(msgs);
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
    let result = extract_images_for_ollama(msgs);
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
    let result = extract_images_for_ollama(msgs);
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
    let result = extract_images_for_ollama(msgs);
    assert_eq!(result[0]["content"], "You are helpful");
    assert_eq!(result[1]["content"], "Sure!");
}
