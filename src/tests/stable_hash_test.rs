//! Tests for `src/stable_hash.rs`: fixed vectors and unambiguous framing.

use crate::stable_hash::stable_hash;

#[test]
fn stable_hash_is_fixed_across_runs_and_fields() {
    // Golden vectors lock the algorithm (SipHash-1-3, keys 0/0, std `Hash`).
    assert_eq!(stable_hash(""), 3476900567878811119);
    assert_eq!(stable_hash("a"), 8186225505942432243);
    // Fields are framed by `Hash` (lengths/terminators), so these differ.
    assert_ne!(stable_hash(&("ab", "c")), stable_hash(&("a", "bc")));
}

#[test]
fn stable_hash_is_deterministic() {
    let v = ("goal", vec!["t1", "desc"], vec!["nonempty artifacts/a.md"]);
    assert_eq!(stable_hash(&v), stable_hash(&v));
}
