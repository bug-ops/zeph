// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JWS signature verification for [`AgentCard`](crate::AgentCard)s (A2A 1.0.0 §8.4).
//!
//! An [`AgentCardSignature`] covers the RFC 8785 JCS
//! canonicalization of the card's JSON representation with the `signatures` key
//! removed. Verification requires an out-of-band trusted-key store — this module
//! deliberately does **not** fetch keys from a card-supplied `jku` URL.
//!
//! # Trust model
//!
//! `jku`/JWKS auto-fetch is not implemented.
//!
//! // TODO(critic): jku/JWKS fetch deferred — SSRF risk on attacker-controlled URL;
//! // out-of-band key store is the trust anchor (#5928 follow-up).
//!
//! An attacker who can forge an entire card can also point a `jku` at a JWKS they
//! control and self-sign; only a pre-shared, operator-configured [`TrustedKey`] store
//! closes that gap. See [`TrustedKey`].
//!
//! # Algorithm support
//!
//! Only ES256 (ECDSA P-256, JWS `alg: "ES256"`) is supported today, per the A2A spec's
//! mandatory example. `EdDSA` and RS256 are deferred (D4) — a signature using an
//! unrecognized `alg` resolves to [`SignatureVerification::Unverifiable`].
//!
//! # Feature flag
//!
//! The [`SignatureVerification`], [`SigAlg`], and [`TrustedKey`] types are always
//! compiled (needed for config plumbing and the crypto-free URL-origin check in
//! [`crate::discovery`]). [`verify_card_signatures`] and [`sign_card`] require the
//! `card-signing` feature; without it, [`verify_card_signatures`] returns
//! [`SignatureVerification::FeatureDisabled`] and `sign_card` does not exist at all
//! (it has no meaningful behavior to fall back to — signing requires the crypto crates).
//!
//! # Known limitation — unvalidated against a real peer
//!
//! The JCS canonicalization and signing-input construction below were implemented from
//! the A2A 1.0.0 spec text (§8.4.1–§8.4.3) retrieved verbatim during design review, not
//! from a real signed-card test vector produced by a reference implementation (e.g. the
//! Python/JS `a2a-sdk`). `canonical_payload` (private, used by both [`verify_card_signatures`]
//! and [`sign_card`]) strips proto3-default-valued fields (empty
//! string, `false`, `0`, empty array/object, recursively through nested objects) before
//! JCS, matching the spec text's canonicalization rules — this closes the specific
//! divergence a compliant signer that strips defaults before signing would otherwise
//! trigger against our verifier canonicalizing the full transmitted card (#6201; see
//! `signature_over_default_stripped_payload_verifies_against_full_transmitted_card`
//! below). This, `self_signed_round_trip_verifies`, and
//! `raw_json_canonicalization_differs_from_typed_struct_reserialization` prove internal
//! self-consistency and guard the bug classes this module exists to avoid, but none of
//! them prove interoperability with a real A2A peer's signer. Treat `require` as unproven
//! until a real vector is obtained and checked in.

#[cfg(feature = "card-signing")]
use base64::Engine as _;
#[cfg(feature = "card-signing")]
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::types::AgentCardSignature;

/// Signature algorithm identifiers recognized when verifying an [`AgentCardSignature`].
///
/// Always compiled — used by config plumbing ([`TrustedKey`]) regardless of whether the
/// `card-signing` feature is enabled. Only [`Es256`](SigAlg::Es256) has a cryptographic
/// implementation today (D4: EdDSA/RS256 deferred).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigAlg {
    /// ECDSA using the P-256 curve and SHA-256 (JWS `alg: "ES256"`).
    Es256,
}

