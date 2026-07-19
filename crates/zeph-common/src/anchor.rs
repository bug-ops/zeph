// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Vault-anchor downgrade-resistance primitives (issue #6449).
//!
//! [`crate::hash_chain`] defends against in-place edits, reordering, and a *partial* strip of
//! chain metadata, but explicitly does **not** defend against a **fully consistent whole-file
//! strip** (delete every `chain` field so a chained file looks pre-feature-legacy) — see that
//! module's "Threat model and honest scope" docs. This module closes that gap for
//! already-anchored content: a small per-file record (an [`Anchor`]) is written to the age vault
//! on finalize/close and checked on read. Because an age vault entry can only be removed by an
//! attacker who holds the age private key (decrypt → re-encrypt → rename), and the threat model
//! here is a file-write-only attacker, a whole-file strip can never make the anchor disappear —
//! so "legacy-looking file, but an anchor exists for its identity" is an unambiguous tamper
//! signature.
//!
//! Mirrors [`crate::hash_chain`]'s layering: this module is pure (no vault dependency) so the
//! adapter crates (`zeph-subagent`, `zeph-session`) depend only on the [`AnchorStore`] trait,
//! never on `zeph-vault` directly (INV-1). The binary provides the concrete
//! age-vault-backed implementation and installs it into each adapter's process-global slot at
//! bootstrap, exactly as it already does for [`crate::hash_chain::ChainKeyRing`].
//!
//! # Absent anchor is never a tamper signature
//!
//! A chained file with **no** anchor is not suspicious: it can only mean the anchor feature was
//! not active when the file was finalized (pre-feature content, `anchor = "none"`, or a vault
//! outage during finalize), never an attacker having deleted a vault entry (that requires the age
//! key). Callers must trust "chained + anchor absent" exactly like plain #6453 behavior — never
//! fail-closed on it, or every session/transcript that predates this feature bricks.

use std::future::Future;
use std::pin::Pin;

use crate::hash_chain::ChainHash;

/// Vault secret name prefix for every anchor key. Also the prefix the reconcile-and-cap sweep
/// filters on when listing vault keys.
pub const ANCHOR_KEY_PREFIX: &str = "ZEPH_HISTORY_ANCHOR_";

/// Which subsystem a given anchor belongs to — folded into the vault key so the two subsystems'
/// anchors never collide even if a `file_id` happened to coincide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnchorSubsystem {
    /// `zeph-subagent` transcript (`<task_id>.jsonl`).
    SubagentTranscript,
    /// `zeph-session` event log (`events.jsonl`).
    SessionLog,
}

impl AnchorSubsystem {
    /// The vault-key segment for this subsystem (`SUBAGENT` / `SESSION`).
    #[must_use]
    pub const fn key_segment(self) -> &'static str {
        match self {
            Self::SubagentTranscript => "SUBAGENT",
            Self::SessionLog => "SESSION",
        }
    }
}

/// A per-file downgrade-resistance record, stored as an age-vault secret keyed by
/// [`anchor_key`].
///
/// Authenticity comes entirely from the vault's own AEAD encryption — the anchor carries no MAC
/// of its own, since an attacker who cannot decrypt the vault cannot forge or delete an entry
/// either way.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Anchor {
    /// Format version, for forward compatibility.
    pub version: u8,
    /// The finalizing key epoch (cross-check + operator diagnostics only — not required to
    /// match on read, since a legitimately re-keyed file may resolve under a different epoch).
    pub epoch: u32,
    /// Total on-disk entry count at the time this anchor was written.
    pub count: u64,
    /// The verified chain head at exactly `count` entries, hex-encoded.
    pub head_hex: String,
    /// Wall-clock milliseconds at write time, embedded **inside** this AEAD-protected value so
    /// it is unforgeable by a file-write-only attacker (unlike filesystem mtime, which such an
    /// attacker can freely rewrite via `utimensat`). Used by the session-anchor reconcile-and-cap
    /// sweep to select the true oldest anchor for eviction (issue #6449 rev2 critic S3) — eviction
    /// ordering must never depend on an attacker-controlled signal.
    pub written_at: u64,
    /// Wall-clock milliseconds at which the reconcile-and-cap sweep first observed this anchor's
    /// backing file/session-directory absent. `None` while the file exists. Set on the first
    /// sweep that finds the file gone, cleared if the file reappears before the grace window
    /// elapses (self-heal), and used to gate orphan reap behind a grace window so a
    /// delete→wait-out-a-sweep→recreate-forged-legacy sequence cannot make the sweep delete the
    /// anchor on the attacker's behalf (issue #6462). `#[serde(default)]` means pre-existing
    /// persisted anchors deserialize with `None`, no vault migration needed;
    /// `skip_serializing_if` keeps steady-state (never-orphaned) anchors byte-identical to their
    /// pre-#6462 serialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_since: Option<u64>,
}

