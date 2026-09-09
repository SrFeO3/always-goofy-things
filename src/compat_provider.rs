//! LLM provider compatibility: API schemas, request formatting, and protocol adaptation.
//!
//! Handles provider-specific request payload formatting and protocol translation.
//! This module manages URL-based provider detection, request DTO definitions, wire-format conversions
//! (including Anthropic's and the OpenAI Responses API's event-based SSE), and tool-definition
//! translation. It also formats message contents, such as tool result rendering, to match each
//! provider's expected protocol format.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;

use crate::compat_resilience::ToolResultFormat;
use crate::file::FileType;
use crate::model::{ChatRequest, Message, Usage};

/// LLM API provider type
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, clap::ValueEnum)]
pub enum LlmProvider {
    OpenAi,
    OpenAiResponses,
    Ollama,
    Anthropic,
}

impl fmt::Display for LlmProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LlmProvider::OpenAi => "openai",
            LlmProvider::OpenAiResponses => "openai-responses",
            LlmProvider::Ollama => "ollama",
            LlmProvider::Anthropic => "anthropic",
        };
        write!(f, "{s}")
    }
}

/// Extra provider behaviors ("dialects") for gateways / non-major providers.
#[derive(Debug, Copy, Clone, PartialEq, clap::ValueEnum)]
pub enum ProviderExtra {
    /// OpenCode Go: send `x-opencode-session` with the stable per-conversation
    /// session UUID (`Session.id`, UUIDv4; empty id => header skipped).
    Opencode,
}

#[derive(Serialize)]
struct OpenAiRequestDto {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct OpenAiResponsesRequestDto {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<serde_json::Value>,
    max_output_tokens: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct OllamaRequestDto {
    model: String,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    options: OllamaOptions,
    stream: bool,
}

#[derive(Serialize)]
struct OllamaOptions {
    /// Generated-token cap. `num_ctx` is intentionally not sent so the
    /// model's own context-window default applies.
    num_predict: usize,
}

#[derive(Serialize)]
struct AnthropicRequestDto {
    model: String,
    max_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
}

/// Detect LLM provider from URL
pub fn detect_provider(url: &str) -> LlmProvider {
    let url_lower = url.to_lowercase();

    if url_lower.contains("api.anthropic.com")
        || url_lower.contains("/anthropic") // e.g. DeepSeek's Anthropic-compatible base
        || url_lower.contains("/v1/messages")
    {
        return LlmProvider::Anthropic;
    }

    if url_lower.contains("/api/chat") || url_lower.contains(":11434") {
        return LlmProvider::Ollama;
    }

    if url_lower.contains("/v1/responses") {
        return LlmProvider::OpenAiResponses;
    }

    if url_lower.contains("api.openai.com") {
        return LlmProvider::OpenAi;
    }

    LlmProvider::OpenAi
}

impl ChatRequest {
    pub fn to_provider_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        let raw_messages = self
            .messages
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;

        let messages =
            reformat_tool_results(&raw_messages, &self.messages, self.tool_result_format);
        // Expand user messages with attached files into content-block arrays
        let messages = attach_file_contents(messages, &self.messages);

