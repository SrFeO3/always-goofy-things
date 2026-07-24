//! CLI entry point and main execution loop.
//!
//! Coordinates the application's lifecycle and runs the interactive
//! LLM conversation loop.
//!
//! # Safety Warning
//!
//! This application executes autonomous actions on your behalf, including
//! file system modifications, shell command execution, and internet access.
//! Review tool calls carefully before granting execution as these operations
//! may impact your local environment or interact with external servers.
//!
//! # Execution Flow
//!
//! 1. [User Input]   : Capture query from the terminal.
//! 2. [Reason Loop]  : Recursive cycle for complex tasks.
//!    - LLM Call     : Process context and decide next action.
//!    - Tool Exec    : If requested, run local tool and get result.
//!    - Feedback     : Add result back to history and repeat.
//! 3. [Final Answer] : Present the completed outcome to the user.

use std::io;
use std::io::Write;

use anyhow::{Result, anyhow};
use clap::Parser;
use futures_util::StreamExt;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use serde::{Deserialize, Serialize};
use serde_json::json;

mod attach;
mod cmd;
mod compat_provider;
mod compat_resilience;
mod file;
mod file_pdf;
mod persistence;
mod pretty;
mod reflex;
mod reflex_literal_filter;
mod startup;
mod tools;
mod tools_fuzzy;

