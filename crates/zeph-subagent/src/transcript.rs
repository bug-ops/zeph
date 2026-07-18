// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSONL-based transcript persistence for sub-agent conversations.
//!
//! Each sub-agent session writes a `<task_id>.jsonl` file of [`TranscriptEntry`] lines
//! and a companion `<task_id>.meta.json` sidecar with [`TranscriptMeta`].
//!
//! Files are created with `0o600` permissions on Unix to prevent other users from
//! reading conversation history.
//!
//! The [`sweep_old_transcripts`] function prunes the oldest `.jsonl` files when a
//! configurable maximum count is exceeded.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock as StdRwLock};

use serde::{Deserialize, Serialize};
use zeph_common::hash_chain::{
    ChainHash, ChainKeyRing, KeyResolution, chain_next, genesis, verify_chained_prefix,
};
use zeph_llm::provider::{Message, MessagePart};

use super::error::SubAgentError;
use super::state::SubAgentState;

/// Domain-separation tag for this subsystem's hash chain (issue #6360) — distinct from
/// `zeph-session`'s so a chain from one subsystem can never verify against the other, and folded
/// into every genesis hash via [`zeph_common::hash_chain::genesis`].
pub const CHAIN_DOMAIN: &str = "zeph-subagent transcript v1";

/// Process-wide history-chain key ring, configured once at bootstrap by resolving
/// `ZEPH_HISTORY_KEY` from the vault (see `zeph_core::history_integrity`).
///
/// # Why a process-global registry, not a constructor parameter
///
/// `TranscriptWriter::new`/[`TranscriptReader::load`] are called from 3+ call sites across
/// crates outside this feature's ownership (`zeph-core`'s scheduler loop and subagent-plan
/// tests, in addition to `zeph-subagent::manager::collect`), and `PayloadCipher`-style explicit
/// `Option<Arc<dyn _>>` injection into every one of those call sites was judged too invasive for
/// this change (would require touching crates outside this PR's scope during a period other
/// teammates are also editing them). A `RwLock` (not `OnceLock`) is used deliberately so tests
/// in this crate and its callers can reconfigure it per-test rather than being limited to a
/// single process-lifetime value — see `configure_history_integrity`'s doc for the tradeoff this
/// accepts. Flagged in the implementation handoff for critic/reviewer scrutiny as a deviation
/// from the codebase's usual per-call dependency injection pattern.
static HISTORY_INTEGRITY: StdRwLock<Option<Arc<ChainKeyRing>>> = StdRwLock::new(None);

/// Configure (or disable, with `None`) history-chain verification for every
/// [`TranscriptWriter`]/[`TranscriptReader`] operation in this process from this point forward.
///
/// Call once at process bootstrap after resolving `ZEPH_HISTORY_KEY` from the vault (see
/// `zeph_core::history_integrity::resolve_key_ring`). Passing `None` — the default until this is
/// called — disables chain computation/verification entirely: writers append unchained entries
/// (as before this feature existed) and readers treat every file as legacy. This is the
/// generate-on-first-use / vault-unavailable fallback posture (spec-069 M2): a transient vault
/// outage degrades to unchained rather than blocking every transcript write.
///
/// # Invariant: single-set-at-startup
///
/// This is `pub` (not `pub(crate)`) specifically so `src/runner.rs` — a different crate from
/// this one — can call it once during CLI bootstrap, before any transcript is written or read
/// (see `configure_history_integrity_from_default_vault` in `src/runner.rs`). It is **not**
/// meant to be called again later by production code: reconfiguring mid-process cannot make an
/// already-constructed `TranscriptWriter` less safe (each writer captures `ring` at construction
/// and is immune to later reconfiguration, and setting `ring = None` only ever makes
/// *subsequent* reads fail-closed on a chained file, never trust-bypassing), but a caller
/// reconfiguring after bootstrap without a clear reason is almost certainly a bug, not an
/// intended feature — no production code path does this today, and none should be added without
/// updating this doc. Tests are the one legitimate exception, calling this per-test under
/// `cargo nextest`'s one-process-per-test isolation.
pub fn configure_history_integrity(ring: Option<Arc<ChainKeyRing>>) {
    if let Ok(mut guard) = HISTORY_INTEGRITY.write() {
        *guard = ring;
    }
}

fn history_integrity() -> Option<Arc<ChainKeyRing>> {
    HISTORY_INTEGRITY.read().ok().and_then(|g| g.clone())
}

/// Derive a transcript file's chain identity from its path (the `task_id`, e.g. `"abc123"` from
/// `"abc123.jsonl"`) — binds the chain to this one file so a whole-file substitution (swapping
/// in another task's transcript) breaks at the genesis hash.
fn file_identity(path: &Path) -> Vec<u8> {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .into_bytes()
}

/// Paths already warned about via [`warn_legacy_under_active_key_once`] this process — kept
/// small (one entry per distinct transcript path actually read while chaining-disabled, not
/// per-read) so a session's history isn't re-warned every time it's reloaded.
static WARNED_LEGACY_UNDER_KEY: std::sync::LazyLock<StdRwLock<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| StdRwLock::new(std::collections::HashSet::new()));

/// Log a structured `WARN` the first time a given path is found to be pure-legacy (no `chain`
/// field anywhere) while a history-integrity key ring IS configured (issue #6360, security
/// review B2 condition (c)).
///
/// Deliberately `WARN`, not a hard failure: a chainless file under an active key is *anomalous*
/// but not distinguishable from genuine pre-upgrade content without the vault anchor (#6449) —
/// this exists purely to make that anomaly observable instead of silent. Deduplicated per path
/// (not per read) to avoid alert fatigue on the many genuinely-legacy files that exist right
/// after upgrading to this feature.
fn warn_legacy_under_active_key_once(path: &Path) {
    let already_warned = WARNED_LEGACY_UNDER_KEY
        .read()
        .is_ok_and(|set| set.contains(path));
    if already_warned {
        return;
    }
    if let Ok(mut set) = WARNED_LEGACY_UNDER_KEY.write()
        && !set.insert(path.to_path_buf())
    {
        return; // another thread warned first between the read and write locks
    }
    tracing::warn!(
        path = %path.display(),
        "history-chain integrity: transcript classifies as legacy (no chain field anywhere) \
         while a history-integrity key IS configured for this process — this is expected for \
         genuine pre-upgrade content, but is also the signature of a full chain-strip downgrade \
         attack (issue #6449, the vault-anchor gap); accepted per FR-006, flagged for operator \
         visibility"
    );
}