        match self.provider {
            LlmProvider::OpenAi => {
                let messages = convert_messages_for_openai(messages);
                let dto = OpenAiRequestDto {
                    model: self.model.clone(),
                    max_completion_tokens: (!self.max_tokens_fallback)
                        .then_some(self.max_output_tokens),
                    max_tokens: self.max_tokens_fallback.then_some(self.max_output_tokens),
                    messages,
                    tools: self.tools.clone(),
                    stream: self.stream,
                    stream_options: if self.stream {
                        Some(StreamOptions {
                            include_usage: true,
                        })
                    } else {
                        None
                    },
                };
                serde_json::to_value(dto)
            }

            LlmProvider::OpenAiResponses => {
                let system_content = self
                    .messages
                    .iter()
                    .find(|m| m.role == "system")
                    .map(|m| m.content.clone());

                let input = convert_messages_for_openai_responses(messages);
                let openai_responses_tools = convert_tools_to_openai_responses(&self.tools);

                let dto = OpenAiResponsesRequestDto {
                    model: self.model.clone(),
                    instructions: system_content,
                    input,
                    max_output_tokens: self.max_output_tokens,
                    tools: openai_responses_tools,
                    stream: self.stream,
                };
                serde_json::to_value(dto)
            }

            LlmProvider::Ollama => {
                let messages = attach_ollama_tool_names(messages, &self.messages);
                let messages = convert_messages_for_ollama(messages);
                let dto = OllamaRequestDto {
                    model: self.model.clone(),
                    messages,
                    tools: self.tools.clone(),
                    options: OllamaOptions {
                        num_predict: self.max_output_tokens,
                    },
                    stream: self.stream,
                };
                serde_json::to_value(dto)
            }

            LlmProvider::Anthropic => {
                let system_content = self
                    .messages
                    .iter()
                    .find(|m| m.role == "system")
                    .map(|m| m.content.clone());

                let filtered_messages: Vec<serde_json::Value> = messages
                    .into_iter()
                    .filter(|v| v["role"] != "system")
                    .map(|v| convert_message_for_anthropic(&v))
                    .collect();

                // Anthropic: parallel tool results must be in ONE user message.
                let filtered_messages = merge_anthropic_tool_results(filtered_messages);

                let anthropic_tools = convert_tools_to_anthropic(&self.tools);

                let dto = AnthropicRequestDto {
                    model: self.model.clone(),
                    max_tokens: self.max_output_tokens,
                    system: system_content,
                    messages: filtered_messages,
                    tools: anthropic_tools,
                    stream: self.stream,
                };
                serde_json::to_value(dto)
            }
        }
    }
}

/// Reformat tool result messages according to the configured [ToolResultFormat].
///
/// Depending on the mode:
/// - `JsonString`: Keep as a JSON-encoded string (default).
/// - `Text`: Render each tool result as a concise text string.
/// - `JsonStructured`: Parse the string as JSON and embed the resulting object directly.
fn reformat_tool_results(
    messages_json: &[serde_json::Value],
    originals: &[Message],
    mode: ToolResultFormat,
) -> Vec<serde_json::Value> {
    match mode {
        ToolResultFormat::JsonString => messages_json.to_vec(),
        ToolResultFormat::Text => messages_json
            .iter()
            .zip(originals)
            .map(|(msg, orig)| {
                if let (Some(tool_name), Some(content)) = (
                    orig.tool_name.as_deref(),
                    msg.get("content").and_then(|v| v.as_str()),
                ) {
                    match serde_json::from_str::<serde_json::Value>(content) {
                        Ok(parsed) => {
                            if let Some(text) = render_tool_text(&parsed, tool_name) {
                                let mut new_msg = msg.clone();
                                new_msg["content"] = json!(text);
                                return new_msg;
                            }
                        }
                        Err(_) => println!(
                            "\x1b[93m(Warning: tool result content is not valid JSON, falling back to raw string)\x1b[0m"
                        ),
                    }
                }
                msg.clone()
            })
            .collect(),
        ToolResultFormat::JsonStructured => messages_json
            .iter()
            .map(|msg| {
                let mut new_msg = msg.clone();
                if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                    match serde_json::from_str::<serde_json::Value>(content) {
                        Ok(parsed) => {
                            new_msg["content"] = parsed;
                        }
                        Err(_) if msg.get("role") == Some(&json!("tool")) => println!(
                            "\x1b[93m(Warning: tool result content is not valid JSON, falling back to raw string)\x1b[0m"
                        ),
                        _ => {}
                    }
                }
                new_msg
            })
            .collect(),
    }
}

/// For user messages that have attached files, expand `content` from a plain string
/// into an array of `{ type: "text", text: "..." }` blocks.
///
/// The first block carries the original query text (if any), followed by one block
/// per attached file wrapped in `<attached_file path="...">...</attached_file>`.
fn attach_file_contents(
    mut messages_json: Vec<serde_json::Value>,
    originals: &[Message],
) -> Vec<serde_json::Value> {
    for (json, orig) in messages_json.iter_mut().zip(originals) {
        if orig.role == "user" && !orig.attached_files.is_empty() {
            let mut blocks = Vec::new();

            // First block: the user's own query text
            if !orig.content.is_empty() {
                blocks.push(json!({
                    "type": "text",
                    "text": orig.content
                }));
            }

            // Subsequent blocks: one per attached file
            for f in &orig.attached_files {
                match &f.attach_type {
                    FileType::Text => {
                        blocks.push(json!({
                            "type": "text",
                            "text": format!(
                                "<attached_file path=\"{}\">\n{}\n</attached_file>",
                                f.path, f.content
                            )
                        }));
                    }
                    FileType::Image { .. } => {
                        // f.content is already a data: URL
                        blocks.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": f.content
                            }
                        }));
                    }
                    FileType::Audio { format } => {
                        blocks.push(json!({
                            "type": "input_audio",
                            "input_audio": {
                                "data": f.content,
                                "format": format
                            }
                        }));
                    }
                    FileType::Document { mime } => {
                        // f.content is already raw Base64
                        blocks.push(json!({
                            "type": "document",
                            "source": {
                                "type": "base64",
                                "media_type": mime,
                                "data": f.content
                            }
                        }));
                    }
                }
            }

            json["content"] = json!(blocks);
        }
    }
    messages_json
}

