//! Tests for `src/persistence.rs` (conversation + stats JSONL round-trips).

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::compat_provider::LlmProvider;
use crate::llm_stats::{CallStatus, LlmCallRecord};
use crate::model::{FunctionCall, ToolCall, Usage};

/// Serializes persistence tests (`SESSION_DATA_DIR` is process-global).
static PERSIST_LOCK: Mutex<()> = Mutex::new(());

/// Points persistence at a unique temp dir, cleaned up on drop.
struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agt_persist_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Rust 2024 marks `set_var` unsafe; `PERSIST_LOCK` serializes tests so
        // no other thread observes the override mid-window.
        unsafe { std::env::set_var("SESSION_DATA_DIR", &dir) };
        Self(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("SESSION_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn test_conversation_round_trip_preserves_wire_format() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    let label = "rt_test";

    let user_with_attach = Message {
        role: "user".to_string(),
        content: "hi @file".to_string(),
        attached_files: vec![crate::attach::AttachedFile {
            path: "a.txt".to_string(),
            content: "irrelevant (excluded)".to_string(),
            attach_type: crate::file::FileType::Text,
            page_range: None,
        }],
        ..Default::default()
    };

    let msgs = vec![
        Message {
            role: "system".to_string(),
            content: "sys".to_string(),
            ..Default::default()
        },
        user_with_attach,
        Message {
            role: "assistant".to_string(),
            content: "answer".to_string(),
            reasoning_content: Some("thought".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: json!({"path": "a.txt"}),
                },
                thought_signature: None,
            }]),
            ..Default::default()
        },
        Message {
            role: "tool".to_string(),
            content: r#"{"status":"ok"}"#.to_string(),
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("read_file".to_string()),
            ..Default::default()
        },
    ];

    for m in &msgs {
        append_message_to_session(label, m).unwrap();
    }

    let path = last_session_path(label).unwrap();
    let restored = read_messages_from(&path).unwrap();
    assert_eq!(restored.len(), msgs.len());

    for (saved, restored) in msgs.iter().zip(restored.iter()) {
        // The wire (serde-visible) representation must be byte-identical, so
        // nothing rewritten on restore can ever change a later LLM request.
        assert_eq!(
            serde_json::to_string(saved).unwrap(),
            serde_json::to_string(restored).unwrap(),
            "wire format changed for role={}",
            saved.role
        );
        // Attached-file paths are persisted manually; verify they survived.
        assert_eq!(
            serde_json::to_string(&saved.attached_files).unwrap(),
            serde_json::to_string(&restored.attached_files).unwrap()
        );
    }
}

/// `rewrite_session` must fully overwrite the on-disk session to match the
/// in-memory `messages`: a truncated conversation must not leave the discarded
/// turns in the file, and the rewritten wire format must round-trip exactly
/// (including manually injected `attached_files`).
#[test]
fn test_rewrite_session_overwrites_and_round_trips() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    let label = "rewrite_test";

    // First append a 3-turn history as if turns 1..3 were completed.
    let full = vec![
        Message {
            role: "system".to_string(),
            content: "sys".to_string(),
            ..Default::default()
        },
        Message {
            role: "user".to_string(),
            content: "q1".to_string(),
            ..Default::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "a1".to_string(),
            ..Default::default()
        },
        Message {
            role: "user".to_string(),
            content: "q2".to_string(),
            ..Default::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "a2".to_string(),
            ..Default::default()
        },
        Message {
            role: "user".to_string(),
            content: "q3".to_string(),
            ..Default::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "a3".to_string(),
            ..Default::default()
        },
    ];
    for m in &full {
        append_message_to_session(label, m).unwrap();
    }

    // /rewind to turn 1: rewrite with only system + turn 1.
    let kept: Vec<Message> = full[..3].to_vec();
    rewrite_session(label, &kept).unwrap();

    let path = last_session_path(label).unwrap();
    let restored = read_messages_from(&path).unwrap();

    // The file must now hold exactly `kept`; discarded turns (q2/q3) are gone.
    assert_eq!(restored.len(), kept.len());
    assert_eq!(
        serde_json::to_string(&restored).unwrap(),
        serde_json::to_string(&kept).unwrap()
    );
    assert!(
        !restored
            .iter()
            .any(|m| m.content == "q2" || m.content == "q3"),
        "discarded turns must not survive a rewrite"
    );

    // Attached-file metadata must survive the rewrite with the same wire
    // format `append_message_to_session` produces.
    let with_attach = Message {
        role: "user".to_string(),
        content: "hi @file".to_string(),
        attached_files: vec![crate::attach::AttachedFile {
            path: "a.txt".to_string(),
            content: "excluded (not serialized)".to_string(),
            attach_type: crate::file::FileType::Text,
            page_range: None,
        }],
        ..Default::default()
    };
    let with_tool = Message {
        role: "assistant".to_string(),
        content: "answer".to_string(),
        reasoning_content: Some("thought".to_string()),
        ..Default::default()
    };
    let msgs = vec![kept[0].clone(), with_attach, with_tool];
    rewrite_session(label, &msgs).unwrap();

    let restored2 = read_messages_from(&path).unwrap();
    assert_eq!(restored2.len(), msgs.len());
    for (saved, got) in msgs.iter().zip(restored2.iter()) {
        assert_eq!(
            serde_json::to_string(saved).unwrap(),
            serde_json::to_string(got).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&saved.attached_files).unwrap(),
            serde_json::to_string(&got.attached_files).unwrap()
        );
    }
}

