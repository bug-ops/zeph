// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Invocation-Bound Capability Tokens (IBCT) for A2A delegation.
//!
//! An IBCT scopes an A2A delegation request to a specific `task_id` and `endpoint`.
//! It is signed with HMAC-SHA256 using a shared secret. The `key_id` field allows
//! multiple active keys so rotation can be performed without coordinated downtime (MF-4 fix).
//!
//! The token is serialized as base64-encoded JSON and transmitted in the
//! `X-Zeph-IBCT` HTTP request header.
//!
//! # Feature flag
//!
//! The `ibct` feature flag enables HMAC-SHA256 signing and verification.
//! The [`Ibct`], [`IbctKey`], and [`IbctError`] types are always present (for
//! deserialization), but [`Ibct::issue`] and [`Ibct::verify`] return
//! [`IbctError::FeatureDisabled`] when compiled without the `ibct` feature.
//!
//! # Security properties
//!
//! - Scope binding: the token is only valid for the specific `task_id` + `endpoint`.
//! - Expiry: `expires_at` is checked on verification with a configurable grace window.
//! - Key rotation: multiple keys indexed by `key_id` allow safe key rotation.
//! - Constant-time comparison: signature verification uses `Mac::verify_slice` to avoid
//!   timing side-channels.
//! - Vault integration: signing keys should be stored in the age vault, referenced by
//!   `ibct_signing_key_vault_ref` in `A2aServerConfig` (MF-3 fix).

use std::time::Duration;
#[cfg(feature = "ibct")]
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(feature = "ibct")]
use hmac::{Hmac, KeyInit, Mac};
#[cfg(feature = "ibct")]
use sha2::Sha256;

/// Grace window added to `expires_at` during verification to tolerate clock skew.
#[cfg(feature = "ibct")]
const CLOCK_SKEW_GRACE_SECS: u64 = 30;

/// Normalizes a full URL (e.g. `https://agent.example.com/a2a/stream`) down to its origin
/// (`https://agent.example.com`) for IBCT scoping.
///
/// Shared by both sides of the IBCT contract so they can never drift apart:
///
/// - **Client** (`crate::client::A2aClient::ibct_header_value`) applies this to the `endpoint`
///   argument before calling [`Ibct::issue`], so a token issued for `POST /a2a` and one issued
///   for `POST /a2a/stream` on the same agent carry the identical `endpoint` field.
/// - **Server** (`crate::server::A2aServer::serve`) applies this to its own advertised
///   `AgentCard::url` before constructing the `IbctConfig` [`Ibct::verify`] compares against —
///   `card.url` is documented as a path-free base URL, but nothing enforces that an operator's
///   configured `public_url` is actually canonical (no trailing slash, no `:443`, lowercase
///   scheme/host). Normalizing both sides through the same function, rather than trusting one
///   side's input to already be in the other side's expected shape, is what keeps `/a2a` and
///   `/a2a/stream` verifiable against one value regardless of path, and keeps a non-canonical
///   `public_url` from silently 403ing every request (the exact bug class of #6260 review S1,
///   moved from the client to the server if only one side normalized).
///
/// Falls back to the input unchanged if it does not parse as a URL (matches [`Ibct::issue`]'s
/// own tolerance of non-URL scope strings, and mirrors `discovery_origin` in
/// `src/tui_remote.rs`, which strips the same route-specific path for the analogous
/// `.well-known/agent.json` lookup).
pub(crate) fn ibct_scope_origin(endpoint: &str) -> String {
    url::Url::parse(endpoint).map_or_else(
        |_| endpoint.to_owned(),
        |u| u.origin().ascii_serialization(),
    )
}