/// Inject `Message.tool_name` into tool-result messages; it is not part of
/// the serialized `Message`, so it is added here for the Ollama dialect only.
fn attach_ollama_tool_names(
    mut messages_json: Vec<serde_json::Value>,
    originals: &[Message],
) -> Vec<serde_json::Value> {
    for (msg, orig) in messages_json.iter_mut().zip(originals) {
        if orig.role == "tool"
            && let Some(name) = orig.tool_name.as_deref()
            && !name.is_empty()
        {
            msg["tool_name"] = json!(name);
        }
    }
    messages_json
}

/// Convert content blocks for Ollama's native `/api/chat` format.
///
/// Extracts Base64 data from `image_url` blocks into a top-level `images`
/// array and collapses text blocks into a plain content string.
/// `document` blocks are skipped (Ollama does not support PDF natively).
fn convert_messages_for_ollama(
    mut messages_json: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    for msg in &mut messages_json {
        if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) else {
            continue;
        };

        let mut images: Vec<String> = Vec::new();
        let mut text_parts: Vec<String> = Vec::new();
        let mut has_unsupported = false;

        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) == Some("image_url")
                && let Some(url) = block
                    .get("image_url")
                    .and_then(|v| v.get("url"))
                    .and_then(|v| v.as_str())
                && let Some((_media_type, data)) = crate::file::parse_data_url(url)
            {
                images.push(data.to_string());
            } else if block.get("type").and_then(|v| v.as_str()) == Some("document") {
                has_unsupported = true;
            } else if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                text_parts.push(text.to_string());
            }
        }

        if has_unsupported {
            eprintln!(
                "{}[Warning] Ollama does not support PDF/document attachments. \
                 Use @@file for text extraction instead.{} ",
                crate::startup::C_YELLOW,
                crate::startup::RESET
            );
        }

        if !images.is_empty() {
            msg["images"] = json!(images);
        }
        // Collapse text blocks to a plain string for Ollama's native API.
        msg["content"] = json!(text_parts.join("\n"));
    }

    // Echo assistant reasoning back to Ollama as its native `thinking` field.
    for msg in &mut messages_json {
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let reasoning = obj
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        if let Some(reasoning) = reasoning {
            obj.remove("reasoning_content");
            obj.insert("thinking".to_string(), serde_json::Value::String(reasoning));
        }
    }
    messages_json
}

/// Convert content blocks for OpenAI's Chat Completions API.
///
/// Transforms `document` blocks into `file` blocks (native Chat Completions
/// format with `file_data` as a `data:` URL).
fn convert_messages_for_openai(
    mut messages_json: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    for msg in &mut messages_json {
        if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) else {
            continue;
        };

        let converted: Vec<serde_json::Value> = blocks
            .iter()
            .map(|block| {
                if block.get("type").and_then(|v| v.as_str()) == Some("document")
                    && let Some(mime) = block
                        .get("source")
                        .and_then(|v| v.get("media_type"))
                        .and_then(|v| v.as_str())
                    && let Some(data) = block
                        .get("source")
                        .and_then(|v| v.get("data"))
                        .and_then(|v| v.as_str())
                {
                    json!({
                        "type": "file",
                        "file": {
                            "file_data": format!("data:{};base64,{}", mime, data)
                        }
                    })
                } else {
                    block.clone()
                }
            })
            .collect();

        msg["content"] = json!(converted);
    }
    messages_json
}

