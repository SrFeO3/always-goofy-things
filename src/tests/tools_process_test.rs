use super::*;

#[tokio::test]
async fn test_read_capped_keeps_tail() {
    let state = Arc::new(Mutex::new(CaptureState::with_capacity(5)));
    read_capped(&b"0123456789"[..], 5, state.clone())
        .await
        .unwrap();
    // Take the assertions in a block so the guard drops before the next
    // `read_capped(...).await` (clippy::await_holding_lock).
    {
        let s = state.lock().unwrap();
        assert_eq!(s.buf.iter().copied().collect::<Vec<_>>(), b"56789");
        assert!(s.truncated, "cap 5 < 10 bytes must be flagged");
        assert_eq!(s.omitted, 5);
    }

    let state = Arc::new(Mutex::new(CaptureState::with_capacity(100)));
    read_capped(&b"short"[..], 100, state.clone())
        .await
        .unwrap();
    let s = state.lock().unwrap();
    assert_eq!(s.buf.iter().copied().collect::<Vec<_>>(), b"short");
    assert!(!s.truncated);
    assert_eq!(s.omitted, 0);
}

#[tokio::test]
async fn test_read_capped_zero_cap_is_unlimited() {
    // cap = 0 means unlimited (mirrors --tool-timeout-secs 0 = unlimited):
    // every byte is kept and nothing is flagged as truncated.
    let state = Arc::new(Mutex::new(CaptureState::with_capacity(0)));
    read_capped(&b"0123456789"[..], 0, state.clone())
        .await
        .unwrap();
    let s = state.lock().unwrap();
    assert_eq!(s.buf.iter().copied().collect::<Vec<_>>(), b"0123456789");
    assert!(!s.truncated);
    assert_eq!(s.omitted, 0);
}

#[tokio::test]
async fn test_run_captured_basic() {
    let out = run_captured("bash", &["-c", "echo hello"], "BASH")
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("hello"));
    assert_eq!(out.signal, None);
    assert!(!out.stdout_truncated);
    assert!(!out.stderr_truncated);
    assert_eq!(out.stdout_omitted, 0);
}

#[tokio::test]
async fn test_run_captured_drain_abort_keeps_partial_output() {
    // bash exits immediately but a background grandchild keeps stdout open:
    // the drain grace must abort and still surface the bytes read so far.
    let out = run_captured("bash", &["-c", "sleep 5 & echo hello"], "BASH")
        .await
        .unwrap();
    assert!(
        out.stdout.contains("hello"),
        "partial output must survive the drain abort, got: {:?}",
        out.stdout
    );
    assert!(out.stdout_truncated);
}
