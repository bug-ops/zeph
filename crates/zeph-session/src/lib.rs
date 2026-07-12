// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conversation-session persistence, event-log replay, and fork engine for Zeph.
//!
//! `zeph-session` implements spec-068: an append-only JSONL event log (the source of truth for
//! a conversation), a metadata index over the existing `acp_sessions` table, a deterministic
//! [`replay::ReplayEngine`], and the [`condenser::Condenser`] contract for durable context
//! condensation. It is consumed by `zeph-core` (agent-loop `SessionSink` wiring, `zeph serve`
//! per-session actors) and `zeph-acp` (session load/list/fork/resume handlers thinned to
//! delegate here).
//!
//! # Architectural placement
//!
//! `zeph-session` mirrors the append-only journal design of `zeph-durable`
//! (sequential event ordering, a single-writer actor model) but is a **separate** crate: the two
//! operate at different abstraction levels (task/step effect-idempotency vs. conversation
//! semantics) and use different storage formats (`SQLite`-backed opaque payloads vs. JSONL
//! domain-typed events). `zeph-session` MUST NOT depend on `zeph-durable`, and vice versa
//! (spec-068 §3, §15 NEVER; INV-1 in spec-064).
//!
//! # Module map
//!
//! - [`error`] — the crate-wide [`error::SessionError`].
//! - [`event`] — the [`event::SessionEvent`] tagged enum and its [`event::SessionEventEnvelope`]
//!   on-disk wrapper. Reuses `zeph_llm::provider::MessagePart` and
//!   `zeph_common::memory::AnchoredSummary` rather than redefining them.
//! - [`log`] — [`log::SessionEventLog`]: the append-only JSONL writer/reader, including the
//!   INV-SP-2 torn-append truncation logic.
//! - [`store`] — [`store::SessionStore`]: CRUD over the `acp_sessions` metadata index (spec §5).
//! - [`replay`] — [`replay::ReplayEngine`]: deterministic fold of an event log into agent-ready
//!   messages. Never calls the LLM or a tool executor.
//! - [`condenser`] — the [`condenser::Condenser`] trait contract and the INV-SP-4 non-overlap
//!   guard.
//! - [`llm_condenser`] — [`llm_condenser::LlmCondenser`]: the default `Condenser` implementation,
//!   reusing `zeph_context::summarization::summarize_structured`.
//! - [`fork`] — [`fork::ForkEngine`]: eager-copy session forking (spec §7).
//!
//! The `zeph-core` `SessionActor` integration (`zeph serve`, spec §9) lands in a later phase of
//! the implementation plan (`specs/068-session-persistence/plan.md`).

pub mod condenser;
pub mod error;
pub mod event;
pub mod fork;
pub mod llm_condenser;
pub mod log;
pub mod replay;
pub mod store;

pub use condenser::{CondensationResult, Condenser};
pub use error::SessionError;
pub use event::{CompactionTier, SessionEvent, SessionEventEnvelope};
pub use fork::{ForkEngine, ForkResult};
pub use llm_condenser::LlmCondenser;
pub use log::SessionEventLog;
pub use replay::{ReconstructedState, ReplayEngine};
pub use store::{SessionFilter, SessionMetadata, SessionStatus, SessionStore};

/// The on-disk directory for one session's event log and blobs, per spec §4.1:
/// `<data_dir>/<session_id>/`.
///
/// `data_dir` (`[session] data_dir`, default `.zeph/sessions`) already names the sessions
/// root — callers must not append an extra `sessions` segment (#5981).
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// let dir = zeph_session::session_dir(Path::new(".zeph/sessions"), "abc-123");
/// assert_eq!(dir, Path::new(".zeph/sessions/abc-123"));
/// ```
#[must_use]
pub fn session_dir(data_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    data_dir.join(session_id)
}

/// The one-time startup migration report returned by [`migrate_legacy_session_layout`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Number of legacy session directories moved up one level to the fixed (#5981) layout.
    pub migrated: usize,
    /// Number of legacy session directories left in place because a directory already existed
    /// at the destination (not clobbered — it may already have been recreated at the new path).
    pub skipped: usize,
}

