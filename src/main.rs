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

use std::env;
use std::io;
use std::io::Write; // Keep Write for flushing stdout for other prints

use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

mod tools;

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let llm_url =
        env::var("LLM_URL").unwrap_or_else(|_| "http://localhost:11434/api/chat".to_string());
    let model = env::var("LLM_MODEL").unwrap_or_else(|_| "gemma4:12b".to_string());
    let truncate_mode = env::var("TRUNCATE_MODE")
        .unwrap_or_else(|_| "2".to_string())
        .parse::<u8>()
        .unwrap_or(2);
    let current_dir = env::current_dir()?.to_string_lossy().to_string();
    let api_key_status = if env::var("LLM_API_KEY").is_ok() {
        "SET"
    } else {
        "NOT SET"
    };

    println!("--- [AGT STARTED] ---");
    println!("WORKING_DIR:   {}", current_dir);
    println!("LLM_URL:       {}", llm_url);
    println!("LLM_MODEL:     {}", model);
    println!("LLM_API_KEY:   {}", api_key_status);
    println!("TRUNCATE_MODE: {}", truncate_mode);
    let mut last_sent_count = 0;

    let mut rl = DefaultEditor::new()?;

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
            tools::WHITE_LIST.join(", ")
        ),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }];

    // Main conversation loop
    loop {
        let readline = rl.readline("\nUser > ");

        let input = match readline {
            Ok(line) => {
                // Add to history
                rl.add_history_entry(line.as_str())?;
                line
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl+C, exit
                println!("Ctrl-C received. Exiting.");
                break;
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
        if input == "exit" || input == "quit" {
            // Keep the exit/quit command
            break;
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
        loop {
            let assistant_msg =
                match call_llm(&llm_url, &model, &messages, truncate_mode, last_sent_count).await {
                    Ok(msg) => msg,
                    Err(e) => {
                        println!("\x1b[91m⚠️ LLM Connection Error: {}\x1b[0m", e);
                        println!("Conversation history preserved. You can try again or rephrase.");
                        break; // Exit the inner reasoning loop, return to User prompt
                    }
                };
            messages.push(assistant_msg.clone());
            last_sent_count = messages.len();

            if let Some(tool_calls) = assistant_msg.tool_calls {
                for call in tool_calls {
                    // Handle arguments that might be a JSON object (Ollama) or a stringified JSON (OpenAI)
                    let args_str = if let Some(s) = call.function.arguments.as_str() {
                        s.to_string()
                    } else {
                        call.function.arguments.to_string()
                    };

                    // Delegate tool confirmation and execution to the tools module
                    let tool_result =
                        tools::confirm_and_execute_tool(&call.function.name, &args_str).await;

                    // Extract the result string, handling potential errors
                    let tool_result_str = match tool_result {
                        Ok(res) => res,
                        Err(e) => format!("Error: {}", e),
                    };

                    println!("Result: {}", tool_result_str);

                    messages.push(Message {
                        role: "tool".to_string(),
                        content: tool_result_str,
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: Some(call.id),
                    });
                }
                // Re-query LLM with tool execution results
                continue;
            }
            break;
        }
    }
    Ok(())
}

async fn call_llm(
    url: &str,
    model: &str,
    messages: &[Message],
    truncate_mode: u8,
    last_msg_count: usize,
) -> Result<Message> {
    let client = reqwest::Client::new();
    let tools = tools::get_tool_definitions();
    let messages_vec = messages.to_vec();

    let req = ChatRequest {
        model: model.to_string(),
        messages: messages_vec,
        stream: true,
        tools,
    };

    let req_json = serde_json::to_string(&req)?;

    // Debug output based on truncate_mode
    match truncate_mode {
        0 => println!(
            "\x1b[90m--- [API REQUEST: {}] ---\n{}\x1b[0m",
            url, req_json
        ),
        1 => {
            if last_msg_count > 0 && last_msg_count < req.messages.len() {
                let mut truncated_req = req.clone();
                truncated_req.messages = truncated_req.messages[last_msg_count..].to_vec();
                let trunc_json = serde_json::to_string(&truncated_req)?;
                println!(
                    "\x1b[90m--- [API REQUEST: {} (Mode 1: Incremental)] ---\x1b[0m",
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
        2 => {
            println!(
                "\x1b[90m--- [API REQUEST: {}] (Content-Length: {}) ---\x1b[0m",
                url,
                req_json.len()
            );
        }
        _ => {} // Mode 3 or others: Silent
    }

    println!("... Waiting for response from {} ...", model);

    let mut request_builder = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "agt-client/0.1.0")
        .json(&req);

    if let Ok(api_key) = env::var("LLM_API_KEY") {
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

    // If no reasoning was provided by the end of the stream, notify the user
    if full_message.reasoning_content.is_none() {
        println!(
            "\x1b[90m(Reasoning content not supported or not provided by model: {})\x1b[0m",
            model
        );
    }

    if is_thinking {
        println!("\x1b[0m");
    }

    if !has_started_content {
        if full_message.tool_calls.is_some() {
            println!("Assistant > [Tool Call]");
        } else if full_message.content.is_empty() {
            println!();
        }
    } else {
        println!();
    }

    Ok(full_message)
}
