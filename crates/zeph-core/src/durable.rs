// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Concrete cryptographic backing for the durable execution layer.
//!
//! `zeph-durable` defines the durable execution *contract* as a pure Layer-0 abstraction and
//! deliberately carries no cryptographic dependency (INV-1). This module supplies the concrete
//! [`XChaCha20Poly1305Cipher`] that satisfies [`zeph_durable::PayloadCipher`]. The binary
//! constructs it from the vault-resolved `ZEPH_DURABLE_KEY` and injects it into a backend as
//! `Option<Arc<dyn PayloadCipher>>`, exactly as a database pool is handed in.
//!
//! `XChaCha20-Poly1305` is chosen for its 192-bit extended nonce: a fresh random nonce per seal
//! (INV-7) has a negligible collision probability even across the lifetime of a long-lived key, so
//! no nonce-sequencing state has to be persisted.
//!
//! # Examples
//!
//! ```
//! use zeph_core::durable::XChaCha20Poly1305Cipher;
//! use zeph_durable::{ExecutionId, StepId, PayloadCipher};
//! use zeph_durable::cipher::{EntryKindTag, PayloadAad};
//!
//! let cipher = XChaCha20Poly1305Cipher::new(0, [7u8; 32]);
//! let aad = PayloadAad::new(ExecutionId::new(), StepId::new(0), EntryKindTag::StepResult, None);
//!
//! let sealed = cipher.seal(b"tool result", &aad).unwrap();
//! assert_eq!(cipher.open(&sealed, &aad).unwrap(), b"tool result");
//! ```

use chacha20poly1305::{
    Key, KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use zeph_durable::{CipherError, PayloadAad, PayloadCipher};
use zeroize::Zeroize;

/// `XChaCha20-Poly1305` key size, in bytes.
const KEY_LEN: usize = 32;
/// `XChaCha20` extended nonce size, in bytes.
const NONCE_LEN: usize = 24;
/// `Poly1305` authentication tag size, in bytes.
const TAG_LEN: usize = 16;
/// Length of the leading key-id selector byte.
const KEY_ID_LEN: usize = 1;
/// Offset one past the nonce, where the ciphertext begins.
const NONCE_END: usize = KEY_ID_LEN + NONCE_LEN;
/// Smallest valid sealed blob: `key_id || nonce || tag` (empty ciphertext).
const MIN_SEALED_LEN: usize = NONCE_END + TAG_LEN;

/// Failure constructing an [`XChaCha20Poly1305Cipher`] from raw vault bytes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CipherKeyError {
    /// The vault-resolved key was not exactly 32 bytes.
    #[error("durable cipher key must be {expected} bytes, got {actual}")]
    InvalidKeyLength {
        /// The required key length in bytes (32).
        expected: usize,
        /// The length of the supplied key material.
        actual: usize,
    },
}

/// One key registered with the cipher, addressed by its on-disk key-id byte.
struct KeySlot {
    key_id: u8,
    cipher: XChaCha20Poly1305,
}

impl KeySlot {
    /// Build a slot, copying the key into the AEAD state and zeroizing the transient input.
    fn new(key_id: u8, mut key: [u8; KEY_LEN]) -> Self {
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
        key.zeroize();
        Self { key_id, cipher }
    }
}

/// A vault-keyed `XChaCha20-Poly1305` [`PayloadCipher`] with a one-key rotation window.
///
/// The cipher holds a *current* key used for all seals, plus an optional *previous* key that
/// [`open`](PayloadCipher::open) can still select during a rotation window. The on-disk layout
/// `key_id(1) || nonce(24) || ciphertext || tag(16)` lets `open` pick the right key by its leading
/// byte; an unrecognized key-id fails closed with [`CipherError::UnknownKeyId`].
///
/// Key rotation is otherwise drain-based: see `book` vault documentation for the operational
/// policy. See [`zeph_durable::PayloadCipher`] for the full contract.
pub struct XChaCha20Poly1305Cipher {
    current: KeySlot,
    previous: Option<KeySlot>,
}

impl XChaCha20Poly1305Cipher {
    /// Construct a cipher with a single current key identified by `key_id`.
    ///
    /// The `key` array is zeroized once copied into the AEAD state.
    #[must_use]
    pub fn new(key_id: u8, key: [u8; KEY_LEN]) -> Self {
        Self {
            current: KeySlot::new(key_id, key),
            previous: None,
        }
    }