use attach::AttachedFile;
use compat_provider::{LlmProvider, convert_anth_to_openai_format};
use compat_resilience::{
    ToolResultFormat, extract_msg_base, merge_tool_call_delta, post_process_tool_calls,
};
use file::FileType;
use startup::{C_CYAN, C_DIM_GREEN, C_GRAY, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, RESET};
use tools::{ToolRunDecision, ToolRunDecisionKind};

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Message {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip)]
    pub tool_name: Option<String>,
    #[serde(skip)]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    #[serde(skip)]
    pub model: Option<String>,
    #[serde(skip)]
    pub tool_call_decision: Option<tools::ToolRunDecision>,
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
            attached_files: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub tool_type: String,
    function: FunctionCall,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct FunctionCall {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatRequest {
    provider: LlmProvider,
    model: String,
    max_output_tokens: usize,
    tools: Vec<serde_json::Value>,
    stream: bool,
    messages: Vec<Message>,
    tool_result_format: ToolResultFormat,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
    /// Anthropic: tokens written to cache this request (billed at full price, cached for future reads)
    #[serde(default)]
    cache_creation_tokens: u32,
    /// OpenAI: audio input tokens (GPT-4o-audio-preview, billed differently)
    #[serde(default)]
    audio_tokens: u32,
}

/// Breakdown of completion/output tokens (OpenAI reasoning models, Anthropic extended thinking).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct CompletionTokensDetails {
    /// OpenAI o1/o3/o4-mini: internal reasoning tokens (billed at a higher rate)
    #[serde(default)]
    reasoning_tokens: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = startup::Config::parse();

    // Validate: -o requires -q
    if config.output_file.is_some() && config.query.is_none() {
        anyhow::bail!("--output/-o requires --query/-q (batch mode)");
    }

    let is_batch = config.query.is_some();
    let provider: LlmProvider = config
        .provider
        .unwrap_or_else(|| compat_provider::detect_provider(&config.llm_url));

    if is_batch {
        // Batch: set up working directory silently (no banner)
        let current_dir = std::fs::canonicalize(&config.working_dir)
            .map_err(|e| anyhow!("Invalid working directory '{}': {}", config.working_dir, e))?;
        std::env::set_current_dir(&current_dir)?;
    } else {
        let _current_dir = startup::print_startup_info(&config, &provider)?;
    }

    let mut last_sent_count = 0;
    let mut last_llm_call: Option<std::time::Instant> = None;
    let mut total_in_normal = 0u64;
    let mut total_in_cached = 0u64;
    let mut total_out = 0u64;
    let mut total_reasoning = 0u64;
    let mut cache_ever_reported = false;

    let mut query_reader = DefaultEditor::new()?;

    // Mutable model name so `/model` can switch it on the fly
    let mut llm_model = config.llm_model.clone();
    // Mutable session label so `/restore <label>` can switch it on the fly
    let mut session_label = config.session_label.clone();
    // Mutable config values so `/config` can switch them on the fly
    let mut verbose_level = config.verbose_level;
    let mut pretty_level = config.pretty_level;
    let mut llm_rpm = config.llm_rpm;
    let mut max_output_tokens = config.max_output_tokens;
    let mut max_empty_retry = config.max_empty_retry;

    if !is_batch {
        println!(
            "\n{}Describe your task and press Enter to start (or /help, /exit, ^D).{}",
            C_CYAN, RESET
        );
    }

    let mut messages = vec![Message {
        role: "system".to_string(), // Set the initial system instructions
        content: format!(
            "You are an expert software engineering assistant. Follow these immutable rules:\n\n\
            ## 0. Workspace Context\n\
            - Current Working Directory: Your root is ./ (the current directory).\n\
            - Relative Paths Only: You MUST use relative paths (e.g., file.txt, ./src/) for all operations.\n\
            - Prohibitions: NEVER use absolute paths starting with /. NEVER use ../ to escape the directory.\n\n\
            ## 1. Command Execution (bash)\n\
            - Allowed command patterns: [{}]\n\
            - Interactive commands (e.g., nano, vim, top, ssh) are strictly forbidden. Always check the whitelist.\n\n\
            ## 2. File Editing (str_replace_editor, write_file)\n\
            - str_replace_editor: Provide 'old_string' exactly as it appears in the file, including all whitespace and indentation.\n\
            - write_file: Use this to create new files or overwrite existing files entirely.\n\n\
            ## 3. Information Retrieval (fetch_web)\n\
            - Supports only http/https. Access to private or local networks is strictly prohibited.\n\n\
            ## 4. Response Style\n\
            - Briefly explain the purpose of a tool before calling it.\n\
            - Maintain system rules at the top of the context for inference efficiency.",
            tools::ALLOW_COMMAND_LIST.join(", ")
        ),
        ..Default::default()
    }];
    // On startup: move meaningful last_session -> previous_session if it exists
    persistence::init_session(&session_label)?;
    // Save system message as the first line of the new session
    persistence::save_message(&session_label, &messages[0])?;

    // Main conversation loop
    let mut turn: i32 = 1;
    let mut batch_input: Option<String> = config.query.clone();
    loop {
        let input = if let Some(q) = batch_input.take() {
            // Batch: use the -q argument as the first (and only) user input
            q
        } else if is_batch {
            // Batch mode with no more input (should not reach here normally)
            break;
        } else {
            // Interactive: read from the user
            let query_prompt = format!("\nUser-{} > ", turn);
            let readline = query_reader.readline(&query_prompt);

            match readline {
                Ok(line) => {
                    // Add to CLI input history (allows using arrow keys to recall previous inputs)
                    query_reader.add_history_entry(line.as_str())?;
                    line
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl+C: Don't exit, show guidance instead
                    println!(
                        "\x1b[93mUse '/exit' or '/quit' to end the session, or press Ctrl+D.\x1b[0m"
                    );
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    // Ctrl+D on an empty line, exit
                    println!("Ctrl-D received. Exiting.");
                    break;
                }
                Err(err) => {
                    println!("Error reading line: {:?}", err);
                    break;
                }
            }
        };
        if input.trim().is_empty() {
            // Check for empty input after trimming
            continue;
        }
        if input == "/exit" || input == "/quit" || input == "exit" || input == "quit" {
            // Keep the exit/quit command
            break;
        }
        // Check for slash commands before processing as a regular query
        if let Some(result) = cmd::try_handle_slash_command(
            &input,
            &mut messages,
            turn,
            &llm_model,
            &session_label,
            &mut verbose_level,
            &mut pretty_level,
            &mut llm_rpm,
            &mut max_output_tokens,
            &mut max_empty_retry,
        ) {
            match result {
                cmd::SlashCmdResult::NoAdvance => {
                    // e.g. /help - just re-prompt
                    continue;
                }
                cmd::SlashCmdResult::RewoundTo(target) => {
                    // Reset turn counter to target + 1 (next turn after rewind point)
                    turn = target + 1;
                    // Reset last_sent_count to match the new message count
                    last_sent_count = messages.len();
                    continue;
                }
                cmd::SlashCmdResult::ModelChanged(new_model) => {
                    // Switch to the new model for subsequent LLM calls
                    llm_model = new_model;
                    continue;
                }
                cmd::SlashCmdResult::ConfigChanged {
                    verbose_level: vl,
                    pretty_level: pl,
                    llm_rpm: rpm,
                    max_output_tokens: mot,
                    max_empty_retry: mer,
                } => {
                    // Switch config values for subsequent LLM calls
                    verbose_level = vl;
                    pretty_level = pl;
                    llm_rpm = rpm;
                    max_output_tokens = mot;
                    max_empty_retry = mer;
                    continue;
                }
                cmd::SlashCmdResult::RestoredTo {
                    turn: target,
                    label,
                } => {
                    // Reset turn counter to target + 1 (next turn after restored point)
                    turn = target + 1;
                    // Reset last_sent_count to match the new message count
                    last_sent_count = messages.len();
                    // Switch the active session label
                    session_label = label;
                    continue;
                }
            }
        }

        // --- Parse @file references from the beginning of input ---
        let query_text;
        let attached_files: Vec<AttachedFile>;
        {
            let (clean, raw_paths, parse_mode) = attach::parse_attached_files(&input);
            if !raw_paths.is_empty() {
                match attach::validate_files(&raw_paths) {
                    Ok(()) => {
                        // Check for oversized files (> 1 MiB)
                        let oversized =
                            attach::check_oversized_files(&raw_paths, attach::OVERLOADED_BYTES);
                        if !oversized.is_empty() {
                            for (path, size) in &oversized {
                                let size_str = attach::format_file_size(*size);
                                if is_batch {
                                    eprintln!(
                                        "\x1b[93m[Warning] {} exceeds 1 MiB: {} (attaching anyway)\x1b[0m",
                                        path, size_str
                                    );
                                } else {
                                    println!(
                                        "{}[Warning] {} exceeds 1 MiB: {}{}",
                                        startup::C_YELLOW,
                                        path,
                                        size_str,
                                        startup::RESET
                                    );
                                }
                            }
                            if !is_batch {
                                print!("Attach these files anyway? (y/N) ");
                                let _ = io::stdout().flush();
                                let mut confirm = String::new();
                                if io::stdin().read_line(&mut confirm).is_err()
                                    || !confirm.trim().eq_ignore_ascii_case("y")
                                {
                                    // User cancelled or error - do not advance
                                    continue;
                                }
                            }
                        }

                        // All files exist - read them
                        match attach::read_attached_files(&raw_paths, parse_mode) {
                            Ok(files) => {
                                for f in &files {
                                    let is_converted_pdf = f.path.to_lowercase().ends_with(".pdf")
                                        && matches!(f.attach_type, FileType::Text);
                                    let label = if is_converted_pdf {
                                        format!("Markdown extracted from {}", f.path)
                                    } else {
                                        let size_str =
                                            attach::format_file_size(f.content.len() as u64);
                                        format!("{} ({})", f.path, size_str)
                                    };
                                    println!(
                                        "{}[Attached] {}{}",
                                        startup::C_DIM_GRAY,
                                        label,
                                        startup::RESET
                                    );
                                }
                                attached_files = files;
                                query_text = clean;
                            }
                            Err(e) => {
                                println!("{}[Error] {}{}", startup::C_RED, e, startup::RESET);
                                continue;
                            }
                        }
                    }
                    Err(missing) => {
                        for p in &missing {
                            println!(
                                "{}[File not found] {}{}",
                                startup::C_YELLOW,
                                p,
                                startup::RESET
                            );
                        }
                        // Do NOT advance turn / history
                        continue;
                    }
                }
            } else {
                query_text = input.to_string();
                attached_files = Vec::new();
            }
        }

        // If a user message already exists for this turn (e.g. previous LLM call failed
        // and tool responses may have been added), update the existing message instead of
        // pushing a new one to avoid inflating the history turn count.
        let user_msg_count = messages.iter().filter(|m| m.role == "user").count();
        if user_msg_count >= turn as usize {
            if let Some(m) = messages.iter_mut().rev().find(|m| m.role == "user") {
                m.content = query_text.clone();
                m.attached_files = attached_files.clone();
            }
        } else {
            messages.push(Message {
                role: "user".to_string(),
                content: query_text,
                attached_files,
                ..Default::default()
            });
            persistence::save_message(&session_label, messages.last().unwrap())?;
        }

        // Inner loop to handle tool execution and sequential LLM reasoning
        // `done` tracks whether this turn completed successfully (assistant responded normally).
        // If the loop exits via error/Ctrl+C/empty-limit, done=false and we do NOT increment turn.
        let mut empty_retry_count: usize = 0;
        let mut reasoning_turn: u32 = 0;
        let mut done: bool = false;
        'reasoning_loop: loop {
            reasoning_turn += 1;
            if reasoning_turn > config.max_reasoning_turns {
                if is_batch {
                    anyhow::bail!(
                        "Max reasoning turns ({}) exceeded without a final answer",
                        config.max_reasoning_turns
                    );
                }
                eprintln!(
                    "\x1b[93m\u{26a0}\u{fe0f} Max reasoning turns ({}) reached. Returning to prompt.\x1b[0m",
                    config.max_reasoning_turns
                );
                break 'reasoning_loop;
            }
            if llm_rpm > 0 {
                let min_interval = std::time::Duration::from_secs_f64(60.0 / llm_rpm as f64);
                if let Some(last_call) = last_llm_call {
                    let elapsed = last_call.elapsed();
                    if elapsed < min_interval {
                        tokio::time::sleep(min_interval - elapsed).await;
                    }
                }
            }
            last_llm_call = Some(std::time::Instant::now());
            let llm_start = std::time::Instant::now();

            let llm_future = call_llm(
                Some(provider),
                &config.llm_url,
                &llm_model,
                config.llm_api_key.as_ref(),
                &messages,
                verbose_level,
                last_sent_count,
                config.tool_result_format,
                max_output_tokens,
            );
            let ctrl_c_future = tokio::signal::ctrl_c();

            let (assistant_msg, usage_opt) = tokio::select! {
               msg_result = llm_future => {
                   match msg_result {
                       Ok(msg) => msg,
                       Err(e) => {
                           println!("\x1b[91m⚠️ LLM Connection Error: {}\x1b[0m", e);
                           println!("Conversation history preserved. You can try again or rephrase.");
                           break 'reasoning_loop; // Exit the inner reasoning loop, return to User prompt
                        }
                    }
                },
                _ = ctrl_c_future => {
                    // Reset terminal formatting before printing the interruption message
                   println!("\x1b[0m"); // Reset all attributes
                   println!("\n\x1b[93m--- [LLM Thinking Interrupted by Ctrl+C] ---\x1b[0m");
                    // Discard the partial message and break from the reasoning loop
                   break 'reasoning_loop;
                }
            };

            // Guard: retry if the assistant returned no content and no tool calls
            // (even if it produced reasoning/thinking without follow-through)
            let has_content = !assistant_msg.content.trim().is_empty();
            let has_tools = assistant_msg.tool_calls.is_some();
            if !has_content && !has_tools {
                empty_retry_count += 1;
                if empty_retry_count > max_empty_retry as usize {
                    println!(
                        "\x1b[91m⚠️ {} repeatedly returned empty responses ({} retries). Stopping.\x1b[0m",
                        llm_model, empty_retry_count
                    );
                    break 'reasoning_loop;
                }
                println!(
                    "\x1b[93m(Empty response from {}, retrying {}...)\x1b[0m",
                    llm_model, empty_retry_count
                );
                continue 'reasoning_loop;
            }
            empty_retry_count = 0;

            messages.push(assistant_msg.clone());
            let _ = persistence::save_message(&session_label, &assistant_msg);
            last_sent_count = messages.len();

            // Display timing (not in silent mode)
            let elapsed = llm_start.elapsed();
            if verbose_level > 0 {
                let secs = elapsed.as_secs_f64();
                if secs >= 60.0 {
                    println!(
                        "\x1b[90m[Time] {:.0}m {:.1}s\x1b[0m",
                        secs / 60.0,
                        secs % 60.0
                    );
                } else {
                    println!("\x1b[90m[Time] {:.1}s\x1b[0m", secs);
                }
            }

            // Accumulate and display statistics for each LLM call
            fn fmt_tokens(n: u32) -> String {
                format!("{:.1}K ({})", n as f64 / 1000.0, n)
            }

            if let Some(usage) = &usage_opt {
                // --- Input tokens: normal + cached ---
                let (normal, cache_turn_str) = if let Some(details) = &usage.prompt_tokens_details {
                    cache_ever_reported = true;
                    let c = details.cached_tokens;
                    total_in_cached += c as u64;
                    (usage.prompt_tokens.saturating_sub(c), fmt_tokens(c))
                } else {
                    (usage.prompt_tokens, "---".to_string())
                };
                total_in_normal += normal as u64;
                total_out += usage.completion_tokens as u64;

                // --- Output tokens: reasoning breakdown (OpenAI o1/o3/o4-mini) ---
                let reasoning = usage
                    .completion_tokens_details
                    .as_ref()
                    .map(|d| d.reasoning_tokens)
                    .unwrap_or(0);
                total_reasoning += reasoning as u64;

                // Build display line
                let cache_total_str = if cache_ever_reported {
                    fmt_tokens(total_in_cached as u32)
                } else {
                    "---".to_string()
                };

                // Turn portion
                let mut turn_part = format!(
                    "In {}, Cache {}, Out {}",
                    fmt_tokens(normal),
                    cache_turn_str,
                    fmt_tokens(usage.completion_tokens),
                );
                if reasoning > 0 {
                    turn_part.push_str(&format!(" (Reasoning {})", fmt_tokens(reasoning)));
                }

                // Total portion
                let mut total_part = format!(
                    "In {}, Cache {}, Out {}",
                    fmt_tokens((total_in_normal + total_in_cached) as u32),
                    cache_total_str,
                    fmt_tokens(total_out as u32),
                );
                if total_reasoning > 0 {
                    total_part.push_str(&format!(
                        " (Reasoning {})",
                        fmt_tokens(total_reasoning as u32)
                    ));
                }

                println!(
                    "\x1b[90m[Tokens] Turn: {} | Total: {}\x1b[0m",
                    turn_part, total_part
                );
                println!();
            } else {
                // No usage info captured - notify user
                println!("\x1b[90m[Tokens] (token info not available for this response)\x1b[0m");
                println!();
            }

            if let Some(tool_calls) = assistant_msg.tool_calls {
                for call in tool_calls {
                    // Normalize tool arguments: parse OpenAI's JSON string or use Ollama's JSON object directly.
                    let (args, args_parse_error) = match &call.function.arguments {
                        serde_json::Value::String(s) => match serde_json::from_str(s) {
                            Ok(v) => (v, None),
                            Err(e) => (serde_json::Value::Null, Some(e.to_string())),
                        },
                        value => (value.clone(), None),
                    };

                    let pretty = pretty_level > 0;
                    let mut user_denied = false;

                    // 0. Broken tool call diagnostic
                    pretty::pretty_print_broken_tool_call(
                        &call.function.name,
                        &call.id,
                        &call.tool_type,
                        &call.function.arguments,
                        &args,
                        args_parse_error.as_deref(),
                        call.thought_signature.as_deref(),
                        &assistant_msg.content,
                    );

                    // 1. Show tool call request (LLM to Application)
                    println!("--- [TOOL EXECUTION REQUESTED] ---");
                    println!("Tool: {}{}{}", C_YELLOW, call.function.name, RESET);
                    if pretty {
                        println!("Args (truncated): {}", pretty::truncate(&args),);
                    } else {
                        println!("Args: {}{}{}", C_YELLOW, &args, RESET);
                    }

                    // 2. Pretty print command
                    if pretty {
                        pretty::pretty_print_command(&call.function.name, &args);
                    }

                    // 3. Confirm and execute
                    let tool_result: serde_json::Value;

                    let tool_call_decision = tools::confirm_execute_tool(
                        &call.function.name,
                        &args,
                        config.unsafe_reflex,
                        is_batch,
                    )
                    .await;
                    let tool_call_decision_reason =
                        tool_call_decision.reason.as_deref().unwrap_or("");

                    if !tool_call_decision.proceed {
                        match &tool_call_decision {
                            ToolRunDecision {
                                kind: ToolRunDecisionKind::UserCancel,
                                ..
                            } => {
                                println!(
                                    "{}*{} Tool execution was canceled by user.",
                                    C_YELLOW, RESET
                                );
                                tool_result = json!({"status": "denied", "message": "Tool execution skipped by user."});
                                user_denied = true;
                            }
                            ToolRunDecision {
                                kind: ToolRunDecisionKind::SystemError,
                                ..
                            } => {
                                println!(
                                    "{}*{} Tool execution was canceled due to a system error: {}",
                                    C_RED, RESET, tool_call_decision_reason
                                );
                                tool_result = json!({"status": "error", "message": format!("Tool execution skipped due to a system error: {}", tool_call_decision_reason)});
                            }
                            _ => {
                                println!(
                                    "{}**{} Tool execution was canceled due to unknown app bug",
                                    C_RED, RESET
                                );
                                tool_result =
                                    json!({"status": "error", "message": "unknown app bug"});
                            }
                        }
                    } else {
                        match &tool_call_decision {
                            ToolRunDecision {
                                kind: ToolRunDecisionKind::UserConfirm,
                                ..
                            } => {
                                println!(
                                    "     {} User-confirmed{}: {}{}",
                                    C_GREEN, RESET, tool_call_decision_reason, RESET
                                );
                            }
                            ToolRunDecision {
                                kind: ToolRunDecisionKind::AutoConfirm,
                                ..
                            } => {
                                println!(
                                    "     {} Auto-confirmed{}: {}{}",
                                    C_MAGENTA, RESET, tool_call_decision_reason, RESET
                                );
                            }
                            _ => unreachable!(
                                "confirmed is true but decision is not a confirm variant"
                            ),
                        }

                        // execute tool and get tool_result json for following steps
                        match tools::execute_tool(&call.function.name, &args).await {
                            Ok(res) => {
                                println!("{}*{} Tool executed successfully.", C_GREEN, RESET);
                                tool_result = res;
                            }
                            Err(e) => {
                                println!("{}*{} Tool execution failed: {}", C_RED, RESET, e);
                                tool_result = json!({"error": e.to_string()});
                            }
                        }
                    }

                    // 4. Pretty print result (only on Ok)
                    if pretty && !user_denied {
                        pretty::pretty_print_result(&call.function.name, &tool_result, Some(&args));
                    }

                    // 5. Show tool call response (Application to LLM)
                    let tool_result_str = serde_json::to_string(&tool_result).unwrap();
                    if pretty {
                        println!(
                            "{}Tool Call Response: {}{}\n",
                            C_GRAY,
                            pretty::truncate(&tool_result),
                            RESET
                        );
                    } else {
                        println!("Tool Call Response: {}\n", tool_result_str);
                    }
                    messages.push(Message {
                        role: "tool".to_string(),
                        content: tool_result_str,
                        tool_call_id: Some(call.id.clone()),
                        tool_name: Some(call.function.name.clone()),
                        tool_call_decision: Some(tool_call_decision),
                        ..Default::default()
                    });
                    persistence::save_message(&session_label, messages.last().unwrap())?;
                }
                // Re-query LLM with tool execution results
                continue 'reasoning_loop;
            }
            done = true;
            break 'reasoning_loop;
        }
        if done {
            if is_batch {
                // Batch: output the final assistant response and exit
                let final_answer = &messages.last().unwrap().content;
                if let Some(output_path) = &config.output_file {
                    std::fs::write(output_path, final_answer).map_err(|e| {
                        anyhow!("Failed to write output to '{}': {}", output_path, e)
                    })?;
                } else {
                    print!("{}", final_answer);
                }
                return Ok(());
            }
            turn += 1;
        }
    }
    Ok(())
}

