//! Session persistence for conversation history.
//!
//! Saves and restores conversation logs in JSON Lines (JSONL) format
//! within platform-specific application data directories.
//!
//! # Persistence Files
//!
//! - `last_session_{label}.jsonl`: Active session log, appended during conversation.
//! - `previous_session_{label}.jsonl`: Prior completed session, moved on startup.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;

use crate::Message;
use crate::attach::AttachedFile;

/// Project directories singleton.
fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "SrFeO3", "always-goofy-things")
}

/// Path to the current session file (`last_session_{label}.jsonl`).
fn last_session_path(label: &str) -> Option<PathBuf> {
    let dirs = project_dirs()?;
    Some(
        dirs.data_local_dir()
            .join(format!("last_session_{}.jsonl", label)),
    )
}

/// Path to the previous session file (`previous_session_{label}.jsonl`).
fn previous_session_path(label: &str) -> Option<PathBuf> {
    let dirs = project_dirs()?;
    Some(
        dirs.data_local_dir()
            .join(format!("previous_session_{}.jsonl", label)),
    )
}

/// Called once at startup.
///
/// - Read `last_session_{label}.jsonl`. If it contains meaningful conversation (>= 1 user turn),
///   move it to `previous_session_{label}.jsonl` and start fresh.
/// - If empty or only system prompt, just clean it up (truncate).
pub fn init_session(label: &str) -> Result<()> {
    let last_path =
        last_session_path(label).ok_or_else(|| anyhow!("Could not determine session path"))?;

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
        let prev_path = previous_session_path(label)
            .ok_or_else(|| anyhow!("Could not determine previous session path"))?;
        if let Some(parent) = prev_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&last_path, &prev_path).with_context(|| {
            format!(
                "Failed to move last_session_{}.jsonl to previous_session_{}.jsonl",
                label, label
            )
        })?;
        // Create fresh empty last_session
        std::fs::File::create(&last_path)?;
    } else {
        // Just truncate - only system message or empty
        std::fs::write(&last_path, "")?;
    }

    Ok(())
}

/// Append a single message as one JSON line to the current session file.
///
/// Note: `attached_files` is `#[serde(skip)]` on `Message`, so it is injected
/// manually here to persist paths (content is excluded to keep JSONL lean).
pub fn save_message(label: &str, message: &Message) -> Result<()> {
    let path =
        last_session_path(label).ok_or_else(|| anyhow!("Could not determine session file path"))?;

    let mut json_val = serde_json::to_value(message)
        .with_context(|| format!("Failed to serialize message: role={}", message.role))?;

    // Inject attached_files metadata (path + attach_type only; content is #[serde(skip)])
    if !message.attached_files.is_empty() {
        let files: Vec<serde_json::Value> = message
            .attached_files
            .iter()
            .map(|f| serde_json::to_value(f).expect("AttachedFile is serializable"))
            .collect();
        json_val["attached_files"] = serde_json::Value::Array(files);
    }

    let json = serde_json::to_string(&json_val)
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

/// Restore (copy) messages from `previous_session_{label}.jsonl` into `last_session_{label}.jsonl`
/// and return the messages.
pub fn restore_previous_session(label: &str) -> Result<Vec<Message>> {
    let prev_path = previous_session_path(label)
        .ok_or_else(|| anyhow!("Could not determine previous session path"))?;

    if !prev_path.exists() {
        return Ok(Vec::new());
    }

    let messages = read_messages_from(&prev_path)?;

    // Copy previous -> last (so the restored session also becomes the new working session)
    let last_path =
        last_session_path(label).ok_or_else(|| anyhow!("Could not determine last session path"))?;
    std::fs::copy(&prev_path, &last_path)?;

    Ok(messages)
}

/// Read messages from any given jsonl file path.
///
/// Because `attached_files` is `#[serde(skip)]` on `Message`, the raw JSON is
/// parsed first so that attached file paths can be extracted manually.
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

        // Parse as raw Value first to extract attached_files before serde strips them
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(val) => {
                let mut msg: Message = serde_json::from_value(val.clone())
                    .with_context(|| format!("Failed to parse message at line {}", idx + 1))?;

                // Manually restore attached_files (skipped by #[serde(skip)] on the field)
                if let Some(files) = val.get("attached_files").and_then(|v| v.as_array()) {
                    for f_val in files {
                        if let Ok(attached) = serde_json::from_value::<AttachedFile>(f_val.clone())
                        {
                            msg.attached_files.push(attached);
                        }
                    }
                }

                messages.push(msg);
            }
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
