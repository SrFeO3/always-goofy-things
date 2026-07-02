//! Slash command dispatching and processing.
//!
//! Provides direct control over the LLM loop's conversation context
//! and session configuration via user-input commands.
//!
//! # Supported Commands
//!
//! - `/help`, `/h`: Display help text.
//! - `/rewind <turn>`: Roll back conversation history to a specific turn.
//! - `/history [-a]`: Show a summary of the conversation history.
//! - `/model [name]`: Switch the active LLM on the fly.
//! - `/restore`: Restore the previous session from disk.
//! - `/exit`, `/quit`, `exit`, `quit`: Exit the application.

use std::io::{self, Write};

use anyhow::{Context, Result, anyhow};

use crate::startup::{C_DIM_GRAY, C_DIM_GREEN, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, RESET};

/// Result of handling a slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCmdResult {
    /// Command requires no turn change (e.g. /help), just re-prompt.
    NoAdvance,
    /// Rewind succeeded — reset the turn counter to this value.
    RewoundTo(i32),
    /// Model was switched to the new name.
    ModelChanged(String),
    /// Session was restored. Reset turn counter to this value.
    RestoredTo(i32),
}

/// Check if the input starts with a slash command, and handle it if so.
///
/// Returns:
/// - `Some(SlashCmdResult)` — slash command was found and handled.
/// - `None` — NOT a slash command; let the caller process it as a normal message.
pub fn try_handle_slash_command(
    input: &str,
    messages: &mut Vec<crate::Message>,
    turn: i32,
    current_model: &str,
) -> Option<SlashCmdResult> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let cmd = parts[0].to_lowercase();
    let arg = parts.get(1).map(|s| s.trim());
    match cmd.as_str() {
        "/h" | "/help" => {
            print_help();
            Some(SlashCmdResult::NoAdvance)
        }
        "/rewind" => match handle_rewind(arg, messages, turn) {
            Ok(target) => Some(SlashCmdResult::RewoundTo(target)),
            Err(e) => {
                eprintln!("\x1b[91mSlash command error: {}\x1b[0m", e);
                Some(SlashCmdResult::NoAdvance)
            }
        },
        "/history" => {
            handle_history(arg, messages);
            Some(SlashCmdResult::NoAdvance)
        }
        "/model" => {
            let new_model = handle_model(arg, current_model);
            Some(SlashCmdResult::ModelChanged(new_model))
        }
        "/restore" => match handle_restore(messages) {
            Ok(new_turn) => Some(SlashCmdResult::RestoredTo(new_turn)),
            Err(e) => {
                eprintln!("\x1b[91mSlash command error: {}\x1b[0m", e);
                Some(SlashCmdResult::NoAdvance)
            }
        },
        _ => {
            eprintln!(
                "\x1b[93mUnknown command: {}\x1b[0m Type /help for available commands.",
                cmd
            );
            Some(SlashCmdResult::NoAdvance)
        }
    }
}

// ---------------------------------------------------------------------------
// /model
// ---------------------------------------------------------------------------