/// Errors produced by [`Ibct::issue`] and [`Ibct::verify`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IbctError {
    /// The HMAC-SHA256 signature does not match the token's fields.
    /// Indicates tampering or use of a wrong key.
    #[error("IBCT signature invalid")]
    InvalidSignature,

    /// The token's `expires_at` is in the past beyond the clock-skew grace window.
    #[error("IBCT expired (expires_at={expires_at}, now={now})")]
    Expired { expires_at: u64, now: u64 },

    /// The token is bound to a different endpoint than the one being verified.
    #[error("IBCT endpoint mismatch: expected {expected}, got {got}")]
    EndpointMismatch { expected: String, got: String },

    /// The token is bound to a different task ID than the one being verified.
    #[error("IBCT task_id mismatch: expected {expected}, got {got}")]
    TaskMismatch { expected: String, got: String },

    /// The token's `key_id` is not present in the verifier's key set.
    /// Either the key was rotated out or the token was issued by a different party.
    #[error("IBCT key_id '{key_id}' not found in the configured key set")]
    UnknownKeyId { key_id: String },

    /// This crate was compiled without the `ibct` feature flag.
    #[error("IBCT feature not enabled (compile with feature 'ibct')")]
    FeatureDisabled,

    /// The base64 token string could not be decoded.
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64_compat::DecodeError),

    /// The decoded bytes are not valid JSON for an [`Ibct`] struct.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A key entry in the IBCT key set.
///
/// Multiple entries allow key rotation: old keys are kept until all in-flight tokens
/// signed with them expire.
///
/// `Serialize` is hand-written and redacts `key_bytes` to `"[REDACTED]"` (mirroring the
/// `Debug` impl below); `Deserialize` is derived and reads the real key bytes untouched.
#[derive(Clone, Deserialize)]
pub struct IbctKey {
    /// Unique key identifier. Embedded in the token so the verifier can look it up.
    pub key_id: String,
    /// HMAC-SHA256 signing key (raw bytes, hex-encoded in config).
    #[serde(with = "hex_bytes")]
    pub key_bytes: Vec<u8>,
}

impl std::fmt::Debug for IbctKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IbctKey")
            .field("key_id", &self.key_id)
            .field("key_bytes", &"[REDACTED]")
            .finish()
    }
}

impl IbctKey {
    /// Construct an `IbctKey` from a hex-encoded signing key, as used by
    /// `[a2a] ibct_keys[].key_hex` in config and by vault-resolved IBCT secrets
    /// (`ibct_signing_key_vault_ref`).
    ///
    /// # Errors
    ///
    /// Returns [`hex::FromHexError`] if `hex_key` is not valid hex.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::IbctKey;
    ///
    /// let key = IbctKey::from_hex("k1", "68656c6c6f2d7365637265742d6b6579").unwrap();
    /// assert_eq!(key.key_id, "k1");
    /// assert!(IbctKey::from_hex("k1", "not-hex").is_err());
    /// ```
    pub fn from_hex(key_id: impl Into<String>, hex_key: &str) -> Result<Self, hex::FromHexError> {
        Ok(Self {
            key_id: key_id.into(),
            key_bytes: hex::decode(hex_key)?,
        })
    }
}

impl Serialize for IbctKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("IbctKey", 2)?;
        s.serialize_field("key_id", &self.key_id)?;
        s.serialize_field("key_bytes", "[REDACTED]")?;
        s.end()
    }
}

/// An Invocation-Bound Capability Token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ibct {
    /// Identifies which key was used for signing, enabling key rotation.
    pub key_id: String,
    /// A2A task ID this token is scoped to.
    pub task_id: String,
    /// A2A agent endpoint this token is scoped to.
    pub endpoint: String,
    /// Unix timestamp (seconds) when this token was issued.
    pub issued_at: u64,
    /// Unix timestamp (seconds) when this token expires.
    pub expires_at: u64,
    /// HMAC-SHA256 over `{key_id}|{task_id}|{endpoint}|{issued_at}|{expires_at}`, hex-encoded.
    pub signature: String,
}

impl Ibct {
    /// Issue a new IBCT scoped to `task_id` + `endpoint`, valid for `ttl`.
    ///
    /// # Errors
    ///
    /// Returns `IbctError::FeatureDisabled` when compiled without the `ibct` feature.
    #[allow(clippy::needless_return)]
    pub fn issue(
        task_id: &str,
        endpoint: &str,
        ttl: Duration,
        key: &IbctKey,
    ) -> Result<Self, IbctError> {
        #[cfg(not(feature = "ibct"))]
        {
            let _ = (task_id, endpoint, ttl, key);
            return Err(IbctError::FeatureDisabled);
        }
        #[cfg(feature = "ibct")]
        {
            let now = unix_now();
            let expires_at = now + ttl.as_secs();
            let signature = sign(
                &key.key_bytes,
                &key.key_id,
                task_id,
                endpoint,
                now,
                expires_at,
            );
            Ok(Self {
                key_id: key.key_id.clone(),
                task_id: task_id.to_owned(),
                endpoint: endpoint.to_owned(),
                issued_at: now,
                expires_at,
                signature,
            })
        }
    }