/// Convert Chat Completions-style messages into OpenAI Responses API `input` items.
///
/// The OpenAI Responses API has no `messages` array; a conversation is rebuilt
/// as typed `input` items:
///
/// | Chat Completions                    | Responses API `input` items                            |
/// |-------------------------------------|--------------------------------------------------------|
/// | `role: "system"`                   | root-level `instructions` (handled by the caller)      |
/// | `role: "user"` (string or blocks)  | `{ role: "user", content: [input_text/input_image/...] }` |
/// | `role: "assistant"` (text)         | `{ role: "assistant", content: [output_text] }`       |
/// | `role: "assistant"` + `tool_calls` | `{ type: "function_call", call_id, name, arguments }` |
/// | `role: "tool"`                     | `{ type: "function_call_output", call_id, output }`   |
fn convert_messages_for_openai_responses(
    messages: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut items: Vec<serde_json::Value> = Vec::new();
    // call_ids of function_call items that still await their function_call_output.
    let mut pending_calls: Vec<String> = Vec::new();
    let mut synth_id = 0usize;

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        match role {
            // role: "system" -> root-level `instructions` (see to_provider_json)
            "system" => {}
            "user" => {
                let mut blocks: Vec<serde_json::Value> = match msg.get("content") {
                    Some(serde_json::Value::Array(arr)) => arr
                        .iter()
                        .map(openai_responses_input_content_block)
                        .collect(),
                    Some(v) => vec![openai_responses_text_block(v)],
                    None => Vec::new(),
                };
                if blocks.is_empty() {
                    blocks.push(openai_responses_text_block(&serde_json::Value::Null));
                }
                items.push(json!({ "role": "user", "content": blocks }));
            }
            "assistant" => {
                if let Some(text) = msg
                    .get("content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    items.push(json!({
                        "role": "assistant",
                        "content": [openai_responses_output_text_block(&json!(text))]
                    }));
                }
                // assistant.tool_calls -> function_call input items
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                synth_id += 1;
                                format!("call_openai_responses_{}", synth_id)
                            });
                        if let Some(func) = tc.get("function") {
                            let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let arguments = match func.get("arguments") {
                                Some(serde_json::Value::String(s)) => s.clone(),
                                Some(v) => v.to_string(),
                                None => String::new(),
                            };
                            items.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": arguments
                            }));
                            pending_calls.push(id);
                        }
                    }
                }
            }
            "tool" => {
                // Prefer the recorded tool_call_id; fall back to the next
                // unmatched function_call item in conversation order.
                let call_id = msg
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| pending_calls.first().cloned())
                    .unwrap_or_else(|| {
                        synth_id += 1;
                        format!("call_openai_responses_{}", synth_id)
                    });
                if let Some(pos) = pending_calls.iter().position(|c| *c == call_id) {
                    pending_calls.remove(pos);
                }
                // `function_call_output.output` must be a string (JSON text ok).
                let output = match msg.get("content") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output
                }));
            }
            // Unknown roles are dropped (Responses has no such roles).
            _ => {}
        }
    }
    items
}

/// Wrap an arbitrary JSON value as an OpenAI Responses API `input_text` content block.
fn openai_responses_text_block(v: &serde_json::Value) -> serde_json::Value {
    json!({ "type": "input_text", "text": v.as_str().unwrap_or("") })
}

/// Wrap an arbitrary JSON value as an OpenAI Responses API `output_text` content block.
/// Used for assistant messages in conversation history.
fn openai_responses_output_text_block(v: &serde_json::Value) -> serde_json::Value {
    json!({ "type": "output_text", "text": v.as_str().unwrap_or("") })
}