/// Moves any session directories still sitting at the pre-#5981 on-disk layout
/// (`<data_dir>/sessions/<session_id>/`) up one level to the fixed layout
/// (`<data_dir>/<session_id>/`), which is what [`session_dir`] now resolves to.
///
/// Before #5981, [`session_dir`] appended a redundant `sessions` segment, so any session created
/// before the fix physically has its `events.jsonl`/`blobs/` one directory level deeper than
/// where the crate now looks. Left unmigrated, [`log::SessionEventLog::open`] silently
/// `create_dir_all`s and creates a blank log at the new (empty) path — the user's real history
/// becomes unreachable with zero error or warning. This function is meant to be called once at
/// process startup, before any session is opened, to make that transition transparent.
///
/// A destination that already exists is left untouched (skipped, with a `tracing::warn!`)
/// rather than clobbered. Idempotent: once every legacy subdirectory has been moved (or
/// skipped), a subsequent run finds an empty (or absent) `<data_dir>/sessions/` and returns
/// cheaply; a missing `<data_dir>/sessions/` (a brand-new install, or one already migrated) is
/// not an error.
///
/// # Errors
///
/// Returns [`SessionError::Io`] if `<data_dir>/sessions/` exists but cannot be listed, or if a
/// rename or an existence check fails for a reason other than the destination not existing.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// # #[tokio::main]
/// # async fn main() {
/// let dir = tempfile::tempdir().unwrap();
/// // Brand-new install: no `sessions/` subdirectory yet — a cheap no-op, not an error.
/// let report = zeph_session::migrate_legacy_session_layout(dir.path()).await.unwrap();
/// assert_eq!(report, zeph_session::MigrationReport::default());
/// # }
/// ```
pub async fn migrate_legacy_session_layout(
    data_dir: &std::path::Path,
) -> Result<MigrationReport, SessionError> {
    let legacy_root = data_dir.join("sessions");

    let mut entries = match tokio::fs::read_dir(&legacy_root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MigrationReport::default());
        }
        Err(e) => return Err(e.into()),
    };

    let mut report = MigrationReport::default();
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }

        let src = entry.path();
        let dest = data_dir.join(entry.file_name());

        if tokio::fs::try_exists(&dest).await? {
            tracing::warn!(
                src = %src.display(),
                dest = %dest.display(),
                "legacy session directory left in place: destination already exists (#5981)"
            );
            report.skipped += 1;
            continue;
        }

        tokio::fs::rename(&src, &dest).await?;
        report.migrated += 1;
    }

    if report.migrated > 0 || report.skipped > 0 {
        tracing::info!(
            migrated = report.migrated,
            skipped = report.skipped,
            "migrated legacy (#5981) session directory layout"
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{MigrationReport, migrate_legacy_session_layout, session_dir};

    /// Regression test for #5981: `session_dir` must not append a redundant `sessions` segment
    /// when `data_dir` (e.g. the configured default `.zeph/sessions`) already names the
    /// sessions root, or every on-disk path double-nests as `sessions/sessions/<id>`.
    #[test]
    fn session_dir_does_not_double_nest() {
        let dir = session_dir(Path::new(".zeph/sessions"), "abc-123");
        assert_eq!(dir, Path::new(".zeph/sessions/abc-123"));
    }

    #[test]
    fn session_dir_joins_arbitrary_data_dir() {
        let dir = session_dir(Path::new("/var/lib/zeph/data"), "s1");
        assert_eq!(dir, Path::new("/var/lib/zeph/data/s1"));
    }

    /// (a) Migrates an existing pre-#5981 session directory up one level, and — run a second
    /// time — confirms idempotency (nothing left to move, cheap no-op).
    #[tokio::test]
    async fn migrate_moves_legacy_session_dir_up_one_level() {
        let data_dir = tempfile::tempdir().unwrap();
        let legacy = data_dir.path().join("sessions").join("abc-123");
        tokio::fs::create_dir_all(&legacy).await.unwrap();
        tokio::fs::write(legacy.join("events.jsonl"), b"{}\n")
            .await
            .unwrap();

        let report = migrate_legacy_session_layout(data_dir.path())
            .await
            .unwrap();
        assert_eq!(
            report,
            MigrationReport {
                migrated: 1,
                skipped: 0
            }
        );
        assert!(
            data_dir
                .path()
                .join("abc-123")
                .join("events.jsonl")
                .exists()
        );
        assert!(!legacy.exists());

        // Idempotency: running again finds nothing left under `sessions/`.
        let second = migrate_legacy_session_layout(data_dir.path())
            .await
            .unwrap();
        assert_eq!(second, MigrationReport::default());
    }

    /// (b) No-op when the legacy `sessions/` directory exists but is empty.
    #[tokio::test]
    async fn migrate_is_noop_when_legacy_dir_is_empty() {
        let data_dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(data_dir.path().join("sessions"))
            .await
            .unwrap();

        let report = migrate_legacy_session_layout(data_dir.path())
            .await
            .unwrap();
        assert_eq!(report, MigrationReport::default());
    }

    /// (c) Skips (with a warning, not an error) when a directory already exists at the
    /// destination — must not clobber a session that may have already been recreated there.
    #[tokio::test]
    async fn migrate_skips_when_destination_already_exists() {
        let data_dir = tempfile::tempdir().unwrap();
        let legacy = data_dir.path().join("sessions").join("abc-123");
        tokio::fs::create_dir_all(&legacy).await.unwrap();
        tokio::fs::write(legacy.join("events.jsonl"), b"old\n")
            .await
            .unwrap();

        let dest = data_dir.path().join("abc-123");
        tokio::fs::create_dir_all(&dest).await.unwrap();
        tokio::fs::write(dest.join("events.jsonl"), b"new\n")
            .await
            .unwrap();

        let report = migrate_legacy_session_layout(data_dir.path())
            .await
            .unwrap();
        assert_eq!(
            report,
            MigrationReport {
                migrated: 0,
                skipped: 1
            }
        );
        assert_eq!(
            tokio::fs::read(dest.join("events.jsonl")).await.unwrap(),
            b"new\n",
            "destination must not be clobbered"
        );
        assert!(legacy.exists(), "legacy directory must be left in place");
    }

    /// (d) No-op, not an error, when `<data_dir>/sessions/` doesn't exist at all (brand-new
    /// install, or a `data_dir` already fully migrated).
    #[tokio::test]
    async fn migrate_is_noop_when_legacy_root_missing() {
        let data_dir = tempfile::tempdir().unwrap();
        let report = migrate_legacy_session_layout(data_dir.path())
            .await
            .unwrap();
        assert_eq!(report, MigrationReport::default());
    }
}