    /// Verify this token against a key set, expected endpoint, and expected `task_id`.
    ///
    /// Looks up the key by `key_id`, verifies the HMAC signature, checks expiry
    /// (with `CLOCK_SKEW_GRACE_SECS` grace), and checks endpoint + `task_id` binding.
    ///
    /// # Errors
    ///
    /// Returns one of `IbctError::*` on any verification failure.
    #[allow(clippy::needless_return)]
    pub fn verify(
        &self,
        keys: &[IbctKey],
        expected_endpoint: &str,
        expected_task_id: &str,
    ) -> Result<(), IbctError> {
        #[cfg(not(feature = "ibct"))]
        {
            let _ = (keys, expected_endpoint, expected_task_id);
            return Err(IbctError::FeatureDisabled);
        }
        #[cfg(feature = "ibct")]
        {
            let key = keys
                .iter()
                .find(|k| k.key_id == self.key_id)
                .ok_or_else(|| IbctError::UnknownKeyId {
                    key_id: self.key_id.clone(),
                })?;

            // Constant-time HMAC verification: reconstruct the MAC and call verify_slice()
            // instead of comparing hex strings, which would be vulnerable to timing attacks.
            if verify_signature(
                &key.key_bytes,
                &self.key_id,
                &self.task_id,
                &self.endpoint,
                self.issued_at,
                self.expires_at,
                &self.signature,
            )
            .is_err()
            {
                return Err(IbctError::InvalidSignature);
            }

            let now = unix_now();
            if now > self.expires_at + CLOCK_SKEW_GRACE_SECS {
                return Err(IbctError::Expired {
                    expires_at: self.expires_at,
                    now,
                });
            }

            if self.endpoint != expected_endpoint {
                return Err(IbctError::EndpointMismatch {
                    expected: expected_endpoint.to_owned(),
                    got: self.endpoint.clone(),
                });
            }

            if self.task_id != expected_task_id {
                return Err(IbctError::TaskMismatch {
                    expected: expected_task_id.to_owned(),
                    got: self.task_id.clone(),
                });
            }

            Ok(())
        }
    }

    /// Encode this token to a base64-JSON string suitable for use in an HTTP header.
    ///
    /// # Errors
    ///
    /// Returns `serde_json::Error` if serialization fails.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        let json = serde_json::to_vec(self)?;
        Ok(base64_compat::encode(&json))
    }

    /// Decode a token from the base64-JSON string produced by `encode()`.
    ///
    /// # Errors
    ///
    /// Returns `IbctError::Base64` or `IbctError::Json` on decode failure.
    pub fn decode(s: &str) -> Result<Self, IbctError> {
        let bytes = base64_compat::decode(s)?;
        let token = serde_json::from_slice(&bytes)?;
        Ok(token)
    }
}

#[cfg(feature = "ibct")]
fn sign(
    key_bytes: &[u8],
    key_id: &str,
    task_id: &str,
    endpoint: &str,
    issued_at: u64,
    expires_at: u64,
) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let msg = format!("{key_id}|{task_id}|{endpoint}|{issued_at}|{expires_at}");
    let mut mac = HmacSha256::new_from_slice(key_bytes).expect("HMAC accepts any key length");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verify an HMAC-SHA256 signature in constant time using `Mac::verify_slice`.