    /// Construct a cipher from vault-resolved key bytes, validating the length.
    ///
    /// # Errors
    ///
    /// Returns [`CipherKeyError::InvalidKeyLength`] when `key` is not exactly 32 bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_core::durable::XChaCha20Poly1305Cipher;
    ///
    /// assert!(XChaCha20Poly1305Cipher::from_vault_bytes(0, &[0u8; 32]).is_ok());
    /// assert!(XChaCha20Poly1305Cipher::from_vault_bytes(0, b"too short").is_err());
    /// ```
    pub fn from_vault_bytes(key_id: u8, key: &[u8]) -> Result<Self, CipherKeyError> {
        let array: [u8; KEY_LEN] =
            key.try_into()
                .map_err(|_| CipherKeyError::InvalidKeyLength {
                    expected: KEY_LEN,
                    actual: key.len(),
                })?;
        Ok(Self::new(key_id, array))
    }

    /// Register a previous key for the rotation window.
    ///
    /// `open` will select this key for blobs whose leading key-id byte matches `key_id`; `seal`
    /// always uses the current key. Use this so in-flight executions sealed under the old key can
    /// still be replayed after a rotation.
    #[must_use]
    pub fn with_previous(mut self, key_id: u8, key: [u8; KEY_LEN]) -> Self {
        self.previous = Some(KeySlot::new(key_id, key));
        self
    }

    /// Select the AEAD state for a given on-disk key-id.
    fn select(&self, key_id: u8) -> Option<&XChaCha20Poly1305> {
        if key_id == self.current.key_id {
            Some(&self.current.cipher)
        } else {
            self.previous
                .as_ref()
                .filter(|slot| slot.key_id == key_id)
                .map(|slot| &slot.cipher)
        }
    }
}

