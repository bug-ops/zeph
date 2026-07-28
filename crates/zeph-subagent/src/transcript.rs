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
use zeph_common::anchor::{Anchor, AnchorStore, AnchorSubsystem};
use zeph_common::hash_chain::{
    ChainHash, ChainKeyRing, KeyResolution, chain_next, genesis,
    verify_chained_prefix_with_checkpoint,
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

/// Process-wide vault-anchor store (issue #6449), configured once at bootstrap alongside
/// [`configure_history_integrity`]. `None` (the default) disables anchor writes/checks entirely —
/// transcripts behave exactly as they did under #6453 (chain-verified, but not
/// downgrade-resistant against a whole-file strip).
static ANCHOR_STORE: StdRwLock<Option<Arc<dyn AnchorStore>>> = StdRwLock::new(None);

/// Configure (or disable, with `None`) the vault-anchor store for every [`TranscriptWriter`]/
/// [`TranscriptReader`] operation in this process from this point forward. See
/// [`configure_history_integrity`]'s doc for the single-set-at-startup contract this mirrors.
pub fn configure_anchor_store(store: Option<Arc<dyn AnchorStore>>) {
    if let Ok(mut guard) = ANCHOR_STORE.write() {
        *guard = store;
    }
}

fn anchor_store() -> Option<Arc<dyn AnchorStore>> {
    ANCHOR_STORE.read().ok().and_then(|g| g.clone())
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
    /// Total on-disk entry count (seeded from any pre-existing content at open time,
    /// incremented on every successful append) — the `count` half of the vault anchor written
    /// by [`TranscriptWriter::finalize`] (issue #6449).
    count: u64,
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

        let (prev, count) = if path.exists() {
            let entries =
                parse_entries(path, false).map_err(|e| io::Error::other(e.to_string()))?;
            let count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
            let anchor = match anchor_store() {
                Some(store) => store
                    .get_sync(AnchorSubsystem::SubagentTranscript, &identity)
                    .map_err(|e| io::Error::other(format!("anchor lookup failed: {e}")))?,
                None => None,
            };
            let (_messages, head) =
                verify_and_extract_messages(path, entries, ring.as_deref(), anchor.as_ref())
                    .map_err(|e| io::Error::other(e.to_string()))?;
            (head, count)
        } else {
            (None, 0)
        };

        let file = zeph_common::fs_secure::append_private(path)?;
        Ok(Self {
            state: Arc::new(Mutex::new(TranscriptWriteState { file, prev, count })),
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
            guard.count += 1;
            Ok(())
        })
        .await
        .map_err(|e| io::Error::other(format!("spawn_blocking panicked: {e}")))?
    }

    /// Finalize this writer: if a vault-anchor store is configured (issue #6449) and this
    /// writer's lifetime saw at least one chained append, persist an [`Anchor`] recording the
    /// final `(epoch, count, head)` — written **last**, after every append is durably flushed,
    /// so a crash before this point leaves the file present with no anchor, which is always
    /// benign (never a false tamper signature — see the module-level anchor docs).
    ///
    /// A no-op, not an error, when no anchor store is configured or this writer never chained
    /// (pure legacy for its whole lifetime): there is nothing to anchor.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the configured anchor store's `put` fails (a store-level failure,
    /// not an absent anchor). Callers should treat this as best-effort and log rather than fail
    /// the whole collection flow — the transcript file itself is already safely written.
    pub async fn finalize(self) -> io::Result<()> {
        let Some(store) = anchor_store() else {
            return Ok(());
        };
        let (head, count) = {
            let guard = self
                .state
                .lock()
                .map_err(|_| io::Error::other("transcript writer lock poisoned"))?;
            let Some(head) = guard.prev else {
                return Ok(());
            };
            (head, guard.count)
        };
        let epoch = self.ring.as_ref().map_or(0, |r| r.current_epoch());
        let anchor = Anchor::new(epoch, count, head);
        store
            .put(
                AnchorSubsystem::SubagentTranscript,
                &self.file_identity,
                anchor,
            )
            .await
            .map_err(|e| io::Error::other(format!("anchor put failed: {e}")))
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
        let identity = file_identity(path);
        let anchor = match anchor_store() {
            Some(store) => store
                .get_sync(AnchorSubsystem::SubagentTranscript, &identity)
                .map_err(|e| SubAgentError::Integrity(format!("anchor lookup failed: {e}")))?,
            None => None,
        };
        let (messages, _head) =
            verify_and_extract_messages(path, entries, ring.as_deref(), anchor.as_ref())?;
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
/// as legacy); a partial strip is detected; [`verify_chained_prefix`] reports a definite tamper
/// ([`ChainError::Mismatch`]) or an unverifiable/possibly-re-keyed chain
/// ([`ChainError::Unverifiable`]); or `anchor` disagrees with the on-disk content (issue #6449 —
/// see the read-side decision table in the module-level anchor docs, `zeph_common::anchor`).
#[allow(clippy::too_many_lines)]
fn verify_and_extract_messages(
    path: &Path,
    entries: Vec<TranscriptEntry>,
    ring: Option<&ChainKeyRing>,
    anchor: Option<&Anchor>,
) -> Result<(Vec<Message>, Option<ChainHash>), SubAgentError> {
    let Some(chain_start) = entries.iter().position(|e| e.chain.is_some()) else {
        // Legacy-looking file (no chain field anywhere) + a vault anchor exists for this file's
        // identity: this IS a tamper signature, unlike the "absent anchor" case below. An anchor
        // can only exist if this file was previously finalized while chained — a file-write-only
        // attacker cannot delete a vault entry, so a legacy-looking file with a live anchor means
        // every `chain` field was deliberately stripped (the whole-strip downgrade attack #6449
        // closes).
        if let Some(anchor) = anchor {
            tracing::error!(
                audit_event = "history_integrity_tamper",
                subsystem = "subagent_transcript",
                reason = "whole_strip_legacy_with_anchor",
                path = %path.display(),
                anchored_count = anchor.count,
                "TAMPER DETECTED: transcript is legacy-looking but a vault anchor exists for it \
                 (issue #6449)"
            );
            return Err(SubAgentError::Integrity(format!(
                "TAMPER DETECTED in transcript '{}': file has no chain metadata (legacy-looking) \
                 but a vault anchor exists for it (anchored at count={}) — this file was \
                 previously chained and its chain fields have been stripped",
                path.display(),
                anchor.count
            )));
        }
        // Pure legacy file: no chain metadata anywhere, and no anchor either. Auto-trusted per
        // FR-006 — but if a key ring IS configured, every legitimately-written file since this
        // process started should carry a chain field, so a chainless file under an active key is
        // anomalous: either genuine pre-upgrade content, or (absent an anchor to prove otherwise)
        // indistinguishable from one. Not a hard failure — but it must be observable, not silent
        // (security review B2 condition, NFR-005).
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
    let on_disk_count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    // The anchor's `count` is a total on-disk count; the chained region starts at `chain_start`,
    // so the checkpoint index within `chained` (already sliced from `chain_start`) is
    // `count - chain_start - 1` (0-based, the position of the anchor's last entry).
    let checkpoint_index = anchor.and_then(|a| {
        a.count
            .checked_sub(u64::try_from(chain_start).unwrap_or(u64::MAX) + 1)
    });
    let (head, checkpoint_head, resolution) = verify_chained_prefix_with_checkpoint(
        ring,
        CHAIN_DOMAIN,
        &identity,
        &chained,
        checkpoint_index.unwrap_or(u64::MAX),
    )
    .map_err(|e| describe_chain_error(path, &e))?;

    if let KeyResolution::Rekeyed(epoch) = resolution {
        tracing::info!(
            path = %path.display(),
            epoch,
            "transcript verified under a previous key epoch (re-keyed, not tampered)"
        );
    }

    if let Some(anchor) = anchor {
        if on_disk_count < anchor.count {
            tracing::error!(
                audit_event = "history_integrity_tamper",
                subsystem = "subagent_transcript",
                reason = "truncated_below_anchor_count",
                path = %path.display(),
                on_disk_count,
                anchored_count = anchor.count,
                "TAMPER DETECTED: transcript truncated below its anchored count (issue #6449)"
            );
            return Err(SubAgentError::Integrity(format!(
                "TAMPER DETECTED in transcript '{}': on-disk entry count ({on_disk_count}) is \
                 below the anchored count ({}) — the file was truncated after being anchored",
                path.display(),
                anchor.count
            )));
        }
        let anchor_head = anchor.head().map_err(|e| {
            SubAgentError::Integrity(format!(
                "transcript '{}' anchor is malformed: {e}",
                path.display()
            ))
        })?;
        match checkpoint_head {
            Some(h) if h == anchor_head => {}
            _ => {
                tracing::error!(
                    audit_event = "history_integrity_tamper",
                    subsystem = "subagent_transcript",
                    reason = "anchor_head_mismatch",
                    path = %path.display(),
                    anchored_count = anchor.count,
                    "TAMPER DETECTED: transcript chain head at the anchored count does not match \
                     the stored vault anchor (issue #6449)"
                );
                return Err(SubAgentError::Integrity(format!(
                    "TAMPER DETECTED in transcript '{}': chain head at the anchored count ({}) \
                     does not match the stored vault anchor",
                    path.display(),
                    anchor.count
                )));
            }
        }
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

/// Delete the oldest `.jsonl` files in `dir` when the count exceeds `max_files`, plus each
/// deleted file's companion `.meta.json` sidecar.
///
/// Files are sorted by modification time (oldest first). Returns the number of
/// files deleted.
///
/// # Vault anchors (issue #6449)
///
/// This function stays deliberately synchronous (it is called from 2+ sync/`spawn_blocking`
/// contexts outside this feature's ownership — see `crates/zeph-subagent/src/manager/collect.rs`
/// — and making it async would force those callers async too, an out-of-scope blast radius).
/// It therefore does **not** delete a swept file's vault anchor inline. This is safe, not merely
/// deferred-and-hoped: an anchor whose file no longer exists is an **orphan**, and an orphan
/// anchor is always benign on read (an anchor is only ever consulted when opening a file that
/// exists — see the module-level anchor docs, `zeph_common::anchor`) — it never produces a false
/// TAMPER verdict for anything. Orphans left behind by this sweep are reaped later by the
/// process-wide reconcile-and-cap sweep (`zeph-core`'s `anchor_store` module), which lists every
/// `ZEPH_HISTORY_ANCHOR_*` vault key and drops any whose file no longer exists on disk, bounding
/// vault growth exactly as it already does for the session-anchor LRU cap.
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

/// RAII guard that resets [`HISTORY_INTEGRITY`] and [`ANCHOR_STORE`] to `None` on drop
/// (issue #6686). See `zeph_session::log::IntegrityConfigGuard`'s identical doc for the full
/// rationale — this mirrors it exactly. Every test in this crate that configures either
/// static, or that constructs a [`TranscriptWriter`]/[`TranscriptReader`] while one could be
/// configured (e.g. `manager::tests::run_agent_loop_finalizes_transcript_anchor_on_llm_error_exit_path`),
/// must both construct this guard and carry
/// `#[serial_test::serial(subagent_transcript_integrity)]`.
#[cfg(test)]
pub(crate) struct IntegrityConfigGuard(());

#[cfg(test)]
impl IntegrityConfigGuard {
    pub(crate) fn new() -> Self {
        Self(())
    }
}

#[cfg(test)]
impl Drop for IntegrityConfigGuard {
    fn drop(&mut self) {
        configure_history_integrity(None);
        configure_anchor_store(None);
    }
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
    fn load_missing_file_no_meta_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.jsonl");
        let messages = TranscriptReader::load(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn load_missing_file_with_meta_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("ghost.meta.json");
        std::fs::write(&meta_path, "{}").unwrap();
        let jsonl_path = dir.path().join("ghost.jsonl");
        let err = TranscriptReader::load(&jsonl_path).unwrap_err();
        assert_matches!(err, SubAgentError::Transcript(_));
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
    fn load_strict_missing_file_no_meta_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.jsonl");
        let messages = TranscriptReader::load_strict(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn meta_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("abc-123");
        TranscriptWriter::write_meta(dir.path(), "abc-123", &meta).unwrap();
        let loaded = TranscriptReader::load_meta(dir.path(), "abc-123").unwrap();
        assert_eq!(loaded.agent_id, "abc-123");
        assert_eq!(loaded.turns_used, 2);
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn meta_not_found_returns_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = TranscriptReader::load_meta(dir.path(), "ghost").unwrap_err();
        assert_matches!(err, SubAgentError::NotFound(_));
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
    fn find_by_prefix_short_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("deadbeef-0000-0000-0000-000000000000");
        TranscriptWriter::write_meta(dir.path(), "deadbeef-0000-0000-0000-000000000000", &meta)
            .unwrap();
        let id = TranscriptReader::find_by_prefix(dir.path(), "deadbeef").unwrap();
        assert_eq!(id, "deadbeef-0000-0000-0000-000000000000");
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn find_by_prefix_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = TranscriptReader::find_by_prefix(dir.path(), "xxxxxxxx").unwrap_err();
        assert_matches!(err, SubAgentError::NotFound(_));
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn find_by_prefix_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        TranscriptWriter::write_meta(dir.path(), "aabb0001-x", &test_meta("aabb0001-x")).unwrap();
        TranscriptWriter::write_meta(dir.path(), "aabb0002-y", &test_meta("aabb0002-y")).unwrap();
        let err = TranscriptReader::find_by_prefix(dir.path(), "aabb").unwrap_err();
        assert_matches!(err, SubAgentError::AmbiguousId(_, 2));
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
    fn sweep_with_zero_max_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), b"").unwrap();
        let deleted = sweep_old_transcripts(dir.path(), 0).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn sweep_below_max_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), b"").unwrap();
        let deleted = sweep_old_transcripts(dir.path(), 50).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn utc_now_format() {
        let ts = utc_now();
        // Basic format check: 2026-03-05T00:18:16Z
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn load_empty_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, b"").unwrap();
        let messages = TranscriptReader::load(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
    fn load_meta_invalid_json_returns_transcript_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.meta.json"), b"not json at all {{{{").unwrap();
        let err = TranscriptReader::load_meta(dir.path(), "bad").unwrap_err();
        assert_matches!(err, SubAgentError::Transcript(_));
    }

    #[test]
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
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
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn chained_writer_reader_roundtrip() {
        let _guard = IntegrityConfigGuard::new();
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
    }

    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn tamper_in_place_edit_is_detected() {
        let _guard = IntegrityConfigGuard::new();
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
    }

    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn legacy_file_is_auto_trusted_once_when_integrity_configured_later() {
        let _guard = IntegrityConfigGuard::new();
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
    }

    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn partial_strip_of_chain_field_is_detected_as_tamper() {
        let _guard = IntegrityConfigGuard::new();
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
    }

    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn key_unavailable_on_chained_file_fails_closed_not_legacy() {
        let _guard = IntegrityConfigGuard::new();
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
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn rotated_key_epoch_verifies_as_rekeyed_not_tampered() {
        let _guard = IntegrityConfigGuard::new();
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
    }

    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn writer_reopen_seeds_chain_from_existing_tail() {
        let _guard = IntegrityConfigGuard::new();
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
    }

    /// S2 regression: concurrent `append` calls via a cloned writer must never desynchronize
    /// on-disk physical order from chain-link order. Mirrors
    /// `zeph_session::log::tests::test_concurrent_append_preserves_seq_order`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn concurrent_append_preserves_chain_order() {
        const N: u32 = 50;
        let _guard = IntegrityConfigGuard::new();
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
    }

    // --- Vault-anchor downgrade-resistance tests (issue #6449) ---

    /// In-memory [`AnchorStore`] mock for tests — a simple `Mutex<HashMap>` keyed by
    /// [`zeph_common::anchor::anchor_key`], mirroring `zeph_vault::MockVaultProvider`'s role for
    /// the history-key tests above.
    #[derive(Default)]
    struct MockAnchorStore {
        map: std::sync::Mutex<std::collections::HashMap<String, Anchor>>,
    }

    impl AnchorStore for MockAnchorStore {
        fn get(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<Anchor>, zeph_common::anchor::AnchorError>,
                    > + Send
                    + '_,
            >,
        > {
            let result = self.get_sync(subsystem, file_id);
            Box::pin(async move { result })
        }

        fn get_sync(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
        ) -> Result<Option<Anchor>, zeph_common::anchor::AnchorError> {
            let key = zeph_common::anchor::anchor_key(subsystem, file_id);
            Ok(self.map.lock().unwrap().get(&key).cloned())
        }

        fn put(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
            anchor: Anchor,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), zeph_common::anchor::AnchorError>>
                    + Send
                    + '_,
            >,
        > {
            let key = zeph_common::anchor::anchor_key(subsystem, file_id);
            self.map.lock().unwrap().insert(key, anchor);
            Box::pin(async { Ok(()) })
        }

        fn delete(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), zeph_common::anchor::AnchorError>>
                    + Send
                    + '_,
            >,
        > {
            let key = zeph_common::anchor::anchor_key(subsystem, file_id);
            self.map.lock().unwrap().remove(&key);
            Box::pin(async { Ok(()) })
        }
    }

    /// Regression test for FINDING B / acceptance criterion 2: a pre-anchor chained file (no
    /// anchor store configured when it was written) must still open normally when an anchor
    /// store comes online later — an absent anchor is never a tamper signature.
    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn pre_anchor_chained_file_still_opens_with_anchor_store_online() {
        let _guard = IntegrityConfigGuard::new();
        configure_history_integrity(Some(test_ring(0, 20)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");

        // Written with no anchor store configured (the #6453-only posture).
        let writer = TranscriptWriter::new(&path).unwrap();
        writer
            .append(0, &test_message(Role::User, "pre-anchor"))
            .await
            .unwrap();
        drop(writer);

        // Now an anchor store comes online, but this file was never anchored.
        configure_anchor_store(Some(Arc::new(MockAnchorStore::default())));
        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(
            messages.len(),
            1,
            "absent anchor must never brick a legacy-chained file"
        );
    }

    /// Acceptance criterion 1/3: whole-strip of an anchored transcript is TAMPER, and so is
    /// truncation below the anchored count.
    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn whole_strip_of_anchored_transcript_is_tamper() {
        let _guard = IntegrityConfigGuard::new();
        configure_history_integrity(Some(test_ring(0, 21)));
        let store: Arc<dyn AnchorStore> = Arc::new(MockAnchorStore::default());
        configure_anchor_store(Some(Arc::clone(&store)));

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
        writer.finalize().await.unwrap();

        // Sanity: with the anchor present and content untouched, the file still opens.
        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);

        // Whole-strip: rewrite every line with its `chain` field removed, so the file looks
        // pre-feature-legacy — the attack #6449 closes.
        let raw = std::fs::read_to_string(&path).unwrap();
        let stripped: String = raw
            .lines()
            .map(|line| {
                let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                value.as_object_mut().unwrap().remove("chain");
                value.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, stripped).unwrap();

        let err = TranscriptReader::load(&path).unwrap_err();
        assert_matches!(err, SubAgentError::Integrity(ref m) if m.contains("TAMPER") && m.contains("vault anchor"));
    }

    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn truncation_below_anchored_count_is_tamper() {
        let _guard = IntegrityConfigGuard::new();
        configure_history_integrity(Some(test_ring(0, 22)));
        let store: Arc<dyn AnchorStore> = Arc::new(MockAnchorStore::default());
        configure_anchor_store(Some(Arc::clone(&store)));

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
        writer.finalize().await.unwrap();

        // Truncate the file to just its first line — content still verifies as a valid (shorter)
        // chain, but disagrees with the anchor's recorded count.
        let raw = std::fs::read_to_string(&path).unwrap();
        let first_line = raw.lines().next().unwrap();
        std::fs::write(&path, format!("{first_line}\n")).unwrap();

        let err = TranscriptReader::load(&path).unwrap_err();
        assert_matches!(err, SubAgentError::Integrity(ref m) if m.contains("TAMPER") && m.contains("truncated"));
    }

    #[tokio::test]
    #[serial_test::serial(subagent_transcript_integrity)]
    async fn finalize_is_noop_without_anchor_store_or_without_chaining() {
        let _guard = IntegrityConfigGuard::new();
        // No anchor store configured: finalize must succeed as a no-op.
        configure_history_integrity(Some(test_ring(0, 23)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.jsonl");
        let writer = TranscriptWriter::new(&path).unwrap();
        writer
            .append(0, &test_message(Role::User, "x"))
            .await
            .unwrap();
        writer.finalize().await.unwrap();
        configure_history_integrity(None);

        // Anchor store configured, but chaining disabled: finalize must still be a no-op (no
        // chain head to anchor).
        let store: Arc<dyn AnchorStore> = Arc::new(MockAnchorStore::default());
        configure_anchor_store(Some(Arc::clone(&store)));
        let path2 = dir.path().join("legacy.jsonl");
        let writer2 = TranscriptWriter::new(&path2).unwrap();
        writer2
            .append(0, &test_message(Role::User, "legacy"))
            .await
            .unwrap();
        writer2.finalize().await.unwrap();
        assert!(
            store
                .get_sync(AnchorSubsystem::SubagentTranscript, b"legacy")
                .unwrap()
                .is_none(),
            "no anchor should be written for an unchained writer"
        );
    }
}