impl SigAlg {
    /// Parse a JWS `alg` header value into a [`SigAlg`], or `None` if unrecognized.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::card_signing::SigAlg;
    ///
    /// assert_eq!(SigAlg::from_jws_alg("ES256"), Some(SigAlg::Es256));
    /// assert_eq!(SigAlg::from_jws_alg("EdDSA"), None);
    /// ```
    #[must_use]
    pub fn from_jws_alg(alg: &str) -> Option<Self> {
        match alg {
            "ES256" => Some(Self::Es256),
            _ => None,
        }
    }
}

/// A public key trusted to sign peer [`AgentCard`](crate::AgentCard)s, keyed by `kid`.
///
/// This is the trust anchor for card signature verification: the operator configures
/// one entry per peer agent whose signature should be honored. There is no automatic
/// key discovery (see the module docs for why `jku` fetch is deferred).
///
/// `key_material` accepts either a JWK JSON object (`{"kty":"EC","crv":"P-256","x":...,"y":...}`)
/// or a PEM-encoded `SubjectPublicKeyInfo`. Parsing happens lazily on each verification
/// attempt — verification is not on a hot path (`AgentRegistry::discover` has no runtime
/// caller yet, see D3), so caching the parsed key is not worth the complexity.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    /// Key identifier, matched against the `kid` in a signature's protected header.
    pub kid: String,
    /// Algorithm this key is trusted to verify.
    pub alg: SigAlg,
    /// JWK JSON or PEM-encoded public key material.
    pub key_material: String,
}

/// Outcome of verifying an [`AgentCard`](crate::AgentCard)'s signature(s) against a
/// [`TrustedKey`] store.
///
/// This is a 3-way (plus [`FeatureDisabled`](Self::FeatureDisabled)) split rather than a
/// bool because policy decisions (`ignore`/`prefer`/`require`) need to distinguish "no
/// opinion" (`Unverifiable` — unsigned peer, or signed by an untrusted/unknown key) from
/// an active tampering signal (`Invalid` — a trusted key's signature does not match).
/// Treating both as "not verified" would let `prefer` silently accept a tampered card
/// from a peer whose `kid` happens to be unknown.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureVerification {
    /// At least one signature verified against a trusted key.
    Verified,
    /// No signature could be checked: the card is unsigned, every signature's `kid`/`alg`
    /// is not in the trust store, or a signature's protected header is malformed.
    Unverifiable {
        /// Human-readable reason, suitable for a `tracing::warn!` log line.
        reason: String,
    },
    /// A signature matched a trusted key by `kid`/`alg` but cryptographic verification
    /// failed, or the card's JSON could not be canonicalized. Signals tampering or a
    /// canonicalization mismatch — never returned for an unsigned card.
    Invalid {
        /// Human-readable reason, suitable for a `tracing::warn!`/`tracing::error!` log line.
        reason: String,
    },
    /// The crate was compiled without the `card-signing` feature, so no cryptographic
    /// verification was attempted.
    FeatureDisabled,
}

/// Errors from [`sign_card`] (test/tooling use only — see module docs on D5).
#[cfg(feature = "card-signing")]
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CardSigningError {
    /// RFC 8785 JCS canonicalization of the card JSON failed.
    #[error("JCS canonicalization failed: {0}")]
    Canonicalization(String),
    /// The JWS protected header could not be serialized.
    #[error("protected header serialization failed: {0}")]
    HeaderSerialization(String),
}