/// Current [`Anchor::version`].
pub const ANCHOR_VERSION: u8 = 1;

impl Anchor {
    /// Construct a new anchor for a file finalized with `epoch`/`count`/`head`, stamping
    /// [`Self::written_at`] with the current wall-clock time.
    #[must_use]
    pub fn new(epoch: u32, count: u64, head: ChainHash) -> Self {
        Self {
            version: ANCHOR_VERSION,
            epoch,
            count,
            head_hex: head.to_hex(),
            written_at: now_unix_millis(),
            orphaned_since: None,
        }
    }

    /// Parse [`Self::head_hex`] back into a [`ChainHash`].
    ///
    /// # Errors
    ///
    /// Returns [`AnchorError::Malformed`] if the stored hex is not a valid chain hash — this can
    /// only happen if the vault entry was corrupted or hand-edited by the age-key holder, not by
    /// a file-write-only attacker.
    pub fn head(&self) -> Result<ChainHash, AnchorError> {
        ChainHash::from_hex(&self.head_hex).map_err(|_| AnchorError::Malformed)
    }
}

/// Current wall-clock time in Unix milliseconds, saturating to `u64::MAX` rather than panicking
/// on an unrepresentable (pre-epoch or post-overflow) system clock.
///
/// Shared by [`Anchor::new`] (stamps [`Anchor::written_at`]) and the reconcile-and-cap sweep
/// (stamps/checks [`Anchor::orphaned_since`], issue #6462) so both use the same time source.
#[must_use]
pub fn now_unix_millis() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Errors from an [`AnchorStore`] operation.
#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    /// The underlying vault operation failed.
    #[error("anchor store I/O failed: {0}")]
    Store(String),
    /// A stored anchor's `head_hex` was not a valid chain hash.
    #[error("stored anchor is malformed")]
    Malformed,
}

