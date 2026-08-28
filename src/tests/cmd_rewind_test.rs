//! Tests for `src/cmd.rs` `/rewind` (and its interaction with persistence/`/restore`).
//!
//! Coverage strategy:
//! - In-process tests cover target validation, truncation boundaries, the
//!   no-confirmation paths (rewind discarding only a broken in-progress turn),
//!   turn-counter bookkeeping, and the slash-command dispatch.
//! - The confirmation prompt (`Proceed? (y/n)`) reads from real stdin, which a
//!   test harness cannot inject in-process. Those flows are exercised by
//!   `#[ignore]`d helper tests that are spawned by a parent test as a
//!   subprocess with piped stdin (`AGT_REWIND_CHILD=1`); `cargo test -- --ignored`
//!   alone skips the helpers via the same env guard, so they can never hang.

use std::io::Write;
use std::path::PathBuf;

use super::*;
use crate::model::Message;
use crate::persistence;

/// Build a message with a role/content; the rest is defaulted.
fn msg(role: &str, content: &str) -> Message {
    Message {
        role: role.to_string(),
        content: content.to_string(),
        ..Default::default()
    }
}

/// Build `[system, t1(user,assistant), t2(user,assistant), ...]` with `turns`
/// completed turns - the same shape `run_reasoning_loop` produces.
fn history(turns: usize) -> Vec<Message> {
    let mut v = vec![msg("system", "sys")];
    for i in 1..=turns {
        v.push(msg("user", &format!("q{}", i)));
        v.push(msg("assistant", &format!("a{}", i)));
    }
    v
}

/// Collapse messages to (role, content) pairs for equality (Message: !PartialEq).
fn pair(msgs: &[Message]) -> Vec<(String, String)> {
    msgs.iter()
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect()
}

fn user_count(msgs: &[Message]) -> usize {
    msgs.iter().filter(|m| m.role == "user").count()
}

// ---------------------------------------------------------------------------
// Target validation / error paths (no confirmation involved)
// ---------------------------------------------------------------------------

#[test]
fn rewind_validates_target_input_and_state() {
    let mut msgs = history(3);
    let before = pair(&msgs);

    // Missing argument.
    assert!(handle_rewind(None, &mut msgs, 4).is_err());
    // Non-numeric / malformed.
    assert!(handle_rewind(Some("abc"), &mut msgs, 4).is_err());
    assert!(handle_rewind(Some("2.5"), &mut msgs, 4).is_err());
    assert!(handle_rewind(Some("1 "), &mut msgs, 4).is_err());
    assert!(handle_rewind(Some(""), &mut msgs, 4).is_err());
    // Out of range.
    assert!(handle_rewind(Some("0"), &mut msgs, 4).is_err());
    assert!(handle_rewind(Some("-1"), &mut msgs, 4).is_err());
    // Target must be strictly less than the current turn.
    assert!(handle_rewind(Some("4"), &mut msgs, 4).is_err());
    assert!(handle_rewind(Some("5"), &mut msgs, 4).is_err());

    // Failed attempts must not mutate the conversation.
    assert_eq!(pair(&msgs), before);
    assert_eq!(msgs.len(), 7);
}

#[test]
fn rewind_errors_on_fresh_session() {
    // Current turn 1: no completed turns, nothing to rewind to.
    let mut msgs = history(0);
    assert!(handle_rewind(Some("1"), &mut msgs, 1).is_err());
    assert_eq!(msgs.len(), 1);
}

// ---------------------------------------------------------------------------
// Truncation correctness (no-confirmation paths)
// ---------------------------------------------------------------------------

#[test]
fn rewind_truncates_broken_in_progress_turn_without_confirmation() {
    // 2 completed turns + a broken turn 3 (user message pushed, LLM errored
    // before any assistant message): rewind to 2 has nothing *completed* to
    // discard (discarded_start 3 > discarded_end 2), so no confirm block runs,
    // but the broken turn's user message must still be truncated away.
    let mut msgs = history(2);
    msgs.push(msg("user", "q3_broken"));
    assert_eq!(msgs.len(), 6);

    let target = handle_rewind(Some("2"), &mut msgs, 3).unwrap();
    assert_eq!(target, 2);
    assert_eq!(pair(&msgs), pair(&history(2)));
    assert_eq!(user_count(&msgs), 2);

    // main.rs bookkeeping: RewoundTo(2) -> turn = 3; the next input must push
    // a NEW user message (user_msg_count 2 < turn 3), not reuse the last one.
    assert!(user_count(&msgs) < 3);
}