/// Verify `signatures` (typically [`AgentCard::signatures`](crate::AgentCard::signatures))
/// against `raw_card` and a `trusted_keys` store.
///
/// `raw_card` **must** be the raw JSON [`Value`] as received on the wire (e.g. parsed
/// directly from the HTTP response body), never a re-serialization of the typed
/// [`AgentCard`](crate::AgentCard) struct — see the module docs and the
/// `raw_json_canonicalization_differs_from_typed_struct_reserialization` unit test for why
/// that distinction is load-bearing, not stylistic.
///
/// All entries in `signatures` are evaluated — order in the wire array never affects the
/// outcome. Returns [`SignatureVerification::Verified`] if **any** signature verifies
/// against a trusted key, even if another entry in the same array is tampered or
/// unresolvable (key rotation and multi-party attestation both put more than one signature
/// on a card; a bad sibling signature must not veto a good one). Otherwise, returns
/// [`SignatureVerification::Invalid`] if any signature matched a trusted key's `kid`/`alg`
/// but failed cryptographic verification. Otherwise, returns
/// [`SignatureVerification::Unverifiable`] if the card is unsigned or no signature resolves
/// to a trusted key. Returns [`SignatureVerification::FeatureDisabled`] when compiled
/// without the `card-signing` feature.
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::card_signing::{verify_card_signatures, SignatureVerification};
/// use serde_json::json;
///
/// let raw_card = json!({"name": "peer", "url": "http://peer.example.com"});
/// let result = verify_card_signatures(&raw_card, &[], &[]);
/// assert!(matches!(
///     result,
///     SignatureVerification::Unverifiable { .. } | SignatureVerification::FeatureDisabled
/// ));
/// ```
#[must_use]
#[allow(clippy::needless_return, unused_variables)]
pub fn verify_card_signatures(
    raw_card: &Value,
    signatures: &[AgentCardSignature],
    trusted_keys: &[TrustedKey],
) -> SignatureVerification {
    #[cfg(not(feature = "card-signing"))]
    {
        return SignatureVerification::FeatureDisabled;
    }
    #[cfg(feature = "card-signing")]
    {
        if signatures.is_empty() {
            return SignatureVerification::Unverifiable {
                reason: "card carries no signatures".to_owned(),
            };
        }

        let payload = match canonical_payload(raw_card) {
            Ok(bytes) => bytes,
            Err(e) => {
                return SignatureVerification::Invalid {
                    reason: format!("canonicalization failed: {e}"),
                };
            }
        };

        // Evaluate every signature before deciding — a tampered or unknown-kid signature
        // earlier in the array must not veto a later signature that verifies (I1): the A2A
        // spec's "verify >= 1 signature" and this function's own contract require checking
        // all of them and taking `Verified` if any one verifies, regardless of position.
        // Real scenarios this protects: key rotation (old+new signature during overlap),
        // multi-party attestation. Precedence when none verify: Invalid > Unverifiable.
        let mut last_unverifiable_reason = "no signature verified".to_owned();
        let mut invalid_reason: Option<String> = None;
        for sig in signatures {
            match verify_one(&payload, sig, trusted_keys) {
                imp::SigOutcome::Verified => return SignatureVerification::Verified,
                imp::SigOutcome::Invalid(reason) => {
                    invalid_reason.get_or_insert(reason);
                }
                imp::SigOutcome::Unverifiable(reason) => last_unverifiable_reason = reason,
            }
        }
        match invalid_reason {
            Some(reason) => SignatureVerification::Invalid { reason },
            None => SignatureVerification::Unverifiable {
                reason: last_unverifiable_reason,
            },
        }
    }
}

