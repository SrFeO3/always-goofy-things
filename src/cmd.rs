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
//! - `/config [k] [v]`: Show or change app configuration (no arg: list all, -s/--short for aliases)
//! - `/restore [label]`: Restore the previous session, optionally for a specific label.
//! - `/stats`: Show LLM resource usage (per-model and session totals).
//! - `/exit`, `/quit`, `exit`, `quit`: Exit the application.

use std::io::{self, Write};

use anyhow::{Context, Result, anyhow};

use crate::llm_stats::{Metrics, ModelTotals, fmt_ms};
use crate::model::{Message, Session, Settings};
use crate::startup::{C_DIM_GRAY, C_DIM_GREEN, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, RESET};

/// Outcome of handling a slash command. The caller still owns the amended
/// turn counter for `RewoundTo` / `RestoredTo`; `Settings`/`Session` mutations
/// happen in-place inside this module so no value is echoed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCmdResult {
    /// No turn change (e.g. /help), just re-prompt.
    NoAdvance,
    /// Rewind succeeded - reset the turn counter to this value.
    RewoundTo(i32),
    /// Session was restored. Reset turn counter to this value.
    RestoredTo { turn: i32, label: String },
    /// User requested termination (`/exit`, `/quit`, bare `exit` / `quit`).
    Exit,
}