#[test]
fn rewind_to_last_completed_turn_is_noop_when_idle() {
    // 3 completed turns, idle at turn 4 (no turn-4 user message yet):
    // rewind to 3 neither prompts (nothing completed to discard) nor truncates
    // (there is no 4th user message to cut at). Documented current behavior:
    // it is a well-behaved no-op that still reports "Rewound to Turn 3".
    let mut msgs = history(3);
    let target = handle_rewind(Some("3"), &mut msgs, 4).unwrap();
    assert_eq!(target, 3);
    assert_eq!(msgs.len(), 7);
    assert_eq!(user_count(&msgs), 3);
}

#[test]
fn rewind_then_resume_keeps_turn_counter_in_sync() {
    // 3 completed turns + broken turn 4 in progress; rewind to 3 (the broken
    // turn only -> discarded_start 4 > discarded_end 3, so no confirmation).
    let mut msgs = history(3);
    msgs.push(msg("user", "q4_broken"));

    let target = handle_rewind(Some("3"), &mut msgs, 4).unwrap();
    assert_eq!(target, 3);
    assert_eq!(pair(&msgs), pair(&history(3)));

    // main.rs: session.turn = target + 1.
    let mut turn = target + 1;
    assert_eq!(user_count(&msgs) as i32, turn - 1);

    // Simulate run_reasoning_loop restarting a normal turn for the resumed
    // conversation: user_msg_count(3) < turn(4) -> push new user message,
    // complete -> main.rs increments the counter. Repeat once.
    for q in ["q4_new", "q5_new"] {
        assert!(
            user_count(&msgs) < turn as usize,
            "loop must push a new user message ({} < {})",
            user_count(&msgs),
            turn
        );
        msgs.push(msg("user", q));
        msgs.push(msg("assistant", &format!("a_{}", q)));
        turn += 1;
    }

    assert_eq!(turn, 6);
    assert_eq!(user_count(&msgs), 5);
    assert_eq!(
        pair(&msgs),
        vec![
            ("system".into(), "sys".into()),
            ("user".into(), "q1".into()),
            ("assistant".into(), "a1".into()),
            ("user".into(), "q2".into()),
            ("assistant".into(), "a2".into()),
            ("user".into(), "q3".into()),
            ("assistant".into(), "a3".into()),
            ("user".into(), "q4_new".into()),
            ("assistant".into(), "a_q4_new".into()),
            ("user".into(), "q5_new".into()),
            ("assistant".into(), "a_q5_new".into()),
        ]
    );
}

// ---------------------------------------------------------------------------
// Slash-command dispatch wiring
// ---------------------------------------------------------------------------

#[test]
fn slash_dispatch_maps_rewind_to_rewoundto_and_truncates_session() {
    use clap::Parser;

    let config = crate::startup::Config::try_parse_from(["agt"]).unwrap();
    let mut session = Session {
        label: "dispatch".to_string(),
        // 3 completed turns + broken turn 4 (no-confirmation state).
        messages: {
            let mut m = history(3);
            m.push(msg("user", "q4_broken"));
            m
        },
        turn: 4,
    };
    let mut settings = Settings::from_config(&config);

    let result = try_handle_slash_command(
        "/rewind 3",
        &mut session,
        &mut settings,
        &Metrics::default(),
    )
    .expect("valid rewind must dispatch");

    assert_eq!(result, SlashCmdResult::RewoundTo(3));
    // messages truncated in place, turn counter updated exactly like main.rs:
    assert_eq!(pair(&session.messages), pair(&history(3)));
    session.turn = 3 + 1;
    assert_eq!(session.turn, 4);
    assert_eq!(user_count(&session.messages), 3);
}

// ---------------------------------------------------------------------------
// Subprocess-driven flows (confirmation reads real stdin)
// ---------------------------------------------------------------------------

/// Spawn this test binary as a child running only the ignored helper test
/// `test_name`, feed `stdin_data` to its confirmation prompt(s), and assert
/// the child passed. The env guard keeps the helper meaningful only when
/// spawned here (it would otherwise hang on a real terminal).
fn spawn_child(test_name: &str, stdin_data: &[u8]) {
    let exe = std::env::current_exe().expect("current test executable");
    let mut child = std::process::Command::new(exe)
        .arg("--ignored")
        .arg(test_name)
        .arg("--nocapture")
        .env("AGT_REWIND_CHILD", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn child test process");
    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        stdin.write_all(stdin_data).expect("write child stdin data");
    }
    // Close stdin so the child sees EOF after its last read.
    drop(child.stdin.take());
    let status = child.wait().expect("wait for child test process");
    assert!(
        status.success(),
        "child test '{}' failed with {:?}",
        test_name,
        status
    );
}