/// Sign `raw_card` (raw JSON with `signatures` removed before canonicalization, per
/// [`verify_card_signatures`]) with `signing_key`, producing an [`AgentCardSignature`].
///
/// This exists to build round-trip tests and interop vectors, mirroring
/// [`crate::Ibct::issue`]/[`crate::Ibct::verify`]. Wiring this into the A2A server so it
/// signs our own served card is deferred (D5) — see module docs.
///
/// # Errors
///
/// Returns [`CardSigningError::Canonicalization`] if `raw_card` cannot be JCS-canonicalized,
/// or [`CardSigningError::HeaderSerialization`] if the protected header cannot be serialized.
///
/// # Examples
///
/// ```rust
/// # #[cfg(feature = "card-signing")]
/// # {
/// use zeph_a2a::card_signing::sign_card;
/// use p256::ecdsa::SigningKey;
/// use serde_json::json;
///
/// let signing_key = SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
/// let raw_card = json!({"name": "my-agent", "url": "http://localhost:8080"});
/// let sig = sign_card(&raw_card, "key-1", &signing_key).unwrap();
/// assert!(!sig.protected.is_empty());
/// # }
/// ```
#[cfg(feature = "card-signing")]
pub fn sign_card(
    raw_card: &Value,
    kid: &str,
    signing_key: &p256::ecdsa::SigningKey,
) -> Result<AgentCardSignature, CardSigningError> {
    use p256::ecdsa::signature::Signer;

    let payload = canonical_payload(raw_card).map_err(CardSigningError::Canonicalization)?;
    let header = serde_json::json!({"alg": "ES256", "kid": kid});
    let header_bytes = serde_json::to_vec(&header)
        .map_err(|e| CardSigningError::HeaderSerialization(e.to_string()))?;
    let protected = URL_SAFE_NO_PAD.encode(header_bytes);
    let signing_input = format!("{protected}.{}", URL_SAFE_NO_PAD.encode(&payload));
    let signature: p256::ecdsa::Signature = Signer::sign(signing_key, signing_input.as_bytes());
    Ok(AgentCardSignature {
        protected,
        signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        header: None,
    })
}

/// RFC 8785 JCS canonicalization of `raw_card` with the `signatures` key removed and
/// proto3-default-valued fields stripped (#6201).
///
/// Operates on the raw received [`Value`] — never on a re-serialization of the typed
/// [`AgentCard`](crate::AgentCard) struct. See module docs.
///
/// A compliant signer may drop proto3-default-valued fields (empty string, `false`, `0`,
/// empty array/object) from the card JSON before canonicalizing and signing, per the A2A
/// spec text, while the transmitted card still carries them explicitly. Stripping the same
/// fields here — recursively, bottom-up so an object that becomes empty after its own
/// fields are stripped is itself dropped from its parent — normalizes both shapes to the
/// same canonical bytes, so a signature computed over either verifies against the other.
#[cfg(feature = "card-signing")]
fn canonical_payload(raw_card: &Value) -> Result<Vec<u8>, String> {
    let mut card = raw_card.clone();
    if let Value::Object(map) = &mut card {
        map.remove("signatures");
    }
    strip_proto3_defaults(&mut card);
    serde_json_canonicalizer::to_vec(&card).map_err(|e| e.to_string())
}