impl PayloadCipher for XChaCha20Poly1305Cipher {
    fn seal(&self, plaintext: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
        let aad_bytes = aad.canonical_bytes();
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .current
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad_bytes,
                },
            )
            .map_err(|_| CipherError::Authentication)?;

        let mut blob = Vec::with_capacity(KEY_ID_LEN + NONCE_LEN + ciphertext.len());
        blob.push(self.current.key_id);
        blob.extend_from_slice(nonce.as_slice());
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    fn open(&self, sealed: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
        if sealed.len() < MIN_SEALED_LEN {
            return Err(CipherError::Malformed {
                context: "sealed blob shorter than key-id + nonce + tag",
            });
        }
        let key_id = sealed[0];
        let cipher = self
            .select(key_id)
            .ok_or(CipherError::UnknownKeyId { key_id })?;

        let nonce = XNonce::from_slice(&sealed[KEY_ID_LEN..NONCE_END]);
        let ciphertext = &sealed[NONCE_END..];
        let aad_bytes = aad.canonical_bytes();

        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad_bytes,
                },
            )
            .map_err(|_| CipherError::Authentication)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use zeph_durable::cipher::EntryKindTag;
    use zeph_durable::{DurableError, ExecutionId, StepId};

    use super::*;

    fn aad_for(exec: ExecutionId, step: u32) -> PayloadAad {
        PayloadAad::new(exec, StepId::new(step), EntryKindTag::StepResult, None)
    }

    #[test]
    fn seal_open_round_trip() {
        let cipher = XChaCha20Poly1305Cipher::new(0, [1u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        for plaintext in [
            b"".as_slice(),
            b"x",
            b"a longer journaled tool result payload",
        ] {
            let sealed = cipher.seal(plaintext, &aad).unwrap();
            assert_eq!(cipher.open(&sealed, &aad).unwrap(), plaintext);
        }
    }

    #[test]
    fn sealed_blob_uses_key_id_nonce_tag_layout() {
        let cipher = XChaCha20Poly1305Cipher::new(3, [2u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        let sealed = cipher.seal(b"", &aad).unwrap();
        // key-id byte, then 24-byte nonce, then a 16-byte tag for empty plaintext.
        assert_eq!(sealed.len(), KEY_ID_LEN + NONCE_LEN + TAG_LEN);
        assert_eq!(sealed[0], 3, "leading byte is the current key-id");
    }

    #[test]
    fn nonce_is_fresh_per_seal() {
        let cipher = XChaCha20Poly1305Cipher::new(0, [9u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        let a = cipher.seal(b"same", &aad).unwrap();
        let b = cipher.seal(b"same", &aad).unwrap();
        // Identical plaintext + identical AAD must still yield distinct nonces (and ciphertext).
        assert_ne!(a[KEY_ID_LEN..NONCE_END], b[KEY_ID_LEN..NONCE_END]);
        assert_ne!(a, b);
    }

    // NFR-DE-06: a CSPRNG nonce of 192 bits must not repeat across 10^6 seals.
    #[test]
    fn one_million_seals_produce_distinct_nonces() {
        const SEALS: usize = 1_000_000;
        let cipher = XChaCha20Poly1305Cipher::new(0, [4u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        let mut nonces: HashSet<[u8; NONCE_LEN]> = HashSet::with_capacity(SEALS);
        for _ in 0..SEALS {
            let sealed = cipher.seal(b"", &aad).unwrap();
            let mut nonce = [0u8; NONCE_LEN];
            nonce.copy_from_slice(&sealed[KEY_ID_LEN..NONCE_END]);
            assert!(nonces.insert(nonce), "nonce reuse detected");
        }
        assert_eq!(nonces.len(), SEALS);
    }

    #[test]
    fn open_under_different_step_fails_replay_integrity() {
        let cipher = XChaCha20Poly1305Cipher::new(0, [5u8; 32]);
        let exec = ExecutionId::new();
        let sealed = cipher.seal(b"result", &aad_for(exec, 7)).unwrap();

        let err = cipher.open(&sealed, &aad_for(exec, 8)).unwrap_err();
        assert!(matches!(err, CipherError::Authentication));
        assert!(matches!(
            DurableError::from(err),
            DurableError::ReplayIntegrity
        ));
    }

    #[test]
    fn open_under_different_execution_fails_replay_integrity() {
        let cipher = XChaCha20Poly1305Cipher::new(0, [6u8; 32]);
        let sealed = cipher
            .seal(b"result", &aad_for(ExecutionId::new(), 0))
            .unwrap();

        let err = cipher
            .open(&sealed, &aad_for(ExecutionId::new(), 0))
            .unwrap_err();
        assert!(matches!(
            DurableError::from(err),
            DurableError::ReplayIntegrity
        ));
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let cipher = XChaCha20Poly1305Cipher::new(0, [7u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        let mut sealed = cipher.seal(b"result", &aad).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        assert!(matches!(
            cipher.open(&sealed, &aad).unwrap_err(),
            CipherError::Authentication
        ));
    }

    #[test]
    fn short_blob_is_malformed() {
        let cipher = XChaCha20Poly1305Cipher::new(0, [0u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        let err = cipher.open(&[0u8; MIN_SEALED_LEN - 1], &aad).unwrap_err();
        assert!(matches!(err, CipherError::Malformed { .. }));
        assert!(matches!(
            DurableError::from(err),
            DurableError::Decode { .. }
        ));
    }

    #[test]
    fn unknown_key_id_fails_closed() {
        let cipher = XChaCha20Poly1305Cipher::new(0, [1u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        let mut sealed = cipher.seal(b"x", &aad).unwrap();
        sealed[0] = 200; // no key registered under id 200
        assert!(matches!(
            cipher.open(&sealed, &aad).unwrap_err(),
            CipherError::UnknownKeyId { key_id: 200 }
        ));
    }

    #[test]
    fn previous_key_opens_during_rotation_window() {
        // Seal under the old key (id 0), then rotate: current is id 1, previous is id 0.
        let old = XChaCha20Poly1305Cipher::new(0, [1u8; 32]);
        let aad = aad_for(ExecutionId::new(), 0);
        let sealed = old.seal(b"in-flight", &aad).unwrap();

        let rotated = XChaCha20Poly1305Cipher::new(1, [2u8; 32]).with_previous(0, [1u8; 32]);
        // The old blob still opens via the previous key...
        assert_eq!(rotated.open(&sealed, &aad).unwrap(), b"in-flight");
        // ...while new seals use the current key-id.
        assert_eq!(rotated.seal(b"new", &aad).unwrap()[0], 1);
    }

    #[test]
    fn from_vault_bytes_validates_length() {
        assert!(XChaCha20Poly1305Cipher::from_vault_bytes(0, &[0u8; 32]).is_ok());
        // The cipher deliberately does not implement `Debug` (it holds key material), so match on
        // the `Result` directly rather than calling `unwrap_err`.
        assert!(matches!(
            XChaCha20Poly1305Cipher::from_vault_bytes(0, b"short"),
            Err(CipherKeyError::InvalidKeyLength {
                expected: 32,
                actual: 5
            })
        ));
    }
}