#[test]
#[ignore]
fn helper_rewind_discard_completed_turns() {
    if std::env::var("AGT_REWIND_CHILD").as_deref() != Ok("1") {
        return; // only meaningful when spawned by its parent test
    }
    // Normal flow: 4 completed turns, idle at turn 5, rewind to 1 confirmed.
    let mut msgs = history(4);
    let target = handle_rewind(Some("1"), &mut msgs, 5).unwrap();
    assert_eq!(target, 1);
    // Everything from turn 2 onward is gone; turn 1 exactly preserved.
    assert_eq!(pair(&msgs), pair(&history(1)));
    assert_eq!(user_count(&msgs), 1);
}

#[test]
fn rewind_confirmed_discard_completed_turns() {
    spawn_child("helper_rewind_discard_completed_turns", b"y\n");
}

#[test]
#[ignore]
fn helper_rewind_cancel_keeps_history() {
    if std::env::var("AGT_REWIND_CHILD").as_deref() != Ok("1") {
        return; // only meaningful when spawned by its parent test
    }
    let mut msgs = history(4);
    let before = pair(&msgs);
    // Answering "n" must abort without touching the conversation.
    let err = handle_rewind(Some("2"), &mut msgs, 5).unwrap_err();
    let text = format!("{}", err);
    assert!(
        text.contains("cancelled"),
        "expected cancellation error, got: {}",
        text
    );
    assert_eq!(pair(&msgs), before);
}

#[test]
fn rewind_cancel_confirmed_keeps_history() {
    spawn_child("helper_rewind_cancel_keeps_history", b"n\n");
}

/// Full rewind -> continue -> exit -> restart -> /restore flow.
///
/// Proves the persistence gap: `/rewind` truncates only the in-memory
/// conversation; `last_session_{label}.jsonl` keeps the discarded turns, and a
/// later `/restore` hands back a Frankenstein history (old turn 2 AND the
/// redone turn 2). It also records the crash-mid-turn vs restore turn-count
/// inconsistency in the final phase.
#[test]
#[ignore]
fn helper_rewind_restore_flow_gap() {
    if std::env::var("AGT_REWIND_CHILD").as_deref() != Ok("1") {
        return; // only meaningful when spawned by its parent test
    }
    // The child runs alone, so a process-global SESSION_DATA_DIR override is safe.
    let dir = std::env::temp_dir().join(format!(
        "agt_rewind_restore_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    unsafe { std::env::set_var("SESSION_DATA_DIR", &dir) };

    fn cleanup(dir: &PathBuf) {
        unsafe { std::env::remove_var("SESSION_DATA_DIR") };
        let _ = std::fs::remove_dir_all(dir);
    }

    let label = "rw_flow";
    // Phase 1: session with turns 1-2 persisted to disk.
    let start = history(2);
    for m in &start {
        persistence::save_message(label, m).unwrap();
    }

    // Phase 2: confirmed rewind to turn 1 (reads "y\n" from piped stdin).
    let mut msgs = start.clone();
    let target = handle_rewind(Some("1"), &mut msgs, 3).unwrap();
    assert_eq!(target, 1);
    assert_eq!(pair(&msgs), pair(&history(1)));

    // Phase 3: continue after the rewind (turn 2 redone, then turn 3).
    for m in [
        msg("user", "q2_new"),
        msg("assistant", "a2_new"),
        msg("user", "q3_new"),
        msg("assistant", "a3_new"),
    ] {
        persistence::save_message(label, &m).unwrap();
    }

    // Phase 4: exit & restart -> init_session moves last_session -> previous.
    persistence::init_session(label).unwrap();

    // Phase 5: /restore semantics -> the whole archived file comes back,
    // including the turns the rewind discarded.
    let restored = persistence::restore_previous_session(label).unwrap();
    let restored_pairs = pair(&restored);
    assert_eq!(
        restored_pairs,
        vec![
            ("system".into(), "sys".into()),
            ("user".into(), "q1".into()),
            ("assistant".into(), "a1".into()),
            ("user".into(), "q2".into()),
            ("assistant".into(), "a2".into()),
            ("user".into(), "q2_new".into()),
            ("assistant".into(), "a2_new".into()),
            ("user".into(), "q3_new".into()),
            ("assistant".into(), "a3_new".into()),
        ],
        "the discarded pre-rewind turns must still be in the restored file \
         (rewind is in-memory only)"
    );
    // handle_restore would report 4 user messages -> turn 5, even though the
    // rewound conversation only ever had 3 turns (1 + redone 2 + 3).
    let restored_turns = restored.iter().filter(|m| m.role == "user").count();
    assert_eq!(restored_turns, 4);

    cleanup(&dir);
}

#[test]
fn rewind_restore_flow_shows_disk_gap() {
    spawn_child("helper_rewind_restore_flow_gap", b"y\n");
}