/// Storage abstraction for per-file vault anchors (issue #6449).
///
/// Implementors persist an [`Anchor`] keyed by [`anchor_key`] in a medium a file-write-only
/// attacker cannot forge or delete — in practice, an age vault secret. The adapter crates
/// (`zeph-subagent`, `zeph-session`) depend only on this trait, never on a concrete vault type
/// (INV-1); the binary provides the concrete implementation.
///
/// Both an async and a sync accessor are provided for [`get`][Self::get]/[`get_sync`][Self::get_sync]:
/// the session-log read path is already fully async, but the transcript read path
/// (`TranscriptReader::load`/`load_strict`) is a plain synchronous function with call sites
/// spread across 3+ crates outside this feature's ownership — making it async would be a large,
/// out-of-scope blast radius (mirrors why `zeph_core::history_integrity` already exposes both
/// [`resolve_key_ring`](../../zeph_core/history_integrity/fn.resolve_key_ring.html) and a `_sync`
/// counterpart for the same reason). `get_sync` is cheap (an in-memory map lookup behind a
/// `std::sync::RwLock`, never a blocking disk read), so calling it from a sync context never
/// risks stalling a tokio worker thread for longer than a brief lock hold.
pub trait AnchorStore: Send + Sync {
    /// Fetch the anchor for `(subsystem, file_id)`, if one is configured and present.
    ///
    /// # Errors
    ///
    /// Returns [`AnchorError`] on a store-level failure. A simply-absent anchor is `Ok(None)`,
    /// never an error — see the module docs' "absent anchor is never a tamper signature" note.
    fn get(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<Option<Anchor>, AnchorError>> + Send + '_>>;

    /// Synchronous variant of [`get`][Self::get] — see the trait docs for why this exists
    /// alongside the async version.
    ///
    /// # Errors
    ///
    /// Same as [`get`][Self::get].
    fn get_sync(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
    ) -> Result<Option<Anchor>, AnchorError>;

    /// Persist `anchor` for `(subsystem, file_id)`, overwriting any prior anchor for the same
    /// identity.
    ///
    /// Implementations performing blocking I/O (age vault re-encryption) **must** route it
    /// through `zeph_common::task_supervisor::TaskSupervisor::spawn_blocking`, never a raw
    /// `tokio::task::spawn_blocking` or an inline blocking call on the calling task.
    ///
    /// # Errors
    ///
    /// Returns [`AnchorError`] on a store-level failure.
    fn put(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
        anchor: Anchor,
    ) -> Pin<Box<dyn Future<Output = Result<(), AnchorError>> + Send + '_>>;

    /// Remove the anchor for `(subsystem, file_id)`, if present. A no-op (not an error) if no
    /// anchor exists for this identity.
    ///
    /// # Errors
    ///
    /// Returns [`AnchorError`] on a store-level failure.
    fn delete(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), AnchorError>> + Send + '_>>;
}

/// Encode `file_id` bytes into the ASCII-safe segment used inside a vault key.
///
/// `file_id`s in this codebase are always a UUID or a directory/file stem, already restricted to
/// `[A-Za-z0-9._-]` in practice — this is a defense-in-depth guard, not a real-world path: any
/// byte outside that set falls back to a `hex:`-prefixed hex encoding so the resulting key is
/// always safe to embed in a vault key string.
fn encode_file_id(file_id: &[u8]) -> String {
    use std::fmt::Write as _;
    let safe = file_id
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if safe {
        // Safety of the assumption checked above: every byte is ASCII, so this is valid UTF-8.
        String::from_utf8_lossy(file_id).into_owned()
    } else {
        let mut out = String::with_capacity(4 + file_id.len() * 2);
        out.push_str("hex:");
        for b in file_id {
            let _ = write!(out, "{b:02x}");
        }
        out
    }
}

/// Decode an [`encode_file_id`]-produced segment back into raw bytes.
fn decode_file_id(segment: &str) -> Option<Vec<u8>> {
    if let Some(hex) = segment.strip_prefix("hex:") {
        if hex.len() % 2 != 0 {
            return None;
        }
        let mut out = Vec::with_capacity(hex.len() / 2);
        let bytes = hex.as_bytes();
        for chunk in bytes.chunks(2) {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
        }
        Some(out)
    } else {
        Some(segment.as_bytes().to_vec())
    }
}

/// Derive the vault key for a given subsystem + file identity.
///
/// Format: `ZEPH_HISTORY_ANCHOR_<SUBSYSTEM>_<file_id>` (ASCII-safe-encoded, falling back to a
/// `hex:`-prefixed hex encoding for any byte outside `[A-Za-z0-9._-]`).
#[must_use]
pub fn anchor_key(subsystem: AnchorSubsystem, file_id: &[u8]) -> String {
    format!(
        "{ANCHOR_KEY_PREFIX}{}_{}",
        subsystem.key_segment(),
        encode_file_id(file_id)
    )
}

/// Parse a vault key back into `(subsystem, file_id)`, if it matches [`ANCHOR_KEY_PREFIX`].
///
/// Used by the reconcile-and-cap sweep to enumerate anchor keys and map them back to an on-disk
/// identity without needing to track a separate index.
#[must_use]
pub fn parse_anchor_key(key: &str) -> Option<(AnchorSubsystem, Vec<u8>)> {
    let rest = key.strip_prefix(ANCHOR_KEY_PREFIX)?;
    let (subsystem, file_id_segment) = if let Some(id) = rest.strip_prefix("SUBAGENT_") {
        (AnchorSubsystem::SubagentTranscript, id)
    } else {
        let id = rest.strip_prefix("SESSION_")?;
        (AnchorSubsystem::SessionLog, id)
    };
    decode_file_id(file_id_segment).map(|id| (subsystem, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_chain::{ChainKey, chain_next, genesis};

    fn sample_head() -> ChainHash {
        let key = ChainKey::new([1u8; 32]);
        let base = genesis(&key, "d", b"f", 0);
        chain_next(&key, &base, b"content")
    }

    #[test]
    fn anchor_key_round_trips_for_safe_ids() {
        let key = anchor_key(AnchorSubsystem::SubagentTranscript, b"abc-123.task");
        assert_eq!(key, "ZEPH_HISTORY_ANCHOR_SUBAGENT_abc-123.task");
        let (subsystem, id) = parse_anchor_key(&key).unwrap();
        assert_eq!(subsystem, AnchorSubsystem::SubagentTranscript);
        assert_eq!(id, b"abc-123.task");
    }

    #[test]
    fn anchor_key_round_trips_for_unsafe_bytes() {
        let file_id = vec![0xffu8, 0x00, b'/'];
        let key = anchor_key(AnchorSubsystem::SessionLog, &file_id);
        assert!(key.starts_with("ZEPH_HISTORY_ANCHOR_SESSION_hex:"));
        let (subsystem, id) = parse_anchor_key(&key).unwrap();
        assert_eq!(subsystem, AnchorSubsystem::SessionLog);
        assert_eq!(id, file_id);
    }

    #[test]
    fn parse_anchor_key_rejects_unrelated_keys() {
        assert!(parse_anchor_key("ZEPH_OPENAI_API_KEY").is_none());
        assert!(parse_anchor_key("ZEPH_HISTORY_ANCHOR_BOGUS_x").is_none());
    }

    #[test]
    fn anchor_new_stamps_written_at_and_round_trips_head() {
        let head = sample_head();
        let anchor = Anchor::new(3, 42, head);
        assert_eq!(anchor.version, ANCHOR_VERSION);
        assert_eq!(anchor.epoch, 3);
        assert_eq!(anchor.count, 42);
        assert!(anchor.written_at > 0);
        assert_eq!(anchor.head().unwrap(), head);
    }

    #[test]
    fn anchor_serializes_to_json_and_back() {
        let anchor = Anchor::new(0, 7, sample_head());
        let json = serde_json::to_string(&anchor).unwrap();
        let round_tripped: Anchor = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.count, 7);
        assert_eq!(round_tripped.head_hex, anchor.head_hex);
        assert_eq!(round_tripped.written_at, anchor.written_at);
        assert_eq!(round_tripped.orphaned_since, None);
    }

    /// A freshly constructed anchor omits `orphaned_since` from its JSON entirely
    /// (`skip_serializing_if`), so a pre-#6462 vault entry stays byte-identical until first
    /// observed orphaned.
    #[test]
    fn anchor_new_omits_orphaned_since_from_serialized_json() {
        let anchor = Anchor::new(0, 1, sample_head());
        let json = serde_json::to_string(&anchor).unwrap();
        assert!(!json.contains("orphaned_since"));
    }

    /// A pre-#6462 anchor (no `orphaned_since` key at all) deserializes with `None` — no vault
    /// migration required.
    #[test]
    fn anchor_deserializes_legacy_json_without_orphaned_since_field() {
        let legacy_json =
            r#"{"version":1,"epoch":0,"count":3,"head_hex":"ab12","written_at":1000}"#;
        let anchor: Anchor = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(anchor.orphaned_since, None);
    }

    /// A stamped anchor (`orphaned_since = Some(_)`) round-trips its value.
    #[test]
    fn anchor_round_trips_orphaned_since_when_set() {
        let mut anchor = Anchor::new(0, 7, sample_head());
        anchor.orphaned_since = Some(123_456);
        let json = serde_json::to_string(&anchor).unwrap();
        assert!(json.contains("orphaned_since"));
        let round_tripped: Anchor = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.orphaned_since, Some(123_456));
    }

    #[test]
    fn anchor_head_rejects_malformed_hex() {
        let mut anchor = Anchor::new(0, 1, sample_head());
        anchor.head_hex = "not-hex".to_owned();
        assert!(matches!(anchor.head(), Err(AnchorError::Malformed)));
    }
}