async fn call_llm(
    provider: Option<LlmProvider>,
    url: &str,
    model: &str,
    api_key: Option<&String>,
    messages: &[Message],
    verbose_level: startup::Verbosity,
    last_msg_count: usize,
    tool_result_format: ToolResultFormat,
    max_output_tokens: u32,
) -> Result<(Message, Option<Usage>)> {
    let client = reqwest::Client::new();
    let tools = tools::get_tool_definitions();
    let messages_vec = messages.to_vec();

    let req = ChatRequest {
        provider: provider.unwrap(),
        model: model.to_string(),
        max_output_tokens: max_output_tokens as usize,
        messages: messages_vec,
        stream: true,
        tools,
        tool_result_format,
    };
    let req_value = req.to_provider_json()?;
    let req_json = serde_json::to_string(&req_value)?;

    // Debug output based on verbose_level
    match verbose_level {
        3 | 4 => println!(
            "\x1b[90m--- [API REQUEST: {}] ---\n{}\x1b[0m",
            url, req_json
        ),
        2 => {
            if last_msg_count > 0 && last_msg_count < req.messages.len() {
                let mut truncated_req = req.clone();
                truncated_req.messages = truncated_req.messages[last_msg_count..].to_vec();
                let trunc_json = serde_json::to_string(&truncated_req)?;
                println!(
                    "\x1b[90m--- [API REQUEST: {} (Verbose 2: Incremental)] ---\x1b[0m",
                    url
                );
                println!(
                    "\x1b[92;2;3m... [{} messages omitted] ...\x1b[0m\n\x1b[90m{}\x1b[0m",
                    last_msg_count, trunc_json
                );
            } else {
                println!(
                    "\x1b[90m--- [API REQUEST: {}] ---\n{}\x1b[0m",
                    url, req_json
                );
            }
        }
        0 => {} // Verbose 0: Silent
        _ => {
            // Verbose 1+: Metadata
            println!(
                "\x1b[90m--- [API REQUEST: {}] (Content-Length: {}) ---\x1b[0m",
                url,
                req_json.len()
            );
        }
    };

    println!(
        "... Waiting for response from {} (Ctrl+C to interrupt) ...",
        model
    );

    let mut request_builder = client
        .post(url)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("{}/{}", startup::APP_BIN_NAME, env!("CARGO_PKG_VERSION")),
        )
        .json(&req_value);

    if let Some(api_key) = api_key.as_deref() {
        let masked_key = "****".to_string();

        request_builder = match provider {
            Some(LlmProvider::Anthropic) => {
                if verbose_level >= 1 {
                    println!(
                        "{}[AUTH: x-api-key] masked: {}{}",
                        startup::C_DIM_GRAY,
                        masked_key,
                        startup::RESET
                    );
                }
                request_builder
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-11-01")
            }
            _ => {
                if verbose_level >= 1 {
                    println!(
                        "{}[AUTH: Authorization/Bearer] masked: Bearer {}{}",
                        startup::C_DIM_GRAY,
                        masked_key,
                        startup::RESET
                    );
                }
                request_builder.header("Authorization", format!("Bearer {}", api_key))
            }
        };
    }

    let res = request_builder.send().await?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res
            .text()
            .await
            .unwrap_or_else(|_| "Could not read body".to_string());
        let payload_display = if req_json.len() > 62 && verbose_level < 2 {
            format!(
                "{}...({}bytes)...{}",
                &req_json[..30],
                req_json.len(),
                &req_json[req_json.len().saturating_sub(30)..]
            )
        } else {
            req_json.clone()
        };
        return Err(anyhow!(
            "API Error ({})\nResponse: {}\nRequest Payload: {}",
            status,
            body,
            payload_display
        ));
    }
    let mut full_message = Message {
        role: "assistant".to_string(),
        model: Some(model.to_string()),
        ..Default::default()
    };

    let stream = res.bytes_stream();
    tokio::pin!(stream);
    let mut is_thinking = false;
    let mut usage_captured: Option<Usage> = None;
    let mut has_started_content = false;
    #[allow(unused_mut, unused_variables)]
    let mut anth_tool_index = 0;
    let mut used_nonstandard_format = false;

    // Buffer raw bytes because chunk boundaries may split multi-byte
    // UTF-8 characters. We only decode to str once a complete line
    // (terminated by \n, which is always a safe UTF-8 boundary) is available.
    let mut line_buf: Vec<u8> = Vec::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        line_buf.extend_from_slice(&chunk);

        // Drain complete lines (ending with \n) from the buffer.
        // Partial lines are kept in line_buf for the next chunk.
        while let Some(nl) = line_buf.iter().position(|&b| b == b'\n') {
            let line_bytes = &line_buf[..nl];
            let line = match std::str::from_utf8(line_bytes) {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    if verbose_level >= 2 {
                        println!("\x1b[93m[SSE UTF-8 Error] Skipping line: {}\x1b[0m", e);
                    }
                    line_buf.drain(..=nl);
                    continue;
                }
            };
            line_buf.drain(..=nl);
            if line.is_empty() {
                continue;
            }

            // Skip Anthropic's SSE event-name
            if line.starts_with("event:") {
                continue;
            }

            let mut payload = line.as_str();
            // Handle OpenAI-compatible SSE prefix (with or without space after colon)
            if let Some(rest) = payload.strip_prefix("data:") {
                payload = rest.trim_start();
            }
            if payload == "[DONE]" {
                break;
            }

            if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(payload) {
                // Convert Anthropic payload to OpenAI-compatible format
                if provider == Some(LlmProvider::Anthropic) {
                    json = convert_anth_to_openai_format(json, &mut anth_tool_index);
                }
                // Normalize non-OpenAI tool call format (name/arguments at top level -> function wrapper)
                used_nonstandard_format |= compat_resilience::normalize_tool_call_format(&mut json);

                // 0. Verbose 4: Display all raw SSE lines
                if verbose_level >= 4 {
                    println!("\x1b[90m[SSE] {}\x1b[0m", payload);
                }

                // 0. Process Usage (Handle OpenAI format or Ollama native)
                compat_provider::accumulate_usage(&json, &mut usage_captured);

                // Handle both Ollama native (/api/chat) and OpenAI-compatible (/v1/chat/completions)
                let msg_base = extract_msg_base(&json);

                // 0.5 Verbose 3: Display raw SSE line for tool_call deltas
                if verbose_level == 3 {
                    let has_tool_calls = msg_base
                        .get("tool_calls")
                        .and_then(|v| v.as_array())
                        .is_some_and(|a| !a.is_empty());
                    if has_tool_calls {
                        println!("\x1b[90m[TOOL RAW] {}\x1b[0m", payload);
                    }
                }

                // 1. Process Reasoning (Thinking) - Supports both 'reasoning_content' and 'reasoning'
                let reasoning_val = msg_base
                    .get("reasoning_content")
                    .or_else(|| msg_base.get("reasoning"));

                if let Some(reasoning) = reasoning_val.and_then(|v| v.as_str()) {
                    if !is_thinking {
                        print!("\n{}[Thinking]\n", C_DIM_GREEN);
                        is_thinking = true;
                    }
                    print!("{}", reasoning);
                    io::stdout().flush()?;
                    full_message
                        .reasoning_content
                        .get_or_insert_with(String::new)
                        .push_str(reasoning);
                }

                // 2. Process Content
                if let Some(content) = msg_base
                    .get("content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    if is_thinking {
                        println!("\x1b[0m\n"); // End italics/gray and add space
                        is_thinking = false;
                    }
                    if !has_started_content {
                        print!("Assistant > ");
                        io::stdout().flush()?;
                        has_started_content = true;
                    }
                    print!("{}", content);
                    io::stdout().flush()?;

                    full_message.content.push_str(content);
                }

                // 3. Process Tool Calls
                if let Some(calls) = msg_base.get("tool_calls").and_then(|v| v.as_array()) {
                    let tool_calls = full_message.tool_calls.get_or_insert_with(Vec::new);
                    merge_tool_call_delta(tool_calls, calls, 0, true);
                }
            } else if verbose_level >= 2 {
                match serde_json::from_str::<serde_json::Value>(payload) {
                    Err(e) => println!(
                        "\x1b[93m[SSE Parse Warning] Skipping unparseable line: {} ({})\x1b[0m",
                        payload.chars().take(120).collect::<String>(),
                        e
                    ),
                    _ => {}
                }
            }
        }
    }

    // Drain any remaining data in the buffer (the final line may lack a trailing \n).
    // This is a defensive edge case: standard SSE always terminates lines with \n,
    // but non-conforming servers or truncated streams could leave trailing data.
    let remainder = std::str::from_utf8(&line_buf).unwrap_or("").trim();
    if !remainder.is_empty() {
        let mut payload = remainder;
        if !payload.starts_with("event:") {
            if let Some(rest) = payload.strip_prefix("data:") {
                payload = rest.trim_start();
            }
            if payload != "[DONE]" {
                if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(payload) {
                    if provider == Some(LlmProvider::Anthropic) {
                        json = convert_anth_to_openai_format(json, &mut anth_tool_index);
                    }
                    used_nonstandard_format |=
                        compat_resilience::normalize_tool_call_format(&mut json);
                    compat_provider::accumulate_usage(&json, &mut usage_captured);

                    let msg_base = extract_msg_base(&json);

                    if let Some(reasoning) = msg_base
                        .get("reasoning_content")
                        .or_else(|| msg_base.get("reasoning"))
                        .and_then(|v| v.as_str())
                    {
                        full_message
                            .reasoning_content
                            .get_or_insert_with(String::new)
                            .push_str(reasoning);
                    }
                    if let Some(content) = msg_base.get("content").and_then(|v| v.as_str()) {
                        full_message.content.push_str(content);
                    }
                    if let Some(calls) = msg_base.get("tool_calls").and_then(|v| v.as_array()) {
                        let tool_calls = full_message.tool_calls.get_or_insert_with(Vec::new);
                        let default_index = tool_calls.len();
                        merge_tool_call_delta(tool_calls, calls, default_index, false);
                    }
                }
            }
        }
    }

    // Post-process tool_calls assembled from SSE streaming:
    // - Fill in missing id/function.name (infer from args for index 0)
    // - Filter out unrecoverable calls (empty name or unparseable arguments)
    if let Some(tool_calls) = &mut full_message.tool_calls {
        post_process_tool_calls(tool_calls, &tools::get_tool_definitions());
        // If all tool calls were filtered out, set back to None
        // to avoid serializing an empty array which APIs reject.
        if tool_calls.is_empty() {
            full_message.tool_calls = None;
        }
    }

    if is_thinking {
        println!("\x1b[0m\n");
    }

    if has_started_content {
        println!();
    } else {
        let has_tool = full_message
            .tool_calls
            .as_ref()
            .is_some_and(|v| !v.is_empty());
        let has_reasoning = full_message
            .reasoning_content
            .as_ref()
            .is_some_and(|r| !r.trim().is_empty());

        match (has_tool, has_reasoning) {
            (true, true) => println!("Assistant > [Tool Call] (after reasoning)"),
            (true, false) => println!("Assistant > [Tool Call]"),
            (false, true) => println!("Assistant > (Thought only)"),
            (false, false) => {
                if full_message.reasoning_content.is_none() {
                    println!("Assistant > (No response - reasoning not supported)");
                } else {
                    println!("Assistant > (No response)");
                }
            }
        }
        if used_nonstandard_format {
            println!(
                "\x1b[93m(Warning: tool call fields (name/arguments) at top level, not inside 'function'. \
                 Assuming non-OpenAI format)\x1b[0m"
            );
        }
    }

    println!();

    Ok((full_message, usage_captured))
}
