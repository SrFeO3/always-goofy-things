//! LLM provider compatibility: Output recovery, format normalization, and resilience.
//!
//! Handles recovery and normalization of non-standard, malformed, or incomplete LLM outputs.
//! This module provides mechanisms to heal flat tool call structures, reconstruct missing fields
//! during streaming, and validate malformed JSON arguments. It also defines how tool execution
//! results should be structured (as text, JSON, or objects) when presenting them back to the LLM.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{FunctionCall, ToolCall};

/// How tool results are formatted when sent back to the LLM.
/// Which format works best depends on the LLM's capabilities;
/// some models handle plain text better, others handle structured JSON.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ToolResultFormat {
    /// Send the full result as a JSON-encoded string (e.g. `"{\"stdout\": \"...\"}"`)
    #[default]
    JsonString,
    /// Render each tool result as a concise text string (e.g. file contents, command output, summary lines)
    Text,
    /// Send the full result as a proper JSON structure (un-escaped object)
    JsonStructured,
}

impl fmt::Display for ToolResultFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ToolResultFormat::JsonString => "json_string",
            ToolResultFormat::Text => "text",
            ToolResultFormat::JsonStructured => "json_structured",
        };
        write!(f, "{s}")
    }
}

/// Infer tool name from argument keys by matching against tool definitions.
///
/// Examines each tool's `parameters.properties` keys and returns the best
/// matching tool name, or `None` if no tool's parameter set matches.
///
/// Scoring:
/// - +3 for each arg key matching a required parameter
/// - +1 for each arg key matching an optional parameter
/// - -3 for each arg key that is not a known parameter of the tool
/// - -1 for each required parameter missing from the args
fn infer_tool_name_from_args(
    tool_defs: &[serde_json::Value],
    args: &serde_json::Value,
) -> Option<String> {
    let args_obj = args.as_object()?;
    let arg_keys: Vec<&str> = args_obj.keys().map(|s| s.as_str()).collect();

    let mut best: Option<(&str, i32)> = None;

    for def in tool_defs {
        let func = def.get("function")?;
        let name = func.get("name")?.as_str()?;
        let params = func.get("parameters")?;
        let props = params.get("properties")?.as_object()?;
        let required = params.get("required")?.as_array()?;

        let required_set: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        let all_params: Vec<&str> = props.keys().map(|s| s.as_str()).collect();

        let mut score = 0i32;

        for key in &arg_keys {
            if required_set.contains(key) {
                score += 3;
            } else if all_params.contains(key) {
                score += 1;
            } else {
                score -= 3; // Unknown key -> strongly penalize
            }
        }

        // Penalize missing required params
        for req in &required_set {
            if !arg_keys.contains(req) {
                score -= 1;
            }
        }

        if score > 0 {
            match best {
                Some((_, s)) if score > s => best = Some((name, score)),
                None => best = Some((name, score)),
                _ => {}
            }
        }
    }

    best.map(|(n, _)| n.to_string())
}

/// Normalizes a single tool_calls array: lifts `name`/`arguments` from top level
/// into a `function` wrapper where missing.
pub(crate) fn normalize_tool_calls_array(tool_calls: &mut [Value]) -> bool {
    let mut modified = false;
    for call in tool_calls.iter_mut() {
        if call.get("function").is_some() {
            continue;
        }
        let Some(obj) = call.as_object_mut() else {
            continue;
        };

        let name = obj.remove("name");
        let arguments = obj.remove("arguments");
        if name.is_none() && arguments.is_none() {
            continue;
        }

        let mut func = serde_json::Map::new();
        if let Some(n) = name {
            func.insert("name".to_string(), n);
        }
        if let Some(a) = arguments {
            func.insert("arguments".to_string(), a);
        }
        obj.insert("function".to_string(), Value::Object(func));
        modified = true;
    }
    modified
}

/// Converts tool_call entries from non-OpenAI format (name/arguments at top level)
/// to standard OpenAI format (name/arguments inside a `function` wrapper).
///
/// Handles both Ollama native (`{message:{tool_calls:[...]}}`) and
/// OpenAI streaming (`{choices:[{delta:{tool_calls:[...]}}]}`) shapes.
///
/// Returns `true` if any tool_call was normalized.
pub fn normalize_tool_call_format(json: &mut Value) -> bool {
    // 1. Try message.tool_calls (Ollama native / non-streaming)
    if let Some(msg) = json.get_mut("message")
        && let Some(arr) = msg.get_mut("tool_calls").and_then(|tc| tc.as_array_mut())
        && normalize_tool_calls_array(arr)
    {
        return true;
    }

    // 2. Try choices[0].{delta,message}.tool_calls (OpenAI streaming)
    if let Some(arr) = json.get_mut("choices").and_then(|c| c.as_array_mut())
        && let Some(choice) = arr.first_mut()
    {
        // 2a. delta.tool_calls
        if let Some(arr) = choice
            .get_mut("delta")
            .and_then(|d| d.get_mut("tool_calls"))
            .and_then(|tc| tc.as_array_mut())
            && normalize_tool_calls_array(arr)
        {
            return true;
        }
        // 2b. message.tool_calls
        if let Some(arr) = choice
            .get_mut("message")
            .and_then(|m| m.get_mut("tool_calls"))
            .and_then(|tc| tc.as_array_mut())
        {
            return normalize_tool_calls_array(arr);
        }
    }

    false
}