/// Check if the input starts with a slash command, and handle it if so.
///
/// Returns:
/// - `Some(SlashCmdResult)` - slash command was found and handled.
/// - `None` - NOT a slash command; let the caller process it as a normal message.
///
/// Bare `exit` / `quit` are also absorbed here so the caller has a single
/// `Exit` arm to break on. Model/config changes mutate `settings` in place.
pub fn try_handle_slash_command(
    input: &str,
    session: &mut Session,
    settings: &mut Settings,
    metrics: &Metrics,
) -> Option<SlashCmdResult> {
    let trimmed = input.trim();
    // Termination aliases (bare or `/`-prefixed). Centralised here so the
    // caller has one `Exit` arm instead of a parallel string-equality check.
    match trimmed {
        "/exit" | "/quit" | "exit" | "quit" => return Some(SlashCmdResult::Exit),
        _ => {}
    }
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
        "/rewind" => {
            match handle_rewind(arg, &session.label, &mut session.messages, session.turn) {
                Ok(target) => Some(SlashCmdResult::RewoundTo(target)),
                Err(e) => {
                    eprintln!("\x1b[91mSlash command error: {}\x1b[0m", e);
                    Some(SlashCmdResult::NoAdvance)
                }
            }
        }
        "/history" => {
            handle_history(arg, &session.messages);
            Some(SlashCmdResult::NoAdvance)
        }
        "/model" => {
            handle_model(arg, &mut settings.llm_model);
            Some(SlashCmdResult::NoAdvance)
        }
        "/config" => match handle_config(arg, settings) {
            Ok(()) => Some(SlashCmdResult::NoAdvance),
            Err(e) => {
                eprintln!("\x1b[91mSlash command error: {}\x1b[0m", e);
                Some(SlashCmdResult::NoAdvance)
            }
        },
        "/restore" => match handle_restore(&mut *session, arg) {
            Ok((new_turn, used_label)) => Some(SlashCmdResult::RestoredTo {
                turn: new_turn,
                label: used_label,
            }),
            Err(e) => {
                eprintln!("\x1b[91mSlash command error: {}\x1b[0m", e);
                Some(SlashCmdResult::NoAdvance)
            }
        },
        "/stats" => {
            handle_stats(metrics, &session.label);
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
// /model
// ---------------------------------------------------------------------------

/// Handle `/model [name]`.
///
/// Without an argument, print the currently active model name.
/// With an argument, switch to the provided model name and confirm.
fn handle_model(arg: Option<&str>, current_model: &mut String) {
    match arg {
        Some(name) if !name.is_empty() => {
            println!(
                "\x1b[32m✓ Switched model: {} -> {}\x1b[0m",
                current_model, name
            );
            *current_model = name.to_string();
        }
        _ => {
            println!("\x1b[93mCurrent model: {}\x1b[0m", current_model);
        }
    }
}

// ---------------------------------------------------------------------------
// /config
// ---------------------------------------------------------------------------

/// Parse `key value` from arg. E.g. "v 2" -> Some(("v", "2"))
/// Supports both short aliases (v, p, r) and full names.
fn parse_config_kv(input: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = input.splitn(2, char::is_whitespace).collect();
    if parts.len() == 2 {
        Some((parts[0].to_lowercase(), parts[1].trim().to_string()))
    } else {
        None
    }
}

/// Handle `/config [k] [v]`.
///
/// - No arg: list all config values.
/// - `-s` / `--short`: show short aliases for quick reference.
/// - `key value`: set the config value (e.g. `v 3`, `pretty-level 2`, `llm-rpm 30`).
fn handle_config(arg: Option<&str>, settings: &mut Settings) -> Result<()> {
    let arg_str = arg.unwrap_or("").trim();

    // No argument - list all config values
    if arg_str.is_empty() {
        println!("  \x1b[1mCurrent configuration:\x1b[0m");
        println!("    verbose-level     : {}", settings.verbose_level);
        println!("    pretty-level      : {}", settings.pretty_level);
        println!("    llm-rpm           : {}", settings.llm_rpm);
        println!("    max-output-tokens : {}", settings.max_output_tokens);
        println!(
            "    max-reasoning-empty-responses: {}",
            settings.max_reasoning_empty_responses
        );
        return Ok(());
    }

    // `-s` or `--short`: show short aliases
    if arg_str == "-s" || arg_str == "--short" {
        println!("  \x1b[1mConfig aliases:\x1b[0m (use these with /config)");
        println!("    \x1b[36mv\x1b[0m   - verbose-level     (0-4)");
        println!("    \x1b[36mp\x1b[0m   - pretty-level       (0-2)");
        println!("    \x1b[36mr\x1b[0m   - llm-rpm            (0 = unlimited)");
        println!("    \x1b[36mt\x1b[0m   - max-output-tokens  (default: 16384)");
        println!(
            "    \x1b[36me\x1b[0m   - max-reasoning-empty-responses (default: 2, 0 = unlimited)"
        );
        return Ok(());
    }

    // Parse "key value"
    let kv = match parse_config_kv(arg_str) {
        Some(kv) => kv,
        None => return Err(anyhow!("Usage: /config <key> <value>  (e.g. /config v 3)")),
    };

    match kv.0.as_str() {
        "v" | "verbose-level" | "verbose_level" | "verbose" => {
            let val: u8 = kv.1.parse().map_err(|_| {
                anyhow!("Invalid value for verbose-level: '{}'. Must be 0-4.", kv.1)
            })?;
            if val > 4 {
                return Err(anyhow!("verbose-level must be 0-4, got {}", val));
            }
            settings.verbose_level = val;
        }
        "p" | "pretty-level" | "pretty_level" | "pretty" => {
            let val: u8 = kv
                .1
                .parse()
                .map_err(|_| anyhow!("Invalid value for pretty-level: '{}'. Must be 0-2.", kv.1))?;
            if val > 2 {
                return Err(anyhow!("pretty-level must be 0-2, got {}", val));
            }
            settings.pretty_level = val;
        }
        "r" | "llm-rpm" | "llm_rpm" | "rpm" => {
            let val: u32 = kv.1.parse().map_err(|_| {
                anyhow!(
                    "Invalid value for llm-rpm: '{}'. Must be a non-negative integer.",
                    kv.1
                )
            })?;
            settings.llm_rpm = val;
        }
        "t" | "max-output-tokens" | "max_output_tokens" | "output-tokens" => {
            let val: u32 = kv.1.parse().map_err(|_| {
                anyhow!(
                    "Invalid value for max-output-tokens: '{}'. Must be a positive integer.",
                    kv.1
                )
            })?;
            if val == 0 {
                return Err(anyhow!("max-output-tokens must be > 0, got {}", val));
            }
            settings.max_output_tokens = val;
        }
        "e"
        | "max-reasoning-empty-responses"
        | "max_reasoning_empty_responses"
        | "reasoning-empty-responses" => {
            let val: u32 = kv.1.parse().map_err(|_| {
                anyhow!(
                    "Invalid value for max-reasoning-empty-responses: '{}'. Must be a non-negative integer.",
                    kv.1
                )
            })?;
            settings.max_reasoning_empty_responses = val;
        }
        _ => {
            return Err(anyhow!(
                "Unknown config key '{}'. Use /config -s for a list of keys.",
                kv.0
            ));
        }
    }

    println!("  \x1b[32m✓ Changed {} to {}\x1b[0m", kv.0, kv.1);
    Ok(())
}

// ---------------------------------------------------------------------------
// /help
// ---------------------------------------------------------------------------

/// Handle `/stats`: print per-model and session-total resource usage.
fn handle_stats(metrics: &Metrics, label: &str) {
    println!("Session stats (label: {})", label);
    if metrics.totals.calls == 0 {
        println!("No LLM calls recorded yet.");
        return;
    }
    let row = |t: &ModelTotals, name: &str| {
        println!(
            "{:<20}{:>7}{:>10}{:>10}{:>10}{:>10}{:>11}{:>10}",
            name,
            t.calls,
            t.in_normal,
            t.in_cached,
            t.in_cache_write,
            t.out_normal,
            t.out_reasoning,
            fmt_ms(t.llm_ms_total),
        );
    };
    println!(
        "{:<20}{:>7}{:>10}{:>10}{:>10}{:>10}{:>11}{:>10}",
        "Model", "Calls", "In", "Cache", "CacheW", "Out", "Reasoning", "LLM time"
    );
    let mut models: Vec<(&String, &ModelTotals)> = metrics.by_model.iter().collect();
    models.sort_by(|a, b| a.0.cmp(b.0));
    for (model, t) in models {
        row(t, model);
    }
    row(&metrics.totals, "total");
}

/// Print the help text.
fn print_help() {
    println!(
        "\x1b[1mUsage:\x1b[0m \x1b[0m/<command> [options]

\x1b[1mCore Commands:\x1b[0m
   /h, /help        Display this help text
   /rewind <turn>   Roll back conversation to <turn> and discard newer history
   /history [-a]    Print conversation history summary (-a, --all for raw payload)
   /model [name]    Switch the active LLM on the fly (no arg: show current)
   /config [k] [v]  Show or change app configuration (no arg: list all, -s for aliases)
   /restore [label] Restore the previous session (optionally specifying a label to switch to)
   /stats           Show LLM resource usage (per-model and session totals)
   /exit, /quit     Exit the application (also accepts 'exit', 'quit', or Ctrl-D)

\x1b[1mExample:\x1b[0m
   \x1b[90m/model        - Show the currently active model\x1b[0m
   \x1b[90m/model qwen   - Switch to 'qwen' model and continue\x1b[0m
   \x1b[90m/config       - List all current config values\x1b[0m
   \x1b[90m/config -s    - Show config key aliases (e.g. v, p, r)\x1b[0m
   \x1b[90m/config v 3   - Set verbose-level to 3\x1b[0m
   \x1b[90m/config p 2   - Set pretty-level to 2\x1b[0m
   \x1b[90m/config r 30  - Set llm-rpm to 30\x1b[0m
   \x1b[90m/config t 4096 - Set max-output-tokens to 4096\x1b[0m
   \x1b[90m/config e 3   - Set max-reasoning-empty-responses to 3 (0 = unlimited)\x1b[0m
   \x1b[90m/rewind 1     - Discard everything after Turn 1 and continue from there\x1b[0m
   \x1b[90m/history -a   - Print raw JSON payload of conversation history\x1b[0m
   \x1b[90m/restore      - Restore the latest session for current label\x1b[0m
   \x1b[90m/restore work - Restore the latest session for label 'work' and switch to it\x1b[0m
   \x1b[90m/stats        - Show LLM resource usage (per-model and session totals)\x1b[0m"
    );
}

// ---------------------------------------------------------------------------
// /rewind
// ---------------------------------------------------------------------------

/// Handle `/rewind <turn>`.
///
/// Finds the turn boundary by counting user messages (each user message
/// starts a new turn), truncates messages from that point, rewrites the
/// on-disk session to match, and returns the `target` turn number so the
/// caller can reset the turn counter.
pub(crate) fn handle_rewind(
    arg: Option<&str>,
    label: &str,
    messages: &mut Vec<Message>,
    current_turn: i32,
) -> Result<i32> {
    use crate::persistence;

    let target: i32 = match arg {
        Some(s) => s
            .parse()
            .map_err(|_| anyhow!("Invalid turn number: '{}'. Must be a positive integer.", s))?,
        None => return Err(anyhow!("Usage: /rewind <turn>\nExample: /rewind 1")),
    };

    if target < 1 {
        return Err(anyhow!("Turn number must be >= 1"));
    }
    if target >= current_turn {
        return Err(anyhow!(
            "Current turn is {}. Rewind target must be less than the current turn.",
            current_turn
        ));
    }

    // Count user messages = turns that have begun (including an in-progress
    // turn whose user message has no assistant reply yet). This is the upper
    // bound of the discard range.
    let user_msg_count = messages.iter().filter(|m| m.role == "user").count() as i32;

    // Calculate truncation point by finding where turn `target+1` starts.
    // Messages: [system(0), t1_user, t1_asst..., t2_user, t2_asst..., ...]
    // system(0) is skipped. User messages delimit each turn.
    // Find the (target+1)-th user message; everything from that index is discarded.
    let truncate_at = {
        let mut user_count = 0i32;
        messages.iter().position(|msg| {
            if msg.role == "user" {
                user_count += 1;
                user_count == target + 1
            } else {
                false
            }
        })
    };

    // No (target+1)-th user message means there is nothing to discard.
    if truncate_at.is_none() {
        return Err(anyhow!(
            "Nothing to discard: turns after {} do not exist",
            target
        ));
    }
    let truncate_at = truncate_at.unwrap();

    // A truncation point exists, so there is at least one turn to discard;
    // always confirm before truncating.
    let discarded_start = target + 1;
    let discarded_end = user_msg_count;
    println!(
        "\x1b[91m⚠️  WARNING: This will discard conversation turns {}-{}.\x1b[0m",
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

    // Truncate at the start of turn `target+1`.
    messages.truncate(truncate_at);

    // Rewrite the on-disk session to match the truncated conversation.
    persistence::rewrite_session(label, messages)?;

    println!(
        "\x1b[32m⏮ Rewound to Turn {}. Ready for your next input (Turn {}).\x1b[0m",
        target,
        target + 1
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
fn handle_history(arg: Option<&str>, messages: &Vec<Message>) {
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
    if let Some(first) = messages.first().filter(|m| !m.session_id.is_empty()) {
        println!("\x1b[90mSession ID: {}\x1b[0m", first.session_id);
    }
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

/// Keep the restored session's ID (first non-empty wins); legacy files get a fresh UUID.
pub(crate) fn adopt_restored_id(messages: &[Message]) -> String {
    messages
        .iter()
        .find(|m| !m.session_id.is_empty())
        .map(|m| m.session_id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Handle `/restore [label]`.
///
/// Restores the previous session from `previous_session_{label}.jsonl`, replacing
/// the current conversation. Returns the new turn count and the label used for restoration.
pub(crate) fn handle_restore(
    session: &mut Session,
    arg_label: Option<&str>,
) -> Result<(i32, String)> {
    use crate::persistence;

    let label = arg_label.unwrap_or(&session.label).trim().to_string();
    let restored = persistence::restore_previous_session(&label)?;
    if restored.is_empty() {
        println!(
            "\x1b[93mNo previous session found for label '{}'.\x1b[0m",
            label
        );
        return Err(anyhow!(
            "No previous session to restore for label '{}'",
            label
        ));
    }

    // Confirm with the user
    println!(
        "\x1b[93m⚠️  Restoring will replace the current conversation with {} saved message(s) from label '{}'.\x1b[0m",
        restored.len(),
        label
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
    session.messages.clear();
    session.messages.extend(restored);

    session.id = adopt_restored_id(&session.messages);

    // --- Re-read attached file contents from disk (paths-only were persisted) ---
    {
        use crate::attach::{self, AttachedFile};
        use std::path::Path;

        let mut restored_paths: Vec<String> = Vec::new();
        let mut missing_paths: Vec<String> = Vec::new();

        for msg in session.messages.iter_mut() {
            if msg.role != "user" || msg.attached_files.is_empty() {
                continue;
            }

            let mut reloaded: Vec<AttachedFile> = Vec::with_capacity(msg.attached_files.len());

            for f in msg.attached_files.drain(..) {
                let path_str = f.path;
                let fallback_type = f.attach_type;
                if Path::new(&path_str).exists() {
                    let spec = attach::AttachedSpec {
                        path: path_str.clone(),
                        page_range: None,
                    };
                    match attach::read_attached_files(
                        std::slice::from_ref(&spec),
                        attach::AttachMode::Raw,
                    ) {
                        Ok(mut entries) => {
                            if let Some(entry) = entries.pop() {
                                reloaded.push(entry);
                                restored_paths.push(path_str);
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "  \x1b[93m  Skipped '{}' (re-read failed: {})\x1b[0m",
                                path_str, e
                            );
                            reloaded.push(AttachedFile {
                                path: path_str.clone(),
                                content: String::new(),
                                attach_type: fallback_type.clone(),
                                page_range: None,
                            });
                            missing_paths.push(path_str);
                        }
                    }
                } else {
                    eprintln!(
                        "  \x1b[93m  Skipped '{}' (no longer exists on disk)\x1b[0m",
                        path_str
                    );
                    reloaded.push(AttachedFile {
                        path: path_str.clone(),
                        content: String::new(),
                        attach_type: fallback_type,
                        page_range: None,
                    });
                    missing_paths.push(path_str);
                }
            }

            msg.attached_files = reloaded;
        }

        if !restored_paths.is_empty() || !missing_paths.is_empty() {
            for p in &restored_paths {
                println!("  \x1b[32m  Restored '{}'\x1b[0m", p);
            }
            if !restored_paths.is_empty() && !missing_paths.is_empty() {
                // blank line between groups for clarity
                println!();
            }
            println!(
                "  \x1b[32m  Attached files: {} restored, {} missing\x1b[0m",
                restored_paths.len(),
                missing_paths.len()
            );
        }
    }

    // Calculate restored turns (each turn = user + assistant/tool)
    let mut restored_turns = 0i32;
    let mut i = 0;
    if !session.messages.is_empty() && session.messages[0].role == "system" {
        i = 1;
    }
    while i < session.messages.len() {
        if session.messages[i].role == "user" {
            restored_turns += 1;
        }
        i += 1;
    }

    // If the last message is a user message, the restored session ends on an
    // unfinished turn (a query with no assistant reply). Don't count it as a
    // completed turn, so the caller sets turn = N and the next input reuses
    // that user message instead of pushing a duplicate.
    if session.messages.last().is_some_and(|m| m.role == "user") {
        restored_turns -= 1;
    }

    println!(
        "\x1b[32m✓ Restored {} messages ({} turn(s)) from label '{}'.\x1b[0m",
        session.messages.len(),
        restored_turns,
        label
    );

    Ok((restored_turns, label))
}

#[cfg(test)]
#[path = "tests/cmd_rewind_test.rs"]
mod tests;