/// A single entry in a JSONL transcript file.
///
/// Each line in `<task_id>.jsonl` deserializes to a `TranscriptEntry`.
/// Entries are written in append order; `seq` is a monotonically increasing counter
/// within a single session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// Zero-based sequence number within the session.
    pub seq: u32,
    /// ISO 8601 UTC timestamp at the time of writing (e.g. `"2026-04-09T12:00:00Z"`).
    pub timestamp: String,
    /// The LLM message that was appended at this sequence position.
    pub message: Message,
    /// Keyed-BLAKE3 hash chain link (hex-encoded), binding this entry's content and the
    /// previous entry's hash (issue #6360). `None` on every entry means this transcript
    /// predates the feature or history-chain verification is disabled for this process
    /// (legacy, auto-trusted-once per spec-069 FR-006). Additive field: `#[serde(default)]`
    /// means an older reader/writer that doesn't know this field ignores it, and legacy files
    /// without it parse unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
}

/// Sidecar metadata for a transcript, written as `<agent_id>.meta.json`.
///
/// The sidecar is written twice: once at spawn time with `status: Submitted` and
/// again at collection time with the final terminal state and `finished_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptMeta {
    /// UUID of this sub-agent session.
    pub agent_id: String,
    /// Runtime agent name (same as `def_name` for non-resumed sessions).
    pub agent_name: String,
    /// Name of the [`SubAgentDef`][crate::SubAgentDef] that was used.
    pub def_name: String,
    /// Terminal lifecycle state recorded at collection time.
    pub status: SubAgentState,
    /// ISO 8601 UTC timestamp when the session was spawned.
    pub started_at: String,
    /// ISO 8601 UTC timestamp when the session finished, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// ID of the original agent session this was resumed from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    /// Number of LLM turns consumed by the session.
    pub turns_used: u32,
    /// MCP tool names available when this session was spawned.
    ///
    /// Persisted so that a resumed session can restore the same tool name annotations
    /// in its system prompt without re-connecting MCP servers.
    #[serde(default)]
    pub mcp_tool_names: Vec<String>,
}

/// Appends [`TranscriptEntry`] lines to a JSONL transcript file.
///
/// The file handle is kept open for the writer's lifetime to avoid
/// race conditions from repeated open/close cycles. The handle is wrapped in
/// `Arc<Mutex<File>>` so the writer can be cheaply cloned and passed to
/// `tokio::task::spawn_blocking` for non-blocking appends.
///
/// # Examples
///
/// ```rust,no_run
/// use std::path::Path;
/// use zeph_subagent::transcript::TranscriptWriter;
///
/// let writer = TranscriptWriter::new(Path::new("/tmp/session.jsonl")).unwrap();
/// // writer.append(seq, &message) to persist each message.
/// ```
struct TranscriptWriteState {
    file: File,
    /// Running chain head. `None` until either the first chained append in this writer's
    /// lifetime (fresh chaining start on a legacy or empty file) or seeded from the file's
    /// existing chained tail at open time (M3, see [`TranscriptWriter::new`]).
    prev: Option<ChainHash>,
}

#[derive(Clone)]
pub struct TranscriptWriter {
    /// `file` and `prev` share one lock so the chain-link read-modify-write is always atomic
    /// with the physical write (S2, issue #6360 critic rev2): two concurrent `append` calls via
    /// `spawn_blocking` can never compute their chain link in one order but land their physical
    /// writes in another, which would desynchronize on-disk order from chain order and produce
    /// a false tamper verdict on read.
    state: Arc<Mutex<TranscriptWriteState>>,
    file_identity: Vec<u8>,
    /// Captured once at construction so every `append` on this writer instance uses one
    /// consistent key ring, even if `configure_history_integrity` is called again concurrently
    /// (which only affects writers/readers constructed afterward).
    ring: Option<Arc<ChainKeyRing>>,
}

impl TranscriptWriter {
    /// Create (or open) a JSONL transcript file in append mode.
    ///
    /// Creates parent directories if they do not already exist. If the file already has content
    /// and history-chain verification is configured (see [`configure_history_integrity`]), the
    /// existing content is scanned and its chain verified before the writer is returned (M3
    /// open-time tail verify/seed) — a writer can never open atop content it hasn't itself
    /// verified, and the running chain state (`prev`) is seeded from the verified tail so the
    /// very next append continues the existing chain rather than restarting it.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the directory cannot be created, the file cannot be opened, or
    /// (per NFR-004) the existing content fails chain verification — a broken chain must never
    /// be silently opened past.
    pub fn new(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let ring = history_integrity();
        let identity = file_identity(path);

        let prev = if path.exists() {
            let entries =
                parse_entries(path, false).map_err(|e| io::Error::other(e.to_string()))?;
            let (_messages, head) = verify_and_extract_messages(path, entries, ring.as_deref())
                .map_err(|e| io::Error::other(e.to_string()))?;
            head
        } else {
            None
        };

        let file = zeph_common::fs_secure::append_private(path)?;
        Ok(Self {
            state: Arc::new(Mutex::new(TranscriptWriteState { file, prev })),
            file_identity: identity,
            ring,
        })
    }

