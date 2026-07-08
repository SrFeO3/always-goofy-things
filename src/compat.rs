//! LLM provider compatibility layer.
//!
//! Handles API-specific differences in request formats, authentication requirements,
//! and protocol adaptations across supported LLM providers.
//! Includes special handling for APIs with unconventional message and tool semantics.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ChatRequest;

/// LLM API provider type
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, clap::ValueEnum)]
pub enum LlmProvider {
    OpenAi,
    OpenAiCompatible,
    Ollama,
    Anthropic,
}

impl fmt::Display for LlmProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LlmProvider::OpenAi => "openai",
            LlmProvider::OpenAiCompatible => "openai-compatible",
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

    LlmProvider::OpenAiCompatible
}

impl ChatRequest {
    pub fn to_provider_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        let raw_messages = self
            .messages
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;

        match self.provider {
            LlmProvider::OpenAi | LlmProvider::OpenAiCompatible => {
                let dto = OpenAiRequestDto {
                    model: self.model.clone(),
                    max_completion_tokens: self.max_output_tokens,
                    messages: raw_messages,
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
                    messages: raw_messages,
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

                let filtered_messages: Vec<serde_json::Value> = raw_messages
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
///   - OpenAI: { role: "tool", tool_call_id: "...", content: "..." }
///   - Anthropic: { role: "user", content: [{ type: "tool_result", tool_use_id: "...", content: "..." }] }
fn convert_message_for_anthropic(msg: &serde_json::Value) -> serde_json::Value {
    if msg.get("role") == Some(&json!("tool")) {
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
    use serde_json::json;

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
                    return json!({ "choices": [{ "delta": { "reasoning_content": thinking } }] });
                }
            }
            // 3. input_json_delta -> choices[0].delta.tool_calls
            else if delta_type == Some("input_json_delta") {
                if let Some(partial) = delta.get("partial_json") {
                    return json!({
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": *tool_index,
                                    "function": { "arguments": partial }
                                }]
                            }
                        }]
                    });
                }
            }
        }
    }
    // 4. content_block_start -> choices[0].delta.tool_calls (id, name)
    else if anth.get("type") == Some(&json!("content_block_start")) {
        if let Some(index) = anth.get("index").and_then(|v| v.as_u64()) {
            *tool_index = index as usize; // update index
        }
        if let Some(block) = anth.get("content_block") {
            if block.get("type") == Some(&json!("tool_use")) {
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
    }
    // 5. message_start / message_delta -> usage
    else if anth.get("type") == Some(&json!("message_start")) {
        if let Some(input_tokens) = anth
            .get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("input_tokens"))
        {
            return json!({ "usage": { "prompt_tokens": input_tokens, "completion_tokens": 0 } });
        }
    } else if anth.get("type") == Some(&json!("message_delta")) {
        if let Some(output_tokens) = anth.get("usage").and_then(|u| u.get("output_tokens")) {
            return json!({ "usage": { "prompt_tokens": 0, "completion_tokens": output_tokens } });
        }
    }

    json!({})
}
