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

use crate::attach::AttachedFile;
use crate::model::Message;

/// Project directories singleton.
fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "SrFeO3", "always-goofy-things")
}

/// Root directory for session + stats JSONL files.
///
/// The `SESSION_DATA_DIR` environment variable (companion of `SESSION_LABEL`)
/// overrides the platform-specific application data directory (see README).
/// Files land directly in this root.
pub(crate) fn data_dir() -> Option<PathBuf> {
    std::env::var_os("SESSION_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| project_dirs().map(|d| d.data_local_dir().to_path_buf()))
}

/// Path to the current session file (`last_session_{label}.jsonl`).
fn last_session_path(label: &str) -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(format!("last_session_{}.jsonl", label)))
}

/// Path to the previous session file (`previous_session_{label}.jsonl`).
fn previous_session_path(label: &str) -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(format!("previous_session_{}.jsonl", label)))
}

/// Path to the resource-stats log (`llm_stats_{label}.jsonl`).
///
/// Separate from the conversation JSONL so a recoverable stats write can never
/// corrupt (or be mistaken for) conversation history.
fn stats_path(label: &str) -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(format!("llm_stats_{}.jsonl", label)))
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

/// Serialize one `Message` to its JSONL line, the exact wire format that both
/// `append_message_to_session` (append one) and `rewrite_session` (overwrite all)
/// write.
///
/// `attached_files` is `#[serde(skip)]` on `Message`, so its metadata (path +
/// attach_type; content is excluded to keep JSONL lean) is injected manually
/// here.
fn serialize_message(message: &Message) -> Result<String> {
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

    serde_json::to_string(&json_val)
        .with_context(|| format!("Failed to serialize message: role={}", message.role))
}

/// Append one `Message` as a JSON line to `last_session_{label}.jsonl`.
///
/// Serialization, including the `attached_files` injection, is delegated to
/// `serialize_message` (the shared wire format for the session file).
pub fn append_message_to_session(label: &str, message: &Message) -> Result<()> {
    let path =
        last_session_path(label).ok_or_else(|| anyhow!("Could not determine session file path"))?;
    let json = serialize_message(message)?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open session file {:?}", path))?;

    writeln!(file, "{}", json)
        .with_context(|| format!("Failed to write to session file {:?}", path))?;
    Ok(())
}

/// Overwrite `last_session_{label}.jsonl` with exactly `messages`, one JSON
/// line per message, via the same `serialize_message` wire format as
/// `append_message_to_session`.
///
/// Writes to a sibling temp file then `rename`s it into place so a crash
/// mid-write cannot leave the session file half-written (rename is atomic
/// within the same directory).
pub fn rewrite_session(label: &str, messages: &[Message]) -> Result<()> {
    let path =
        last_session_path(label).ok_or_else(|| anyhow!("Could not determine session file path"))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }

    // Build the full file content first (one line per message).
    let mut content = String::new();
    for m in messages {
        content.push_str(&serialize_message(m)?);
        content.push('\n');
    }

    // Write to a sibling temp file, then rename atomically over the target.
    let tmp = path.with_extension(format!("jsonl.tmp.{}", std::process::id()));
    std::fs::write(&tmp, content)
        .with_context(|| format!("Failed to write temp session file {:?}", tmp))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("Failed to replace session file {:?} with {:?}", tmp, path))?;

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

/// Read the current session (`last_session_{label}.jsonl`) back into memory.
/// Test-only (no production path reads it back).
#[cfg(test)]
pub fn load_current_session(label: &str) -> Result<Vec<Message>> {
    let path =
        last_session_path(label).ok_or_else(|| anyhow!("Could not determine session file path"))?;
    read_messages_from(&path)
}

/// Archive a todo task's session: rename `last_session_{label}.jsonl` -> `todo_loop_{task_index}_{label}.jsonl`.
///
/// Called after a todo task completes to preserve its conversation history
/// separately from the current session file.
pub fn archive_todo_session(label: &str, task_index: usize) -> Result<()> {
    let last_path =
        last_session_path(label).ok_or_else(|| anyhow!("Could not determine last session path"))?;

    if !last_path.exists() {
        return Ok(());
    }

    // Resolve the archive path from the same data root as the session file so
    // a `SESSION_DATA_DIR` override relocates everything together and the
    // rename below never crosses filesystems (EXDEV).
    let data_dir = data_dir().ok_or_else(|| anyhow!("Could not determine data dir"))?;
    let archive_path = data_dir.join(format!("todo_loop_{}_{}.jsonl", task_index, label));

    if let Some(parent) = archive_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Move a pre-existing archive to a timestamped name instead of
    // overwriting the earlier run's history.
    if archive_path.exists() {
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let backup = data_dir.join(format!("todo_loop_{}_{}_{}.jsonl", task_index, label, ts));
        std::fs::rename(&archive_path, &backup).with_context(|| {
            format!(
                "Failed to move existing archive {:?} -> {:?}",
                archive_path, backup
            )
        })?;
    }
    std::fs::rename(&last_path, &archive_path).with_context(|| {
        format!(
            "Failed to archive session {:?} -> {:?}",
            last_path, archive_path
        )
    })?;

    Ok(())
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

/// Append one LLM call record as a JSON line to `llm_stats_{label}.jsonl`.
pub fn save_call_record(label: &str, rec: &crate::llm_stats::LlmCallRecord) -> Result<()> {
    let path = stats_path(label).ok_or_else(|| anyhow!("Could not determine stats path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }
    let json = serde_json::to_string(rec).context("Failed to serialize LLM call record")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open stats file {:?}", path))?;
    writeln!(file, "{}", json)
        .with_context(|| format!("Failed to write to stats file {:?}", path))?;
    Ok(())
}

/// Read all LLM call records from `llm_stats_{label}.jsonl`.
///
/// A missing file yields an empty list; malformed lines are skipped with a
/// warning (matching `read_messages_from` behaviour).
pub fn load_stats(label: &str) -> Result<Vec<crate::llm_stats::LlmCallRecord>> {
    let Some(path) = stats_path(label) else {
        return Ok(Vec::new());
    };
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path)
        .with_context(|| format!("Failed to open stats file {:?}", path))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("Failed to read stats line {}", idx + 1))?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<crate::llm_stats::LlmCallRecord>(&line) {
            Ok(rec) => records.push(rec),
            Err(e) => {
                eprintln!(
                    "\x1b[93mWarning: Skipping malformed stats line {}: {} \x1b[0m",
                    idx + 1,
                    e
                );
            }
        }
    }
    Ok(records)
}

#[cfg(test)]
#[path = "tests/persistence_test.rs"]
mod tests;
