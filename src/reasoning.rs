//! ReAct reasoning loop and LLM communication.
//!
//! Contains `run_reasoning_loop` (the core ReAct cycle: LLM -> tools -> feedback)
//! and `call_llm` (low-level HTTP streaming client for LLM providers).

use std::io;
use std::io::Write;

use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use serde_json::json;

use crate::attach::AttachedFile;
use crate::compat_provider::{self, LlmProvider, convert_anth_to_openai_format};
use crate::compat_resilience;
use crate::compat_resilience::{extract_msg_base, merge_tool_call_delta, post_process_tool_calls};
use crate::model::*;
use crate::persistence;
use crate::pretty;
use crate::startup;
use crate::startup::{C_DIM_GRAY, C_DIM_GREEN, C_GRAY, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, RESET};
use crate::tools::{self, ToolRunDecision, ToolRunDecisionKind};

/// Reasoning loop for one user turn: LLM -> tools -> feedback -> repeat.
///
/// `tools::confirm_execute_tool`, `persistence::save_message`, and
/// `tokio::signal::ctrl_c` are called directly.
///
/// Returns `Ok(true)` when the assistant emitted a final answer; `Ok(false)`
/// when interrupted (Ctrl+C / connection error / empty-retry limit); `Err(_)`
/// on persistence failure or batch-mode max-reasoning-turns exceeded.
pub(crate) async fn run_reasoning_loop(
    config: &startup::Config,
    provider: LlmProvider,
    session: &mut Session,
    settings: &mut Settings,
    metrics: &mut Metrics,
    user_input: String,
    attached_files: Vec<AttachedFile>,
) -> Result<bool> {
    // Batch mode is derived from `-q/--query`. Re-deriving here keeps the
    // signature smaller and avoids the caller having to pass it through.
    let is_batch = config.query.is_some();
    // Update the existing user message if a previous broken turn pushed one,
    // else push a new one (avoids inflating the history turn count).
    let user_msg_count = session.messages.iter().filter(|m| m.role == "user").count();
    if user_msg_count >= session.turn as usize {
        if let Some(m) = session.messages.iter_mut().rev().find(|m| m.role == "user") {
            m.content = user_input.clone();
            m.attached_files = attached_files.clone();
        }
    } else {
        session.messages.push(Message {
            role: "user".to_string(),
            content: user_input,
            attached_files,
            ..Default::default()
        });
        persistence::save_message(&session.label, session.messages.last().unwrap())?;
    }

    // Inner loop to handle tool execution and sequential LLM reasoning.
    // `done` tracks whether this turn completed successfully (assistant
    // responded normally). If the loop exits via error/Ctrl+C/empty-limit, done
    // is false and the caller does NOT increment the turn counter.
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
        if settings.llm_rpm > 0 {
            let min_interval = std::time::Duration::from_secs_f64(60.0 / settings.llm_rpm as f64);
            if let Some(last_call) = settings.last_llm_call {
                let elapsed = last_call.elapsed();
                if elapsed < min_interval {
                    tokio::time::sleep(min_interval - elapsed).await;
                }
            }
        }
        settings.last_llm_call = Some(std::time::Instant::now());
        let llm_start = std::time::Instant::now();

        let llm_future = call_llm(config, settings, provider, &session.messages);
        let ctrl_c_future = tokio::signal::ctrl_c();

        let (assistant_msg, usage_opt) = tokio::select! {
           msg_result = llm_future => {
               match msg_result {
                   Ok(msg) => msg,
                   Err(e) => {
                       println!("\x1b[91m⚠️ LLM Connection Error: {}\x1b[0m", e);
                       println!("Conversation history preserved. You can try again or rephrase.");
                       #[cfg(feature = "gui")]
                       {
                           LLM_STREAM_BUF.lock().unwrap().1.push_str(&format!("[LLM Error] {}", e));
                       }
                       break 'reasoning_loop;
                    }
                }
            },
            _ = ctrl_c_future => {
               println!("\x1b[0m");
               println!("\n\x1b[93m--- [LLM Thinking Interrupted by Ctrl+C] ---\x1b[0m");
               break 'reasoning_loop;
            }
        };

        // Guard: retry if the assistant returned no content and no tool calls
        let has_content = !assistant_msg.content.trim().is_empty();
        let has_tools = assistant_msg.tool_calls.is_some();
        if !has_content && !has_tools {
            empty_retry_count += 1;
            if empty_retry_count > settings.max_empty_retry as usize {
                println!(
                    "\x1b[91m⚠️ {} repeatedly returned empty responses ({} retries). Stopping.\x1b[0m",
                    settings.llm_model, empty_retry_count
                );
                break 'reasoning_loop;
            }
            println!(
                "\x1b[93m(Empty response from {}, retrying {}...)\x1b[0m",
                settings.llm_model, empty_retry_count
            );
            continue 'reasoning_loop;
        }
        empty_retry_count = 0;

        session.messages.push(assistant_msg.clone());
        // Was `let _ =` (swallowed). Propagate persistence errors now.
        persistence::save_message(&session.label, session.messages.last().unwrap())?;
        // Cache snapshot after assistant push but BEFORE tool pushes: drives
        // verbose-2 incremental display on the next `call_llm` (see Settings).
        settings.last_sent_count = session.messages.len();

        // Display timing (not in silent mode)
        let elapsed = llm_start.elapsed();
        if settings.verbose_level > 0 {
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
                metrics.cache_ever_reported = true;
                let c = details.cached_tokens;
                metrics.in_cached += c as u64;
                (usage.prompt_tokens.saturating_sub(c), fmt_tokens(c))
            } else {
                (usage.prompt_tokens, "---".to_string())
            };
            metrics.in_normal += normal as u64;
            metrics.out += usage.completion_tokens as u64;

            // --- Output tokens: reasoning breakdown (OpenAI o1/o3/o4-mini) ---
            let reasoning = usage
                .completion_tokens_details
                .as_ref()
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0);
            metrics.reasoning += reasoning as u64;

            // Build display line
            let cache_total_str = if metrics.cache_ever_reported {
                fmt_tokens(metrics.in_cached as u32)
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
                fmt_tokens((metrics.in_normal + metrics.in_cached) as u32),
                cache_total_str,
                fmt_tokens(metrics.out as u32),
            );
            if metrics.reasoning > 0 {
                total_part.push_str(&format!(
                    " (Reasoning {})",
                    fmt_tokens(metrics.reasoning as u32)
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

                let pretty = settings.pretty_level > 0;
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
                    println!("Args: {}{}{}", C_YELLOW, args, RESET);
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
                let tool_call_decision_reason = tool_call_decision.reason.as_deref().unwrap_or("");

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
                            tool_result = json!({"status": "error", "message": "unknown app bug"});
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
                        _ => {
                            unreachable!("confirmed is true but decision is not a confirm variant")
                        }
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
                session.messages.push(Message {
                    role: "tool".to_string(),
                    content: tool_result_str,
                    tool_call_id: Some(call.id.clone()),
                    tool_name: Some(call.function.name.clone()),
                    tool_call_decision: Some(tool_call_decision),
                    ..Default::default()
                });
                persistence::save_message(&session.label, session.messages.last().unwrap())?;
            }
            // Re-query LLM with tool execution results
            continue 'reasoning_loop;
        }
        done = true;
        break 'reasoning_loop;
    }
    Ok(done)
}