/// Convert one Chat Completions content block to its OpenAI Responses API
/// equivalent (`input_text` / `input_image` / `input_file` / `input_audio`).
fn openai_responses_input_content_block(block: &serde_json::Value) -> serde_json::Value {
    match block.get("type").and_then(|v| v.as_str()) {
        Some("text") => json!({
            "type": "input_text",
            "text": block.get("text").and_then(|v| v.as_str()).unwrap_or("")
        }),
        Some("image_url") => json!({
            "type": "input_image",
            "image_url": block
                .get("image_url")
                .and_then(|v| v.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        }),
        Some("document") => json!({
            "type": "input_file",
            "file_data": block
                .get("source")
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        }),
        // Chat Completions file block: file.file_data is a data: URL.
        Some("file") => {
            let file_data = block
                .get("file")
                .and_then(|v| v.get("file_data"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let base64 = crate::file::parse_data_url(file_data)
                .map(|(_, data)| data.to_string())
                .unwrap_or_else(|| file_data.to_string());
            json!({ "type": "input_file", "file_data": base64 })
        }
        // input_audio has the same shape on both APIs.
        _ => block.clone(),
    }
}

/// Merge consecutive tool-result-only user messages into one.
///
/// `run_reasoning_loop` pushes one `role:"tool"` message per tool call, and
/// [`convert_message_for_anthropic`] turns each into its own user message;
/// Anthropic requires all `tool_result` blocks (parallel calls) in a single
/// user message and rejects consecutive same-role messages.
fn merge_anthropic_tool_results(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let is_tool_result_user = |v: &serde_json::Value| -> bool {
        v.get("role").and_then(|r| r.as_str()) == Some("user")
            && v.get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|blocks| {
                    !blocks.is_empty()
                        && blocks
                            .iter()
                            .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                })
    };

    let mut out: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        if is_tool_result_user(&msg)
            && let Some(prev) = out.last_mut()
            && is_tool_result_user(prev)
            && let (Some(cur), Some(prev_blocks)) = (
                msg.get("content").and_then(|c| c.as_array()).cloned(),
                prev.get_mut("content").and_then(|c| c.as_array_mut()),
            )
        {
            prev_blocks.extend(cur);
            continue;
        }
        out.push(msg);
    }
    out
}

/// Render a tool result as a concise text string.
fn render_tool_text(result: &Value, tool_name: &str) -> Option<String> {
    match tool_name {
        "read_file" | "fetch_web" => result
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "execute_bash" => {
            let stdout = result.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let exit_code = result
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if exit_code != 0 {
                let stderr = result.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                Some(format!("{}\nstderr:\n{}", stdout, stderr))
            } else {
                Some(stdout.to_string())
            }
        }
        "write_file" => {
            let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let bytes = result
                .get("bytes_written")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(format!("Written {} bytes to {}", bytes, path))
        }
        "str_replace_editor" => {
            let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let n = result
                .get("occurrences_replaced")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let match_type = result
                .get("match_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut text = format!("Replaced {} occurrence(s) in {} ({})", n, path, match_type);
            // Append fuzzy detail if present
            if let Some(detail) = result.get("fuzzy_match_detail") {
                if let Some(issues) = detail.get("line_issues").and_then(|v| v.as_array()) {
                    for issue in issues {
                        let line = issue.get("line").and_then(|v| v.as_str()).unwrap_or("?");
                        if let Some(diff) = issue.get("numerical_diff") {
                            let parts: Vec<String> = diff
                                .as_object()
                                .map(|obj| {
                                    obj.iter()
                                        .map(|(k, v)| format!("{}: {}", k.replace('_', " "), v))
                                        .collect()
                                })
                                .unwrap_or_default();
                            text.push_str(&format!("\n  Line {}: {}", line, parts.join(", ")));
                        }
                    }
                }
                if let Some(hint) = detail.get("hint").and_then(|v| v.as_str()) {
                    text.push_str(&format!("\n\n{}", hint));
                }
            }
            Some(text)
        }
        "grep_search" => {
            let truncated = result
                .get("truncated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let matches = result.get("matches").and_then(|v| v.as_array())?;
            if matches.is_empty() {
                return Some(if truncated {
                    "Matches were truncated (the kept tail has none); narrow the query or scope the path.".to_string()
                } else {
                    "No matches found.".to_string()
                });
            }
            let mut text = String::new();
            for m in matches {
                let p = m.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                let l = m.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                let t = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
                text.push_str(&format!("{}:{}:{}\n", p, l, t));
            }
            text.push_str(&format!("\u{2192} {} matches", matches.len()));
            if truncated {
                text.push_str(" (truncated; narrow the query or scope the path)");
            }
            Some(text)
        }
        "list_directory" => {
            let entries = result.get("entries").and_then(|v| v.as_array())?;
            if entries.is_empty() {
                return Some("(empty directory)".to_string());
            }
            let mut text = String::new();
            for entry in entries {
                let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let typ = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                let size = entry.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                text.push_str(&format!("{}\t{}\t{} bytes\n", name, typ, size));
            }
            Some(text.trim_end().to_string())
        }
        _ => None,
    }
}

/// Convert OpenAI Chat-style tool definitions to the OpenAI Responses API shape.
/// OpenAI Chat: { type: "function", function: { name, description, parameters } }
/// Responses:   { type: "function", name, description, parameters }
fn convert_tools_to_openai_responses(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            let name = func.get("name")?.as_str()?.to_string();
            let parameters = func.get("parameters").cloned();
            let mut tool = serde_json::Map::new();
            tool.insert("type".to_string(), json!("function"));
            if let Some(description) = func
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                tool.insert("description".to_string(), json!(description));
            }
            tool.insert("name".to_string(), json!(name));
            if let Some(parameters) = parameters {
                tool.insert("parameters".to_string(), parameters);
            }
            Some(serde_json::Value::Object(tool))
        })
        .collect()
}

