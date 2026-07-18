// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Keyed-BLAKE3 hash-chain primitive for tamper-evident, append-only JSONL history.
//!
//! This module is the shared core behind the transcript-integrity feature (issue #6360):
//! [`zeph-subagent`](../../zeph_subagent/index.html)'s `<task_id>.jsonl` transcripts and
//! [`zeph-session`](../../zeph_session/index.html)'s `events.jsonl` event log both chain their
//! entries with the same primitive, reusing the project's existing keyed-BLAKE3 pattern
//! (`crates/zeph-durable/src/backend/local.rs`'s `compute_control_hmac`,
//! `crates/zeph-core/src/durable.rs`'s `derive_control_hmac_key_b64`) rather than introducing a
//! new cryptographic dependency (constitution VII, spec-069 §8 Always).
//!
//! # The scheme
//!
//! ```text
//! genesis  = blake3::keyed_hash(key, DOMAIN_TAG || file_identity || key_epoch)
//! chain[0] = blake3::keyed_hash(key, genesis   || content_bytes[0])
//! chain[i] = blake3::keyed_hash(key, chain[i-1] || content_bytes[i])
//! ```
//!
//! `content_bytes[i]` is the entry serialized with its own chain field excluded (see the adapter
//! crates for the exact strip-then-reserialize procedure). `DOMAIN_TAG` and `file_identity`
//! (e.g. a `task_id` or `session_id`) bind the chain to one subsystem and one file, so neither a
//! cross-subsystem replay nor a wholesale substitution of one file for another produces a valid
//! chain.
//!
//! # Threat model and honest scope (spec-069 §9, critic rev1-3)
//!
//! This defends against an attacker with filesystem write access but **not** vault access. Such
//! an attacker cannot forge a valid chain link, so **in-place content edits, entry reordering,
//! and a partial strip of chain metadata are always detected** (any chained file with a `chain`
//! field on some but not all of its post-chain-start lines is a hard tamper failure, never a
//! legacy downgrade). A **fully consistent whole-file strip** (delete every `chain` field so the
//! file looks pre-feature-legacy) is a distinct, harder threat: nothing in this module alone
//! defends against it — that requires an anchor stored outside filesystem-write reach (the
//! opt-in/default `integrity.anchor = "vault"` mechanism the adapter crates layer on top, or the
//! deferred P3 external anchor). Do not read this module in isolation as providing
//! downgrade-resistance; see each adapter's module docs for its anchor posture.
//!
//! # Key rotation (FR-008)
//!
//! [`ChainKeyRing`] carries a current epoch and an optional previous epoch so a legitimate
//! single-step key rotation does not turn all pre-rotation history into apparent tamper.
//! [`verify_chained_prefix`] tries the current epoch's genesis first, then the previous epoch's;
//! whichever produces a valid link for the *first* chained entry is used for the rest of the
//! file, and the result is tagged with [`KeyResolution`] so callers can distinguish "re-keyed"
//! from "tampered" in their error reporting (never conflate the two — see spec-069 FR-008).
//!
//! A `key_epoch` that resolves to neither the current nor the previous epoch is
//! [`ChainError::Unverifiable`] — NOT legacy. Per NFR-004, an integrity check that cannot be
//! evaluated must fail, never silently degrade to trusted-legacy; degrading here would let an
//! attacker force legacy trust by writing a bogus epoch (the downgrade lever the critic's
//! rev2/rev3 review closed).

/// 32-byte keyed-BLAKE3 subkey for one subsystem's hash chain, already domain-separated via
/// `blake3::derive_key` from the root `ZEPH_HISTORY_KEY` vault secret by the caller (mirroring
/// `derive_control_hmac_key_b64`).
///
/// Deliberately opaque: [`Debug`] never prints the key material (mirrors the
/// secret-bearing-Debug-derive lesson learned elsewhere in this codebase — a derived `Debug`
/// would leak key bytes into logs/panics).
#[derive(Clone, Copy)]
pub struct ChainKey([u8; 32]);

impl ChainKey {
    /// Wrap raw key bytes as a [`ChainKey`].
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for ChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChainKey(..)")
    }
}

