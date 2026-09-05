//! Tests for `src/model.rs` (session ID generation and message stamping).

use super::*;

#[test]
fn test_session_new_generates_unique_and_immutable_id() {
    let sys = Message {
        role: "system".to_string(),
        content: "sys".to_string(),
        ..Default::default()
    };

    let a = Session::new("default".to_string(), sys.clone());
    let b = Session::new("default".to_string(), sys);

    assert_ne!(a.id, b.id, "sessions sharing a label must not share an ID");
    assert_eq!(a.messages[0].session_id, a.id);
    assert_eq!(b.messages[0].session_id, b.id);
}

#[test]
fn test_session_push_message_stamps_id() {
    let sys = Message {
        role: "system".to_string(),
        content: "sys".to_string(),
        ..Default::default()
    };
    let mut s = Session::new("default".to_string(), sys);
    let sid = s.id.clone();
    let m = s.push_message(Message {
        role: "user".to_string(),
        content: "hi".to_string(),
        ..Default::default()
    });
    assert_eq!(m.session_id, sid);
    assert_eq!(s.messages.last().unwrap().session_id, s.id);
}