#[test]
fn test_stats_round_trip() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    let label = "stats_test";

    let rec = LlmCallRecord {
        timestamp: chrono::Utc::now(),
        model: "gpt-4o".to_string(),
        provider: LlmProvider::OpenAi,
        call_label: "main".to_string(),
        usage: Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            ..Default::default()
        },
        latency_ms: 1234,
        ttft_ms: 100,
        request_bytes: 512,
        response_bytes: 9800,
        retry_count: 1,
        status: CallStatus::Ok,
    };

    save_call_record(label, &rec).unwrap();
    let loaded = load_stats(label).unwrap();
    assert_eq!(loaded.len(), 1);
    let l = &loaded[0];
    assert_eq!(l.model, "gpt-4o");
    assert_eq!(l.provider, LlmProvider::OpenAi);
    assert_eq!(l.call_label, "main");
    assert_eq!(l.usage.prompt_tokens, 100);
    assert_eq!(l.latency_ms, 1234);
    assert_eq!(l.ttft_ms, 100);
    assert_eq!(l.request_bytes, 512);
    assert_eq!(l.response_bytes, 9800);
    assert_eq!(l.retry_count, 1);
    assert_eq!(l.status, CallStatus::Ok);
    assert_eq!(l.timestamp.timestamp(), rec.timestamp.timestamp());
}

#[test]
fn test_load_stats_missing_file_is_empty() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    assert!(load_stats("no_such_label").unwrap().is_empty());
}

/// Stats records must never leak into the conversation JSONL (separate files).
#[test]
fn test_stats_do_not_mix_into_conversation() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    let label = "mix_guard";

    save_call_record(
        label,
        &LlmCallRecord {
            timestamp: chrono::Utc::now(),
            model: "gpt-4o".to_string(),
            provider: LlmProvider::OpenAi,
            call_label: "main".to_string(),
            usage: Usage::default(),
            latency_ms: 1,
            ttft_ms: 1,
            request_bytes: 1,
            response_bytes: 1,
            retry_count: 0,
            status: CallStatus::Ok,
        },
    )
    .unwrap();

    let path = last_session_path(label).unwrap();
    // No conversation file exists (only stats were written) -> empty restore.
    assert!(!path.exists() || read_messages_from(&path).unwrap().is_empty());
    // Conversation restore yields nothing, which is the correct "no session" case.
    assert!(restore_previous_session(label).unwrap().is_empty());
}

/// Re-archiving a task keeps the previous archive under a timestamped name.
#[test]
fn test_todo_archive_preserves_existing_archive() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    let label = "archive_keep";
    let task_index = 3usize;
    let data_dir = std::env::var("SESSION_DATA_DIR").unwrap();
    let archive =
        std::path::Path::new(&data_dir).join(format!("todo_loop_{}_{}.jsonl", task_index, label));

    // Archive, then re-archive the same task (resume).
    append_message_to_session(
        label,
        &Message {
            role: "user".to_string(),
            content: "first".to_string(),
            ..Default::default()
        },
    )
    .unwrap();
    archive_todo_session(label, task_index).unwrap();
    assert!(archive.exists(), "first archive must exist");

    append_message_to_session(
        label,
        &Message {
            role: "user".to_string(),
            content: "second".to_string(),
            ..Default::default()
        },
    )
    .unwrap();
    archive_todo_session(label, task_index).unwrap();

    // The canonical archive holds the new session; the old one is kept
    // under a timestamped name.
    let archived = read_messages_from(&archive).unwrap();
    assert!(
        archived.iter().any(|m| m.content == "second"),
        "canonical archive must hold the newest session"
    );
    let backups: Vec<_> = std::fs::read_dir(&data_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(&format!("todo_loop_{}_{}_", task_index, label))
        })
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "previous archive must be preserved with a timestamp suffix"
    );
}

/// Todo archives must land in the same data root as the session files, so a
/// `SESSION_DATA_DIR` override relocates everything together and `rename` never
/// crosses filesystems.
#[test]
fn test_todo_archive_uses_data_dir_override() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    let label = "archive_test";
    let task_index = 3usize;

    // Create the session file that would be archived.
    append_message_to_session(
        label,
        &Message {
            role: "user".to_string(),
            content: "hi".to_string(),
            ..Default::default()
        },
    )
    .unwrap();
    let last = last_session_path(label).unwrap();
    assert!(last.exists(), "last_session must exist before archiving");

    archive_todo_session(label, task_index).unwrap();

    // The archive must live under the SESSION_DATA_DIR override.
    let data_dir = std::env::var("SESSION_DATA_DIR").unwrap();
    let archive =
        std::path::Path::new(&data_dir).join(format!("todo_loop_{}_{}.jsonl", task_index, label));
    assert!(
        archive.exists(),
        "todo archive must live under SESSION_DATA_DIR: {}",
        archive.display()
    );
    // And the original session file must be renamed away.
    assert!(
        !last.exists(),
        "last_session should be renamed to the archive"
    );
}
