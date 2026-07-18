// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Vault-key resolution for the transcript/session-log hash-chain (issue #6360).
//!
//! `zeph-common::hash_chain` and the `zeph-subagent`/`zeph-session` adapters deliberately carry
//! no vault dependency (mirrors `zeph-durable`'s INV-1: the abstraction stays pure, the binary
//! resolves concrete key material). This module is the concrete vault-key resolution layer,
//! exactly parallel to [`crate::durable::derive_control_hmac_key_b64`] — this module derives the
//! *history-chain* subkeys from a **separate** root secret (`ZEPH_HISTORY_KEY`), not
//! `ZEPH_DURABLE_KEY`, so history-log integrity works even when durable encryption is disabled
//! (spec-069 §6 "decoupled from durable").
//!
//! # Examples
//!
//! ```
//! use zeph_core::history_integrity::{derive_history_chain_key_b64, generate_history_key_b64};
//!
//! let key = generate_history_key_b64();
//! assert!(derive_history_chain_key_b64(&key, "zeph-session log v1").is_ok());
//! assert!(derive_history_chain_key_b64("not base64!", "zeph-session log v1").is_err());
//! ```

use zeph_common::hash_chain::{ChainKey, ChainKeyRing};
use zeph_vault::VaultProvider;

/// 32-byte key length shared by the root secret and every derived subkey.
const KEY_LEN: usize = 32;

/// Vault secret name for the current root history-integrity key.
pub const HISTORY_KEY_SECRET: &str = "ZEPH_HISTORY_KEY";
/// Vault secret name for the current root key's epoch number (decimal `u32`). Absent means
/// epoch 0 — the common case before any rotation has occurred.
pub const HISTORY_KEY_EPOCH_SECRET: &str = "ZEPH_HISTORY_KEY_EPOCH";
/// Vault secret name for a previous-epoch root key retained during a rotation window (FR-008).
/// Absent means no rotation window is configured.
pub const HISTORY_KEY_PREVIOUS_SECRET: &str = "ZEPH_HISTORY_KEY_PREVIOUS";
/// Vault secret name for `ZEPH_HISTORY_KEY_PREVIOUS`'s epoch number (decimal `u32`).
pub const HISTORY_KEY_PREVIOUS_EPOCH_SECRET: &str = "ZEPH_HISTORY_KEY_PREVIOUS_EPOCH";

/// Domain-separation context prefix for deriving a subsystem's chain key from
/// `ZEPH_HISTORY_KEY` via BLAKE3 `derive_key`. Combined with the caller-supplied domain (e.g.
/// `"zeph-session log v1"`) so each subsystem's chain key is cryptographically independent even
/// though all subsystems share one root secret.
const HISTORY_CHAIN_KEY_CONTEXT_PREFIX: &str = "zeph-history v1 chain key domain=";

/// Errors resolving or deriving history-chain key material.
#[derive(Debug, thiserror::Error)]
pub enum HistoryKeyError {
    /// The vault-resolved key was not exactly 32 bytes.
    #[error("history chain key must be {KEY_LEN} bytes, got {actual}")]
    InvalidKeyLength {
        /// The length of the supplied key material.
        actual: usize,
    },
    /// The vault-resolved key string was not valid base64.
    #[error("history chain key is not valid base64")]
    MalformedEncoding,
    /// `ZEPH_HISTORY_KEY_EPOCH` or `ZEPH_HISTORY_KEY_PREVIOUS_EPOCH` was not a valid `u32`.
    #[error("history key epoch is not a valid non-negative integer")]
    MalformedEpoch,
}

/// Derive one subsystem's chain key (a BLAKE3 `derive_key` subkey of the base64-encoded root
/// key) domain-separated by `domain`.
///
/// The chain key is not a separate vault secret: it is derived the same way
/// [`crate::durable::derive_control_hmac_key_b64`] derives the control-entry HMAC key from
/// `ZEPH_DURABLE_KEY` — one root secret, many cryptographically independent subkeys.
///
/// # Errors
///
/// Returns [`HistoryKeyError::MalformedEncoding`] when `b64_key` is not valid base64, or
/// [`HistoryKeyError::InvalidKeyLength`] when the decoded key is not exactly 32 bytes.
pub fn derive_history_chain_key_b64(
    b64_key: &str,
    domain: &str,
) -> Result<ChainKey, HistoryKeyError> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_key.trim())
        .map_err(|_| HistoryKeyError::MalformedEncoding)?;
    if bytes.len() != KEY_LEN {
        return Err(HistoryKeyError::InvalidKeyLength {
            actual: bytes.len(),
        });
    }
    let context = format!("{HISTORY_CHAIN_KEY_CONTEXT_PREFIX}{domain}");
    Ok(ChainKey::new(blake3::derive_key(&context, &bytes)))
}

