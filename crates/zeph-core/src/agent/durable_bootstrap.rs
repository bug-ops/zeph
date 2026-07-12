// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared durable-backend construction, used by both the P1 (agent-turn) and P2
//! (orchestration) durable adapters so backend/writer setup stays consistent across every
//! adapter that reads the shared `[durable]` config section (#5452).
//!
//! This module owns only the mechanical "open backend, init schema, attach cipher, spawn
//! writer" sequence. Each adapter keeps its own cache slot (`services.orchestration.durable_*`
//! for P2, `services.session.durable_*` for P1) and its own [`zeph_durable::ExecutionId`]
//! derivation — those decisions are adapter-specific and stay in `plan.rs` / `durable_bootstrap.rs`
//! respectively.

use std::sync::Arc;

use zeph_durable::{DurableBackendEnum, JournalWriterHandle, LocalBackend, PayloadCipher};

use crate::agent::Agent;
use crate::channel::Channel;

/// Open a [`LocalBackend`] at `db_url`, initialise its schema, attach `cipher` and `hmac_key` if
/// present, and spawn its [`JournalWriter`](zeph_durable::JournalWriter) actor via
/// `task_supervisor`.
///
/// `hmac_key` is `None` for a single-user local, non-shared database (INV-8) — the documented
/// stance where control entries carry no HMAC.
///
/// Returns `None` (after logging a `tracing::warn!`) on any I/O failure so callers degrade to
/// non-durable mode rather than fail session bootstrap (#5452 FR-004).
pub(crate) async fn open_durable_backend(
    task_supervisor: &zeph_common::TaskSupervisor,
    writer_task_name: &'static str,
    cfg: &zeph_config::DurableConfig,
    db_url: &str,
    cipher: Option<Arc<dyn PayloadCipher>>,
    hmac_key: Option<[u8; 32]>,
) -> Option<(
    Arc<DurableBackendEnum>,
    JournalWriterHandle,
    zeph_common::task_supervisor::BlockingHandle<()>,
)> {
    let local = match LocalBackend::open(db_url, cfg.max_payload_bytes).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, db_url, "durable: failed to open backend; skipping");
            return None;
        }
    };
    if let Err(e) = local.init().await {
        tracing::warn!(error = %e, "durable: failed to init schema; skipping");
        return None;
    }
    let local = if let Some(c) = cipher {
        local.with_cipher(c)
    } else {
        local
    };
    let local = if let Some(k) = hmac_key {
        local.with_hmac_key(k)
    } else {
        local
    };
    let local = Arc::new(local);
    let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
    let (writer_actor, handle) = zeph_durable::JournalWriter::new(local, cfg);
    let task_handle =
        task_supervisor.spawn_oneshot(Arc::from(writer_task_name), move || async move {
            writer_actor.run().await;
        });
    Some((backend, handle, task_handle))
}