/// Post-processes tool calls assembled from SSE streaming to handle
/// DeepSeek-style missing fields (id, function.name) and validate
/// arguments JSON in parallel tool calling scenarios.
pub fn post_process_tool_calls(tool_calls: &mut Vec<ToolCall>, tool_defs: &[serde_json::Value]) {
    // First pass: fill in missing ids and function names
    for i in 0..tool_calls.len() {
        if tool_calls[i].id.is_empty() {
            tool_calls[i].id = format!("call_missing_{}", i);
        }
        if tool_calls[i].function.name.is_empty() {
            if i > 0 {
                // Copy function.name from the previous tool call
                // (assumes parallel calls use the same tool)
                let prev_name = tool_calls[i - 1].function.name.clone();
                if !prev_name.is_empty() {
                    tool_calls[i].function.name = prev_name;
                }
            }
            // Try to infer from argument keys against tool definitions
            if tool_calls[i].function.name.is_empty() {
                // Parse arguments JSON string if needed
                let args = match &tool_calls[i].function.arguments {
                    serde_json::Value::String(s) => {
                        serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                    }
                    other => other.clone(),
                };
                if let Some(inferred) = infer_tool_name_from_args(tool_defs, &args) {
                    tool_calls[i].function.name = inferred;
                }
            }
        }
    }
    // Second pass: remove tool calls that still have empty function.name
    // or have unparseable arguments JSON (completely broken tool calls)
    tool_calls.retain(|tc| {
        // Guard: empty function.name -> unrecoverable
        if tc.function.name.is_empty() {
            return false;
        }
        // Guard: unparseable arguments JSON -> also unrecoverable
        if let serde_json::Value::String(s) = &tc.function.arguments
            && serde_json::from_str::<serde_json::Value>(s).is_err()
        {
            return false;
        }
        true
    });
}

/// Extract the per-event message base from a parsed SSE payload,
/// supporting OpenAI (`choices[0].delta`) and Ollama native (`message`)
/// shapes. Anthropic events are pre-converted to OpenAI format by the
/// caller.
pub fn extract_msg_base(json: &Value) -> &Value {
    if let Some(message) = json.get("message") {
        message // Ollama native
    } else if let Some(choices) = json.get("choices") {
        choices
            .get(0)
            .and_then(|c| c.get("delta"))
            .unwrap_or(&Value::Null) // OpenAI delta
    } else {
        &Value::Null
    }
}

/// Merge an array of streaming `tool_calls` deltas into the assembled vector.
///
/// `default_index` is used when a delta lacks an explicit `index` field.
/// `with_thought_signature=false` skips the `thought_signature` field
/// (preserving the trailing-buffer drain's historical behavior).
pub fn merge_tool_call_delta(
    tool_calls: &mut Vec<ToolCall>,
    calls: &[Value],
    default_index: usize,
    with_thought_signature: bool,
) {
    for call_json in calls {
        let index = call_json
            .get("index")
            .and_then(|v| v.as_u64())
            .unwrap_or(default_index as u64) as usize;

        while tool_calls.len() <= index {
            tool_calls.push(ToolCall {
                id: String::new(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: String::new(),
                    arguments: serde_json::Value::String(String::new()),
                },
                thought_signature: None,
            });
        }

        let target = &mut tool_calls[index];
        if let Some(id) = call_json.get("id").and_then(|v| v.as_str()) {
            target.id.push_str(id);
        }
        if with_thought_signature
            && let Some(sig) = call_json.get("thought_signature").and_then(|v| v.as_str())
        {
            target.thought_signature = Some(sig.to_string());
        }
        if let Some(func) = call_json.get("function") {
            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                target.function.name.push_str(name);
            }
            if let Some(args) = func.get("arguments") {
                match args {
                    serde_json::Value::String(s) => {
                        // Stream delta: append to existing string
                        if let Some(existing) = target.function.arguments.as_str() {
                            target.function.arguments =
                                serde_json::Value::String(format!("{}{}", existing, s));
                        }
                    }
                    _ => {
                        // Full object: replace (common in some local providers)
                        target.function.arguments = args.clone();
                    }
                }
            }
        }
    }
}