/// `true` when `value` is a proto3 default: empty string, `false`, `0` (integer or
/// float), an empty array, or an empty object. `null` is not a proto3 JSON-mapping
/// default value and is left untouched.
// TODO(critic): the `{}` (empty object) and `0` (number) cases are the highest-risk,
// unvalidated part of this heuristic (S2, #6201 follow-up). Proto3 JSON mapping has
// *message presence*: a message field explicitly **set** to an empty message serializes
// to `{}` and is distinct from a field left **unset** (which is omitted entirely) — a
// real signer that emits `{}` for a deliberately-set-but-empty message would sign
// *with* that key present, while this function strips it, reproducing the exact
// canonical-bytes divergence #6201 exists to eliminate. The same shape applies to a
// semantically meaningful `0`. This is invisible to every in-tree test because
// `canonical_payload` is applied symmetrically to both `sign_card` and
// `verify_card_signatures` (see module docs' "unvalidated against a real peer"
// section) — it only bites against a real external `a2a-sdk` signer. If a real vector
// ever mismatches specifically on an empty-object or zero-valued field, narrow this
// function (e.g. drop the `{}`/`0` arms, keeping only string/bool/array) rather than
// assuming the JCS library itself is at fault.
#[cfg(feature = "card-signing")]
fn is_proto3_default(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// Recursively drops object keys whose value is a proto3 default (see
/// [`is_proto3_default`]), processing children first so a nested object that becomes
/// empty only after its own defaults are stripped is also removed from its parent.
/// Array elements are recursed into but never removed — a repeated field's cardinality
/// is significant and unlike a struct field has no "default value" to omit.
#[cfg(feature = "card-signing")]
fn strip_proto3_defaults(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| {
                strip_proto3_defaults(v);
                !is_proto3_default(v)
            });
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_proto3_defaults(v);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[cfg(feature = "card-signing")]
mod imp {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use p256::ecdsa::signature::Verifier;

    use super::{SigAlg, TrustedKey};
    use crate::types::AgentCardSignature;

    pub(super) enum SigOutcome {
        Verified,
        Unverifiable(String),
        Invalid(String),
    }

    struct ProtectedHeader {
        alg: String,
        kid: Option<String>,
    }

    fn decode_protected_header(protected_b64: &str) -> Option<ProtectedHeader> {
        let bytes = URL_SAFE_NO_PAD.decode(protected_b64).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let alg = v.get("alg")?.as_str()?.to_owned();
        let kid = v
            .get("kid")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Some(ProtectedHeader { alg, kid })
    }

    /// Parse `material` (JWK JSON or PEM `SubjectPublicKeyInfo`) into a P-256 verifying key.
    fn parse_verifying_key(material: &str) -> Result<p256::ecdsa::VerifyingKey, String> {
        use p256::pkcs8::DecodePublicKey;

        let trimmed = material.trim();
        if trimmed.starts_with("-----BEGIN") {
            return p256::ecdsa::VerifyingKey::from_public_key_pem(trimmed)
                .map_err(|e| format!("PEM public key: {e}"));
        }

        let jwk: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("key_material is neither PEM nor valid JWK JSON: {e}"))?;
        let x = jwk
            .get("x")
            .and_then(serde_json::Value::as_str)
            .ok_or("JWK missing 'x' coordinate")?;
        let y = jwk
            .get("y")
            .and_then(serde_json::Value::as_str)
            .ok_or("JWK missing 'y' coordinate")?;
        let x_bytes = URL_SAFE_NO_PAD
            .decode(x)
            .map_err(|e| format!("JWK 'x' is not valid base64url: {e}"))?;
        let y_bytes = URL_SAFE_NO_PAD
            .decode(y)
            .map_err(|e| format!("JWK 'y' is not valid base64url: {e}"))?;

        let mut sec1 = Vec::with_capacity(1 + x_bytes.len() + y_bytes.len());
        sec1.push(0x04); // SEC1 uncompressed point tag.
        sec1.extend_from_slice(&x_bytes);
        sec1.extend_from_slice(&y_bytes);
        p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1)
            .map_err(|e| format!("invalid P-256 point: {e}"))
    }

    pub(super) fn verify_one(
        payload: &[u8],
        sig: &AgentCardSignature,
        trusted_keys: &[TrustedKey],
    ) -> SigOutcome {
        let Some(header) = decode_protected_header(&sig.protected) else {
            return SigOutcome::Unverifiable("malformed protected header".to_owned());
        };
        let Some(alg) = SigAlg::from_jws_alg(&header.alg) else {
            return SigOutcome::Unverifiable(format!("unsupported alg '{}'", header.alg));
        };
        let Some(kid) = header.kid else {
            return SigOutcome::Unverifiable("protected header missing 'kid'".to_owned());
        };
        let Some(key) = trusted_keys.iter().find(|k| k.kid == kid && k.alg == alg) else {
            return SigOutcome::Unverifiable(format!("no trusted key for kid '{kid}'"));
        };
        let verifying_key = match parse_verifying_key(&key.key_material) {
            Ok(vk) => vk,
            Err(e) => return SigOutcome::Invalid(format!("trusted key '{kid}' unparsable: {e}")),
        };

        let Ok(sig_bytes) = URL_SAFE_NO_PAD.decode(&sig.signature) else {
            return SigOutcome::Invalid("signature is not valid base64url".to_owned());
        };
        let Ok(ecdsa_sig) = p256::ecdsa::Signature::from_slice(&sig_bytes) else {
            return SigOutcome::Invalid(
                "signature has invalid length/encoding for ES256".to_owned(),
            );
        };

        let signing_input = format!("{}.{}", sig.protected, URL_SAFE_NO_PAD.encode(payload));
        match verifying_key.verify(signing_input.as_bytes(), &ecdsa_sig) {
            Ok(()) => SigOutcome::Verified,
            Err(_) => SigOutcome::Invalid("ECDSA verification failed".to_owned()),
        }
    }
}

