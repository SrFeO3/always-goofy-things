//! Session persistence: save and restore conversations.
//!
//! Uses `directories::ProjectDirs` to locate the data directory.
//! Each conversation line is stored as one JSON object (JSONL format).
//!
//! File schema:
//!   - `last_session.jsonl`    — current session (appended during conversation)
//!   - `previous_session.jsonl` — last completed session (moved here on startup)

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;

use crate::Message;

/// Project directories singleton.
fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "SrFeO3", "always-goofy-things")
}

/// Path to the current session file (`last_session.jsonl`).
fn last_session_path() -> Option<PathBuf> {
    let dirs = project_dirs()?;
    Some(dirs.data_local_dir().join("last_session.jsonl"))
}

/// Path to the previous session file (`previous_session.jsonl`).
fn previous_session_path() -> Option<PathBuf> {
    let dirs = project_dirs()?;
    Some(dirs.data_local_dir().join("previous_session.jsonl"))
}

/// Called once at startup.
///
/// - Read `last_session.jsonl`. If it contains meaningful conversation (>= 1 user turn),
///   move it to `previous_session.jsonl` and start fresh.
/// - If empty or only system prompt, just clean it up (truncate).
pub fn init_session() -> Result<()> {
    let last_path =
        last_session_path().ok_or_else(|| anyhow!("Could not determine session path"))?;

    // Ensure parent dir exists
    if let Some(parent) = last_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }

    // If no previous file exists yet, just touch the last file
    if !last_path.exists() {
        std::fs::File::create(&last_path)?;
        return Ok(());
    }

    let restored = read_messages_from(&last_path)?;

    // "valid" = has at least one user turn (system + user = >= 2 messages)
    let has_user_turn = restored.iter().any(|m| m.role == "user");

    if has_user_turn {
        // Move last -> previous
        let prev_path = previous_session_path()
            .ok_or_else(|| anyhow!("Could not determine previous session path"))?;
        if let Some(parent) = prev_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&last_path, &prev_path)
            .with_context(|| "Failed to move last_session.jsonl to previous_session.jsonl")?;
        // Create fresh empty last_session
        std::fs::File::create(&last_path)?;
    } else {
        // Just truncate — only system message or empty
        std::fs::write(&last_path, "")?;
    }

    Ok(())
}

/// Append a single message as one JSON line to the current session file.
pub fn save_message(message: &Message) -> Result<()> {
    let path =
        last_session_path().ok_or_else(|| anyhow!("Could not determine session file path"))?;

    let json = serde_json::to_string(message)
        .with_context(|| format!("Failed to serialize message: role={}", message.role))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open session file {:?}", path))?;

    writeln!(file, "{}", json)
        .with_context(|| format!("Failed to write to session file {:?}", path))?;
    Ok(())
}

/// Restore (copy) messages from `previous_session.jsonl` into `last_session.jsonl`
/// and return the messages.
pub fn restore_previous_session() -> Result<Vec<Message>> {
    let prev_path = previous_session_path()
        .ok_or_else(|| anyhow!("Could not determine previous session path"))?;

    if !prev_path.exists() {
        return Ok(Vec::new());
    }

    let messages = read_messages_from(&prev_path)?;

    // Copy previous -> last (so the restored session also becomes the new working session)
    let last_path =
        last_session_path().ok_or_else(|| anyhow!("Could not determine last session path"))?;
    std::fs::copy(&prev_path, &last_path)?;

    Ok(messages)
}

/// Read messages from any given jsonl file path.
fn read_messages_from(path: &std::path::Path) -> Result<Vec<Message>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open session file {:?}", path))?;

    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("Failed to read line {}", idx + 1))?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Message>(&line) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                eprintln!(
                    "\x1b[93mWarning: Skipping malformed line {}: {} \x1b[0m",
                    idx + 1,
                    e
                );
            }
        }
    }

    Ok(messages)
}

/// Return the human-readable path of the session files (for display purposes).
pub fn session_file_display() -> String {
    format!(
        "last={}, previous={}",
        last_session_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        previous_session_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    )
}