/// Generate a fresh random 32-byte history-integrity root key, base64-encoded for vault storage
/// under [`HISTORY_KEY_SECRET`]. Drawn from the OS CSPRNG via the workspace `rand` dependency.
///
/// # Examples
///
/// ```
/// use zeph_core::history_integrity::{derive_history_chain_key_b64, generate_history_key_b64};
///
/// let key = generate_history_key_b64();
/// assert!(derive_history_chain_key_b64(&key, "zeph-subagent transcript v1").is_ok());
/// ```
#[must_use]
pub fn generate_history_key_b64() -> String {
    use base64::Engine as _;
    use rand::Rng as _;
    let mut bytes = [0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolve one subsystem's [`ChainKeyRing`] from the vault, reading the current root key
/// ([`HISTORY_KEY_SECRET`] + optional [`HISTORY_KEY_EPOCH_SECRET`]) and an optional
/// previous-epoch root key ([`HISTORY_KEY_PREVIOUS_SECRET`] + [`HISTORY_KEY_PREVIOUS_EPOCH_SECRET`])
/// for the rotation window (FR-008).
///
/// Returns `Ok(None)` — not an error — when [`HISTORY_KEY_SECRET`] is not present in the vault:
/// the caller (bootstrap code) should log a loud warning and continue with history-chain
/// verification disabled for this process, per spec-069's generate-on-first-use bootstrap
/// posture (M2) rather than fail process startup outright.
///
/// # Errors
///
/// Returns [`HistoryKeyError`] when a present secret is malformed (not valid base64, wrong
/// length, or a non-numeric epoch) — a *misconfigured* key is distinct from an *absent* one and
/// must not be silently treated as "chaining disabled" (NFR-004).
pub async fn resolve_key_ring(
    vault: &dyn VaultProvider,
    domain: &str,
) -> Result<Option<ChainKeyRing>, HistoryKeyError> {
    let get = |key: &'static str| async move { vault.get_secret(key).await.ok().flatten() };
    build_key_ring(
        get(HISTORY_KEY_SECRET).await,
        get(HISTORY_KEY_EPOCH_SECRET).await,
        get(HISTORY_KEY_PREVIOUS_SECRET).await,
        get(HISTORY_KEY_PREVIOUS_EPOCH_SECRET).await,
        domain,
    )
}

/// Synchronous variant of [`resolve_key_ring`] for CLI/bootstrap call sites that already hold a
/// loaded [`zeph_vault::AgeVaultProvider`] and use its synchronous
/// [`get`](zeph_vault::AgeVaultProvider::get) accessor — the same pattern
/// `crate::durable`'s vault-key loading (`load_write_hwm_key` et al.) uses in `src/commands/`,
/// rather than the async [`VaultProvider`] trait method.
///
/// # Errors
///
/// Same as [`resolve_key_ring`].
pub fn resolve_key_ring_sync(
    provider: &zeph_vault::AgeVaultProvider,
    domain: &str,
) -> Result<Option<ChainKeyRing>, HistoryKeyError> {
    build_key_ring(
        provider.get(HISTORY_KEY_SECRET).map(str::to_owned),
        provider.get(HISTORY_KEY_EPOCH_SECRET).map(str::to_owned),
        provider.get(HISTORY_KEY_PREVIOUS_SECRET).map(str::to_owned),
        provider
            .get(HISTORY_KEY_PREVIOUS_EPOCH_SECRET)
            .map(str::to_owned),
        domain,
    )
}

/// Shared epoch-parsing/key-derivation core for [`resolve_key_ring`] and
/// [`resolve_key_ring_sync`].
fn build_key_ring(
    current_b64: Option<String>,
    current_epoch: Option<String>,
    previous_b64: Option<String>,
    previous_epoch: Option<String>,
    domain: &str,
) -> Result<Option<ChainKeyRing>, HistoryKeyError> {
    let Some(current_b64) = current_b64 else {
        return Ok(None);
    };
    let current_epoch = match current_epoch {
        Some(s) => s
            .trim()
            .parse::<u32>()
            .map_err(|_| HistoryKeyError::MalformedEpoch)?,
        None => 0,
    };
    let current_key = derive_history_chain_key_b64(&current_b64, domain)?;
    let mut ring = ChainKeyRing::new(current_epoch, current_key);

    if let Some(previous_b64) = previous_b64 {
        let previous_epoch = match previous_epoch {
            Some(s) => s
                .trim()
                .parse::<u32>()
                .map_err(|_| HistoryKeyError::MalformedEpoch)?,
            None => current_epoch.saturating_sub(1),
        };
        let previous_key = derive_history_chain_key_b64(&previous_b64, domain)?;
        ring = ring.with_previous(previous_epoch, previous_key);
    }

    Ok(Some(ring))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_history_chain_key_rejects_malformed_base64() {
        assert!(matches!(
            derive_history_chain_key_b64("not base64!", "d"),
            Err(HistoryKeyError::MalformedEncoding)
        ));
    }

    #[test]
    fn derive_history_chain_key_rejects_wrong_length() {
        use base64::Engine as _;
        let short = base64::engine::general_purpose::STANDARD.encode(b"short");
        assert!(matches!(
            derive_history_chain_key_b64(&short, "d"),
            Err(HistoryKeyError::InvalidKeyLength { .. })
        ));
    }

    #[test]
    fn derive_history_chain_key_is_domain_separated() {
        let key = generate_history_key_b64();
        let a = derive_history_chain_key_b64(&key, "domain-a").unwrap();
        let b = derive_history_chain_key_b64(&key, "domain-b").unwrap();
        // ChainKey has no PartialEq (deliberately, to discourage timing-unsafe comparisons of
        // key material) — compare via a derived hash instead, which is exactly what domain
        // separation is meant to protect.
        let probe = zeph_common::hash_chain::genesis(&a, "x", b"y", 0);
        let probe_b = zeph_common::hash_chain::genesis(&b, "x", b"y", 0);
        assert_ne!(
            probe, probe_b,
            "chain keys for different domains must differ"
        );
    }

    #[test]
    fn derive_history_chain_key_independent_from_durable_key_derivation() {
        // Same root secret bytes, but this module's context prefix must differ from
        // CONTROL_HMAC_CONTEXT in crate::durable, so the two derived keys are independent even
        // when an operator (mistakenly) reuses ZEPH_DURABLE_KEY's value for ZEPH_HISTORY_KEY.
        let key = generate_history_key_b64();
        let history_key = derive_history_chain_key_b64(&key, "zeph-session log v1").unwrap();
        let durable_hmac_key = crate::durable::derive_control_hmac_key_b64(&key).unwrap();
        let probe = zeph_common::hash_chain::genesis(&history_key, "x", b"y", 0);
        let probe_durable = zeph_common::hash_chain::genesis(
            &zeph_common::hash_chain::ChainKey::new(durable_hmac_key),
            "x",
            b"y",
            0,
        );
        assert_ne!(probe, probe_durable);
    }

    #[tokio::test]
    async fn resolve_key_ring_returns_none_when_unprovisioned() {
        let vault = zeph_vault::MockVaultProvider::new();
        let ring = resolve_key_ring(&vault, "zeph-session log v1")
            .await
            .unwrap();
        assert!(ring.is_none());
    }

    #[tokio::test]
    async fn resolve_key_ring_resolves_current_only() {
        let key = generate_history_key_b64();
        let vault = zeph_vault::MockVaultProvider::new().with_secret(HISTORY_KEY_SECRET, &key);
        let ring = resolve_key_ring(&vault, "zeph-session log v1")
            .await
            .unwrap()
            .expect("key ring must resolve once the root secret is provisioned");
        assert_eq!(ring.current_epoch(), 0);
    }

    #[tokio::test]
    async fn resolve_key_ring_resolves_rotation_window() {
        let current = generate_history_key_b64();
        let previous = generate_history_key_b64();
        let vault = zeph_vault::MockVaultProvider::new()
            .with_secret(HISTORY_KEY_SECRET, &current)
            .with_secret(HISTORY_KEY_EPOCH_SECRET, "2")
            .with_secret(HISTORY_KEY_PREVIOUS_SECRET, &previous)
            .with_secret(HISTORY_KEY_PREVIOUS_EPOCH_SECRET, "1");

        let ring = resolve_key_ring(&vault, "zeph-subagent transcript v1")
            .await
            .unwrap()
            .expect("must resolve");
        assert_eq!(ring.current_epoch(), 2);

        // Build a chain under the previous epoch's key and confirm the ring resolves it as
        // Rekeyed, not Unverifiable — end-to-end confirmation that the vault-sourced ring
        // plumbs correctly into `verify_chained_prefix`.
        let previous_key =
            derive_history_chain_key_b64(&previous, "zeph-subagent transcript v1").unwrap();
        let base = zeph_common::hash_chain::genesis(
            &previous_key,
            "zeph-subagent transcript v1",
            b"file",
            1,
        );
        let h0 = zeph_common::hash_chain::chain_next(&previous_key, &base, b"entry");
        let entries = vec![(b"entry".to_vec(), h0)];
        let (_head, resolution) = zeph_common::hash_chain::verify_chained_prefix(
            &ring,
            "zeph-subagent transcript v1",
            b"file",
            &entries,
        )
        .unwrap();
        assert_eq!(
            resolution,
            zeph_common::hash_chain::KeyResolution::Rekeyed(1)
        );
    }

    #[tokio::test]
    async fn resolve_key_ring_fails_on_malformed_epoch() {
        let key = generate_history_key_b64();
        let vault = zeph_vault::MockVaultProvider::new()
            .with_secret(HISTORY_KEY_SECRET, &key)
            .with_secret(HISTORY_KEY_EPOCH_SECRET, "not-a-number");
        let err = resolve_key_ring(&vault, "d").await.unwrap_err();
        assert!(matches!(err, HistoryKeyError::MalformedEpoch));
    }
}
