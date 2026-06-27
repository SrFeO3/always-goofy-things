//! Slash command handling.
//!
//! Currently implements:
//! - `/help`, `/h`    - display help text
//! - `/rewind <turn>` - roll back conversation history to a specific turn
//! - `/history [-a]`  - print conversation history summary

use std::io::{self, Write};

use anyhow::{Context, Result, anyhow};

/// Result of handling a slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCmdResult {
    /// Command requires no turn change (e.g. /help), just re-prompt.
    NoAdvance,
    /// Rewind succeeded — reset the turn counter to this value.
    RewoundTo(i32),
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
// /help
// ---------------------------------------------------------------------------

/// Print the help text (matches the spec in `work/spec/slash_command.md`).
fn print_help() {
    println!(
        "\
\x1b[1mUsage:\x1b[0m \x1b[0m/<command> [options]

\x1b[1mCore Commands:\x1b[0m
   /h, /help        Display this help text and exit
   /rewind <turn>   Roll back conversation to <turn> and discard newer history
   /history [-a]    Print conversation history summary (-a, --all for raw payload)
   /exit, /quit     Exit the application (also accepts 'exit', 'quit', or Ctrl-D)

\x1b[1mExample:\x1b[0m
   \x1b[90m/rewind 1     - Discard everything after Turn 1 and continue from there\x1b[0m
   \x1b[90m/history -a   - Print raw JSON payload of conversation history\x1b[0m"
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
        println!("\x1b[36m[System] system prompt\x1b[0m");
        i = 1;
    }

    while i < messages.len() {
        // Collect user message
        if i < messages.len() && messages[i].role == "user" {
            turn += 1;
            let content = &messages[i].content;
            let preview = truncate(content, 80);
            println!(
                "\x1b[1mTurn {}:\x1b[0m \x1b[34m(User)\x1b[0m {}",
                turn, preview
            );
            i += 1;

            // Collect assistant message(s)
            while i < messages.len() && messages[i].role != "user" {
                let msg = &messages[i];
                if msg.role == "assistant" {
                    let mut summary = String::from("  \x1b[32m(Assistant)\x1b[0m");
                    if let Some(ref reasoning) = msg.reasoning_content {
                        let reasoning_preview = truncate(reasoning, 60);
                        summary.push_str(&format!(
                            "\n    \x1b[90m(reasoning)\x1b[0m {}",
                            reasoning_preview
                        ));
                    }
                    let content_preview = truncate(&msg.content, 60);
                    summary.push_str(&format!("\n    {}", content_preview));

                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            summary.push_str(&format!(
                                "\n    \x1b[35m[tool]\x1b[0m {}",
                                tc.function.name
                            ));
                        }
                    }
                    println!("{}", summary);
                } else if msg.role == "tool" {
                    let content_preview = truncate(&msg.content, 60);
                    println!("    \x1b[35m[tool result]\x1b[0m {}", content_preview);
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
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().take(max).collect();
    let result: String = chars.into_iter().collect();
    if s.chars().count() > max {
        format!("{}...", result)
    } else {
        result
    }
}