    /// Append a single message as a JSON line and flush immediately.
    ///
    /// `MessagePart::Image` parts are stripped (via [`MessagePart::strip_images`]) from the
    /// persisted copy before serialization — they are ephemeral, current-turn-only vision input
    /// (spec-072 §4, C1) and must never reach a transcript file on disk, mirroring the strip point
    /// already enforced for `Agent::persist_message`'s `SQLite`/Qdrant/durable-JSONL writers. The
    /// caller's `message` is untouched, so callers that hold onto it for the current turn's
    /// provider request keep their `Image` parts.
    ///
    /// When history-chain verification is configured, the chain-link read-modify-write,
    /// canonicalization (serialize with `chain: None`, hash, then serialize again with the
    /// computed hash), physical write, and flush all happen inside the same
    /// `tokio::task::spawn_blocking` critical section, under the single lock guarding both the
    /// file handle and the running chain state (S2) — so on-disk order always matches chain
    /// order even under concurrent `append` calls from a cloned writer.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on serialization, write failure, lock poison, or thread-pool panic.
    pub async fn append(&self, seq: u32, message: &Message) -> io::Result<()> {
        let mut persisted_message = message.clone();
        persisted_message.parts = MessagePart::strip_images(&persisted_message.parts);
        let timestamp = utc_now();
        let state = Arc::clone(&self.state);
        let ring = self.ring.clone();
        let identity = self.file_identity.clone();

        tokio::task::spawn_blocking(move || {
            let mut guard = state
                .lock()
                .map_err(|_| io::Error::other("transcript writer lock poisoned"))?;

            let mut entry = TranscriptEntry {
                seq,
                timestamp,
                message: persisted_message,
                chain: None,
            };

            let new_head = match ring.as_deref() {
                Some(ring) => {
                    let content = serde_json::to_vec(&entry)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    let base = guard.prev.unwrap_or_else(|| {
                        genesis(
                            &ring.current_key(),
                            CHAIN_DOMAIN,
                            &identity,
                            ring.current_epoch(),
                        )
                    });
                    let h = chain_next(&ring.current_key(), &base, &content);
                    entry.chain = Some(h.to_hex());
                    Some(h)
                }
                None => None,
            };

            let line = serde_json::to_string(&entry)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            guard.file.write_all(line.as_bytes())?;
            guard.file.write_all(b"\n")?;
            guard.file.flush()?;

            // Only advance the running chain state after the write+flush succeeded — a failed
            // write must not desynchronize `prev` from what is actually durable on disk.
            if let Some(h) = new_head {
                guard.prev = Some(h);
            }
            Ok(())
        })
        .await
        .map_err(|e| io::Error::other(format!("spawn_blocking panicked: {e}")))?
    }

    /// Write the meta sidecar file for an agent.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on serialization or write failure.
    pub fn write_meta(dir: &Path, agent_id: &str, meta: &TranscriptMeta) -> io::Result<()> {
        let path = dir.join(format!("{agent_id}.meta.json"));
        let content = serde_json::to_string_pretty(meta)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        zeph_common::fs_secure::write_private(&path, content.as_bytes())
    }

    /// Async variant of [`write_meta`][Self::write_meta] that offloads the blocking FS write
    /// to a `spawn_blocking` thread so the Tokio executor is not stalled.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on serialization, write failure, or thread-pool panic.
    pub async fn write_meta_async(
        dir: &Path,
        agent_id: &str,
        meta: &TranscriptMeta,
    ) -> io::Result<()> {
        let path = dir.join(format!("{agent_id}.meta.json"));
        let content = serde_json::to_string_pretty(meta)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let bytes = content.into_bytes();
        tokio::task::spawn_blocking(move || zeph_common::fs_secure::write_private(&path, &bytes))
            .await
            .map_err(|e| io::Error::other(format!("spawn_blocking panicked: {e}")))?
    }
}

/// Reads and reconstructs message history from JSONL transcript files.
///
/// `TranscriptReader` is a zero-size marker type with only associated functions.
/// Use [`TranscriptReader::load`] to reconstruct a message history from a `.jsonl` file,
/// [`TranscriptReader::load_meta`] to read the companion `.meta.json` sidecar, and
/// [`TranscriptReader::find_by_prefix`] to resolve a short ID prefix to a full UUID.
pub struct TranscriptReader;

impl TranscriptReader {
    /// Load all messages from a JSONL transcript file.
    ///
    /// Malformed lines are skipped with a warning. An empty or missing file
    /// returns an empty `Vec`. If the file does not exist at all but a matching
    /// `.meta.json` sidecar exists, returns `SubAgentError::Transcript` with a
    /// clear message so the caller knows the data is gone rather than silently
    /// degrading to a fresh start.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Transcript`] on unrecoverable I/O failures, or
    /// when the transcript file is missing but meta exists (data-loss guard).
    pub fn load(path: &Path) -> Result<Vec<Message>, SubAgentError> {
        Self::load_impl(path, false)
    }