/// Convert OpenAI-style tool definitions to Anthropic-style.
/// OpenAI: { type: "function", function: { name, description, parameters: {...} } }
/// Anthropic: { name, description, input_schema: {...} }
fn convert_tools_to_anthropic(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            let name = func.get("name")?.as_str()?.to_string();
            let parameters = func.get("parameters")?.clone();
            let mut tool = serde_json::Map::new();
            if let Some(description) = func
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                tool.insert("description".to_string(), json!(description));
            }
            tool.insert("name".to_string(), json!(name));
            tool.insert("input_schema".to_string(), parameters);
            Some(serde_json::Value::Object(tool))
        })
        .collect()
}

/// Convert messages for Anthropic API.
/// Anthropic does not accept role: "tool". Instead:
///
/// ```json
/// OpenAI:    { role: "tool", tool_call_id: "...", content: "..." }
/// Anthropic: { role: "user", content: [{ type: "tool_result", tool_use_id: "...", content: "..." }] }
/// ```
///
/// Also converts role: "assistant" messages that contain tool_calls or reasoning_content
/// into Anthropic's content-block-based format:
///
/// ```json
/// OpenAI:    { role: "assistant", content: "...", tool_calls: [...] }
/// Anthropic: { role: "assistant", content: [
///   { type: "thinking", thinking: "..." },
///   { type: "text", text: "..." },
///   { type: "tool_use", id: "...", name: "...", input: {...} }
/// ] }
/// ```
fn convert_message_for_anthropic(msg: &serde_json::Value) -> serde_json::Value {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

    // 1. role: "tool" messages (Tool result: Application -> LLM)
    //    OpenAI/Ollama: { role: "tool", tool_call_id: "...", content: "..." }
    //    Anthropic:     { role: "user", content: [{ type: "tool_result", tool_use_id: "...", content: "..." }] }
    if role == "tool" {
        let tool_use_id = msg
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // `tool_result.content` accepts a string (or block array); a structured
        // result object is sent back as JSON text.
        let content = match msg.get("content") {
            Some(serde_json::Value::String(s)) => json!(s),
            Some(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
                json!(v.to_string())
            }
            _ => json!(""),
        };
        json!({
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content
                }
            ]
        })
    }
    // 2. role: "assistant" messages that contain tool_calls or reasoning_content (Assistant response: LLM -> Application -> LLM)
    //    OpenAI/Ollama: { role: "assistant", content: "...", tool_calls: [...], reasoning_content: "..." }
    //    Anthropic:     { role: "assistant", content: [
    //        { type: "thinking", thinking: "..." },
    //        { type: "text", text: "..." },
    //        { type: "tool_use", id: "...", name: "...", input: {...} }
    //      ] }
    else if role == "assistant" {
        let has_tool_calls = msg.get("tool_calls").and_then(|v| v.as_array()).is_some();
        let has_reasoning = msg
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());

        if has_tool_calls || has_reasoning {
            let mut content_blocks: Vec<serde_json::Value> = Vec::new();

            // 2a. Add thinking block first if reasoning_content exists
            if let Some(reasoning) = msg.get("reasoning_content").and_then(|v| v.as_str())
                && !reasoning.is_empty()
            {
                content_blocks.push(json!({
                    "type": "thinking",
                    "thinking": reasoning
                }));
            }

            // 2b. Add text content block if present
            if let Some(content_str) = msg.get("content").and_then(|v| v.as_str())
                && !content_str.is_empty()
            {
                content_blocks.push(json!({
                    "type": "text",
                    "text": content_str
                }));
            }

            // 2c. Add tool_use blocks from tool_calls array
            if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(func) = tc.get("function") {
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let args = func.get("arguments");
                        // Normalize arguments: parse JSON string to object, or use object directly
                        let input = match args {
                            Some(serde_json::Value::String(s)) => {
                                serde_json::from_str(s).unwrap_or_else(|_| json!({}))
                            }
                            Some(v) => v.clone(),
                            None => json!({}),
                        };

                        content_blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input
                        }));
                    }
                }
            }

            json!({
                "role": "assistant",
                "content": content_blocks
            })
        } else {
            msg.clone()
        }
    } else if role == "user" {
        // User messages may have content as an array (when attachments are present).
        // Convert OpenAI-format image_url blocks to Anthropic image source blocks.
        if let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) {
            let converted: Vec<serde_json::Value> = blocks
                .iter()
                .map(|block| {
                    if block.get("type").and_then(|v| v.as_str()) == Some("image_url")
                        && let Some(url) = block
                            .get("image_url")
                            .and_then(|v| v.get("url"))
                            .and_then(|v| v.as_str())
                        && let Some((media_type, data)) = crate::file::parse_data_url(url)
                    {
                        return json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data
                            }
                        });
                    }
                    block.clone()
                })
                .collect();
            json!({
                "role": "user",
                "content": converted
            })
        } else {
            msg.clone()
        }
    } else {
        msg.clone()
    }
}

