// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Returns BLAKE3 hex digest of arbitrary bytes.
#[must_use]
pub fn content_hash(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_known_value() {
        // BLAKE3 of "hello" is deterministic
        let hash = content_hash(b"hello");
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, blake3::hash(b"hello").to_hex().to_string());
    }

    #[test]
    fn test_content_hash_empty_input() {
        let hash = content_hash(b"");
        assert_eq!(hash.len(), 64);
        // Empty input produces consistent output
        assert_eq!(hash, content_hash(b""));
    }

    #[test]
    fn test_content_hash_unicode() {
        let input = "こんにちは世界".as_bytes();
        let hash = content_hash(input);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, content_hash(input));
    }

    #[test]
    fn test_content_hash_large_input() {
        let data = vec![0xABu8; 1024 * 1024]; // 1 MiB
        let hash = content_hash(&data);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, content_hash(&data));
    }

    #[test]
    fn test_content_hash_different_inputs_differ() {
        let h1 = content_hash(b"foo");
        let h2 = content_hash(b"bar");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_content_hash_output_is_hex() {
        let hash = content_hash(b"test");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