impl<C: Channel> Agent<C> {
    /// Lazily construct the session's [`DurableContext`](zeph_durable::DurableContext) for the
    /// P1 agent-turn adapter (#5452), the first time a durable-gated call site needs it.
    ///
    /// Deferred to first use (rather than built eagerly in the `AgentBuilder` chain) because the
    /// real, shutdown-linked `TaskSupervisor` is only attached via `with_task_supervisor` late in
    /// bootstrap — constructing here (well after `.build()`) guarantees the journal-writer actor
    /// spawns onto the correct supervisor. A no-op after the first attempt (success or failure):
    /// `durable_ctx_init_attempted` suppresses retrying I/O on every subsequent turn.
    ///
    /// The execution is keyed on the session's `ConversationId` (not per-turn), so every turn in
    /// the session journals as a step within the *same* execution and a crash mid-session can
    /// resume from any prior turn's journal state.
    pub(crate) async fn ensure_session_durable_ctx(&mut self) {
        if self.services.session.durable_ctx.is_some()
            || self.services.session.durable_ctx_init_attempted
        {
            return;
        }
        self.services.session.durable_ctx_init_attempted = true;

        let Some(cfg) = self.services.session.durable_agent_turns_config.clone() else {
            return;
        };
        let Some(db_url) = self.services.session.durable_agent_turns_db_url.clone() else {
            return;
        };
        let sqlite_path = self
            .services
            .session
            .durable_agent_turns_sqlite_path
            .clone()
            .unwrap_or_default();
        let Some(conversation_id) = self.services.memory.persistence.conversation_id else {
            tracing::warn!(
                "durable agent_turns: no conversation_id at bootstrap; degrading to non-durable"
            );
            return;
        };
        let cipher = self.services.session.durable_agent_turns_cipher.clone();
        let hmac_key = self.services.session.durable_agent_turns_hmac_key;

        tracing::debug!("durable agent_turns: opening backend start");
        let backend_result = open_durable_backend(
            &self.runtime.lifecycle.task_supervisor,
            "agent.durable.turn_journal_writer",
            &cfg,
            &db_url,
            cipher,
            hmac_key,
        )
        .await;
        tracing::debug!("durable agent_turns: opening backend done");
        let Some((backend, writer, task_handle)) = backend_result else {
            tracing::warn!(
                "durable agent_turns: backend construction failed; degrading to non-durable"
            );
            return;
        };

        let zeph_durable::DurableBackendEnum::Local(local_backend) = &*backend else {
            tracing::warn!(
                "durable agent_turns: only LocalBackend is supported; degrading to non-durable"
            );
            return;
        };

        // Fold `sqlite_path` in alongside the fixed-width `ConversationId` bytes so that even if
        // two distinct memory databases were ever configured to share the same durable journal
        // `db_url`, their first-ever conversation (always `ConversationId(1)`) still cannot
        // derive the same `ExecutionId` (#5553). The journal-file-per-database fix in
        // `resolve_durable_db_url` already prevents the collision in the common case; this is
        // defense in depth for that derivation.
        let mut exec_payload = conversation_id.0.to_le_bytes().to_vec();
        exec_payload.extend_from_slice(sqlite_path.as_bytes());
        let exec_id = zeph_durable::ExecutionId::derive(b"zeph.agent_turn.v1", &exec_payload);
        tracing::debug!("durable agent_turns: open_execution start");
        let open_execution_result = local_backend
            .open_execution(exec_id, zeph_durable::ExecutionKind::AgentTurn)
            .await;
        tracing::debug!("durable agent_turns: open_execution done");
        let is_resume = match open_execution_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "durable agent_turns: open_execution failed; degrading to non-durable"
                );
                return;
            }
        };

        let ctx = zeph_durable::DurableContext::new(
            exec_id,
            zeph_durable::ExecutionKind::AgentTurn,
            is_resume,
            backend,
            writer.clone(),
            &cfg,
        );

        tracing::info!(
            execution_id = %exec_id.as_uuid(),
            is_resume,
            "durable agent_turns: DurableContext attached to session"
        );
        self.services.session.durable_ctx = Some(Arc::new(ctx));
        self.services.session.durable_writer = Some(writer);
        self.services.session.durable_writer_task = Some(task_handle);
    }

    /// Detach the P1 durable execution before a conversation switch (`/new`, `/conv resume`,
    /// `/conv fork` — #5452 critic finding S1).
    ///
    /// `ensure_session_durable_ctx` keys its `ExecutionId` on `ConversationId` and then latches
    /// `durable_ctx_init_attempted` so it never re-derives the execution again. Without this
    /// reset, every turn after a conversation switch would keep journaling under the *old*
    /// conversation's execution — silently mixing two conversations' turn state and defeating the
    /// per-conversation crash-resume the keying is meant to provide. Flushes and aborts the old
    /// writer (best-effort, same 2s deadline as `flush_durable_writer` on shutdown) before
    /// clearing the session's durable fields so the next durable-gated call re-derives a fresh
    /// execution for the new `conversation_id`.
    pub(in crate::agent) async fn reset_durable_ctx_for_conversation_switch(&mut self) {
        if let Some(ref writer) = self.services.session.durable_writer {
            let flush_deadline = std::time::Duration::from_secs(2);
            match tokio::time::timeout(flush_deadline, writer.flush()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        "durable agent_turns writer: flush on conversation switch failed"
                    );
                }
                Err(_) => tracing::warn!(
                    "durable agent_turns writer: flush timed out on conversation switch"
                ),
            }
        }
        if let Some(h) = self.services.session.durable_writer_task.take() {
            h.abort();
        }
        self.services.session.durable_ctx = None;
        self.services.session.durable_writer = None;
        self.services.session.durable_ctx_init_attempted = false;
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::agent_tests::*;

    fn agent_with_conversation() -> crate::agent::Agent<MockChannel> {
        let provider = mock_provider(vec!["ok".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.memory.persistence.conversation_id = Some(zeph_memory::ConversationId(1));
        agent
    }

    #[tokio::test]
    async fn populates_durable_ctx_when_agent_turns_enabled() {
        let mut agent = agent_with_conversation();
        agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
            enabled: true,
            agent_turns: true,
            ..zeph_config::DurableConfig::default()
        });
        agent.services.session.durable_agent_turns_db_url = Some(":memory:".to_owned());

        agent.ensure_session_durable_ctx().await;

        assert!(agent.services.session.durable_ctx.is_some());
        assert!(agent.services.session.durable_writer.is_some());
        assert!(agent.services.session.durable_ctx_init_attempted);
    }

    #[tokio::test]
    async fn stays_none_when_agent_turns_not_configured() {
        // FR-002: no `with_durable_agent_turns` call at all (the builder-level gate), so the
        // session's stash fields are `None` — mirrors a plain `[durable] enabled=false` deployment.
        let mut agent = agent_with_conversation();

        agent.ensure_session_durable_ctx().await;

        assert!(agent.services.session.durable_ctx.is_none());
        assert!(agent.services.session.durable_ctx_init_attempted);
    }

    #[tokio::test]
    async fn degrades_when_conversation_id_missing() {
        // FR-004: construction must not panic or hard-fail bootstrap when the conversation_id
        // gate can't be satisfied — it degrades to non-durable instead.
        let provider = mock_provider(vec!["ok".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
            enabled: true,
            agent_turns: true,
            ..zeph_config::DurableConfig::default()
        });
        agent.services.session.durable_agent_turns_db_url = Some(":memory:".to_owned());

        agent.ensure_session_durable_ctx().await;

        assert!(agent.services.session.durable_ctx.is_none());
    }

    #[tokio::test]
    async fn is_a_noop_after_first_attempt() {
        let mut agent = agent_with_conversation();
        agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
            enabled: true,
            agent_turns: true,
            ..zeph_config::DurableConfig::default()
        });
        agent.services.session.durable_agent_turns_db_url = Some(":memory:".to_owned());

        agent.ensure_session_durable_ctx().await;
        let first = agent
            .services
            .session
            .durable_ctx
            .clone()
            .expect("durable_ctx should be populated");

        // Second call must not reconstruct — same Arc instance, no panic on double-init.
        agent.ensure_session_durable_ctx().await;
        let second = agent
            .services
            .session
            .durable_ctx
            .clone()
            .expect("durable_ctx should still be populated");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn conversation_switch_rebinds_execution_id() {
        // Regression test for critic finding S1: a conversation switch must not leave the P1
        // execution bound to the stale (old) ConversationId.
        let mut agent = agent_with_conversation();
        agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
            enabled: true,
            agent_turns: true,
            ..zeph_config::DurableConfig::default()
        });
        agent.services.session.durable_agent_turns_db_url = Some(":memory:".to_owned());

        agent.ensure_session_durable_ctx().await;
        let first_exec_id = agent
            .services
            .session
            .durable_ctx
            .as_ref()
            .expect("durable_ctx should be populated")
            .execution_id();

        // Simulate `reset_conversation`'s durable-detach step, then the new conversation_id.
        agent.reset_durable_ctx_for_conversation_switch().await;
        assert!(
            agent.services.session.durable_ctx.is_none(),
            "durable_ctx must be cleared by the switch"
        );
        assert!(
            !agent.services.session.durable_ctx_init_attempted,
            "latch must be reset so the next call re-derives the execution"
        );
        agent.services.memory.persistence.conversation_id = Some(zeph_memory::ConversationId(2));

        agent.ensure_session_durable_ctx().await;
        let second_exec_id = agent
            .services
            .session
            .durable_ctx
            .as_ref()
            .expect("durable_ctx should be repopulated for the new conversation")
            .execution_id();

        assert_ne!(
            first_exec_id, second_exec_id,
            "a conversation switch must rebind the P1 execution to the new conversation_id"
        );
    }

    #[tokio::test]
    async fn distinct_sqlite_paths_do_not_collide_on_first_conversation() {
        // Regression test for #5553: two agents pointed at different memory databases (but
        // sharing the same durable `db_url`, e.g. via directory collision) must not derive the
        // same `ExecutionId` for their respective first-ever `ConversationId(1)`.
        async fn exec_id_for(sqlite_path: &str) -> zeph_durable::ExecutionId {
            let mut agent = agent_with_conversation();
            agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
                enabled: true,
                agent_turns: true,
                ..zeph_config::DurableConfig::default()
            });
            agent.services.session.durable_agent_turns_db_url = Some(":memory:".to_owned());
            agent.services.session.durable_agent_turns_sqlite_path = Some(sqlite_path.to_owned());

            agent.ensure_session_durable_ctx().await;
            agent
                .services
                .session
                .durable_ctx
                .as_ref()
                .expect("durable_ctx should be populated")
                .execution_id()
        }

        let a = Box::pin(exec_id_for("/data/alpha/zeph.db")).await;
        let b = Box::pin(exec_id_for("/data/beta/zeph.db")).await;

        assert_ne!(
            a, b,
            "two databases' first conversation must not derive the same ExecutionId"
        );
    }

    #[tokio::test]
    async fn legacy_shared_durable_db_upgrade_path_does_not_collide() {
        // Regression for #5553's "upgrade" scenario: a directory already has a legacy bare
        // `durable.db` (the pre-fix layout), so `resolve_durable_db_url` (src/commands/durable.rs)
        // deliberately keeps every database in that directory pointed at the *same* legacy file
        // rather than namespacing it — this is the one path where the file-separation half of the
        // fix does NOT kick in. The `ExecutionId` fold over `sqlite_path` (the defense-in-depth
        // half, exercised here through the real production code path) is the only thing that
        // still prevents a second database's first-ever conversation from colliding with the
        // first database's execution already journaled in that shared file.
        async fn bootstrap(
            legacy_db_url: &str,
            sqlite_path: &str,
        ) -> crate::agent::Agent<MockChannel> {
            let mut agent = agent_with_conversation();
            agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
                enabled: true,
                agent_turns: true,
                ..zeph_config::DurableConfig::default()
            });
            agent.services.session.durable_agent_turns_db_url = Some(legacy_db_url.to_owned());
            agent.services.session.durable_agent_turns_sqlite_path = Some(sqlite_path.to_owned());
            agent.ensure_session_durable_ctx().await;
            agent
        }

        let dir = tempfile::tempdir().unwrap();
        let legacy_db_url = dir.path().join("durable.db").to_string_lossy().into_owned();
        let sqlite_a = dir.path().join("alpha.db").to_string_lossy().into_owned();
        let sqlite_b = dir.path().join("beta.db").to_string_lossy().into_owned();

        // DB A runs first, journaling its first-conversation execution into the legacy file.
        let agent_a = Box::pin(bootstrap(&legacy_db_url, &sqlite_a)).await;
        let exec_a = agent_a
            .services
            .session
            .durable_ctx
            .as_ref()
            .expect("DB A's durable_ctx should be populated")
            .execution_id();

        // DB B is a distinct database but, per the legacy-preferred branch of
        // `resolve_durable_db_url`, resolves to the SAME shared journal file.
        let agent_b = Box::pin(bootstrap(&legacy_db_url, &sqlite_b)).await;
        let exec_b = agent_b
            .services
            .session
            .durable_ctx
            .as_ref()
            .expect("DB B's durable_ctx should be populated")
            .execution_id();

        assert_ne!(
            exec_a, exec_b,
            "DB B's first conversation must not collide with DB A's execution in the shared legacy journal"
        );

        // Confirm both landed as two genuinely distinct rows in the shared file, not one
        // execution spuriously "resumed" by the other.
        let backend = zeph_durable::LocalBackend::open(&legacy_db_url, 1_000_000)
            .await
            .expect("legacy journal file must be openable after both bootstraps");
        let executions = backend
            .list_executions(None, None, 10)
            .await
            .expect("list_executions must succeed");
        assert_eq!(
            executions.len(),
            2,
            "the shared legacy journal must contain two distinct executions, not a collapsed one"
        );
    }
}