    /// Load all messages from a JSONL transcript file, failing closed on the first skipped line.
    ///
    /// Unlike [`load`][Self::load], which tolerates an unreadable or malformed line by skipping
    /// it with a warning and returning the surviving entries as `Ok`, `load_strict` returns
    /// `SubAgentError::Transcript` the moment any line would be skipped. Callers that must be
    /// able to distinguish a genuinely complete trace from a partial one — e.g. tool-call
    /// grounding, where a silently dropped `ToolUse` entry would misrepresent a partial read as
    /// an authoritative "no tool ran" trace — should use this instead of [`load`][Self::load].
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Transcript`] if any line is unreadable or fails to parse, or if
    /// the file is missing but a meta sidecar exists (data-loss guard, same as
    /// [`load`][Self::load]).
    pub fn load_strict(path: &Path) -> Result<Vec<Message>, SubAgentError> {
        Self::load_impl(path, true)
    }

    fn load_impl(path: &Path, strict: bool) -> Result<Vec<Message>, SubAgentError> {
        if !path.exists() {
            // Check if a meta sidecar exists — if so, data has been lost.
            // Build meta path from the file stem (e.g. "abc" from "abc.jsonl")
            // so it is consistent with write_meta which uses format!("{agent_id}.meta.json").
            let meta_path = if let (Some(parent), Some(stem)) = (path.parent(), path.file_stem()) {
                parent.join(format!("{}.meta.json", stem.to_string_lossy()))
            } else {
                path.with_extension("meta.json")
            };
            if meta_path.exists() {
                return Err(SubAgentError::Transcript(format!(
                    "transcript file '{}' is missing but meta sidecar exists — \
                     transcript data may have been deleted",
                    path.display()
                )));
            }
            return Ok(vec![]);
        }

        let entries = parse_entries(path, strict)?;
        let ring = history_integrity();
        let (messages, _head) = verify_and_extract_messages(path, entries, ring.as_deref())?;
        Ok(messages)
    }

    /// Load the meta sidecar for an agent.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the file does not exist,
    /// [`SubAgentError::Transcript`] on parse failure.
    pub fn load_meta(dir: &Path, agent_id: &str) -> Result<TranscriptMeta, SubAgentError> {
        let path = dir.join(format!("{agent_id}.meta.json"));
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                SubAgentError::NotFound(agent_id.to_owned())
            } else {
                SubAgentError::Transcript(format!("failed to read meta '{}': {e}", path.display()))
            }
        })?;
        serde_json::from_str(&content).map_err(|e| {
            SubAgentError::Transcript(format!("failed to parse meta '{}': {e}", path.display()))
        })
    }

    /// Find the full agent ID by scanning `dir` for `.meta.json` files whose names
    /// start with `prefix`.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if no match is found,
    /// [`SubAgentError::AmbiguousId`] if multiple matches are found,
    /// [`SubAgentError::Transcript`] on I/O failure.
    pub fn find_by_prefix(dir: &Path, prefix: &str) -> Result<String, SubAgentError> {
        let entries = fs::read_dir(dir).map_err(|e| {
            SubAgentError::Transcript(format!(
                "failed to read transcript dir '{}': {e}",
                dir.display()
            ))
        })?;

        let mut matches: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| SubAgentError::Transcript(format!("failed to read dir entry: {e}")))?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(agent_id) = name_str.strip_suffix(".meta.json")
                && agent_id.starts_with(prefix)
            {
                matches.push(agent_id.to_owned());
            }
        }

        match matches.len() {
            0 => Err(SubAgentError::NotFound(prefix.to_owned())),
            1 => Ok(matches.remove(0)),
            n => Err(SubAgentError::AmbiguousId(prefix.to_owned(), n)),
        }
    }
}

/// Open and parse every line of an existing transcript file into [`TranscriptEntry`] values,
/// applying the same read/parse leniency [`TranscriptReader::load`]/[`TranscriptReader::load_strict`]
/// use (`strict` fails on the first unreadable/malformed line; lenient warns and skips it).
///
/// Assumes `path` exists — callers needing the "missing file" / "meta sidecar exists"
/// disambiguation must check that first (see [`TranscriptReader::load_impl`]).
///
/// Note this is purely JSON-syntax leniency, unrelated to chain verification: chain breaks
/// always escalate to a hard error in both modes (Q3, see [`verify_and_extract_messages`]).
///
/// # Errors
///
/// Returns [`SubAgentError::Transcript`] if the file cannot be opened, or if `strict` and any
/// line is unreadable or fails to parse as JSON.
fn parse_entries(path: &Path, strict: bool) -> Result<Vec<TranscriptEntry>, SubAgentError> {
    let file = File::open(path).map_err(|e| {
        SubAgentError::Transcript(format!(
            "failed to open transcript '{}': {e}",
            path.display()
        ))
    })?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for (line_no, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                if strict {
                    return Err(SubAgentError::Transcript(format!(
                        "failed to read transcript '{}' line {}: {e}",
                        path.display(),
                        line_no + 1
                    )));
                }
                tracing::warn!(
                    path = %path.display(),
                    line = line_no + 1,
                    error = %e,
                    "failed to read transcript line — skipping"
                );
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<TranscriptEntry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                if strict {
                    return Err(SubAgentError::Transcript(format!(
                        "malformed transcript entry in '{}' line {}: {e}",
                        path.display(),
                        line_no + 1
                    )));
                }
                tracing::warn!(
                    path = %path.display(),
                    line = line_no + 1,
                    error = %e,
                    "malformed transcript entry — skipping"
                );
            }
        }
    }
    Ok(entries)
}

/// Walk a transcript's parsed entries, verifying the hash chain over the chained region
/// (spec-069 FR-001/FR-002) and returning the trusted messages plus the verified head hash (used
/// by [`TranscriptWriter::new`]'s open-time seeding, M3).
///
/// The **legacy prefix** — entries before the first one carrying a `chain` field — is
/// auto-trusted-once (FR-006 Q2): best-effort, unverified, exactly as this transcript format
/// behaved before this feature existed. A file with no `chain` field anywhere is pure legacy;
/// its messages are returned with `head = None` and no key is required.
///
/// Once a `chain` field appears, **every subsequent entry MUST also carry one**: a missing field
/// after the chained region starts is a partial strip, not a legacy tail, and is a hard tamper
/// failure (critic C1) — this check runs regardless of the caller's lenient/strict JSON-parsing
/// mode, because a chain break invalidates trust in everything downstream of it, unlike a single
/// malformed line (Q3).
///
/// # Errors
///
/// Returns [`SubAgentError::Integrity`] when: the file carries chain metadata but no
/// history-integrity key ring is configured (`ring.is_none()`, NFR-004 — never silently treated
/// as legacy); a partial strip is detected; or [`verify_chained_prefix`] reports a definite
/// tamper ([`ChainError::Mismatch`]) or an unverifiable/possibly-re-keyed chain
/// ([`ChainError::Unverifiable`]).
fn verify_and_extract_messages(
    path: &Path,
    entries: Vec<TranscriptEntry>,
    ring: Option<&ChainKeyRing>,
) -> Result<(Vec<Message>, Option<ChainHash>), SubAgentError> {
    let Some(chain_start) = entries.iter().position(|e| e.chain.is_some()) else {
        // Pure legacy file: no chain metadata anywhere. Auto-trusted per FR-006 — but if a key
        // ring IS configured, every legitimately-written file since this process started should
        // carry a chain field, so a chainless file under an active key is anomalous: either
        // genuine pre-upgrade content, or the signature of a full-strip downgrade attack (issue
        // #6449, the vault-anchor gap that would otherwise catch this). Not distinguishable from
        // here, so this stays accepted (never a hard failure) — but it must be observable, not
        // silent (security review B2 condition, NFR-005).
        if ring.is_some() {
            warn_legacy_under_active_key_once(path);
        }
        return Ok((entries.into_iter().map(|e| e.message).collect(), None));
    };

    for (offset, entry) in entries[chain_start..].iter().enumerate() {
        if entry.chain.is_none() {
            return Err(SubAgentError::Integrity(format!(
                "transcript '{}' entry at chained-region position {offset} is missing its \
                 chain field while earlier entries in this file are chained — partial strip \
                 detected, TAMPER DETECTED",
                path.display()
            )));
        }
    }

    let Some(ring) = ring else {
        return Err(SubAgentError::Integrity(format!(
            "transcript '{}' carries chain metadata but no history-integrity key is configured \
             for this process — refusing to trust it unverified (NFR-004)",
            path.display()
        )));
    };

    let mut chained: Vec<(Vec<u8>, ChainHash)> = Vec::with_capacity(entries.len() - chain_start);
    for entry in &entries[chain_start..] {
        let stored_hex = entry.chain.as_deref().unwrap_or_default();
        let stored = ChainHash::from_hex(stored_hex).map_err(|_| {
            SubAgentError::Integrity(format!(
                "transcript '{}' has a malformed chain hash",
                path.display()
            ))
        })?;
        let mut stripped = entry.clone();
        stripped.chain = None;
        let content = serde_json::to_vec(&stripped).map_err(|e| {
            SubAgentError::Transcript(format!("failed to canonicalize transcript entry: {e}"))
        })?;
        chained.push((content, stored));
    }

    let identity = file_identity(path);
    let (head, resolution) = verify_chained_prefix(ring, CHAIN_DOMAIN, &identity, &chained)
        .map_err(|e| describe_chain_error(path, &e))?;

    if let KeyResolution::Rekeyed(epoch) = resolution {
        tracing::info!(
            path = %path.display(),
            epoch,
            "transcript verified under a previous key epoch (re-keyed, not tampered)"
        );
    }

    let messages = entries.into_iter().map(|e| e.message).collect();
    Ok((messages, Some(head)))
}

/// Render a [`ChainError`] as a [`SubAgentError::Integrity`] with operator-actionable wording
/// that distinguishes a definite tamper verdict from an ambiguous/possibly-re-keyed one (FR-008
/// — an operator must not be misled into believing a re-keyed transcript was tampered with).
fn describe_chain_error(path: &Path, err: &zeph_common::hash_chain::ChainError) -> SubAgentError {
    use zeph_common::hash_chain::ChainError;
    match err {
        ChainError::Unverifiable => SubAgentError::Integrity(format!(
            "transcript '{}' is unverifiable: no known key epoch (current or previous rotation \
             window) produces a valid chain — possibly re-keyed past the rotation window, or \
             tampered; this is fail-closed by design (NFR-004) and cannot be auto-recovered",
            path.display()
        )),
        ChainError::Mismatch { index } => SubAgentError::Integrity(format!(
            "TAMPER DETECTED in transcript '{}': chain hash mismatch at chained-entry index \
             {index} — content was modified, reordered, or deleted after being written",
            path.display()
        )),
        other => SubAgentError::Integrity(format!(
            "transcript '{}' failed chain verification: {other}",
            path.display()
        )),
    }
}

/// Delete the oldest `.jsonl` files in `dir` when the count exceeds `max_files`.
///
/// Files are sorted by modification time (oldest first). Returns the number of
/// files deleted.
///
/// # Errors
///
/// Returns `io::Error` if the directory cannot be read or a file cannot be deleted.
pub fn sweep_old_transcripts(dir: &Path, max_files: usize) -> io::Result<usize> {
    if max_files == 0 {
        return Ok(0);
    }

    // Create the directory if it does not exist yet (first run).
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        return Ok(0);
    }

    let mut jsonl_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            jsonl_files.push((path, mtime));
        }
    }

    if jsonl_files.len() <= max_files {
        return Ok(0);
    }

    // Sort oldest first.
    jsonl_files.sort_by_key(|(_, mtime)| *mtime);

    let to_delete = jsonl_files.len() - max_files;
    let mut deleted = 0;
    for (path, _) in jsonl_files.into_iter().take(to_delete) {
        // Also remove the companion .meta.json sidecar if present.
        let meta = path.with_extension("meta.json");
        if meta.exists() {
            let _ = fs::remove_file(&meta);
        }
        fs::remove_file(&path)?;
        deleted += 1;
    }
    Ok(deleted)
}

/// Returns the current UTC time as an ISO 8601 string (`"YYYY-MM-DDTHH:MM:SSZ"`).
#[must_use]
pub(crate) fn utc_now() -> String {
    // Use SystemTime for a zero-dependency ISO 8601 timestamp.
    // Format: 2026-03-05T00:18:16Z
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = epoch_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert Unix epoch seconds to (year, month, day, hour, minute, second).
///
/// Uses the proleptic Gregorian calendar algorithm (Fliegel-Van Flandern variant).
/// All values are u64 throughout to avoid truncating casts; the caller knows values
/// fit in u32 for the ranges used (years 1970–2554, seconds/minutes/hours/days).
fn epoch_to_parts(epoch: u64) -> (u32, u32, u32, u32, u32, u32) {
    let sec = epoch % 60;
    let epoch = epoch / 60;
    let min = epoch % 60;
    let epoch = epoch / 60;
    let hour = epoch % 24;
    let days = epoch / 24;

    // Days since 1970-01-01 → civil calendar (Gregorian).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    // All values are in range for u32 for any timestamp in [1970, 2554].
    #[allow(clippy::cast_possible_truncation)]
    (
        year as u32,
        month as u32,
        day as u32,
        hour as u32,
        min as u32,
        sec as u32,
    )
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use zeph_llm::provider::{ImageData, Message, MessageMetadata, MessagePart, Role};

    use super::*;

    fn test_message(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn test_meta(agent_id: &str) -> TranscriptMeta {
        TranscriptMeta {
            agent_id: agent_id.to_owned(),
            agent_name: "bot".to_owned(),
            def_name: "bot".to_owned(),
            status: SubAgentState::Completed,
            started_at: "2026-01-01T00:00:00Z".to_owned(),
            finished_at: Some("2026-01-01T00:01:00Z".to_owned()),
            resumed_from: None,
            turns_used: 2,
            mcp_tool_names: Vec::new(),
        }
    }

    #[tokio::test]
    async fn writer_reader_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let msg1 = test_message(Role::User, "hello");
        let msg2 = test_message(Role::Assistant, "world");

        let writer = TranscriptWriter::new(&path).unwrap();
        writer.append(0, &msg1).await.unwrap();
        writer.append(1, &msg2).await.unwrap();
        drop(writer);

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].content, "world");
    }

    /// #6305: `MessagePart::Image` must never reach the on-disk transcript — it is ephemeral,
    /// current-turn-only vision input (spec-072 §4, C1), mirroring the strip already enforced
    /// for `Agent::persist_message`'s `SQLite`/Qdrant/durable-JSONL writers.
    #[tokio::test]
    async fn append_strips_image_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let mut msg = test_message(Role::User, "look at this");
        msg.parts = vec![
            MessagePart::Text {
                text: "look at this".to_owned(),
            },
            MessagePart::Image(Box::new(ImageData {
                data: vec![0xFFu8, 0xD8, 0xFF, 0xE0],
                mime_type: "image/jpeg".to_owned(),
            })),
        ];

        let writer = TranscriptWriter::new(&path).unwrap();
        writer.append(0, &msg).await.unwrap();

        // The caller's own copy keeps the Image part for the current turn's provider request.
        assert_eq!(msg.parts.len(), 2);

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].parts.len(), 1);
        assert!(matches!(messages[0].parts[0], MessagePart::Text { .. }));
        assert!(
            !messages[0]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::Image(_))),
            "transcript must not retain Image parts"
        );

        // The image payload must not appear anywhere in the file on disk either.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("mime_type") && !raw.contains("image/jpeg"),
            "raw image payload leaked into transcript file"
        );
    }

    #[tokio::test]
    async fn append_preserves_non_image_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let mut msg = test_message(Role::Assistant, "used a tool");
        msg.parts = vec![
            MessagePart::Text {
                text: "used a tool".to_owned(),
            },
            MessagePart::ToolUse {
                id: "call-1".to_owned(),
                name: "search".to_owned(),
                input: serde_json::json!({"query": "rust"}),
            },
        ];

        let writer = TranscriptWriter::new(&path).unwrap();
        writer.append(0, &msg).await.unwrap();

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].parts.len(), 2);
        assert!(matches!(messages[0].parts[0], MessagePart::Text { .. }));
        assert!(matches!(messages[0].parts[1], MessagePart::ToolUse { .. }));
    }

    #[tokio::test]
    async fn append_empty_parts_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        // Mirrors the `task_msg` / turn-generated-message call sites in `agent_loop.rs`, which
        // always pass an empty `parts` vec — the strip must be a no-op for them.
        let msg = test_message(Role::User, "plain task message");
        assert!(msg.parts.is_empty());

        let writer = TranscriptWriter::new(&path).unwrap();
        writer.append(0, &msg).await.unwrap();

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].parts.is_empty());
        assert_eq!(messages[0].content, "plain task message");
    }

    #[test]
    fn load_missing_file_no_meta_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.jsonl");
        let messages = TranscriptReader::load(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn load_missing_file_with_meta_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("ghost.meta.json");
        std::fs::write(&meta_path, "{}").unwrap();
        let jsonl_path = dir.path().join("ghost.jsonl");
        let err = TranscriptReader::load(&jsonl_path).unwrap_err();
        assert_matches!(err, SubAgentError::Transcript(_));
    }

    #[test]
    fn load_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");

        let good = test_message(Role::User, "good");
        let entry = TranscriptEntry {
            seq: 0,
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            message: good.clone(),
            chain: None,
        };
        let good_line = serde_json::to_string(&entry).unwrap();
        let content = format!("{good_line}\nnot valid json\n{good_line}\n");
        std::fs::write(&path, &content).unwrap();

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn load_strict_fails_on_first_malformed_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");

        let good = test_message(Role::User, "good");
        let entry = TranscriptEntry {
            seq: 0,
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            message: good.clone(),
            chain: None,
        };
        let good_line = serde_json::to_string(&entry).unwrap();
        // A torn/malformed line sits between two well-formed entries — simulates a sub-agent
        // canceled/killed mid-write.
        let content = format!("{good_line}\nnot valid json\n{good_line}\n");
        std::fs::write(&path, &content).unwrap();

        let err = TranscriptReader::load_strict(&path).unwrap_err();
        assert_matches!(err, SubAgentError::Transcript(_));

        // The lenient reader still tolerates the same file, proving the two variants diverge
        // only in this failure mode.
        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn load_strict_succeeds_on_intact_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clean.jsonl");

        let good = test_message(Role::User, "good");
        let entry = TranscriptEntry {
            seq: 0,
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            message: good,
            chain: None,
        };
        let good_line = serde_json::to_string(&entry).unwrap();
        std::fs::write(&path, format!("{good_line}\n")).unwrap();

        let messages = TranscriptReader::load_strict(&path).unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn load_strict_missing_file_no_meta_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.jsonl");
        let messages = TranscriptReader::load_strict(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn meta_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("abc-123");
        TranscriptWriter::write_meta(dir.path(), "abc-123", &meta).unwrap();
        let loaded = TranscriptReader::load_meta(dir.path(), "abc-123").unwrap();
        assert_eq!(loaded.agent_id, "abc-123");
        assert_eq!(loaded.turns_used, 2);
    }

    #[test]
    fn meta_not_found_returns_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = TranscriptReader::load_meta(dir.path(), "ghost").unwrap_err();
        assert_matches!(err, SubAgentError::NotFound(_));
    }

    #[test]
    fn find_by_prefix_exact() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("abcdef01-0000-0000-0000-000000000000");
        TranscriptWriter::write_meta(dir.path(), "abcdef01-0000-0000-0000-000000000000", &meta)
            .unwrap();
        let id =
            TranscriptReader::find_by_prefix(dir.path(), "abcdef01-0000-0000-0000-000000000000")
                .unwrap();
        assert_eq!(id, "abcdef01-0000-0000-0000-000000000000");
    }

    #[test]
    fn find_by_prefix_short_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("deadbeef-0000-0000-0000-000000000000");
        TranscriptWriter::write_meta(dir.path(), "deadbeef-0000-0000-0000-000000000000", &meta)
            .unwrap();
        let id = TranscriptReader::find_by_prefix(dir.path(), "deadbeef").unwrap();
        assert_eq!(id, "deadbeef-0000-0000-0000-000000000000");
    }

    #[test]
    fn find_by_prefix_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = TranscriptReader::find_by_prefix(dir.path(), "xxxxxxxx").unwrap_err();
        assert_matches!(err, SubAgentError::NotFound(_));
    }

    #[test]
    fn find_by_prefix_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        TranscriptWriter::write_meta(dir.path(), "aabb0001-x", &test_meta("aabb0001-x")).unwrap();
        TranscriptWriter::write_meta(dir.path(), "aabb0002-y", &test_meta("aabb0002-y")).unwrap();
        let err = TranscriptReader::find_by_prefix(dir.path(), "aabb").unwrap_err();
        assert_matches!(err, SubAgentError::AmbiguousId(_, 2));
    }

    #[test]
    fn sweep_old_transcripts_removes_oldest() {
        let dir = tempfile::tempdir().unwrap();

        for i in 0..5u32 {
            let path = dir.path().join(format!("file{i:02}.jsonl"));
            std::fs::write(&path, b"").unwrap();
            // Vary mtime by touching the file — not reliable without explicit mtime set,
            // but tempdir files get sequential syscall timestamps in practice.
            // We set the mtime explicitly via filetime crate... but we have no filetime dep.
            // Instead we just verify count is correct.
        }

        let deleted = sweep_old_transcripts(dir.path(), 3).unwrap();
        assert_eq!(deleted, 2);

        let remaining: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        assert_eq!(remaining.len(), 3);
    }

    #[test]
    fn sweep_with_zero_max_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), b"").unwrap();
        let deleted = sweep_old_transcripts(dir.path(), 0).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn sweep_below_max_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), b"").unwrap();
        let deleted = sweep_old_transcripts(dir.path(), 50).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn utc_now_format() {
        let ts = utc_now();
        // Basic format check: 2026-03-05T00:18:16Z
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }

    #[test]
    fn load_empty_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, b"").unwrap();
        let messages = TranscriptReader::load(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn load_meta_invalid_json_returns_transcript_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.meta.json"), b"not json at all {{{{").unwrap();
        let err = TranscriptReader::load_meta(dir.path(), "bad").unwrap_err();
        assert_matches!(err, SubAgentError::Transcript(_));
    }

    #[test]
    fn sweep_removes_companion_meta() {
        let dir = tempfile::tempdir().unwrap();
        // Create 4 JSONL files each with a companion meta sidecar.
        for i in 0..4u32 {
            let stem = format!("file{i:02}");
            std::fs::write(dir.path().join(format!("{stem}.jsonl")), b"").unwrap();
            std::fs::write(dir.path().join(format!("{stem}.meta.json")), b"{}").unwrap();
        }
        let deleted = sweep_old_transcripts(dir.path(), 2).unwrap();
        assert_eq!(deleted, 2);
        // Companion metas for the two deleted files should also be gone.
        let meta_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().to_string_lossy().ends_with(".meta.json"))
            .count();
        assert_eq!(
            meta_count, 2,
            "orphaned meta sidecars should have been removed"
        );
    }

    #[test]
    fn data_loss_guard_uses_stem_based_meta_path() {
        // path.with_extension("meta.json") on "abc.jsonl" should yield "abc.meta.json"
        // which matches write_meta's format!("{agent_id}.meta.json") when agent_id == stem.
        let dir = tempfile::tempdir().unwrap();
        let agent_id = "deadbeef-0000-0000-0000-000000000000";
        // Write meta sidecar but not the JSONL file.
        std::fs::write(dir.path().join(format!("{agent_id}.meta.json")), b"{}").unwrap();
        let jsonl_path = dir.path().join(format!("{agent_id}.jsonl"));
        let err = TranscriptReader::load(&jsonl_path).unwrap_err();
        assert_matches!(err, SubAgentError::Transcript(ref m) if m.contains("missing"));
    }

    #[test]
    fn meta_roundtrip_preserves_mcp_tool_names() {
        let dir = tempfile::tempdir().unwrap();
        let agent_id = "abc-123";
        let mut meta = test_meta(agent_id);
        meta.mcp_tool_names = vec!["search".into(), "write_file".into()];
        TranscriptWriter::write_meta(dir.path(), agent_id, &meta).unwrap();
        let loaded = TranscriptReader::load_meta(dir.path(), agent_id).unwrap();
        assert_eq!(loaded.mcp_tool_names, vec!["search", "write_file"]);
    }

    // --- Hash-chain integrity tests (issue #6360) ---
    //
    // `configure_history_integrity` mutates process-global state, so these tests rely on
    // `cargo nextest`'s one-process-per-test model for isolation (never run this module with
    // plain `cargo test`, which shares one process across tests in a binary and could race).

    fn test_ring(epoch: u32, byte: u8) -> Arc<ChainKeyRing> {
        Arc::new(ChainKeyRing::new(
            epoch,
            zeph_common::hash_chain::ChainKey::new([byte; 32]),
        ))
    }

    #[tokio::test]
    async fn chained_writer_reader_roundtrip() {
        configure_history_integrity(Some(test_ring(0, 1)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");

        let writer = TranscriptWriter::new(&path).unwrap();
        writer
            .append(0, &test_message(Role::User, "hello"))
            .await
            .unwrap();
        writer
            .append(1, &test_message(Role::Assistant, "world"))
            .await
            .unwrap();
        drop(writer);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.lines().all(|l| l.contains("\"chain\":")),
            "every line must carry a chain field once integrity is configured"
        );

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].content, "world");

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn tamper_in_place_edit_is_detected() {
        configure_history_integrity(Some(test_ring(0, 2)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");

        let writer = TranscriptWriter::new(&path).unwrap();
        // A first, untouched entry so the key epoch resolves cleanly there; tampering the
        // *second* entry below then produces a definite Mismatch (not an ambiguous
        // Unverifiable, which is what tampering the very first chained entry would produce).
        writer
            .append(0, &test_message(Role::User, "untouched"))
            .await
            .unwrap();
        writer
            .append(1, &test_message(Role::Assistant, "original"))
            .await
            .unwrap();
        drop(writer);

        let raw = std::fs::read_to_string(&path).unwrap();
        let tampered = raw.replace("original", "forged-approval");
        assert_ne!(raw, tampered);
        std::fs::write(&path, tampered).unwrap();

        let err = TranscriptReader::load(&path).unwrap_err();
        assert_matches!(err, SubAgentError::Integrity(ref m) if m.contains("TAMPER"));
        // load_strict must fail identically — chain breaks always escalate (Q3), even in modes
        // that otherwise differ only on JSON-syntax leniency.
        let err = TranscriptReader::load_strict(&path).unwrap_err();
        assert_matches!(err, SubAgentError::Integrity(_));

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn legacy_file_is_auto_trusted_once_when_integrity_configured_later() {
        // Written with integrity disabled (the pre-feature/legacy shape).
        configure_history_integrity(None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.jsonl");
        let writer = TranscriptWriter::new(&path).unwrap();
        writer
            .append(0, &test_message(Role::User, "pre-feature message"))
            .await
            .unwrap();
        drop(writer);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("\"chain\":"),
            "legacy file must carry no chain field"
        );

        // Now integrity comes online for this process (e.g. vault became available).
        configure_history_integrity(Some(test_ring(0, 3)));
        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(
            messages.len(),
            1,
            "legacy content must be auto-trusted, not rejected"
        );

        // A legacy file read while a key IS configured must be flagged exactly once per path
        // (security review B2 condition (c)) — repeat reads must not re-warn.
        assert!(
            WARNED_LEGACY_UNDER_KEY.read().unwrap().contains(&path),
            "path must be recorded as warned after the first legacy-under-active-key read"
        );
        let warned_count_before = WARNED_LEGACY_UNDER_KEY.read().unwrap().len();
        let _ = TranscriptReader::load(&path).unwrap();
        assert_eq!(
            WARNED_LEGACY_UNDER_KEY.read().unwrap().len(),
            warned_count_before,
            "a second read of the same path must not add a second warned-set entry"
        );

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn partial_strip_of_chain_field_is_detected_as_tamper() {
        configure_history_integrity(Some(test_ring(0, 4)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");

        let writer = TranscriptWriter::new(&path).unwrap();
        writer
            .append(0, &test_message(Role::User, "one"))
            .await
            .unwrap();
        writer
            .append(1, &test_message(Role::Assistant, "two"))
            .await
            .unwrap();
        drop(writer);

        // Strip the chain field from only the second line, simulating an attacker who deletes
        // one line's chain metadata rather than the whole file's (the C1 partial-strip attack).
        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let mut second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        second.as_object_mut().unwrap().remove("chain");
        let stripped = format!("{}\n{}\n", lines[0], second);
        std::fs::write(&path, stripped).unwrap();

        let err = TranscriptReader::load(&path).unwrap_err();
        assert_matches!(err, SubAgentError::Integrity(ref m) if m.contains("partial strip"));

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn key_unavailable_on_chained_file_fails_closed_not_legacy() {
        configure_history_integrity(Some(test_ring(0, 5)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");
        let writer = TranscriptWriter::new(&path).unwrap();
        writer
            .append(0, &test_message(Role::User, "chained"))
            .await
            .unwrap();
        drop(writer);

        // Simulate the vault becoming unavailable: no key ring configured at read time.
        configure_history_integrity(None);
        let err = TranscriptReader::load(&path).unwrap_err();
        assert_matches!(err, SubAgentError::Integrity(ref m) if m.contains("NFR-004") || m.contains("no history-integrity key"));
    }

    #[tokio::test]
    async fn rotated_key_epoch_verifies_as_rekeyed_not_tampered() {
        let old_key_byte = 6u8;
        configure_history_integrity(Some(test_ring(0, old_key_byte)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");
        let writer = TranscriptWriter::new(&path).unwrap();
        writer
            .append(0, &test_message(Role::User, "written before rotation"))
            .await
            .unwrap();
        drop(writer);

        // Rotate: new current epoch 1, old epoch 0 retained as the previous window.
        let ring = Arc::new(
            ChainKeyRing::new(1, zeph_common::hash_chain::ChainKey::new([9u8; 32])).with_previous(
                0,
                zeph_common::hash_chain::ChainKey::new([old_key_byte; 32]),
            ),
        );
        configure_history_integrity(Some(ring));

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(
            messages.len(),
            1,
            "a legitimately re-keyed file must still verify"
        );

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn writer_reopen_seeds_chain_from_existing_tail() {
        configure_history_integrity(Some(test_ring(0, 7)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");

        {
            let writer = TranscriptWriter::new(&path).unwrap();
            writer
                .append(0, &test_message(Role::User, "first session"))
                .await
                .unwrap();
        }
        // Reopen a fresh writer on the same file (M3 open-time tail seed) and append more.
        {
            let writer = TranscriptWriter::new(&path).unwrap();
            writer
                .append(1, &test_message(Role::Assistant, "second session"))
                .await
                .unwrap();
        }

        // The full file, spanning both writer instances, must verify as one continuous chain.
        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);

        configure_history_integrity(None);
    }

    /// S2 regression: concurrent `append` calls via a cloned writer must never desynchronize
    /// on-disk physical order from chain-link order. Mirrors
    /// `zeph_session::log::tests::test_concurrent_append_preserves_seq_order`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_append_preserves_chain_order() {
        const N: u32 = 50;
        configure_history_integrity(Some(test_ring(0, 8)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");
        let writer = TranscriptWriter::new(&path).unwrap();

        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..N {
            let writer = writer.clone();
            tasks.spawn(async move {
                writer
                    .append(i, &test_message(Role::User, &format!("msg-{i}")))
                    .await
                    .unwrap();
            });
        }
        while tasks.join_next().await.is_some() {}
        drop(writer);

        // If chain order had diverged from physical write order, this would fail with a
        // definite Mismatch tamper verdict even though nothing was actually tampered with.
        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), usize::try_from(N).unwrap());

        configure_history_integrity(None);
    }
}