/// A 32-byte chain link value: either a genesis seed or the output of [`chain_next`].
///
/// Not secret (it is stored on disk alongside the content it authenticates), so [`Debug`] and
/// hex encode/decode are unrestricted. Equality compares via [`blake3::Hash`], which is
/// constant-time — the same idiom already used for the promise resolver-token check and
/// `verify_control_hmac`, so a forged stored hash reveals no timing signal.
#[derive(Clone, Copy)]
pub struct ChainHash([u8; 32]);

impl ChainHash {
    /// Render as a lowercase hex string for JSONL storage.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex_encode(&self.0)
    }

    /// Parse a lowercase (or uppercase) hex string produced by [`Self::to_hex`].
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::MalformedHash`] if `s` is not exactly 64 hex characters.
    pub fn from_hex(s: &str) -> Result<Self, ChainError> {
        hex_decode(s).map(Self).ok_or(ChainError::MalformedHash)
    }
}

impl std::fmt::Debug for ChainHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChainHash({})", self.to_hex())
    }
}

impl PartialEq for ChainHash {
    fn eq(&self, other: &Self) -> bool {
        blake3::Hash::from(self.0) == blake3::Hash::from(other.0)
    }
}

impl Eq for ChainHash {}

fn hex_encode(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hex_decode(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

/// Compute the genesis (seed) hash for one chained file.
///
/// `domain` distinguishes subsystems (e.g. `"zeph-subagent transcript v1"`,
/// `"zeph-session log v1"`) so a chain valid in one subsystem can never verify in another.
/// `file_identity` (e.g. a `task_id` or `session_id`, as raw bytes) binds the chain to this one
/// file, so wholesale substitution of one chained file for another (same subsystem) breaks at
/// the first entry. `key_epoch` is folded in so a rotation changes genesis deterministically
/// (FR-008) — see the module docs' Key rotation section.
#[must_use]
pub fn genesis(key: &ChainKey, domain: &str, file_identity: &[u8], key_epoch: u32) -> ChainHash {
    let mut input = Vec::with_capacity(domain.len() + file_identity.len() + 4);
    input.extend_from_slice(domain.as_bytes());
    input.extend_from_slice(file_identity);
    input.extend_from_slice(&key_epoch.to_le_bytes());
    ChainHash(*blake3::keyed_hash(&key.0, &input).as_bytes())
}

/// Compute the next chain link from the previous link and this entry's canonicalized content
/// bytes (the entry serialized with its own chain field excluded — see each adapter for the
/// exact strip-then-reserialize procedure).
#[must_use]
pub fn chain_next(key: &ChainKey, prev: &ChainHash, content: &[u8]) -> ChainHash {
    let mut input = Vec::with_capacity(32 + content.len());
    input.extend_from_slice(&prev.0);
    input.extend_from_slice(content);
    ChainHash(*blake3::keyed_hash(&key.0, &input).as_bytes())
}

/// Errors from computing or verifying a hash chain.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChainError {
    /// A stored hex-encoded chain hash was not exactly 64 hex characters.
    #[error("malformed chain hash (expected 64 hex characters)")]
    MalformedHash,

    /// A chain link recomputed under a **known** key (current or a previous rotation-window
    /// epoch) did not match the stored value at `index` within the chained region — this is a
    /// definite tamper verdict: the key resolved correctly at the first chained entry, so a
    /// later mismatch means the content itself was altered, reordered, or deleted-and-replaced.
    #[error(
        "chain hash mismatch at chained-entry index {index}: content was modified after being written"
    )]
    Mismatch {
        /// Zero-based index within the chained region (not the file's physical line number).
        index: u64,
    },

    /// Neither the current epoch's key nor any previous-epoch key in the rotation window
    /// produced a valid link for the first chained entry. This is genuinely ambiguous — it
    /// could be tamper under an unknown key, or a legitimate session that predates the
    /// rotation window — so it fails closed without asserting either. Per NFR-004 this is
    /// never downgraded to trusted-legacy: only a file with **no** chain metadata anywhere is
    /// legacy.
    #[error(
        "chain is unverifiable: no known key epoch (current or previous rotation window) \
         produces a valid link for this file — possibly re-keyed past the rotation window, \
         or tampered"
    )]
    Unverifiable,

    /// No chain key is available at all (e.g. the vault key was never provisioned, or the
    /// vault is unreachable) for a file that carries chain metadata. Per NFR-004 this is a
    /// failure, never a silent skip.
    #[error("no chain key is available to verify a chained file")]
    KeyUnavailable,
}

/// How a chained file's key epoch was resolved during verification (FR-008): distinguishes a
/// legitimate rotation from tamper so callers can report the two differently rather than
/// misleading an operator into believing a re-keyed (but otherwise intact) file was tampered
/// with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyResolution {
    /// The file verifies under the current epoch's key — the common case.
    Current,
    /// The file verifies under a previous epoch's key still held in the rotation window — a
    /// legitimate rotation, not tamper.
    Rekeyed(u32),
}

/// Current epoch plus an optional previous epoch, both carrying their own domain-separated
/// [`ChainKey`] — the rotation window a chain verification is checked against (FR-008).
///
/// Building the full multi-epoch vault rotation *tooling* (issuing a new epoch, retiring old
/// ones) is spec-056's concern; this type only carries whatever window the caller resolved from
/// the vault at verification time.
#[derive(Clone, Copy)]
pub struct ChainKeyRing {
    current_epoch: u32,
    current_key: ChainKey,
    previous: Option<(u32, ChainKey)>,
}

impl std::fmt::Debug for ChainKeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainKeyRing")
            .field("current_epoch", &self.current_epoch)
            .field("previous_epoch", &self.previous.map(|(epoch, _)| epoch))
            .finish_non_exhaustive()
    }
}

impl ChainKeyRing {
    /// Construct a ring with only a current epoch (no rotation window yet).
    #[must_use]
    pub fn new(current_epoch: u32, current_key: ChainKey) -> Self {
        Self {
            current_epoch,
            current_key,
            previous: None,
        }
    }

    /// Add a previous epoch to the rotation window.
    #[must_use]
    pub fn with_previous(mut self, epoch: u32, key: ChainKey) -> Self {
        self.previous = Some((epoch, key));
        self
    }

    /// The current epoch number. New appends are always written under this epoch.
    #[must_use]
    pub fn current_epoch(&self) -> u32 {
        self.current_epoch
    }

    /// The current epoch's key, for writing new entries.
    #[must_use]
    pub fn current_key(&self) -> ChainKey {
        self.current_key
    }

    /// Every candidate `(epoch, key, resolution)` to try during verification, current epoch
    /// first.
    fn candidates(&self) -> Vec<(u32, ChainKey, KeyResolution)> {
        let mut out = vec![(self.current_epoch, self.current_key, KeyResolution::Current)];
        if let Some((epoch, key)) = self.previous {
            out.push((epoch, key, KeyResolution::Rekeyed(epoch)));
        }
        out
    }
}

/// Incremental, O(1)-memory chain verifier: folds one entry at a time, carrying forward only the
/// last verified hash (NFR-002's "carry forward only the last verified hash as state" shape,
/// reused here for the JSONL adapters even though NFR-002 itself was written for durable's
/// segment reads).
pub struct ChainVerifier {
    key: ChainKey,
    prev: ChainHash,
    index: u64,
}

impl ChainVerifier {
    /// Start a verifier at `genesis` (or at the last verified head, when resuming mid-file).
    #[must_use]
    pub fn new(key: ChainKey, genesis: ChainHash) -> Self {
        Self {
            key,
            prev: genesis,
            index: 0,
        }
    }

    /// Verify one more entry against the running chain state, advancing it on success.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Mismatch`] if the recomputed hash does not equal `stored`.
    pub fn verify_next(&mut self, content: &[u8], stored: &ChainHash) -> Result<(), ChainError> {
        let expected = chain_next(&self.key, &self.prev, content);
        if expected != *stored {
            return Err(ChainError::Mismatch { index: self.index });
        }
        self.prev = expected;
        self.index += 1;
        Ok(())
    }

    /// Compute the next link without advancing state — used by writers to compute the hash to
    /// store for a new append, immediately followed by a manual [`Self::advance`] once the write
    /// is known to have succeeded.
    #[must_use]
    pub fn peek_next(&self, content: &[u8]) -> ChainHash {
        chain_next(&self.key, &self.prev, content)
    }

    /// Advance the running state to `head` after a write using [`Self::peek_next`]'s result has
    /// been durably committed.
    pub fn advance(&mut self, head: ChainHash) {
        self.prev = head;
        self.index += 1;
    }

    /// The current head hash (the last verified, or last advanced-to, link).
    #[must_use]
    pub fn head(&self) -> ChainHash {
        self.prev
    }

    /// How many entries have been verified/advanced so far.
    #[must_use]
    pub fn index(&self) -> u64 {
        self.index
    }
}

