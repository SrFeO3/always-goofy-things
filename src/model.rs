//! Domain model types shared across the application.
//!
//! Contains the core data structures for messages, sessions, settings,
//! and LLM communication payloads. All types are owned and `Clone`-able.

#[cfg(feature = "gui")]
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    /// Session ID, never sent to the LLM (`#[serde(skip)]`). Fixed for one
    /// `Session` (kept across `/rewind`/`/restore`); todo sub-sessions get their own.
    #[serde(skip)]
    pub(crate) session_id: String,
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
            session_id: String::new(),
        }
    }
}

/// Append-only conversation state. Single mutable owner, never concurrent.
#[derive(Clone)]
pub(crate) struct Session {
    /// Session ID (UUID v4); persisted per-message via `Message.session_id`.
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) turn: i32,
}

impl Session {
    /// New session starting at turn 1 with the system message as `messages[0]`.
    pub(crate) fn new(label: String, system_message: Message) -> Self {
        let id = Uuid::new_v4().to_string();
        let mut messages = vec![system_message];
        // Stamp messages[0] so the ID persists to JSONL (survives restore/rewind/archive).
        messages[0].session_id = id.clone();
        Self {
            id,
            label,
            messages,
            turn: 1,
        }
    }

    /// Push a message stamped with this session's ID; returns it for persistence.
    pub(crate) fn push_message(&mut self, mut msg: Message) -> &mut Message {
        msg.session_id = self.id.clone();
        self.messages.push(msg);
        self.messages.last_mut().unwrap()
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
    pub(crate) max_reasoning_empty_responses: u32,
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
            max_reasoning_empty_responses: config.max_reasoning_empty_responses,
            last_llm_call: None,
            last_sent_count: 0,
        }
    }
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
    /// OpenAI fallback: use `max_tokens` instead of `max_completion_tokens`
    /// (legacy models 400 on it); set by the `call_llm` retry.
    #[serde(default)]
    pub(crate) max_tokens_fallback: bool,
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

#[cfg(test)]
#[path = "tests/model_test.rs"]
mod tests;
