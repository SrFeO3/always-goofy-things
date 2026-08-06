//! Domain model types shared across the application.
//!
//! Contains the core data structures for messages, sessions, settings,
//! and LLM communication payloads. All types are owned and `Clone`-able.

#[cfg(feature = "gui")]
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::attach::AttachedFile;
use crate::compat_provider::LlmProvider;
use crate::compat_resilience::ToolResultFormat;
use crate::startup;
use crate::tools::ToolRunDecision;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Message {
    pub(crate) role: String,
    pub(crate) content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
    #[serde(skip)]
    pub tool_name: Option<String>,
    #[serde(skip)]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    #[serde(skip)]
    pub model: Option<String>,
    #[serde(skip)]
    pub tool_call_decision: Option<ToolRunDecision>,
    #[serde(skip)]
    #[allow(dead_code)]
    pub tool_args: Option<serde_json::Value>,
    #[serde(skip)]
    pub attached_files: Vec<AttachedFile>,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            role: String::new(),
            content: String::new(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            timestamp: chrono::Utc::now(),
            model: None,
            tool_call_decision: None,
            tool_args: None,
            attached_files: Vec::new(),
        }
    }
}

/// Append-only conversation state. Single mutable owner, never concurrent.
#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) label: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) turn: i32,
}

impl Session {
    /// New session starting at turn 1 with the system message as `messages[0]`.
    pub(crate) fn new(label: String, system_message: Message) -> Self {
        Self {
            label,
            messages: vec![system_message],
            turn: 1,
        }
    }
}

/// Runtime settings mutable via `/model` / `/config` slash commands.
#[derive(Clone)]
pub(crate) struct Settings {
    pub(crate) llm_model: String,
    pub(crate) verbose_level: startup::Verbosity,
    pub(crate) pretty_level: u8,
    pub(crate) llm_rpm: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) max_empty_retry: u32,
    pub(crate) last_llm_call: Option<std::time::Instant>,
    /// `session.messages.len()` snapshot right after the last assistant push.
    /// Drives verbose-level 2 incremental display in `call_llm`. Updated after
    /// assistant push but NOT after tool push, so tool messages show up as the
    /// diff on the next call. `/rewind` / `/restore` leave this stale on
    /// purpose: a stale value fails the `last < req.messages.len()` check and
    /// falls back to full display, matching the old explicit-reset behaviour.
    pub(crate) last_sent_count: usize,
}

impl Settings {
    /// Initialize runtime settings from CLI `Config`.
    pub(crate) fn from_config(config: &startup::Config) -> Self {
        Self {
            llm_model: config.llm_model.clone(),
            verbose_level: config.verbose_level,
            pretty_level: config.pretty_level,
            llm_rpm: config.llm_rpm,
            max_output_tokens: config.max_output_tokens,
            max_empty_retry: config.max_empty_retry,
            last_llm_call: None,
            last_sent_count: 0,
        }
    }
}

/// Accumulated token metrics, derived from LLM responses.
#[derive(Clone, Default)]
pub(crate) struct Metrics {
    pub(crate) in_normal: u64,
    pub(crate) in_cached: u64,
    pub(crate) out: u64,
    pub(crate) reasoning: u64,
    pub(crate) cache_ever_reported: bool,
}

/// Shared buffer for LLM streaming output.
/// `.0` = reasoning, `.1` = content, `.2` = system, `.3` = user.
/// Worker writes chunks via `push_str`; the GUI reads and clears them each frame.
#[cfg(feature = "gui")]
pub(crate) static LLM_STREAM_BUF: LazyLock<Mutex<(String, String, String, String)>> =
    LazyLock::new(|| Mutex::new((String::new(), String::new(), String::new(), String::new())));

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub tool_type: String,
    pub(crate) function: FunctionCall,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct FunctionCall {
    pub(crate) name: String,
    pub(crate) arguments: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatRequest {
    pub(crate) provider: LlmProvider,
    pub(crate) model: String,
    pub(crate) max_output_tokens: usize,
    pub(crate) tools: Vec<serde_json::Value>,
    pub(crate) stream: bool,
    pub(crate) messages: Vec<Message>,
    pub(crate) tool_result_format: ToolResultFormat,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) prompt_tokens: u32,
    #[serde(default)]
    pub(crate) completion_tokens: u32,
    #[serde(default)]
    pub(crate) prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub(crate) completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub(crate) cached_tokens: u32,
    /// Anthropic: tokens written to cache this request (billed at full price, cached for future reads)
    #[serde(default)]
    pub(crate) cache_creation_tokens: u32,
    /// OpenAI: audio input tokens (GPT-4o-audio-preview, billed differently)
    #[serde(default)]
    pub(crate) audio_tokens: u32,
}

/// Breakdown of completion/output tokens (OpenAI reasoning models, Anthropic extended thinking).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct CompletionTokensDetails {
    /// OpenAI o1/o3/o4-mini: internal reasoning tokens (billed at a higher rate)
    #[serde(default)]
    pub(crate) reasoning_tokens: u32,
}
