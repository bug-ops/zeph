// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ECDSA request signer for the Gonka AI gateway protocol.
//!
//! `RequestSigner` holds a secp256k1 signing key and produces deterministic
//! base64-encoded signatures for Gonka HTTP requests. Signatures bind the
//! request body, a nanosecond timestamp, and the target transfer address so
//! that each request is non-replayable.
//!
//! # Examples
//!
//! ```rust,no_run
//! use zeph_llm::gonka::RequestSigner;
//!
//! # fn example() -> Result<(), zeph_llm::LlmError> {
//! let signer = RequestSigner::from_hex(
//!     "0000000000000000000000000000000000000000000000000000000000000001",
//!     "gonka",
//! )?;
//! println!("address: {}", signer.address());
//! let sig = signer.sign(b"hello", 1_000_000_000u128, "gonka1test")?;
//! println!("signature: {sig}");
//! # Ok(())
//! # }
//! ```

use base64::Engine as _;
use k256::ecdsa::signature::hazmat::PrehashSigner as _;
use k256::elliptic_curve::sec1::ToEncodedPoint as _;
use ripemd::Digest as _;
use zeroize::Zeroizing;

use crate::error::LlmError;

/// Gonka request signer backed by a secp256k1 private key.
///
/// Derives a bech32 address from the compressed public key (Bitcoin-style
/// SHA-256 → RIPEMD-160 hash) and produces RFC6979-deterministic ECDSA
/// signatures over a SHA-256 prehash of each request.
pub struct RequestSigner {
    signing_key: k256::ecdsa::SigningKey,
    address: String,
}

impl std::fmt::Debug for RequestSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestSigner")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl RequestSigner {
    /// Constructs a signer from a 32-byte private key encoded as lowercase hex.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Other`] if `priv_hex` is not valid hex or does not
    /// decode to a valid secp256k1 scalar.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use zeph_llm::gonka::RequestSigner;
    /// # fn example() -> Result<(), zeph_llm::LlmError> {
    /// let signer = RequestSigner::from_hex(
    ///     "0000000000000000000000000000000000000000000000000000000000000001",
    ///     "gonka",
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_hex(priv_hex: &str, chain_prefix: &str) -> Result<Self, LlmError> {
        let key_bytes: Zeroizing<Vec<u8>> = Zeroizing::new(
            hex::decode(priv_hex).map_err(|e| LlmError::Other(format!("invalid hex key: {e}")))?,
        );
        let signing_key = k256::ecdsa::SigningKey::from_slice(&key_bytes)
            .map_err(|e| LlmError::Other(format!("invalid secp256k1 key: {e}")))?;
        let pubkey = k256::PublicKey::from(signing_key.verifying_key());
        let address = derive_address(&pubkey, chain_prefix);
        Ok(Self {
            signing_key,
            address,
        })
    }

    /// Returns the bech32 address derived from this signer's public key.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Signs a Gonka request and returns a base64-encoded (STANDARD, with padding) signature.
    ///
    /// The signing input is:
    /// ```text
    /// SHA-256( hex(SHA-256(body)) || decimal(timestamp_ns) || transfer_address )
    /// ```
    ///
    /// This binds the body content, the timestamp, and the destination address
    /// into a single prehash that is then signed with RFC6979-deterministic ECDSA.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Other`] if the underlying ECDSA signing operation fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use zeph_llm::gonka::RequestSigner;
    /// # fn example() -> Result<(), zeph_llm::LlmError> {
    /// let signer = RequestSigner::from_hex(
    ///     "0000000000000000000000000000000000000000000000000000000000000001",
    ///     "gonka",
    /// )?;
    /// let sig = signer.sign(b"hello", 1_000_000_000u128, "gonka1test")?;
    /// assert_eq!(sig.len(), 88); // base64 STANDARD encoding of 64 bytes
    /// # Ok(())
    /// # }
    /// ```
    pub fn sign(
        &self,
        body_bytes: &[u8],
        timestamp_ns: u128,
        transfer_address: &str,
    ) -> Result<String, LlmError> {
        let _span =
            tracing::info_span!("llm.gonka.sign", transfer_address = %transfer_address).entered();
        let payload_hash_hex = hex::encode(sha2::Sha256::digest(body_bytes));
        let input = format!("{payload_hash_hex}{timestamp_ns}{transfer_address}");
        let digest = sha2::Sha256::digest(input.as_bytes());
        let sig: k256::ecdsa::Signature = self
            .signing_key
            .sign_prehash(digest.as_ref())
            .map_err(|e| LlmError::Other(format!("signing failed: {e}")))?;
        // Use STANDARD (with padding) as specified in the Gonka gateway protocol.
        // Note: the spec document says STANDARD_NO_PAD (86 chars) but the issue
        // description and confirmed test vectors use STANDARD (88 chars). The issue
        // overrides the spec here.
        Ok(base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()))
    }
}

