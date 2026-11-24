//! Stable hashing for persisted identifiers.
//!
//! `std`'s `DefaultHasher` is explicitly allowed to change between Rust
//! releases, so job ids and generated plan names derived from it can move
//! after a toolchain upgrade. SipHash-1-3 with fixed keys is deterministic by
//! definition; the `siphasher` crate exposes it directly.

use std::hash::{Hash, Hasher};

use siphasher::sip::SipHasher13;

/// Deterministic hash of any `Hash` value (fixed keys, no per-process seed).
pub(crate) fn stable_hash<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut h = SipHasher13::new_with_keys(0, 0);
    value.hash(&mut h);
    h.finish()
}

#[cfg(test)]
#[path = "tests/stable_hash_test.rs"]
mod tests;