///
/// Decodes the hex `signature`, recomputes the MAC over the canonical message,
/// and calls `verify_slice` — which uses a constant-time comparison internally.
///
/// # Errors
///
/// Returns an error if the hex is malformed or if the signature does not match.
#[cfg(feature = "ibct")]
fn verify_signature(
    key_bytes: &[u8],
    key_id: &str,
    task_id: &str,
    endpoint: &str,
    issued_at: u64,
    expires_at: u64,
    signature_hex: &str,
) -> Result<(), ()> {
    type HmacSha256 = Hmac<Sha256>;
    let decoded = hex::decode(signature_hex).map_err(|_| ())?;
    let msg = format!("{key_id}|{task_id}|{endpoint}|{issued_at}|{expires_at}");
    let mut mac = HmacSha256::new_from_slice(key_bytes).expect("HMAC accepts any key length");
    mac.update(msg.as_bytes());
    mac.verify_slice(&decoded).map_err(|_| ())
}

#[cfg(feature = "ibct")]
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Serde helper for hex-encoded byte vectors.
mod hex_bytes {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        hex::decode(&s).map_err(serde::de::Error::custom)
    }
}

/// Minimal base64 compatibility layer (uses the `base64` crate already in the dep tree
/// transitively via reqwest; we don't add a new dep).
///
/// This module wraps `base64::engine::general_purpose::STANDARD` under a stable API.
mod base64_compat {
    use base64::Engine as _;

    pub use base64::DecodeError;

    pub fn encode(input: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(input)
    }

    pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
        base64::engine::general_purpose::STANDARD.decode(input)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "ibct")]
    use super::*;
    #[cfg(feature = "ibct")]
    use std::assert_matches;

    #[cfg(feature = "ibct")]
    fn test_key() -> IbctKey {
        IbctKey {
            key_id: "k1".into(),
            key_bytes: b"super-secret-key-for-testing-only".to_vec(),
        }
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn issue_and_verify_round_trip() {
        let key = test_key();
        let token = Ibct::issue(
            "task-123",
            "https://agent.example.com",
            Duration::from_mins(5),
            &key,
        )
        .unwrap();
        assert!(
            token
                .verify(&[key], "https://agent.example.com", "task-123")
                .is_ok()
        );
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn verify_rejects_wrong_endpoint() {
        let key = test_key();
        let token = Ibct::issue(
            "task-123",
            "https://agent.example.com",
            Duration::from_mins(5),
            &key,
        )
        .unwrap();
        let err = token
            .verify(&[key], "https://evil.example.com", "task-123")
            .unwrap_err();
        assert_matches!(err, IbctError::EndpointMismatch { .. });
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn verify_rejects_wrong_task() {
        let key = test_key();
        let token = Ibct::issue(
            "task-123",
            "https://agent.example.com",
            Duration::from_mins(5),
            &key,
        )
        .unwrap();
        let err = token
            .verify(&[key], "https://agent.example.com", "task-999")
            .unwrap_err();
        assert_matches!(err, IbctError::TaskMismatch { .. });
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn verify_rejects_tampered_signature() {
        let key = test_key();
        let mut token = Ibct::issue(
            "task-123",
            "https://agent.example.com",
            Duration::from_mins(5),
            &key,
        )
        .unwrap();
        token.signature = "deadbeef".repeat(8);
        let err = token
            .verify(&[key], "https://agent.example.com", "task-123")
            .unwrap_err();
        assert_matches!(err, IbctError::InvalidSignature);
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn verify_rejects_unknown_key_id() {
        let key = test_key();
        let token = Ibct::issue(
            "task-123",
            "https://agent.example.com",
            Duration::from_mins(5),
            &key,
        )
        .unwrap();
        let other_key = IbctKey {
            key_id: "k99".into(),
            key_bytes: b"other".to_vec(),
        };
        let err = token
            .verify(&[other_key], "https://agent.example.com", "task-123")
            .unwrap_err();
        assert_matches!(err, IbctError::UnknownKeyId { .. });
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn encode_decode_round_trip() {
        let key = test_key();
        let token = Ibct::issue(
            "task-abc",
            "https://agent.example.com",
            Duration::from_mins(1),
            &key,
        )
        .unwrap();
        let encoded = token.encode().unwrap();
        let decoded = Ibct::decode(&encoded).unwrap();
        assert_eq!(decoded.task_id, "task-abc");
        assert_eq!(decoded.key_id, "k1");
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn verify_rejects_expired_token() {
        let key = test_key();
        // Manually construct a token with expires_at in the past (beyond grace window).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Set expires_at to 120 seconds ago (well beyond CLOCK_SKEW_GRACE_SECS=30).
        let expired_at = now.saturating_sub(120);
        let issued_at = expired_at.saturating_sub(300);
        // Build the signature manually so it matches the token fields.
        #[cfg(feature = "ibct")]
        let signature = {
            use hmac::{Hmac, KeyInit, Mac};
            use sha2::Sha256;
            type HmacSha256 = Hmac<Sha256>;
            let msg = format!(
                "{}|{}|{}|{}|{}",
                key.key_id, "task-expired", "https://agent.example.com", issued_at, expired_at
            );
            let mut mac =
                HmacSha256::new_from_slice(&key.key_bytes).expect("HMAC accepts any key length");
            mac.update(msg.as_bytes());
            hex::encode(mac.finalize().into_bytes())
        };
        let token = Ibct {
            key_id: key.key_id.clone(),
            task_id: "task-expired".into(),
            endpoint: "https://agent.example.com".into(),
            issued_at,
            expires_at: expired_at,
            signature,
        };
        let err = token
            .verify(&[key], "https://agent.example.com", "task-expired")
            .unwrap_err();
        assert!(
            matches!(err, IbctError::Expired { .. }),
            "expected Expired, got {err:?}"
        );
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn key_rotation_verifies_with_old_key() {
        let old_key = IbctKey {
            key_id: "k1".into(),
            key_bytes: b"old-key".to_vec(),
        };
        let new_key = IbctKey {
            key_id: "k2".into(),
            key_bytes: b"new-key".to_vec(),
        };
        let token = Ibct::issue(
            "task-1",
            "https://agent.example.com",
            Duration::from_mins(5),
            &old_key,
        )
        .unwrap();
        // Verifier has both keys — old token still verifies
        assert!(
            token
                .verify(&[old_key, new_key], "https://agent.example.com", "task-1")
                .is_ok()
        );
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn ibct_key_debug_redacts_key_bytes() {
        let key = test_key();
        let debug = format!("{key:?}");
        assert!(!debug.contains("super-secret-key-for-testing-only"));
        assert!(debug.contains("k1"));
        assert!(debug.contains("REDACTED"));
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn ibct_key_from_hex_decodes_bytes() {
        let key = IbctKey::from_hex("k1", "68656c6c6f").unwrap();
        assert_eq!(key.key_id, "k1");
        assert_eq!(key.key_bytes, b"hello");
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn ibct_key_from_hex_rejects_invalid_hex() {
        assert!(IbctKey::from_hex("k1", "not-hex").is_err());
    }

    // #6260 review M6: `ibct_scope_origin` moved here from `client.rs` so both the client
    // (`A2aClient::ibct_header_value`) and the server (`A2aServer::serve`, normalizing
    // `card.url`) share one normalization, instead of only the client normalizing while the
    // server compared a raw, possibly non-canonical `card.url` (which would silently 403
    // every request for a non-canonical `public_url` — the same bug class as S1, just moved
    // to the other side). `client.rs`'s `ibct_scope_origin_*` tests and `router.rs`'s
    // `non_canonical_card_url_still_matches_client_issued_token` cover the two call sites
    // directly; this test covers the one normalization case neither of those exercises: an
    // explicit default port must be stripped (`url::Url::Origin::ascii_serialization`'s own
    // behavior, but worth pinning since #6260's review flagged `:443` by name).
    #[cfg(feature = "ibct")]
    #[test]
    fn ibct_scope_origin_strips_explicit_default_port() {
        assert_eq!(
            ibct_scope_origin("https://agent.example.com:443/a2a"),
            "https://agent.example.com"
        );
        assert_eq!(
            ibct_scope_origin("http://agent.example.com:80/a2a/stream"),
            "http://agent.example.com"
        );
    }

    #[cfg(feature = "ibct")]
    #[test]
    fn ibct_key_serialize_redacts_key_bytes() {
        let key = test_key();
        let json = serde_json::to_string(&key).unwrap();
        assert!(!json.contains(&hex::encode(b"super-secret-key-for-testing-only")));
        assert!(json.contains("k1"));
        assert!(json.contains("REDACTED"));
    }
}
