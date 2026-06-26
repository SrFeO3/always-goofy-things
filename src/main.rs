//! Main entry point for the CLI.
//!
//! WARNING: This application executes autonomous actions on your behalf, including
//! file system modifications, shell command execution, and internet access.
//! Review tool calls carefully before granting execution as these operations
//! may impact your local environment or interact with external servers.
//!
//! Logic Flow:
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

mod cmd;
mod pretty;
mod startup;
mod tools;

use startup::{C_CYAN, C_GRAY, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, RESET};

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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub tool_type: String,
    function: FunctionCall,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct FunctionCall {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ChatRequest {
    model: String,
    tools: Vec<serde_json::Value>,
    stream: bool,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = startup::Config::parse();
    let _current_dir = startup::print_startup_info(&config)?;

    let mut last_sent_count = 0;
    let mut total_in_normal = 0u64;
    let mut total_in_cached = 0u64;
    let mut total_out = 0u64;

    let mut query_reader = DefaultEditor::new()?;

    println!(
        "\n{}Describe your task and press Enter to start (or /help, /exit, ^D).{}",
        C_CYAN, RESET
    );

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
            tools::COMMAND_ALLOW_LIST.join(", ")
        ),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }];

    // Main conversation loop
    let mut turn: i32 = 1;
    loop {
        let query_prompt = format!("\n(user-{}) ", turn);
        let readline = query_reader.readline(&query_prompt);

        let input = match readline {
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
        if let Some(result) = cmd::try_handle_slash_command(&input, &mut messages, turn) {
            match result {
                cmd::SlashCmdResult::NoAdvance => {
                    // e.g. /help - just re-prompt
                    continue;
                }
                cmd::SlashCmdResult::RewoundTo(target) => {
                    // Reset turn counter to target + 1 (next turn after rewound point)
                    turn = target + 1;
                    // Reset last_sent_count to match the new message count
                    last_sent_count = messages.len();
                    continue;
                }
            }
        }

        // If the last message was from the user, it means the previous LLM call failed.
        // We update the existing message instead of pushing a new one to avoid consecutive user roles.
        if messages.last().map(|m| m.role.as_str()) == Some("user") {
            if let Some(m) = messages.last_mut() {
                m.content = input.to_string();
            }
        } else {
            messages.push(Message {
                role: "user".to_string(),
                content: input.to_string(),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Inner loop to handle tool execution and sequential LLM reasoning
        let mut empty_retry_count: usize = 0;
        'reasoning_loop: loop {
            let llm_future = call_llm(
                &config.llm_url,
                &config.llm_model,
                config.llm_api_key.as_ref(),
                &messages,
                config.verbose_level,
                last_sent_count,
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

            // Guard: retry if the assistant returned an empty response (no content, no tools, no reasoning)
            let has_content = !assistant_msg.content.trim().is_empty();
            let has_tools = assistant_msg.tool_calls.is_some();
            let has_reasoning = assistant_msg
                .reasoning_content
                .as_ref()
                .map(|r| !r.trim().is_empty())
                .unwrap_or(false);
            if !has_content && !has_tools && !has_reasoning {
                empty_retry_count += 1;
                if empty_retry_count > startup::MAX_EMPTY_RETRY {
                    println!(
                        "\x1b[91m⚠️ {} repeatedly returned empty responses ({} retries). Stopping.\x1b[0m",
                        config.llm_model, empty_retry_count
                    );
                    break 'reasoning_loop;
                }
                println!(
                    "\x1b[93m(Empty response from {}, retrying {}...)\x1b[0m",
                    config.llm_model, empty_retry_count
                );
                continue 'reasoning_loop;
            }
            empty_retry_count = 0;

            messages.push(assistant_msg.clone());
            last_sent_count = messages.len();

            // Accumulate and display statistics for each LLM call
            if let Some(usage) = usage_opt {
                let cached = usage
                    .prompt_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                let normal = usage.prompt_tokens.saturating_sub(cached);
                total_in_normal += normal as u64;
                total_in_cached += cached as u64;
                total_out += usage.completion_tokens as u64;

                println!(
                    "\x1b[90m[Tokens] Turn: In {}, Cache {}, Out {} | Total: In {}, Cache {}, Out {}\x1b[0m",
                    normal,
                    cached,
                    usage.completion_tokens,
                    total_in_normal + total_in_cached,
                    total_in_cached,
                    total_out
                );
                println!();
            }

            if let Some(tool_calls) = assistant_msg.tool_calls {
                for call in tool_calls {
                    // Parse stringified JSON from OpenAI, or fallback to Null if it's already an object/null.
                    // This handles both raw JSON objects (Ollama) and stringified JSON (OpenAI) safely.
                    let args = call
                        .function
                        .arguments
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(serde_json::Value::Null);

                    let pretty = config.pretty_level > 0;
                    let mut user_denied = false;

                    // 1. Show tool call request (LLM to Application)
                    println!("--- [TOOL EXECUTION REQUESTED] ---");
                    println!("Tool: {}{}{}", C_YELLOW, call.function.name, RESET);
                    if pretty {
                        println!(
                            "Args (truncated): {}{}{}",
                            C_YELLOW,
                            pretty::truncate(&args),
                            RESET
                        );
                    } else {
                        println!("Args: {}{}{}", C_YELLOW, &args, RESET);
                    }

                    // 2. Pretty print command
                    if pretty {
                        pretty::pretty_print_command(&call.function.name, &args);
                    }

                    // 3. Confirm and execute
                    let tool_result =
                        match tools::confirm_and_execute_tool(&call.function.name, &args).await {
                            Ok(res) => {
                                if res.get("status").and_then(|s| s.as_str()) == Some("denied") {
                                    user_denied = true;
                                    println!(
                                        "{}*{} Tool execution was denied by user.",
                                        C_MAGENTA, RESET
                                    );
                                } else {
                                    println!("{}*{} Tool executed successfully.", C_GREEN, RESET);
                                }
                                res
                            }
                            Err(e) => {
                                println!("{}*{} Tool execution failed.: {}", C_RED, RESET, e);
                                serde_json::json!({"error": e.to_string()})
                            }
                        };

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
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: Some(call.id),
                    });
                }
                // Re-query LLM with tool execution results
                continue 'reasoning_loop;
            }
            break 'reasoning_loop;
        }
        turn += 1;
    }
    Ok(())
}

async fn call_llm(
    url: &str,
    model: &str,
    api_key: Option<&String>,
    messages: &[Message],
    verbose_level: startup::Verbosity,
    last_msg_count: usize,
) -> Result<(Message, Option<Usage>)> {
    let client = reqwest::Client::new();
    let tools = tools::get_tool_definitions();
    let messages_vec = messages.to_vec();

    // Request usage data if using OpenAI-compatible endpoint or API key is set
    let stream_options = if api_key.is_some() || url.contains("/v1/") {
        Some(StreamOptions {
            include_usage: true,
        })
    } else {
        None
    };

    let req = ChatRequest {
        model: model.to_string(),
        messages: messages_vec,
        stream: true,
        tools,
        stream_options,
    };

    let req_json = serde_json::to_string(&req)?;

    // Debug output based on verbose_level
    match verbose_level {
        3 => println!(
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
    }

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
        .json(&req);

    if let Some(api_key) = api_key {
        request_builder = request_builder.header("Authorization", format!("Bearer {}", api_key));
    }

    let res = request_builder.send().await?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res
            .text()
            .await
            .unwrap_or_else(|_| "Could not read body".to_string());
        return Err(anyhow!(
            "API Error ({})\nResponse: {}\nRequest Payload: {}",
            status,
            body,
            req_json
        ));
    }

    let mut full_message = Message {
        role: "assistant".to_string(),
        content: String::new(),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    };

    let stream = res.bytes_stream();
    tokio::pin!(stream);
    let mut is_thinking = false;
    let mut usage_captured: Option<Usage> = None;
    let mut has_started_content = false;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        let text = std::str::from_utf8(&chunk)?;

        // Ollama/OpenAI stream can send multiple JSON objects in one chunk separated by newlines
        for line in text.lines() {
            let mut line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Handle OpenAI-compatible SSE prefix
            if line.starts_with("data: ") {
                line = &line[6..];
            }
            if line == "[DONE]" {
                break;
            }

            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                // 0. Process Usage (Handle OpenAI format or Ollama native)
                if let Some(usage_val) = json.get("usage") {
                    if let Ok(u) = serde_json::from_value::<Usage>(usage_val.clone()) {
                        usage_captured = Some(u);
                    }
                } else if json.get("done") == Some(&serde_json::Value::Bool(true)) {
                    // Map Ollama native stats to Usage struct
                    let p = json
                        .get("prompt_eval_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let c = json.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    if p > 0 || c > 0 {
                        usage_captured = Some(Usage {
                            prompt_tokens: p,
                            completion_tokens: c,
                            prompt_tokens_details: None,
                        });
                    }
                }

                // Handle both Ollama native (/api/chat) and OpenAI-compatible (/v1/chat/completions)
                let msg_base = if let Some(message) = json.get("message") {
                    message // Ollama native
                } else if let Some(choices) = json.get("choices") {
                    choices
                        .get(0)
                        .and_then(|c| c.get("delta"))
                        .unwrap_or(&serde_json::Value::Null) // OpenAI delta
                } else {
                    &serde_json::Value::Null
                };

                // 1. Process Reasoning (Thinking) - Supports both 'reasoning_content' and 'reasoning'
                let reasoning_val = msg_base
                    .get("reasoning_content")
                    .or_else(|| msg_base.get("reasoning"));

                if let Some(reasoning) = reasoning_val.and_then(|v| v.as_str()) {
                    if !is_thinking {
                        print!("\n\x1b[92;2;3m[Thinking]\n");
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
                    for call_json in calls {
                        let index =
                            call_json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                        let tool_calls = full_message.tool_calls.get_or_insert_with(Vec::new);
                        while tool_calls.len() <= index {
                            tool_calls.push(ToolCall {
                                id: String::new(),
                                tool_type: "function".to_string(),
                                function: FunctionCall {
                                    name: String::new(),
                                    arguments: serde_json::Value::String(String::new()),
                                },
                            });
                        }

                        let target = &mut tool_calls[index];
                        if let Some(id) = call_json.get("id").and_then(|v| v.as_str()) {
                            target.id.push_str(id);
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
                                            target.function.arguments = serde_json::Value::String(
                                                format!("{}{}", existing, s),
                                            );
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
            }
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
        println!();
    }

    Ok((full_message, usage_captured))
}