async fn call_llm(
    config: &startup::Config,
    settings: &Settings,
    provider: LlmProvider,
    messages: &[Message],
) -> Result<(Message, Option<Usage>)> {
    let client = reqwest::Client::new();
    let tools = tools::get_tool_definitions();
    let messages_vec = messages.to_vec();

    let req = ChatRequest {
        provider,
        model: settings.llm_model.clone(),
        max_output_tokens: settings.max_output_tokens as usize,
        messages: messages_vec,
        stream: true,
        tools,
        tool_result_format: config.tool_result_format,
    };
    let req_value = req.to_provider_json()?;
    let req_json = serde_json::to_string(&req_value)?;

    // Debug output based on verbose_level
    match settings.verbose_level {
        3 | 4 => println!(
            "\x1b[90m--- [API REQUEST: {}] ---\n{}\x1b[0m",
            config.llm_url, req_json
        ),
        2 => {
            if settings.last_sent_count > 0 && settings.last_sent_count < req.messages.len() {
                let mut truncated_req = req.clone();
                truncated_req.messages =
                    truncated_req.messages[settings.last_sent_count..].to_vec();
                let trunc_json = serde_json::to_string(&truncated_req)?;
                println!(
                    "\x1b[90m--- [API REQUEST: {} (Verbose 2: Incremental)] ---\x1b[0m",
                    config.llm_url
                );
                println!(
                    "\x1b[92;2;3m... [{} messages omitted] ...\x1b[0m\n\x1b[90m{}\x1b[0m",
                    settings.last_sent_count, trunc_json
                );
            } else {
                println!(
                    "\x1b[90m--- [API REQUEST: {}] ---\n{}\x1b[0m",
                    config.llm_url, req_json
                );
            }
        }
        0 => {} // Verbose 0: Silent
        _ => {
            // Verbose 1+: Metadata
            println!(
                "\x1b[90m--- [API REQUEST: {}] (Content-Length: {}) ---\x1b[0m",
                config.llm_url,
                req_json.len()
            );
        }
    };

    println!(
        "... Waiting for response from {} (Ctrl+C to interrupt) ...",
        settings.llm_model
    );

    let mut request_builder = client
        .post(&config.llm_url)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("{}/{}", startup::APP_BIN_NAME, env!("CARGO_PKG_VERSION")),
        )
        .json(&req_value);

    if let Some(api_key) = config.llm_api_key.as_deref() {
        let masked_key = "****".to_string();

        request_builder = if provider == LlmProvider::Anthropic {
            if settings.verbose_level >= 1 {
                println!(
                    "{}[AUTH: x-api-key] masked: {}{}",
                    C_DIM_GRAY, masked_key, RESET
                );
            }
            request_builder
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-11-01")
        } else {
            if settings.verbose_level >= 1 {
                println!(
                    "{}[AUTH: Authorization/Bearer] masked: Bearer {}{}",
                    C_DIM_GRAY, masked_key, RESET
                );
            }
            request_builder.header("Authorization", format!("Bearer {}", api_key))
        };
    }

    let res = request_builder.send().await?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res
            .text()
            .await
            .unwrap_or_else(|_| "Could not read body".to_string());
        let payload_display = if req_json.len() > 62 && settings.verbose_level < 2 {
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
        model: Some(settings.llm_model.clone()),
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
                    if settings.verbose_level >= 2 {
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

            // SSE protocol lines (RFC 7320): event-type ("event:" prefix) and
            // comments (":" prefix). Both must be silently ignored by the client.
            if line.starts_with("event:") || line.starts_with(":") {
                if settings.verbose_level >= 4 {
                    println!("\x1b[90m[SSE] {}\x1b[0m", line);
                }
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
                if provider == LlmProvider::Anthropic {
                    json = convert_anth_to_openai_format(json, &mut anth_tool_index);
                }
                // Normalize non-OpenAI tool call format (name/arguments at top level -> function wrapper)
                used_nonstandard_format |= compat_resilience::normalize_tool_call_format(&mut json);

                // 0. Verbose 4: Display all raw SSE lines
                if settings.verbose_level >= 4 {
                    println!("\x1b[90m[SSE] {}\x1b[0m", payload);
                }

                // 0. Process Usage (Handle OpenAI format or Ollama native)
                compat_provider::accumulate_usage(&json, &mut usage_captured);

                // Handle both Ollama native (/api/chat) and OpenAI-compatible (/v1/chat/completions)
                let msg_base = extract_msg_base(&json);

                // 0.5 Verbose 3: Display raw SSE line for tool_call deltas
                if settings.verbose_level == 3 {
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
                    #[cfg(feature = "gui")]
                    LLM_STREAM_BUF.lock().unwrap().0.push_str(reasoning);
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
                    #[cfg(feature = "gui")]
                    LLM_STREAM_BUF.lock().unwrap().1.push_str(content);

                    full_message.content.push_str(content);
                }

                // 3. Process Tool Calls
                if let Some(calls) = msg_base.get("tool_calls").and_then(|v| v.as_array()) {
                    let tool_calls = full_message.tool_calls.get_or_insert_with(Vec::new);
                    merge_tool_call_delta(tool_calls, calls, 0, true);
                }
            } else if settings.verbose_level >= 2
                && let Err(e) = serde_json::from_str::<serde_json::Value>(payload)
            {
                println!(
                    "\x1b[93m[SSE Parse Warning] Skipping unparseable line: {} ({})\x1b[0m",
                    payload.chars().take(120).collect::<String>(),
                    e
                );
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
            if payload != "[DONE]"
                && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(payload)
            {
                if provider == LlmProvider::Anthropic {
                    json = convert_anth_to_openai_format(json, &mut anth_tool_index);
                }
                used_nonstandard_format |= compat_resilience::normalize_tool_call_format(&mut json);
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