/// Streaming chain verifier that resolves the key epoch incrementally from the first chained
/// entry it sees, without requiring the caller to buffer the whole chained region in memory.
///
/// Before the epoch is resolved, every candidate epoch in the [`ChainKeyRing`] is tried against
/// each incoming entry in parallel (at most 2: current + previous — a small, fixed, O(1) memory
/// cost independent of file size); once exactly one candidate's genesis produces a valid link,
/// verification collapses to a single incremental [`ChainVerifier`] for the rest of the stream.
/// This lets [`zeph-session`](../../zeph_session/index.html)'s `read_chunked` verify a
/// replay-trusted log's chain without materializing the whole file — the same bounded-memory
/// shape its chunked read already provides for the torn-tail check.
pub struct ChainStreamVerifier {
    domain: String,
    file_identity: Vec<u8>,
    /// Unresolved candidates, tried against every entry until exactly one survives. Empty once
    /// [`Self::resolved`] is `Some`.
    candidates: Vec<(u32, ChainKey, KeyResolution)>,
    resolved: Option<ChainVerifier>,
    resolution: Option<KeyResolution>,
}

impl ChainStreamVerifier {
    /// Start a streaming verifier for one chained region.
    #[must_use]
    pub fn new(
        ring: &ChainKeyRing,
        domain: impl Into<String>,
        file_identity: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            domain: domain.into(),
            file_identity: file_identity.into(),
            candidates: ring.candidates(),
            resolved: None,
            resolution: None,
        }
    }

    /// Verify the next entry in on-disk order, resolving the key epoch on the first call.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Unverifiable`] on the first call if no candidate epoch's genesis
    /// produces a valid link, or [`ChainError::Mismatch`] on any call once the epoch is resolved
    /// and a later entry breaks the chain.
    pub fn verify_next(&mut self, content: &[u8], stored: &ChainHash) -> Result<(), ChainError> {
        if let Some(verifier) = self.resolved.as_mut() {
            return verifier.verify_next(content, stored);
        }

        let mut survivor = None;
        for (epoch, key, resolution) in &self.candidates {
            let base = genesis(key, &self.domain, &self.file_identity, *epoch);
            if chain_next(key, &base, content) == *stored {
                survivor = Some((*epoch, *key, *resolution));
                break;
            }
        }
        let Some((epoch, key, resolution)) = survivor else {
            return Err(ChainError::Unverifiable);
        };

        let base = genesis(&key, &self.domain, &self.file_identity, epoch);
        let mut verifier = ChainVerifier::new(key, base);
        verifier.verify_next(content, stored)?; // re-derives the same hash just matched; infallible in practice
        self.resolved = Some(verifier);
        self.resolution = Some(resolution);
        self.candidates.clear();
        Ok(())
    }

    /// The verified head hash, once at least one entry has been verified.
    #[must_use]
    pub fn head(&self) -> Option<ChainHash> {
        self.resolved.as_ref().map(ChainVerifier::head)
    }

    /// How the key epoch was resolved, once at least one entry has been verified.
    #[must_use]
    pub fn resolution(&self) -> Option<KeyResolution> {
        self.resolution
    }
}

