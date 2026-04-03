// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hash utilities: BLAKE3 hex digests and fast SipHash-based u64 hashing.

use std::hash::{DefaultHasher, Hash, Hasher};

/// Returns the BLAKE3 hex digest of arbitrary bytes.
#[must_use]
pub fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Returns the BLAKE3 hex digest of a UTF-8 string.
#[must_use]
pub fn blake3_hex_str(s: &str) -> String {
    blake3_hex(s.as_bytes())
}

/// Returns a fast non-cryptographic `u64` hash of a string (`SipHash` via [`DefaultHasher`]).
#[must_use]
pub fn fast_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_hex_is_64_chars() {
        let h = blake3_hex(b"hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn blake3_hex_deterministic() {
        assert_eq!(blake3_hex(b"foo"), blake3_hex(b"foo"));
    }

    #[test]
    fn blake3_hex_str_matches_bytes() {
        assert_eq!(blake3_hex_str("hello"), blake3_hex(b"hello"));
    }

    #[test]
    fn fast_hash_deterministic() {
        assert_eq!(fast_hash("abc"), fast_hash("abc"));
    }

    #[test]
    fn fast_hash_different_inputs_differ() {
        assert_ne!(fast_hash("abc"), fast_hash("def"));
    }

    #[test]
    fn fast_hash_empty() {
        let _ = fast_hash(""); // must not panic
    }
}