#[cfg(feature = "card-signing")]
use imp::verify_one;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sig_alg_from_jws_alg() {
        assert_eq!(SigAlg::from_jws_alg("ES256"), Some(SigAlg::Es256));
        assert_eq!(SigAlg::from_jws_alg("EdDSA"), None);
        assert_eq!(SigAlg::from_jws_alg("none"), None);
    }

    #[test]
    fn verify_empty_signatures_is_unverifiable_or_disabled() {
        let raw = serde_json::json!({"name": "peer"});
        let result = verify_card_signatures(&raw, &[], &[]);
        assert!(matches!(
            result,
            SignatureVerification::Unverifiable { .. } | SignatureVerification::FeatureDisabled
        ));
    }

    #[cfg(feature = "card-signing")]
    mod crypto {
        use std::assert_matches;

        use p256::ecdsa::SigningKey;
        use p256::pkcs8::EncodePublicKey;

        use super::super::*;

        fn test_signing_key() -> SigningKey {
            SigningKey::from_bytes(&[9u8; 32].into()).expect("valid scalar")
        }

        fn trusted_key_for(kid: &str, signing_key: &SigningKey) -> TrustedKey {
            let verifying_key = signing_key.verifying_key();
            let pem = verifying_key
                .to_public_key_pem(p256::pkcs8::LineEnding::LF)
                .expect("pem encode");
            TrustedKey {
                kid: kid.to_owned(),
                alg: SigAlg::Es256,
                key_material: pem,
            }
        }

        #[test]
        fn self_signed_round_trip_verifies() {
            let signing_key = test_signing_key();
            let raw_card = serde_json::json!({
                "name": "peer-agent",
                "url": "http://peer.example.com",
                "description": "",
            });
            let sig = sign_card(&raw_card, "key-1", &signing_key).unwrap();
            let card_with_sig = {
                let mut v = raw_card.clone();
                v["signatures"] = serde_json::json!([&sig]);
                v
            };
            let trusted = vec![trusted_key_for("key-1", &signing_key)];
            let result = verify_card_signatures(&card_with_sig, &[sig], &trusted);
            assert_eq!(result, SignatureVerification::Verified);
        }

        #[test]
        fn tampered_signature_is_invalid() {
            let signing_key = test_signing_key();
            let raw_card =
                serde_json::json!({"name": "peer-agent", "url": "http://peer.example.com"});
            let mut sig = sign_card(&raw_card, "key-1", &signing_key).unwrap();
            sig.signature = URL_SAFE_NO_PAD.encode([0u8; 64]);
            let trusted = vec![trusted_key_for("key-1", &signing_key)];
            let result = verify_card_signatures(&raw_card, &[sig], &trusted);
            assert_matches!(result, SignatureVerification::Invalid { .. });
        }

        #[test]
        fn tampered_payload_is_invalid() {
            let signing_key = test_signing_key();
            let raw_card =
                serde_json::json!({"name": "peer-agent", "url": "http://peer.example.com"});
            let sig = sign_card(&raw_card, "key-1", &signing_key).unwrap();
            let mut tampered_card = raw_card.clone();
            tampered_card["name"] = serde_json::json!("evil-agent");
            let trusted = vec![trusted_key_for("key-1", &signing_key)];
            let result = verify_card_signatures(&tampered_card, &[sig], &trusted);
            assert_matches!(result, SignatureVerification::Invalid { .. });
        }

        #[test]
        fn unknown_kid_is_unverifiable() {
            let signing_key = test_signing_key();
            let raw_card =
                serde_json::json!({"name": "peer-agent", "url": "http://peer.example.com"});
            let sig = sign_card(&raw_card, "unknown-key", &signing_key).unwrap();
            let other_key = trusted_key_for("key-1", &test_signing_key());
            let result = verify_card_signatures(&raw_card, &[sig], &[other_key]);
            assert_matches!(result, SignatureVerification::Unverifiable { .. });
        }

        /// Regression test for I1: a tampered-but-trusted-`kid` signature earlier in the
        /// array must not veto a later signature that verifies — the outcome must be
        /// order-independent. Models key rotation (old signature tampered/expired, new
        /// signature valid) and multi-party attestation.
        #[test]
        fn verified_signature_wins_regardless_of_position_invalid_then_verified() {
            let key_a = SigningKey::from_bytes(&[11u8; 32].into()).unwrap();
            let key_b = SigningKey::from_bytes(&[22u8; 32].into()).unwrap();
            let raw_card =
                serde_json::json!({"name": "peer-agent", "url": "http://peer.example.com"});

            let mut sig_a = sign_card(&raw_card, "key-a", &key_a).unwrap();
            sig_a.signature = URL_SAFE_NO_PAD.encode([0u8; 64]); // tamper: now Invalid
            let sig_b = sign_card(&raw_card, "key-b", &key_b).unwrap(); // untouched: Verified

            let trusted = vec![
                trusted_key_for("key-a", &key_a),
                trusted_key_for("key-b", &key_b),
            ];

            let result_invalid_first =
                verify_card_signatures(&raw_card, &[sig_a.clone(), sig_b.clone()], &trusted);
            assert_eq!(result_invalid_first, SignatureVerification::Verified);

            let result_verified_first =
                verify_card_signatures(&raw_card, &[sig_b, sig_a], &trusted);
            assert_eq!(result_verified_first, SignatureVerification::Verified);
        }

        /// When no signature verifies, `Invalid` must win over `Unverifiable` regardless of
        /// which entry appears first — a tampered signature is a stronger reject signal than
        /// an unresolvable one.
        #[test]
        fn invalid_wins_over_unverifiable_when_none_verify() {
            let key_a = SigningKey::from_bytes(&[33u8; 32].into()).unwrap();
            let raw_card =
                serde_json::json!({"name": "peer-agent", "url": "http://peer.example.com"});

            let mut sig_a = sign_card(&raw_card, "key-a", &key_a).unwrap();
            sig_a.signature = URL_SAFE_NO_PAD.encode([0u8; 64]); // trusted kid, tampered → Invalid
            let sig_unknown = sign_card(&raw_card, "unknown-key", &key_a).unwrap(); // Unverifiable

            let trusted = vec![trusted_key_for("key-a", &key_a)];

            let result = verify_card_signatures(&raw_card, &[sig_unknown, sig_a], &trusted);
            assert_matches!(result, SignatureVerification::Invalid { .. });
        }

        /// Regression test for the S1 bug class: JCS **must** canonicalize the raw received
        /// JSON, never a re-serialization of the typed `AgentCard` struct. The typed struct
        /// silently drops any JSON key it doesn't recognize (no `deny_unknown_fields`, no
        /// catch-all field) — canonicalizing the typed struct's re-serialization instead of
        /// the raw bytes would make a genuinely valid signature fail verification whenever a
        /// peer's card carries a vendor extension field the schema doesn't model.
        ///
        /// Before #6201's proto3-default-stripping fix, this test's premise was a *different*
        /// divergence source (proto3-default fields the raw JSON omitted but the typed
        /// struct's `Serialize` impl always re-materializes) — that source is now normalized
        /// away by [`canonical_payload`]'s stripping, so the test uses an irreducible
        /// divergence (an unknown field) that stripping cannot close.
        #[test]
        fn raw_json_canonicalization_differs_from_typed_struct_reserialization() {
            let raw_json = serde_json::json!({
                "name": "peer",
                "description": "a peer agent",
                "url": "http://peer.example.com",
                "version": "0.1.0",
                "protocolVersion": "0.2.1",
                "capabilities": {"streaming": true},
                "vendorExtension": {"trustScore": 42},
            });

            // Deserializing into the typed `AgentCard` and re-serializing silently drops
            // `vendorExtension` — it has no field to land in.
            let typed: crate::types::AgentCard = serde_json::from_value(raw_json.clone()).unwrap();
            let reserialized = serde_json::to_value(&typed).unwrap();

            let raw_canonical = canonical_payload(&raw_json).unwrap();
            let reserialized_canonical = canonical_payload(&reserialized).unwrap();

            assert_ne!(
                raw_canonical, reserialized_canonical,
                "raw and re-serialized-typed-struct canonical bytes must differ when the raw \
                 JSON carries a field the AgentCard schema doesn't model — if this assertion \
                 fails, unknown fields are somehow surviving the typed round-trip and this \
                 test's premise no longer holds"
            );
        }

        /// Regression test for #6201: a compliant A2A signer may strip proto3-default-valued
        /// fields (empty string/`false`/`0`/empty array/object) from the card JSON before JCS
        /// canonicalization and signing (A2A spec §8.4.1), while the transmitted card still
        /// carries those defaults explicitly. Before this fix, `canonical_payload` canonicalized
        /// the raw received JSON verbatim (`signatures` removed only), so a signature computed
        /// over the signer's default-stripped payload would fail to verify against the full
        /// transmitted card — a fail-closed availability bug rejecting a genuinely valid,
        /// untampered card.
        #[test]
        fn signature_over_default_stripped_payload_verifies_against_full_transmitted_card() {
            let signing_key = SigningKey::from_bytes(&[44u8; 32].into()).unwrap();

            // What a compliant signer canonicalizes and signs: proto3-default fields
            // (`description`, `defaultInputModes`, `pushNotifications`, ...) are absent.
            let signer_payload = serde_json::json!({
                "name": "peer-agent",
                "url": "http://peer.example.com",
                "version": "0.1.0",
                "protocolVersion": "0.2.1",
                "capabilities": {"streaming": true},
            });
            let sig = sign_card(&signer_payload, "key-1", &signing_key).unwrap();

            // What actually arrives over the wire: the same card with every proto3-default
            // field present and explicit.
            let transmitted_card = serde_json::json!({
                "name": "peer-agent",
                "description": "",
                "url": "http://peer.example.com",
                "version": "0.1.0",
                "protocolVersion": "0.2.1",
                "capabilities": {
                    "streaming": true,
                    "pushNotifications": false,
                    "stateTransitionHistory": false,
                    "images": false,
                    "audio": false,
                    "files": false
                },
                "defaultInputModes": [],
                "defaultOutputModes": [],
                "skills": [],
                "signatures": [&sig],
            });

            let trusted = vec![trusted_key_for("key-1", &signing_key)];
            let result = verify_card_signatures(&transmitted_card, &[sig], &trusted);
            assert_eq!(
                result,
                SignatureVerification::Verified,
                "verification must succeed even when the signer stripped proto3-default \
                 fields before signing but the transmitted card carries them explicitly"
            );
        }

        #[test]
        fn strip_proto3_defaults_removes_nested_object_that_becomes_empty() {
            let mut value = serde_json::json!({
                "name": "peer",
                "capabilities": {"streaming": false, "images": false},
                "skills": [{"id": "s1", "tags": []}],
            });
            strip_proto3_defaults(&mut value);
            assert_eq!(
                value,
                serde_json::json!({
                    "name": "peer",
                    "skills": [{"id": "s1"}],
                }),
                "an object whose fields are all proto3 defaults must itself be dropped from \
                 its parent, and array elements must be recursed into (never removed from \
                 the array itself)"
            );
        }
    }
}