/// Handle `/model [name]`.
///
/// Without an argument, print the currently active model name.
/// With an argument, switch to the provided model name and confirm.
fn handle_model(arg: Option<&str>, current_model: &str) -> String {
    match arg {
        Some(name) if !name.is_empty() => {
            println!(
                "\x1b[32m✓ Switched model: {} → {}\x1b[0m",
                current_model, name
            );
            name.to_string()
        }
        _ => {
            println!("\x1b[93mCurrent model: {}\x1b[0m", current_model);
            current_model.to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// /help
// ---------------------------------------------------------------------------

/// Print the help text (matches the spec in `work/spec/slash_command.md`).
fn print_help() {
    println!(
        "\x1b[1mUsage:\x1b[0m \x1b[0m/<command> [options]

\x1b[1mCore Commands:\x1b[0m
   /h, /help        Display this help text
   /rewind <turn>   Roll back conversation to <turn> and discard newer history
   /history [-a]    Print conversation history summary (-a, --all for raw payload)
   /model [name]    Switch the active LLM on the fly (no arg: show current)
   /restore         Restore the previous session from disk
   /exit, /quit     Exit the application (also accepts 'exit', 'quit', or Ctrl-D)

\x1b[1mExample:\x1b[0m
   \x1b[90m/model        - Show the currently active model\x1b[0m
   \x1b[90m/model qwen   - Switch to 'qwen' model and continue\x1b[0m
   \x1b[90m/rewind 1     - Discard everything after Turn 1 and continue from there\x1b[0m
   \x1b[90m/history -a   - Print raw JSON payload of conversation history\x1b[0m
   \x1b[90m/restore      - Restore the latest session from disk\x1b[0m"
    );
}

// ---------------------------------------------------------------------------
// /rewind
// ---------------------------------------------------------------------------

/// Handle `/rewind <turn>`.
///
/// Returns the `target` turn number on success so the caller can reset
/// the turn counter. Returns an error on invalid input or user cancellation.
fn handle_rewind(
    arg: Option<&str>,
    messages: &mut Vec<crate::Message>,
    current_turn: i32,
) -> Result<i32> {
    let target: i32 = match arg {
        Some(s) => s
            .parse()
            .map_err(|_| anyhow!("Invalid turn number: '{}'. Must be a positive integer.", s))?,
        None => return Err(anyhow!("Usage: /rewind <turn>\nExample: /rewind 1")),
    };

    if target < 0 {
        return Err(anyhow!("Turn number must be >= 0"));
    }
    if target >= current_turn {
        return Err(anyhow!(
            "Current turn is {}. Rewind target must be less than the current turn.",
            current_turn
        ));
    }

    // Calculate how many messages to keep.
    // Index 0 = system message.
    // Each completed turn adds 2 messages (user + assistant).
    // So messages up to end of turn `target` = 1 + 2 * target
    let keep_count = 1 + (target as usize) * 2;

    let discarded_start = target + 1;
    let discarded_end = current_turn;

    // Warn the user (matches spec)
    println!(
        "\x1b[91m⚠️  ARNING: This will discard conversation turns {}-{}.\x1b[0m",
        discarded_start, discarded_end
    );
    println!(
        "\x1b[93m   Note that any local file changes made during these turns CANNOT be undone.\x1b[0m"
    );
    print!("\x1b[1m   Proceed? (y/n) > \x1b[0m");
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut confirm = String::new();
    io::stdin()
        .read_line(&mut confirm)
        .context("Failed to read confirmation")?;
    if !confirm.trim().eq_ignore_ascii_case("y") {
        println!("\x1b[93mCancelled.\x1b[0m");
        return Err(anyhow!("User cancelled the rewind"));
    }

    // Truncate messages to keep only up to the target turn
    if keep_count < messages.len() {
        messages.truncate(keep_count);
    }

    println!(
        "\x1b[32m⏮ Rewound to Turn {}. Ready for your next input (Turn {}).\x1b[0m",
        target,
        (target + 1)
    );

    Ok(target)
}

// ---------------------------------------------------------------------------
// /history
// ---------------------------------------------------------------------------

/// Handle `/history [-a]`.
///
/// Without `-a` / `--all`, prints a human-readable summary of each turn.
/// With `-a` / `--all`, prints the full raw JSON payload.
fn handle_history(arg: Option<&str>, messages: &Vec<crate::Message>) {
    let show_all = match arg {
        Some(s) => s == "-a" || s == "--all",
        None => false,
    };

    if show_all {
        let json = serde_json::to_string_pretty(messages)
            .unwrap_or_else(|e| format!("Error serializing messages: {}", e));
        println!("{}", json);
        return;
    }

    // Summary mode: iterate over messages and print a condensed view per turn
    if messages.is_empty() {
        println!("\x1b[93mNo conversation history.\x1b[0m");
        return;
    }

    println!(
        "\x1b[1mConversation History ({} message(s))\x1b[0m",
        messages.len()
    );
    println!("\x1b[90m{}\x1b[0m", "-".repeat(40));

    let mut turn = 0;
    let mut i = 0;

    // Skip the first message if it's the system prompt
    if !messages.is_empty() && messages[0].role == "system" {
        let ts = messages[0].timestamp.format("%m/%d %H:%M:%S");
        println!("\x1b[36m[System]\x1b[0m {} system prompt", ts);
        i = 1;
    }

    while i < messages.len() {
        // Collect user message
        if i < messages.len() && messages[i].role == "user" {
            turn += 1;
            let ts = messages[i].timestamp.format("%m/%d %H:%M:%S");
            let content = &messages[i].content;
            let preview = truncate_and_flatten(content, 60);
            println!(
                "\x1b[1mTurn {}:\x1b[0m {} \x1b[34m(User)\x1b[0m {}",
                turn, ts, preview
            );
            i += 1;

            // Collect assistant message(s)
            let mut llm_call_num = 0;
            while i < messages.len() && messages[i].role != "user" {
                let msg = &messages[i];
                if msg.role == "assistant" {
                    llm_call_num += 1;
                    let ts = msg.timestamp.format("%m/%d %H:%M:%S");
                    let model = msg.model.as_deref().unwrap_or("?");
                    println!(
                        "   {}LLM call{}({}-{}) {}: {}[{}]{}",
                        C_GREEN, RESET, turn, llm_call_num, ts, C_DIM_GREEN, model, RESET
                    );
                    if let Some(ref reasoning) = msg.reasoning_content {
                        println!("     Thinking: {}", truncate_and_flatten(reasoning, 60));
                    }
                    if msg.content.trim().is_empty() {
                        println!("     Response: {}No message content{}", C_DIM_GRAY, RESET);
                    } else {
                        println!("     Response: {}", truncate_and_flatten(&msg.content, 60));
                    }
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            println!(
                                "       {}Tool call{}({}): {}",
                                C_YELLOW, RESET, tc.id, tc.function.name
                            );
                        }
                    }
                } else if msg.role == "tool" {
                    if let Some(ref decision) = msg.tool_call_decision {
                        let (label, color) = match &decision.kind {
                            crate::tools::ToolRunDecisionKind::UserConfirm => {
                                ("User-confirmed", C_GREEN)
                            }
                            crate::tools::ToolRunDecisionKind::AutoConfirm => {
                                ("Auto-confirmed", C_MAGENTA)
                            }
                            crate::tools::ToolRunDecisionKind::UserCancel => {
                                ("User-canceled", C_YELLOW)
                            }
                            crate::tools::ToolRunDecisionKind::SystemError => {
                                ("System-error", C_RED)
                            }
                        };
                        let reason_str = decision
                            .reason
                            .as_deref()
                            .map(|r| format!(": {}", r))
                            .unwrap_or_default();
                        println!(
                            "         Decision({}): {}{}{}{}",
                            msg.tool_call_id.as_deref().unwrap_or("?"),
                            color,
                            label,
                            RESET,
                            reason_str
                        );
                    }
                    println!(
                        "       Tool result({}): {}",
                        msg.tool_call_id.as_deref().unwrap_or("?"),
                        truncate_and_flatten(&msg.content, 60)
                    );
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    println!("\x1b[90m{}\x1b[0m", "-".repeat(40));
    println!("Total turns: {}", turn);
}

/// Truncate a string to a max length, appending "..." if truncated.
fn truncate_and_flatten(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().take(max + 1).collect();
    let result = if chars.len() > max {
        let truncated: String = chars[..max].iter().collect();
        format!("{}...", truncated)
    } else {
        chars.into_iter().collect()
    };
    result.replace("\r\n", " \\n ").replace('\n', " \\n ")
}

// ---------------------------------------------------------------------------
// /restore
// ---------------------------------------------------------------------------

/// Handle `/restore`.
///
/// Restores the previous session from `previous_session.jsonl`, replacing
/// the current conversation. Returns the calculated new turn count.
fn handle_restore(messages: &mut Vec<crate::Message>) -> Result<i32> {
    use crate::persistence;

    let restored = persistence::restore_previous_session()?;
    if restored.is_empty() {
        println!(
            "\x1b[93mNo previous session found.{}\x1b[0m",
            persistence::session_file_display()
        );
        return Err(anyhow!("No previous session to restore"));
    }

    // Confirm with the user
    println!(
        "\x1b[93m⚠️  Restoring will replace the current conversation with {} saved message(s).\x1b[0m",
        restored.len()
    );
    print!("\x1b[1m   Proceed? (y/n) > \x1b[0m");
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut confirm = String::new();
    io::stdin()
        .read_line(&mut confirm)
        .context("Failed to read confirmation")?;
    if !confirm.trim().eq_ignore_ascii_case("y") {
        println!("\x1b[93mCancelled.\x1b[0m");
        return Err(anyhow!("User cancelled the restore"));
    }

    // Replace messages with restored ones
    messages.clear();
    messages.extend(restored);

    // Calculate restored turns (each turn = user + assistant/tool)
    let mut restored_turns = 0i32;
    let mut i = 0;
    if !messages.is_empty() && messages[0].role == "system" {
        i = 1;
    }
    while i < messages.len() {
        if messages[i].role == "user" {
            restored_turns += 1;
        }
        i += 1;
    }

    println!(
        "\x1b[32m✓ Restored {} messages ({} turn(s)) from previous session.\x1b[0m",
        messages.len(),
        restored_turns
    );

    Ok(restored_turns)
}