/// Verify a contiguous run of chained entries already held in memory (the file's chained region
/// — see each adapter for how it locates where legacy content ends and chaining begins). A thin
/// convenience wrapper over [`ChainStreamVerifier`] for callers that already have the whole
/// region as a slice (both JSONL adapters' non-chunked read paths); streaming callers (e.g.
/// bounded-memory chunked reads) should drive [`ChainStreamVerifier`] directly instead.
///
/// `entries` is `(content_bytes, stored_hash)` pairs in on-disk order, content bytes excluding
/// the entry's own chain field.
///
/// # Errors
///
/// Returns [`ChainError::Unverifiable`] if no candidate epoch's genesis produces a valid link
/// for the first entry, or [`ChainError::Mismatch`] if a later entry breaks the chain under the
/// key epoch that verified the first entry.
///
/// # Examples
///
/// ```
/// use zeph_common::hash_chain::{ChainKey, ChainKeyRing, chain_next, genesis, verify_chained_prefix};
///
/// let key = ChainKey::new([7u8; 32]);
/// let ring = ChainKeyRing::new(0, key);
/// let base = genesis(&key, "test v1", b"file-1", 0);
/// let h0 = chain_next(&key, &base, b"line0");
/// let h1 = chain_next(&key, &h0, b"line1");
///
/// let entries = vec![(b"line0".to_vec(), h0), (b"line1".to_vec(), h1)];
/// let (head, _resolution) =
///     verify_chained_prefix(&ring, "test v1", b"file-1", &entries).unwrap();
/// assert_eq!(head, h1);
/// ```
pub fn verify_chained_prefix(
    ring: &ChainKeyRing,
    domain: &str,
    file_identity: &[u8],
    entries: &[(Vec<u8>, ChainHash)],
) -> Result<(ChainHash, KeyResolution), ChainError> {
    let (head, _checkpoint, resolution) =
        verify_chained_prefix_with_checkpoint(ring, domain, file_identity, entries, u64::MAX)?;
    Ok((head, resolution))
}