/// Parse and accumulate usage/token information from a single SSE stream event.
///
/// This is the central aggregation point for token accounting across all providers.
/// It handles three wire formats transparently:
///
/// | Provider   | Wire format                                              |
/// |-----------|----------------------------------------------------------|
/// | OpenAI    | `{ usage: { prompt_tokens, completion_tokens, ... } }`   |
/// | Anthropic | Pre-converted by [`convert_anth_to_openai_format`]       |
/// | Ollama    | `{ done: true, prompt_eval_count, eval_count }`          |
///
/// Because Anthropic splits input and output tokens across separate SSE events
/// (`message_start` and `message_delta`), this function **accumulates** into
/// `current` rather than replacing it.
pub(crate) fn accumulate_usage(json: &serde_json::Value, current: &mut Option<Usage>) {
    if let Some(usage_val) = json.get("usage") {
        if let Ok(u) = serde_json::from_value::<Usage>(usage_val.clone()) {
            if let Some(existing) = current.as_mut() {
                // Accumulate tokens to handle Anthropic split usage events
                existing.prompt_tokens += u.prompt_tokens;
                existing.completion_tokens += u.completion_tokens;
                // Preserve prompt_tokens_details from the first non-empty details
                if u.prompt_tokens_details.as_ref().is_some_and(|d| {
                    d.cached_tokens > 0 || d.cache_creation_tokens > 0 || d.audio_tokens > 0
                }) && existing.prompt_tokens_details.is_none()
                {
                    existing.prompt_tokens_details = u.prompt_tokens_details;
                }
                // Accumulate completion_tokens_details (reasoning tokens from OpenAI o1/o3)
                if let Some(ref details) = u.completion_tokens_details {
                    if let Some(existing_details) = existing.completion_tokens_details.as_mut() {
                        existing_details.reasoning_tokens += details.reasoning_tokens;
                    } else {
                        existing.completion_tokens_details = Some(details.clone());
                    }
                }
            } else {
                *current = Some(u);
            }
        }
    } else if json.get("done") == Some(&serde_json::Value::Bool(true)) {
        // Map Ollama native stats to Usage struct
        let p = json
            .get("prompt_eval_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let c = json.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if p > 0 || c > 0 {
            *current = Some(Usage {
                prompt_tokens: p,
                completion_tokens: c,
                ..Default::default()
            });
        }
    }
}

/// Converts Anthropic stream events into an OpenAI-compatible JSON format
/// for the existing pipeline.
pub fn convert_anth_to_openai_format(
    anth: serde_json::Value,
    tool_index: &mut usize,
) -> serde_json::Value {
    // 1. text_delta -> choices[0].delta.content
    if anth.get("type") == Some(&json!("content_block_delta")) {
        if let Some(delta) = anth.get("delta") {
            let delta_type = delta.get("type").and_then(|v| v.as_str());

            if delta_type == Some("text_delta") {
                if let Some(text) = delta.get("text") {
                    return json!({ "choices": [{ "delta": { "content": text } }] });
                }
            }
            // 2. thinking_delta -> choices[0].delta.reasoning_content
            else if delta_type == Some("thinking_delta") {
                if let Some(thinking) = delta.get("thinking") {
                    return json!({
                        "choices": [{ "delta": { "reasoning_content": thinking } }]
                    });
                }
            }
            // 3. input_json_delta -> choices[0].delta.tool_calls
            // Use the event's own index (Anthropic sends per-block index)
            // to correctly route arguments to parallel tool calls.
            else if delta_type == Some("input_json_delta")
                && let Some(partial) = delta.get("partial_json")
            {
                let index = anth
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(*tool_index as u64) as usize;
                return json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "function": { "arguments": partial }
                            }]
                        }
                    }]
                });
            }
        }
    }
    // 4. content_block_start -> choices[0].delta.tool_calls (id, name)
    else if anth.get("type") == Some(&json!("content_block_start")) {
        if let Some(index) = anth.get("index").and_then(|v| v.as_u64()) {
            *tool_index = index as usize; // update index
        }
        if let Some(block) = anth.get("content_block")
            && block.get("type") == Some(&json!("tool_use"))
        {
            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            return json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": *tool_index,
                            "id": id,
                            "function": { "name": name, "arguments": "" }
                        }]
                    }
                }]
            });
        }
    }
    // 5. message_start / message_delta -> usage
    else if anth.get("type") == Some(&json!("message_start")) {
        if let Some(usage) = anth.get("message").and_then(|m| m.get("usage")) {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_create = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            return json!({
                "usage": {
                    "prompt_tokens": input_tokens,
                    "completion_tokens": 0,
                    "prompt_tokens_details": {
                        "cached_tokens": cache_read,
                        "cache_creation_tokens": cache_create
                    }
                }
            });
        }
    } else if anth.get("type") == Some(&json!("message_delta"))
        && let Some(usage) = anth.get("usage")
    {
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        return json!({
            "usage": { "prompt_tokens": 0, "completion_tokens": output_tokens }
        });
    }

    json!({})
}

