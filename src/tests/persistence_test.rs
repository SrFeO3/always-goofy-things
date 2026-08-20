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
        save_message(label, m).unwrap();
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

#[test]
fn test_stats_round_trip() {
    let _g = PERSIST_LOCK.lock().unwrap();
    let _d = TestDir::new();
    let label = "stats_test";

    let rec = LlmCallRecord {
        timestamp: chrono::Utc::now(),
        model: "gpt-4o".to_string(),
        provider: LlmProvider::OpenAi,
        phase: "main".to_string(),
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
    assert_eq!(l.phase, "main");
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
            phase: "main".to_string(),
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
    save_message(
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