/// Derives a bech32 address from a secp256k1 public key.
///
/// Algorithm: SHA-256(compressed_pubkey) → RIPEMD-160 → `bech32(chain_prefix, data)`.
/// This matches the Bitcoin/Cosmos address derivation standard.
fn derive_address(pubkey: &k256::PublicKey, chain_prefix: &str) -> String {
    use bech32::ToBase32 as _;

    let compressed = pubkey.to_encoded_point(true);
    let sha_hash = sha2::Sha256::digest(compressed.as_bytes());
    let sha_bytes: &[u8] = &sha_hash[..];
    let ripe_hash = ripemd::Ripemd160::digest(sha_bytes);
    let ripe_bytes: &[u8] = &ripe_hash[..];
    let data5 = ripe_bytes.to_base32();
    // bech32::encode only fails on invalid HRP characters, which chain_prefix
    // is guaranteed to avoid in practice (lowercase ASCII letters only).
    bech32::encode(chain_prefix, data5, bech32::Variant::Bech32)
        .unwrap_or_else(|e| panic!("bech32 encode failed for prefix '{chain_prefix}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIV_KEY_1: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const PRIV_KEY_N_MINUS_1: &str =
        "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140";

    #[test]
    fn address_key_1_matches_fixture() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        assert_eq!(
            signer.address(),
            "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6"
        );
    }

    #[test]
    fn address_key_n_minus_1_matches_fixture() {
        let signer = RequestSigner::from_hex(PRIV_KEY_N_MINUS_1, "gonka").unwrap();
        assert_eq!(
            signer.address(),
            "gonka14h0ycu78h88wzldxc7e79vhw5xsde0n85evmum"
        );
    }

    #[test]
    fn sign_known_vector_1() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        let sig = signer
            .sign(b"hello", 1_000_000_000u128, "gonka1test")
            .unwrap();
        assert_eq!(
            sig,
            "/x6JuvqXWpT9YNgjYt0eNLxK8nDjccY/VyJrDn4bGjNbWWu3Px9doIlUQUOOf2Eu7SqyZ4oyGlDoY+4XpGA2JQ=="
        );
    }

    #[test]
    fn sign_known_vector_empty_body() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        let sig = signer.sign(b"", 0u128, "").unwrap();
        assert_eq!(
            sig,
            "NyKMAuRc/FRjptcjm94Q3Fqeevcl2fJUeb0Dxwh1HZpH7sgFk7ajJdBPt8FVa1mxG5OY623oKNb6xkGerdqIiw=="
        );
    }

    #[test]
    fn sign_known_vector_key_n_minus_1() {
        let signer = RequestSigner::from_hex(PRIV_KEY_N_MINUS_1, "gonka").unwrap();
        let sig = signer
            .sign(b"hello", 1_000_000_000u128, "gonka1test")
            .unwrap();
        assert_eq!(
            sig,
            "jtbBiX1nUgLiIH7FjLxy7Nn1Ckp3jq+8t6iLNjCKliJPXKOGHoo997xqbo3R9FNsTK8TmCyQW3PPvRJQFKhRXg=="
        );
    }

    #[test]
    fn sign_returns_88_char_base64() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        let sig = signer.sign(b"test", 42u128, "gonka1addr").unwrap();
        assert_eq!(sig.len(), 88);
    }

    #[test]
    fn sign_is_deterministic() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        let sig1 = signer.sign(b"data", 999u128, "gonka1addr").unwrap();
        let sig2 = signer.sign(b"data", 999u128, "gonka1addr").unwrap();
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn sign_different_inputs_produce_different_signatures() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        let sig1 = signer.sign(b"a", 1u128, "addr").unwrap();
        let sig2 = signer.sign(b"b", 1u128, "addr").unwrap();
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn from_hex_odd_length_returns_error() {
        let result = RequestSigner::from_hex("abc", "gonka");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid hex key"), "unexpected: {msg}");
    }

    #[test]
    fn from_hex_invalid_chars_returns_error() {
        let result = RequestSigner::from_hex(
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "gonka",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid hex key"), "unexpected: {msg}");
    }

    #[test]
    fn from_hex_all_zeros_returns_error() {
        // Scalar 0 is not a valid secp256k1 private key.
        let result = RequestSigner::from_hex(
            "0000000000000000000000000000000000000000000000000000000000000000",
            "gonka",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid secp256k1 key"), "unexpected: {msg}");
    }

    #[test]
    fn sign_with_unicode_transfer_address_does_not_panic() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        // Unicode in transfer_address is allowed by the API; bech32 addresses are
        // ASCII-only in practice but the signer must not panic on arbitrary input.
        let result = signer.sign(b"body", 1u128, "gonka1\u{1F600}test");
        assert!(result.is_ok());
    }

    #[test]
    fn sign_u128_max_timestamp() {
        let signer = RequestSigner::from_hex(PRIV_KEY_1, "gonka").unwrap();
        let result = signer.sign(b"", u128::MAX, "addr");
        assert!(result.is_ok());
    }
}
