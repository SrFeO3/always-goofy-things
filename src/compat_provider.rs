//! LLM provider compatibility: API schemas, request formatting, and protocol adaptation.
//!
//! Handles provider-specific request payload formatting and protocol translation.
//! This module manages URL-based provider detection, request DTO definitions, wire-format conversions
//! (including Anthropic's streaming SSE), and tool-definition translation. It also formats message contents,
//! such as tool result rendering, to match each provider's expected request structure.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;

use crate::compat_resilience::ToolResultFormat;
use crate::{ChatRequest, Message};

/// LLM API provider type
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, clap::ValueEnum)]
pub enum LlmProvider {
    OpenAi,
    Ollama,
    Anthropic,
}

impl fmt::Display for LlmProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LlmProvider::OpenAi => "openai",
            LlmProvider::Ollama => "ollama",
            LlmProvider::Anthropic => "anthropic",
        };
        write!(f, "{s}")
    }
}

#[derive(Serialize)]
struct OpenAiRequestDto {
    model: String,
    max_completion_tokens: usize,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
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
    num_predict: usize,
    num_ctx: usize,
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

    if url_lower.contains("api.anthropic.com") || url_lower.contains("/v1/messages") {
        return LlmProvider::Anthropic;
    }

    if url_lower.contains("/api/chat") || url_lower.contains(":11434") {
        return LlmProvider::Ollama;
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

        match self.provider {
            LlmProvider::OpenAi => {
                let dto = OpenAiRequestDto {
                    model: self.model.clone(),
                    max_completion_tokens: self.max_output_tokens,
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

            LlmProvider::Ollama => {
                let dto = OllamaRequestDto {
                    model: self.model.clone(),
                    messages,
                    tools: self.tools.clone(),
                    options: OllamaOptions {
                        num_predict: self.max_output_tokens,
                        num_ctx: self.max_output_tokens,
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
            let matches = result.get("matches").and_then(|v| v.as_array())?;
            if matches.is_empty() {
                return Some("No matches found.".to_string());
            }
            let mut text = String::new();
            for m in matches {
                let p = m.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                let l = m.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                let t = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
                text.push_str(&format!("{}:{}:{}\n", p, l, t));
            }
            text.push_str(&format!("\u{2192} {} matches", matches.len()));
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

/// Convert OpenAI-style tool definitions to Anthropic-style.
/// OpenAI: { type: "function", function: { name, description, parameters: {...} } }
/// Anthropic: { name, description, input_schema: {...} }
fn convert_tools_to_anthropic(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            let name = func.get("name")?.as_str()?.to_string();
            let description = func.get("description")?.as_str()?.to_string();
            let parameters = func.get("parameters")?.clone();
            Some(json!({
                "name": name,
                "description": description,
                "input_schema": parameters
            }))
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

    // 1. role: "tool" messages (Tool result: Application → LLM)
    //    OpenAI/Ollama: { role: "tool", tool_call_id: "...", content: "..." }
    //    Anthropic:     { role: "user", content: [{ type: "tool_result", tool_use_id: "...", content: "..." }] }
    if role == "tool" {
        let tool_use_id = msg
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = msg.get("content").cloned().unwrap_or_else(|| json!(""));
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
    // 2. role: "assistant" messages that contain tool_calls or reasoning_content (Assistant response: LLM → Application → LLM)
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
    } else {
        msg.clone()
    }
}

/// Converts Anthropic stream events into an OpenAI-compatible JSON format
/// for the existing agent pipeline.
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
        if let Some(input_tokens) = anth
            .get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("input_tokens"))
        {
            return json!({
                "usage": { "prompt_tokens": input_tokens, "completion_tokens": 0 }
            });
        }
    } else if anth.get("type") == Some(&json!("message_delta"))
        && let Some(output_tokens) = anth.get("usage").and_then(|u| u.get("output_tokens"))
    {
        return json!({
            "usage": { "prompt_tokens": 0, "completion_tokens": output_tokens }
        });
    }

    json!({})
}