/// Converts one OpenAI Responses API stream event into the internal OpenAI
/// Chat Completions delta format (`choices[0].delta.{content,reasoning_content,tool_calls}`)
/// used by the rest of the pipeline (mirrors [`convert_anth_to_openai_format`]).
///
/// OpenAI Responses streams are event-based: each `data:` payload carries a
/// `type` discriminator. Tool-call `arguments` are delivered as one complete
/// JSON string on `response.output_item.done` (whose `output_index` matches the
/// earlier `response.output_item.added`), so no cross-event state is required.
pub fn convert_openai_responses_event_to_openai_format(
    event: serde_json::Value,
) -> serde_json::Value {
    let event_type = event.get("type").and_then(|v| v.as_str());
    match event_type {
        // 1. text chunks (output_text, or refusal surfaced as text)
        Some("response.output_text.delta" | "response.refusal.delta") => {
            if let Some(text) = event.get("delta").and_then(|v| v.as_str()) {
                return json!({ "choices": [{ "delta": { "content": text } }] });
            }
        }
        // 2. reasoning chunks (readable summary or raw reasoning text)
        Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
            if let Some(text) = event.get("delta").and_then(|v| v.as_str()) {
                return json!({ "choices": [{ "delta": { "reasoning_content": text } }] });
            }
        }
        // 3. tool call start: id + name (arguments stream separately)
        Some("response.output_item.added") => {
            let item = event.get("item");
            if item.and_then(|v| v.get("type")).and_then(|v| v.as_str()) == Some("function_call") {
                let index = event
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let id = item
                    .and_then(|v| v.get("call_id"))
                    .and_then(|v| v.as_str())
                    .or_else(|| item.and_then(|v| v.get("id")).and_then(|v| v.as_str()))
                    .unwrap_or("");
                let name = item
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                return json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": id,
                                "function": { "name": name, "arguments": "" }
                            }]
                        }
                    }]
                });
            }
        }
        // 4. tool call completion: complete `arguments` JSON string
        Some("response.output_item.done") => {
            let item = event.get("item");
            if item.and_then(|v| v.get("type")).and_then(|v| v.as_str()) == Some("function_call") {
                let index = event
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let arguments = item
                    .and_then(|v| v.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                return json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "function": { "arguments": arguments }
                            }]
                        }
                    }]
                });
            }
        }
        // 5. final events: map `response.status` to the internal finish_reason
        //    and (for `response.completed`) `response.usage` to the internal
        //    usage shape. `response.incomplete` is the abnormal terminal
        //    event: no usage, but `incomplete_details.reason` says why.
        Some("response.completed" | "response.incomplete") => {
            let status = event
                .get("response")
                .and_then(|r| r.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let reason = event
                .get("response")
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let finish_reason = if reason.is_empty() {
                format!("{} (responses)", status)
            } else {
                format!("{} (responses: {})", status, reason)
            };
            let mut converted = json!({
                "choices": [{ "finish_reason": finish_reason }]
            });
            if let Some(usage) = event.get("response").and_then(|r| r.get("usage")) {
                let input_tokens = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output_tokens = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cached_tokens = usage
                    .get("input_tokens_details")
                    .and_then(|v| v.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let reasoning_tokens = usage
                    .get("output_tokens_details")
                    .and_then(|v| v.get("reasoning_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                converted["usage"] = json!({
                    "prompt_tokens": input_tokens,
                    "completion_tokens": output_tokens,
                    "prompt_tokens_details": { "cached_tokens": cached_tokens },
                    "completion_tokens_details": { "reasoning_tokens": reasoning_tokens }
                });
            }
            return converted;
        }
        _ => {}
    }

    json!({})
}

#[cfg(test)]
#[path = "tests/compat_provider_test.rs"]
mod tests;
