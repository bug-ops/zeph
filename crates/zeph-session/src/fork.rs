// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ForkEngine`]: eager-copy session forking (spec §7).
//!
//! Copy-on-write forking is explicitly deferred (spec §7.2, §15 NEVER) — eager copy is simple and
//! self-contained for MVP, and robust to either side independently condensing the shared prefix
//! afterward (the child log is fully self-contained; `forked_at_seq` is historical metadata only).

use std::path::Path;

use tokio::fs;

use crate::error::SessionError;
use crate::event::{SessionEvent, SessionEventEnvelope};
use crate::log::SessionEventLog;
use crate::replay::ReplayEngine;
use crate::store::SessionStore;

/// Name of the per-session directory holding content-hash-addressed blob files (spec §4.1).
const BLOBS_DIR_NAME: &str = "blobs";

/// The result of a successful fork.
#[derive(Debug, Clone)]
pub struct ForkResult {
    /// The newly allocated child session id.
    pub new_session_id: String,
    /// Number of events copied from the parent's log (excludes the child's own `SessionStarted`
    /// header, which is synthesized fresh).
    pub events_copied: usize,
}

/// Forks a session at a given `seq`, producing a new, fully self-contained child session.
pub struct ForkEngine;

impl ForkEngine {
    /// Fork `src_id` at `at_seq` into a caller-allocated `new_id` (`at_seq` is an exclusive upper
    /// bound — matches [`ReplayEngine::replay`]'s `up_to` semantics: the child receives events
    /// `[0, at_seq)` from the parent, plus a synthetic `SessionStarted` header recording
    /// `forked_from`). `at_seq = None` forks at the current end of the log (copies everything) —
    /// the default for callers with no explicit cut point (ACP's `fork_session`, which has no
    /// `seq` parameter, and the CLI's optional `--at`).
    ///
    /// `new_id` is caller-supplied rather than minted internally: callers such as ACP's
    /// `do_fork_session` need the id before the fork call completes (to construct the session's
    /// `LoopbackChannel`/entry), and the CLI mints a fresh `SessionId::generate()` before calling
    /// in.
    ///
    /// `owner` stamps the child row's `owner_key` (#5868) — see [`SessionStore::record_fork`].
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::NotFound`] if `src_id` has no session-store row,
    /// [`SessionError::InvalidForkPoint`] if `at_seq` exceeds the parent log's event count, or
    /// [`SessionError::Io`]/[`SessionError::Db`] if the copy or store update fails.
    #[tracing::instrument(name = "session.fork.run", skip_all, level = "info", fields(at_seq))]
    pub async fn fork(
        data_dir: &Path,
        src_id: &str,
        new_id: &str,
        at_seq: Option<u64>,
        store: &SessionStore,
        owner: Option<&str>,
    ) -> Result<ForkResult, SessionError> {
        if store.get(src_id).await?.is_none() {
            return Err(SessionError::NotFound(src_id.to_owned()));
        }

        let src_dir = crate::session_dir(data_dir, src_id);
        let src_log = SessionEventLog::open(&src_dir).await?;
        let all_events = src_log.read_all().await?;

        let total = u64::try_from(all_events.len()).unwrap_or(u64::MAX);
        let at_seq = at_seq.unwrap_or(total);
        if at_seq > total {
            return Err(SessionError::InvalidForkPoint(format!(
                "at_seq={at_seq} exceeds source session's event count={total}"
            )));
        }

        // Validate the cut point is internally consistent (spec §7.2 step 2) — replay must not
        // error. The reconstructed state itself is not needed further here.
        ReplayEngine::replay(&src_dir, Some(at_seq)).await?;

        let take_n = usize::try_from(at_seq).unwrap_or(usize::MAX);
        let to_copy: Vec<_> = all_events.iter().take(take_n).cloned().collect();
        let (cwd, provider_name, model) = to_copy
            .iter()
            .find_map(|e| match &e.kind {
                SessionEvent::SessionStarted {
                    cwd,
                    provider_name,
                    model,
                    ..
                } => Some((cwd.clone(), provider_name.clone(), model.clone())),
                _ => None,
            })
            .unwrap_or_default();

        let child_dir = crate::session_dir(data_dir, new_id);
        let child_log = SessionEventLog::open(&child_dir).await?;

        child_log
            .append(
                None,
                None,
                SessionEvent::SessionStarted {
                    session_id: new_id.to_owned(),
                    cwd,
                    provider_name,
                    model,
                    forked_from: Some((src_id.to_owned(), at_seq)),
                },
            )
            .await?;
        for envelope in &to_copy {
            child_log
                .append(envelope.turn_id, envelope.parent_seq, envelope.kind.clone())
                .await?;
        }

        copy_referenced_blobs(&src_dir, &child_dir, &to_copy).await?;

        store.record_fork(new_id, src_id, at_seq, owner).await?;
        store
            .update_seq(
                new_id,
                child_log.last_seq().unwrap_or(0),
                to_copy.len() as u64 + 1,
            )
            .await?;

        // Non-destructive provenance record on the parent (spec §7.2 step 8).
        src_log
            .append(
                None,
                None,
                SessionEvent::ForkPoint {
                    new_session_id: new_id.to_owned(),
                },
            )
            .await?;

        Ok(ForkResult {
            new_session_id: new_id.to_owned(),
            events_copied: to_copy.len(),
        })
    }
}

/// Copy the `blobs/` files referenced by `UserMessage.image_refs` in `events` from the parent's
/// session directory into the child's (spec §7.2 step 6). Hard-links each blob (cheap, same
/// filesystem — content-hash-addressed blobs are immutable so sharing the inode is safe); falls
/// back to a full copy if the hard-link fails (e.g. `src_dir`/`child_dir` are on different
/// filesystems/devices).
///
/// A referenced blob missing on disk is logged and skipped rather than treated as a hard
/// error: the event-log copy (the fork's primary content) already succeeded by this point, and
/// a missing blob only means the child loses one attachment rather than the whole conversation
/// history — consistent with [`crate::log`]'s own torn-tail handling, which prefers a
/// best-effort recovery over failing the whole read.
///
/// # Write-once contract
///
/// Hard-linking is only safe if blobs are content-addressed and never mutated in place after
/// being written. No blob writer exists yet anywhere in this codebase to enforce that; when one
/// lands, it MUST use append-by-new-hash semantics (never overwrite an existing hash's file) or
/// this fork's hard-link would let a later parent-side mutation silently corrupt the child's
/// copy through the shared inode.
///
/// # Errors
///
/// Returns [`SessionError::InvalidBlobHash`] if any `image_refs` entry is not a non-empty,
/// bare hex string (rejected before use in [`Path::join`] to prevent path traversal), or
/// [`SessionError::Io`] if directory creation, the hard-link, or the copy fallback fails.
///
/// A destination that already exists (e.g. a retried fork against the same `child_dir`) is not
/// an error: blobs are content-addressed by hash, so a pre-existing entry at the hash-named path
/// is treated as already the same content and the link is skipped as a no-op. This assumes the
/// pre-existing file is intact; see the `TODO` on the `AlreadyExists` match arm below for the one
/// known gap (an interrupted cross-device copy from a prior run).
async fn copy_referenced_blobs(
    src_dir: &Path,
    child_dir: &Path,
    events: &[SessionEventEnvelope],
) -> Result<(), SessionError> {
    let mut hashes: Vec<&str> = Vec::new();
    for envelope in events {
        let SessionEvent::UserMessage { image_refs, .. } = &envelope.kind else {
            continue;
        };
        for hash in image_refs {
            validate_blob_hash(hash)?;
            hashes.push(hash.as_str());
        }
    }

    if hashes.is_empty() {
        return Ok(());
    }

    // Dedup: the same hash can legitimately appear twice (repeated attachment, or reused across
    // messages) — without this, the second `hard_link` on an already-linked destination returns
    // `AlreadyExists`, which the loop below already handles as a no-op. Dedup here is a
    // micro-optimization to skip that redundant syscall+no-op, not a guard against the copy
    // fallback (which `AlreadyExists` never reaches).
    hashes.sort_unstable();
    hashes.dedup();

    let src_blobs = src_dir.join(BLOBS_DIR_NAME);
    let child_blobs = child_dir.join(BLOBS_DIR_NAME);
    fs::create_dir_all(&child_blobs).await?;
    crate::log::set_permissions(&child_blobs, 0o700).await?;

    for hash in hashes {
        let src_blob = src_blobs.join(hash);
        let child_blob = child_blobs.join(hash);

        match fs::hard_link(&src_blob, &child_blob).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    blob = hash,
                    path = %src_blob.display(),
                    "fork: referenced blob missing on parent's disk, skipping"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Destination already exists — most likely a retried fork against the same
                // child_dir. Blobs are content-addressed by hash (see `validate_blob_hash` and
                // the module doc), so a pre-existing entry at this hash-named path is assumed to
                // already be the right content. Treat as a no-op: NOT the generic fallback below,
                // since `fs::copy` onto an existing hard-link truncates the shared inode to 0
                // bytes, corrupting every link to it (including the parent's original blob).
                //
                // TODO(critic): the copy fallback below writes directly to `child_blob` rather
                // than a `.tmp` path + rename, so it is not atomic. If a prior run's fallback
                // (triggered by genuine EXDEV) was interrupted mid-write, it can leave a
                // truncated file at this path; this no-op would then silently accept that
                // truncated file as "already correct" on retry. No concurrent-retry call site
                // exists yet, so this is a documented known gap rather than a fix — an atomic
                // write (temp file + rename) would close it if/when retries become concurrent.
                tracing::debug!(
                    blob = hash,
                    path = %child_blob.display(),
                    "fork: blob already linked in child, skipping"
                );
            }
            Err(_) => {
                // Hard-link failed for a reason other than a missing source or an already-linked
                // destination (e.g. cross-device link, EXDEV) — fall back to a full copy. Reached
                // only when the destination does not exist (dest-exists implies AlreadyExists on
                // all target platforms), so writing directly to `child_blob` here is safe.
                fs::copy(&src_blob, &child_blob).await?;
            }
        }
    }

    Ok(())
}

/// Rejects any `image_refs` hash that is not a non-empty, bare hex string, before it is used in
/// a [`Path::join`] (#5982 follow-up). Content hashes elsewhere in this codebase are BLAKE3 hex
/// (64 lowercase chars, `zeph_common::hash::blake3_hex`), but no length is enforced here since
/// no blob writer exists yet to fix the format — a bare hexdigit charset already rules out `/`,
/// `..`, and absolute paths, which is what makes `join` safe.
fn validate_blob_hash(hash: &str) -> Result<(), SessionError> {
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SessionError::InvalidBlobHash(hash.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::SessionStore;

    async fn make_pool() -> zeph_db::DbPool {
        let config = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        };
        let pool = config
            .connect()
            .await
            .expect("connect in-memory sqlite pool");
        zeph_db::run_migrations(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn seed_parent(data_dir: &Path, store: &SessionStore, id: &str) {
        store.create(id).await.unwrap();
        let dir = crate::session_dir(data_dir, id);
        let log = SessionEventLog::open(&dir).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionStarted {
                session_id: id.to_owned(),
                cwd: "/repo".to_owned(),
                provider_name: "claude".to_owned(),
                model: "opus".to_owned(),
                forked_from: None,
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "hello".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::AssistantMessage {
                parts: vec![zeph_llm::provider::MessagePart::Text {
                    text: "hi".to_owned(),
                }],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "second turn".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        store
            .update_seq(id, log.last_seq().unwrap(), 4)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_copies_events() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let result = ForkEngine::fork(data_dir.path(), "parent", "child", Some(3), &store, None)
            .await
            .unwrap();
        assert_eq!(result.events_copied, 3);
        assert_eq!(result.new_session_id, "child");

        let child_dir = crate::session_dir(data_dir.path(), &result.new_session_id);
        let child_log = SessionEventLog::open(&child_dir).await.unwrap();
        let events = child_log.read_all().await.unwrap();
        // 1 synthesized SessionStarted header + 3 copied events.
        assert_eq!(events.len(), 4);
    }

    /// Issue #6360, S-new-3 (critic rev3): fork must not launder a tampered parent's history into
    /// a "fresh, trusted" child. `ForkEngine::fork` reads the parent via `SessionEventLog::read_all`
    /// (chain-verified) and separately validates the cut point via `ReplayEngine::replay`
    /// (also chain-verified) — either one must reject a tampered parent before any event is
    /// copied into the child log.
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_rejects_a_tampered_parent_chain() {
        let _guard = crate::log::IntegrityConfigGuard::new();
        let ring = Arc::new(zeph_common::hash_chain::ChainKeyRing::new(
            0,
            zeph_common::hash_chain::ChainKey::new([77u8; 32]),
        ));
        crate::log::configure_history_integrity(Some(ring));

        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let events_path = crate::session_dir(data_dir.path(), "parent").join("events.jsonl");
        let raw = std::fs::read_to_string(&events_path).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        assert!(
            lines.len() >= 2,
            "fixture must have a non-first line to tamper"
        );
        let tampered = lines[1].replace("hello", "forged-approval");
        lines[1] = &tampered;
        std::fs::write(&events_path, lines.join("\n") + "\n").unwrap();

        let err = ForkEngine::fork(data_dir.path(), "parent", "child", Some(3), &store, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SessionError::Integrity(_)),
            "tampering the parent's chain must abort the fork with an Integrity error, not \
             silently produce a child; got {err:?}"
        );

        // The child directory must not exist as a fresh, trusted session — fork failed before
        // any laundering could occur.
        let child_dir = crate::session_dir(data_dir.path(), "child");
        assert!(
            !child_dir.join("events.jsonl").exists(),
            "a rejected fork must not leave behind a partially-written child log"
        );
    }

    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_provenance_metadata() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        ForkEngine::fork(data_dir.path(), "parent", "child", Some(2), &store, None)
            .await
            .unwrap();

        let meta = store.get("child").await.unwrap().unwrap();
        assert_eq!(meta.forked_from.as_deref(), Some("parent"));
        assert_eq!(meta.forked_at_seq, Some(2));
    }

    /// Regression test (#5868): `ForkEngine::fork`'s `owner` argument must reach the child
    /// row's `owner_key` column end-to-end (through `record_fork`), not just at the
    /// `SessionStore::record_fork` unit level.
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn fork_propagates_owner_to_child_row() {
        let pool = make_pool().await;
        let store = SessionStore::new(pool.clone());
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        ForkEngine::fork(
            data_dir.path(),
            "parent",
            "child",
            Some(2),
            &store,
            Some("alice"),
        )
        .await
        .unwrap();

        let owner_key: Option<String> = zeph_db::query_scalar(zeph_db::sql!(
            "SELECT owner_key FROM acp_sessions WHERE id = ?"
        ))
        .bind("child")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(owner_key.as_deref(), Some("alice"));
    }

    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_appends_forkpoint_to_parent() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        ForkEngine::fork(data_dir.path(), "parent", "child", Some(2), &store, None)
            .await
            .unwrap();

        let parent_dir = crate::session_dir(data_dir.path(), "parent");
        let parent_log = SessionEventLog::open(&parent_dir).await.unwrap();
        let events = parent_log.read_all().await.unwrap();
        assert!(matches!(
            events.last().unwrap().kind,
            SessionEvent::ForkPoint { .. }
        ));
    }

    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_rejects_seq_beyond_source() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let err = ForkEngine::fork(data_dir.path(), "parent", "child", Some(100), &store, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidForkPoint(_)));
    }

    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_rejects_unknown_source() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();

        let err = ForkEngine::fork(data_dir.path(), "no-such", "child", Some(0), &store, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_none_copies_everything() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let result = ForkEngine::fork(data_dir.path(), "parent", "child", None, &store, None)
            .await
            .unwrap();
        // seed_parent appends 4 events total.
        assert_eq!(result.events_copied, 4);
    }

    /// Regression test for #5982 (spec §7.2 step 6): a blob referenced by a copied
    /// `UserMessage.image_refs` must be hard-linked into the child's `blobs/` directory.
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_copies_referenced_blobs() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let parent_dir = crate::session_dir(data_dir.path(), "parent");
        let parent_blobs = parent_dir.join("blobs");
        tokio::fs::create_dir_all(&parent_blobs).await.unwrap();
        tokio::fs::write(parent_blobs.join("a1b2c3"), b"image-bytes")
            .await
            .unwrap();

        let parent_log = SessionEventLog::open(&parent_dir).await.unwrap();
        parent_log
            .append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: "with image".to_owned(),
                    image_refs: vec!["a1b2c3".to_owned()],
                },
            )
            .await
            .unwrap();
        store.update_seq("parent", 4, 5).await.unwrap();

        let result = ForkEngine::fork(data_dir.path(), "parent", "child", Some(5), &store, None)
            .await
            .unwrap();
        assert_eq!(result.events_copied, 5);

        let child_dir = crate::session_dir(data_dir.path(), "child");
        let child_blob = child_dir.join("blobs").join("a1b2c3");
        let copied = tokio::fs::read(&child_blob).await.unwrap();
        assert_eq!(copied, b"image-bytes");
    }

    /// Regression test for #5982: a referenced blob missing on the parent's disk must not fail
    /// the fork — it is logged and skipped, since the event-log copy (the fork's primary
    /// content) already succeeded.
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_skips_missing_blob_without_failing() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let parent_dir = crate::session_dir(data_dir.path(), "parent");
        let parent_log = SessionEventLog::open(&parent_dir).await.unwrap();
        parent_log
            .append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: "with missing image".to_owned(),
                    image_refs: vec!["deadbeef".to_owned()],
                },
            )
            .await
            .unwrap();
        store.update_seq("parent", 4, 5).await.unwrap();

        let result = ForkEngine::fork(data_dir.path(), "parent", "child", Some(5), &store, None)
            .await
            .unwrap();
        assert_eq!(result.events_copied, 5);

        let child_dir = crate::session_dir(data_dir.path(), "child");
        assert!(!child_dir.join("blobs").join("deadbeef").exists());
    }

    /// Regression test for #5982: when no copied event references a blob, `fork` must not
    /// create an empty `blobs/` directory in the child (keeps the eager-copy path a no-op for
    /// the common, image-free case).
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_without_image_refs_creates_no_blobs_dir() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        ForkEngine::fork(data_dir.path(), "parent", "child", Some(2), &store, None)
            .await
            .unwrap();

        let child_dir = crate::session_dir(data_dir.path(), "child");
        assert!(!child_dir.join("blobs").exists());
    }

    /// Regression test for the critic's S3 finding: a malicious `image_refs` entry containing a
    /// path-traversal sequence must be rejected before it reaches `PathBuf::join`, not silently
    /// joined (which would let the parent-side `hard_link` read an arbitrary file, or the
    /// child-side path escape `blobs/`).
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_rejects_path_traversal_in_image_refs() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let parent_dir = crate::session_dir(data_dir.path(), "parent");
        let parent_log = SessionEventLog::open(&parent_dir).await.unwrap();
        parent_log
            .append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: "malicious ref".to_owned(),
                    image_refs: vec!["../../../etc/passwd".to_owned()],
                },
            )
            .await
            .unwrap();
        store.update_seq("parent", 4, 5).await.unwrap();

        let err = ForkEngine::fork(data_dir.path(), "parent", "child", Some(5), &store, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidBlobHash(_)));

        // No child directory content should have been created by the rejected fork attempt.
        let child_dir = crate::session_dir(data_dir.path(), "child");
        assert!(!child_dir.join("blobs").exists());
    }

    /// Regression test for the critic's S3 finding: an absolute-path `image_refs` entry must
    /// also be rejected — `PathBuf::join` with an absolute path silently discards the base
    /// directory entirely, which is the most severe form of this traversal.
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_rejects_absolute_path_in_image_refs() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let parent_dir = crate::session_dir(data_dir.path(), "parent");
        let parent_log = SessionEventLog::open(&parent_dir).await.unwrap();
        parent_log
            .append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: "malicious absolute ref".to_owned(),
                    image_refs: vec!["/etc/passwd".to_owned()],
                },
            )
            .await
            .unwrap();
        store.update_seq("parent", 4, 5).await.unwrap();

        let err = ForkEngine::fork(data_dir.path(), "parent", "child", Some(5), &store, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidBlobHash(_)));
    }

    /// Regression test for the critic's M3 finding: the same hash referenced twice in the
    /// copied range must not trigger the cross-device copy fallback on the second occurrence —
    /// the hash list is deduped before any `hard_link` is attempted.
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_dedups_duplicate_blob_hash() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let parent_dir = crate::session_dir(data_dir.path(), "parent");
        let parent_blobs = parent_dir.join("blobs");
        tokio::fs::create_dir_all(&parent_blobs).await.unwrap();
        tokio::fs::write(parent_blobs.join("cafe01"), b"shared-bytes")
            .await
            .unwrap();

        let parent_log = SessionEventLog::open(&parent_dir).await.unwrap();
        parent_log
            .append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: "first ref".to_owned(),
                    image_refs: vec!["cafe01".to_owned()],
                },
            )
            .await
            .unwrap();
        parent_log
            .append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: "second ref, same hash".to_owned(),
                    image_refs: vec!["cafe01".to_owned()],
                },
            )
            .await
            .unwrap();
        store.update_seq("parent", 4, 6).await.unwrap();

        let result = ForkEngine::fork(data_dir.path(), "parent", "child", Some(6), &store, None)
            .await
            .unwrap();
        assert_eq!(result.events_copied, 6);

        let child_dir = crate::session_dir(data_dir.path(), "child");
        let child_blob = child_dir.join("blobs").join("cafe01");
        assert_eq!(tokio::fs::read(&child_blob).await.unwrap(), b"shared-bytes");
    }

    /// Regression test for #6153: re-running `copy_referenced_blobs` against the SAME
    /// `child_dir` (e.g. a retried fork against the same `new_id`) must not corrupt the
    /// shared blob. Before the fix, the second `hard_link` attempt returned `AlreadyExists`,
    /// which fell into the generic `Err(_) => fs::copy` fallback arm; `fs::copy` onto a
    /// destination that is already a hard link to the source truncates the shared inode to 0
    /// bytes, corrupting every link to it — including the parent's original blob.
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_copy_referenced_blobs_retry_does_not_truncate_shared_blob() {
        let data_dir = tempfile::tempdir().unwrap();
        let src_dir = data_dir.path().join("parent");
        let child_dir = data_dir.path().join("child");

        let src_blobs = src_dir.join("blobs");
        tokio::fs::create_dir_all(&src_blobs).await.unwrap();
        let original_content = b"image-bytes-not-empty";
        tokio::fs::write(src_blobs.join("a1b2c3"), original_content)
            .await
            .unwrap();

        let events = vec![SessionEventEnvelope {
            seq: 0,
            ts_ms: 0,
            turn_id: None,
            parent_seq: None,
            kind: SessionEvent::UserMessage {
                text: "with image".to_owned(),
                image_refs: vec!["a1b2c3".to_owned()],
            },
            chain: None,
        }];

        // First run: hard-links the blob into the child.
        copy_referenced_blobs(&src_dir, &child_dir, &events)
            .await
            .unwrap();

        let child_blob = child_dir.join("blobs").join("a1b2c3");
        assert_eq!(
            tokio::fs::read(&child_blob).await.unwrap(),
            original_content
        );

        // Second run against the SAME child_dir — this is what previously triggered
        // AlreadyExists -> fs::copy -> truncation.
        copy_referenced_blobs(&src_dir, &child_dir, &events)
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read(&child_blob).await.unwrap(),
            original_content,
            "child blob must not be truncated by a retried fork against the same child_dir"
        );
        assert_eq!(
            tokio::fs::read(src_blobs.join("a1b2c3")).await.unwrap(),
            original_content,
            "parent's original blob must not be truncated by a retried fork against the same child_dir"
        );
    }

    /// Regression test for the critic's M1 finding: the child's `blobs/` directory must get the
    /// same `0o700` permission the crate already enforces on the sibling session directory.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(session_history_integrity)]
    async fn test_fork_sets_0700_on_child_blobs_dir() {
        use std::os::unix::fs::PermissionsExt;

        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();
        seed_parent(data_dir.path(), &store, "parent").await;

        let parent_dir = crate::session_dir(data_dir.path(), "parent");
        let parent_blobs = parent_dir.join("blobs");
        tokio::fs::create_dir_all(&parent_blobs).await.unwrap();
        tokio::fs::write(parent_blobs.join("a1b2c3"), b"image-bytes")
            .await
            .unwrap();

        let parent_log = SessionEventLog::open(&parent_dir).await.unwrap();
        parent_log
            .append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: "with image".to_owned(),
                    image_refs: vec!["a1b2c3".to_owned()],
                },
            )
            .await
            .unwrap();
        store.update_seq("parent", 4, 5).await.unwrap();

        ForkEngine::fork(data_dir.path(), "parent", "child", Some(5), &store, None)
            .await
            .unwrap();

        let child_dir = crate::session_dir(data_dir.path(), "child");
        let meta = tokio::fs::metadata(child_dir.join("blobs")).await.unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    }
}