/// Like [`verify_chained_prefix`], but additionally captures the chain head immediately after
/// entry `checkpoint_index` (0-based within `entries`) is verified — used by the vault-anchor
/// downgrade-resistance mechanism (issue #6449) to compare a stored anchor's head against the
/// file's head at the anchor's recorded count, without re-deriving the chain a second time.
///
/// Returns `(final_head, checkpoint_head, resolution)`. `checkpoint_head` is `None` if
/// `checkpoint_index >= entries.len()` (out of range — including the common case of passing
/// `u64::MAX` from [`verify_chained_prefix`] to opt out of capturing a checkpoint).
///
/// # Errors
///
/// Same as [`verify_chained_prefix`].
pub fn verify_chained_prefix_with_checkpoint(
    ring: &ChainKeyRing,
    domain: &str,
    file_identity: &[u8],
    entries: &[(Vec<u8>, ChainHash)],
    checkpoint_index: u64,
) -> Result<(ChainHash, Option<ChainHash>, KeyResolution), ChainError> {
    if entries.is_empty() {
        // Nothing to verify: an empty chained region has no key to resolve. Callers should not
        // invoke this with an empty slice; treat it as trivially verified at the current epoch.
        return Ok((
            genesis(&ring.current_key, domain, file_identity, ring.current_epoch),
            None,
            KeyResolution::Current,
        ));
    }

    let mut streaming = ChainStreamVerifier::new(ring, domain, file_identity.to_vec());
    let mut checkpoint_head = None;
    for (i, (content, stored)) in entries.iter().enumerate() {
        streaming.verify_next(content, stored)?;
        if i as u64 == checkpoint_index {
            checkpoint_head = streaming.head();
        }
    }
    // Infallible: the loop above verified at least one entry (entries is non-empty), which
    // always sets `resolved`/`resolution` on success.
    let head = streaming
        .head()
        .unwrap_or_else(|| genesis(&ring.current_key, domain, file_identity, ring.current_epoch));
    let resolution = streaming.resolution().unwrap_or(KeyResolution::Current);
    Ok((head, checkpoint_head, resolution))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> ChainKey {
        ChainKey::new([byte; 32])
    }

    #[test]
    fn hex_round_trip() {
        let h = chain_next(&key(1), &genesis(&key(1), "d", b"f", 0), b"content");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        let back = ChainHash::from_hex(&hex).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        assert_eq!(ChainHash::from_hex("abc"), Err(ChainError::MalformedHash));
    }

    #[test]
    fn from_hex_rejects_non_hex() {
        let bad = "z".repeat(64);
        assert_eq!(ChainHash::from_hex(&bad), Err(ChainError::MalformedHash));
    }

    #[test]
    fn genesis_differs_per_domain() {
        let a = genesis(&key(1), "domain-a", b"file", 0);
        let b = genesis(&key(1), "domain-b", b"file", 0);
        assert_ne!(a, b, "cross-subsystem genesis must differ");
    }

    #[test]
    fn genesis_differs_per_file_identity() {
        let a = genesis(&key(1), "d", b"file-a", 0);
        let b = genesis(&key(1), "d", b"file-b", 0);
        assert_ne!(a, b, "whole-file substitution must break at genesis");
    }

    #[test]
    fn genesis_differs_per_epoch() {
        let a = genesis(&key(1), "d", b"file", 0);
        let b = genesis(&key(1), "d", b"file", 1);
        assert_ne!(a, b, "key rotation must change genesis deterministically");
    }

    #[test]
    fn verifier_detects_in_place_edit() {
        let k = key(9);
        let base = genesis(&k, "d", b"f", 0);
        let mut writer = ChainVerifier::new(k, base);
        let h0 = writer.peek_next(b"original");
        writer.advance(h0);

        // Reader recomputes over tampered content but the stored hash is unchanged.
        let mut reader = ChainVerifier::new(k, base);
        let err = reader.verify_next(b"tampered", &h0).unwrap_err();
        assert_eq!(err, ChainError::Mismatch { index: 0 });
    }

    #[test]
    fn verifier_detects_reorder() {
        let k = key(3);
        let base = genesis(&k, "d", b"f", 0);
        let h0 = chain_next(&k, &base, b"a");
        let h1 = chain_next(&k, &h0, b"b");
        let _h2 = chain_next(&k, &h1, b"c");

        // Entry 0 ("a") is untouched, so key-epoch resolution succeeds there. Entries 1 and 2
        // ("b", "c") are then swapped physically while keeping their originally-computed stored
        // hashes — the swapped-in entry's prev-hash no longer matches its actual predecessor,
        // which must surface as a definite tamper (Mismatch), not an ambiguous Unverifiable.
        let entries = vec![(b"a".to_vec(), h0), (b"c".to_vec(), h1)];
        let ring = ChainKeyRing::new(0, k);
        let err = verify_chained_prefix(&ring, "d", b"f", &entries).unwrap_err();
        assert_eq!(err, ChainError::Mismatch { index: 1 });
    }

    #[test]
    fn verify_chained_prefix_happy_path() {
        let k = key(4);
        let ring = ChainKeyRing::new(0, k);
        let base = genesis(&k, "d", b"f", 0);
        let h0 = chain_next(&k, &base, b"a");
        let h1 = chain_next(&k, &h0, b"b");
        let entries = vec![(b"a".to_vec(), h0), (b"b".to_vec(), h1)];
        let (head, resolution) = verify_chained_prefix(&ring, "d", b"f", &entries).unwrap();
        assert_eq!(head, h1);
        assert_eq!(resolution, KeyResolution::Current);
    }

    #[test]
    fn verify_chained_prefix_resolves_previous_epoch_as_rekeyed() {
        let old_key = key(5);
        let new_key = key(6);
        let ring = ChainKeyRing::new(1, new_key).with_previous(0, old_key);

        // File was fully written under the old (epoch 0) key before rotation.
        let base = genesis(&old_key, "d", b"f", 0);
        let h0 = chain_next(&old_key, &base, b"a");
        let entries = vec![(b"a".to_vec(), h0)];

        let (_head, resolution) = verify_chained_prefix(&ring, "d", b"f", &entries).unwrap();
        assert_eq!(resolution, KeyResolution::Rekeyed(0));
    }

    #[test]
    fn verify_chained_prefix_unverifiable_when_no_epoch_matches() {
        let ring = ChainKeyRing::new(0, key(1));
        let wrong = genesis(&key(99), "d", b"f", 0);
        let h0 = chain_next(&key(99), &wrong, b"a");
        let entries = vec![(b"a".to_vec(), h0)];
        let err = verify_chained_prefix(&ring, "d", b"f", &entries).unwrap_err();
        assert_eq!(err, ChainError::Unverifiable);
    }

    #[test]
    fn verify_chained_prefix_mismatch_after_correct_genesis_is_definite_tamper() {
        let k = key(7);
        let ring = ChainKeyRing::new(0, k);
        let base = genesis(&k, "d", b"f", 0);
        let h0 = chain_next(&k, &base, b"a");
        // Second entry's stored hash does not follow h0 at all (forged/dangling).
        let bogus = ChainHash(*blake3::hash(b"not a real chain link").as_bytes());
        let entries = vec![(b"a".to_vec(), h0), (b"b".to_vec(), bogus)];
        let err = verify_chained_prefix(&ring, "d", b"f", &entries).unwrap_err();
        assert_eq!(err, ChainError::Mismatch { index: 1 });
    }

    /// M1 (canonicalization config guard), **corrected during implementation**: the original
    /// design assumed this workspace's `serde_json` has `preserve_order` disabled everywhere
    /// (root `Cargo.toml` alone has no `preserve_order` feature declared, which is what the
    /// critic's rev1-3 reviews checked). Building with the actual CI feature set
    /// (`desktop,ide,server,chat,pdf,scheduler,testing`) and running this exact test empirically
    /// falsified that assumption: `agent-client-protocol`/`agent-client-protocol-schema`
    /// (pulled in by the `acp`/`ide` feature, via `schemars`) and `tree-sitter`'s build script
    /// (via `zeph-common`'s `treesitter` feature) both transitively enable
    /// `serde_json/preserve_order`, and Cargo feature unification makes that apply
    /// workspace-wide to every crate depending on `serde_json` — including this one — for any
    /// build that enables ACP.
    ///
    /// The canonicalization scheme remains sound despite this: it never required *sorted* key
    /// order, only that **serialize → deserialize → serialize reproduces byte-identical
    /// output** (see [`round_trip_serialization_is_byte_identical`], the test that actually
    /// matters and which passes under both `preserve_order` on and off — insertion order is
    /// preserved through a deserialize/reserialize round-trip exactly as faithfully as sorted
    /// order is, since neither backend's iteration order is affected by *which* representation
    /// is compiled in, only *what write path produced the bytes on disk*). This test now
    /// documents that reality directly, instead of asserting the disproven "must be sorted"
    /// claim.
    #[test]
    fn serde_json_value_maps_serialize_deterministically_whichever_backend_is_compiled_in() {
        let mut map = serde_json::Map::new();
        // Insertion order deliberately not sorted.
        map.insert("zebra".to_owned(), serde_json::json!(1));
        map.insert("alpha".to_owned(), serde_json::json!(2));
        map.insert("mango".to_owned(), serde_json::json!(3));
        let value = serde_json::Value::Object(map);

        // Whichever backend is compiled in (sorted `BTreeMap` with `preserve_order` off,
        // insertion-order `IndexMap` with it on — currently on, transitively, for this exact
        // feature set), two consecutive serializations of the same unmutated value must agree.
        let first = serde_json::to_string(&value).unwrap();
        let second = serde_json::to_string(&value).unwrap();
        assert_eq!(
            first, second,
            "serde_json::Value must serialize deterministically for a fixed in-memory value, \
             regardless of which backend (sorted BTreeMap or insertion-order IndexMap) is \
             compiled in — this is the actual invariant canonicalization depends on, not sorted \
             key order (see the corrected M1 note on this test)"
        );
    }

    /// A second determinism fixture: round-tripping a struct through serialize → deserialize →
    /// serialize must reproduce byte-identical output, which is what each adapter's write path
    /// (serialize once to hash, again to store) and read path (deserialize, strip chain,
    /// re-serialize to verify) both depend on.
    #[test]
    fn round_trip_serialization_is_byte_identical() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Fixture {
            seq: u64,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            chain: Option<String>,
            payload: serde_json::Value,
        }

        let mut map = serde_json::Map::new();
        map.insert("z".to_owned(), serde_json::json!("last"));
        map.insert("a".to_owned(), serde_json::json!("first"));
        map.insert(
            "big".to_owned(),
            serde_json::json!(9_007_199_254_740_993u64),
        ); // > 2^53

        let original = Fixture {
            seq: 5,
            chain: None,
            payload: serde_json::Value::Object(map),
        };
        let bytes1 = serde_json::to_vec(&original).unwrap();
        let round_tripped: Fixture = serde_json::from_slice(&bytes1).unwrap();
        let bytes2 = serde_json::to_vec(&round_tripped).unwrap();
        assert_eq!(
            bytes1, bytes2,
            "serialize -> deserialize -> serialize must be byte-identical for canonicalization \
             to be sound"
        );
        // u64 > 2^53 must round-trip losslessly — this is exactly what JCS (serde_json_canonicalizer)
        // would lose (it normalizes to ES6 f64), which is why this module deliberately does NOT
        // adopt it (M1).
        assert_eq!(
            round_tripped.payload.get("big").unwrap(),
            &serde_json::json!(9_007_199_254_740_993u64)
        );
    }

    #[test]
    fn chain_stream_verifier_matches_whole_slice_verification() {
        let k = key(11);
        let ring = ChainKeyRing::new(0, k);
        let base = genesis(&k, "d", b"f", 0);
        let h0 = chain_next(&k, &base, b"a");
        let h1 = chain_next(&k, &h0, b"b");
        let h2 = chain_next(&k, &h1, b"c");
        let entries = vec![
            (b"a".to_vec(), h0),
            (b"b".to_vec(), h1),
            (b"c".to_vec(), h2),
        ];

        let (whole_head, whole_res) = verify_chained_prefix(&ring, "d", b"f", &entries).unwrap();

        // Feed the same entries one at a time, simulating a bounded-memory chunked reader.
        let mut streaming = ChainStreamVerifier::new(&ring, "d", b"f".to_vec());
        for (content, stored) in &entries {
            streaming.verify_next(content, stored).unwrap();
        }
        assert_eq!(streaming.head(), Some(whole_head));
        assert_eq!(streaming.resolution(), Some(whole_res));
    }

    #[test]
    fn verify_chained_prefix_with_checkpoint_captures_intermediate_head() {
        let k = key(13);
        let ring = ChainKeyRing::new(0, k);
        let base = genesis(&k, "d", b"f", 0);
        let h0 = chain_next(&k, &base, b"a");
        let h1 = chain_next(&k, &h0, b"b");
        let h2 = chain_next(&k, &h1, b"c");
        let entries = vec![
            (b"a".to_vec(), h0),
            (b"b".to_vec(), h1),
            (b"c".to_vec(), h2),
        ];

        let (final_head, checkpoint, _res) =
            verify_chained_prefix_with_checkpoint(&ring, "d", b"f", &entries, 1).unwrap();
        assert_eq!(final_head, h2);
        assert_eq!(checkpoint, Some(h1), "checkpoint at index 1 must be h1");

        let (_final, out_of_range, _res) =
            verify_chained_prefix_with_checkpoint(&ring, "d", b"f", &entries, 99).unwrap();
        assert_eq!(out_of_range, None, "an out-of-range checkpoint is None");
    }

    #[test]
    fn chain_stream_verifier_detects_tamper_mid_stream() {
        let k = key(12);
        let ring = ChainKeyRing::new(0, k);
        let base = genesis(&k, "d", b"f", 0);
        let h0 = chain_next(&k, &base, b"a");

        let mut streaming = ChainStreamVerifier::new(&ring, "d", b"f".to_vec());
        streaming.verify_next(b"a", &h0).unwrap();
        // Second entry's stored hash does not follow h0 at all.
        let bogus = ChainHash(*blake3::hash(b"forged").as_bytes());
        let err = streaming.verify_next(b"b", &bogus).unwrap_err();
        assert_eq!(err, ChainError::Mismatch { index: 1 });
    }

    #[test]
    fn chain_key_debug_does_not_leak_key_material() {
        let k = key(0xAB);
        let debug = format!("{k:?}");
        assert!(
            !debug.contains("171"),
            "ChainKey Debug must not print key bytes"
        );
        assert_eq!(debug, "ChainKey(..)");
    }
}
