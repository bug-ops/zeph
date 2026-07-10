// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ForkEngine`]: eager-copy session forking (spec §7).
//!
//! Copy-on-write forking is explicitly deferred (spec §7.2, §15 NEVER) — eager copy is simple and
//! self-contained for MVP, and robust to either side independently condensing the shared prefix
//! afterward (the child log is fully self-contained; `forked_at_seq` is historical metadata only).

use std::path::Path;

use crate::error::SessionError;
use crate::event::SessionEvent;
use crate::log::SessionEventLog;
use crate::replay::ReplayEngine;
use crate::store::SessionStore;

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

#[cfg(test)]
mod tests {
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

    #[tokio::test]
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
    async fn test_fork_rejects_unknown_source() {
        let store = SessionStore::new(make_pool().await);
        let data_dir = tempfile::tempdir().unwrap();

        let err = ForkEngine::fork(data_dir.path(), "no-such", "child", Some(0), &store, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
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
}
