// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builds the per-session `build_agent` closure handed to `SessionActor::spawn` (spec-068 §9.4,
//! #5343).
//!
//! Mirrors `src/acp.rs`'s `SessionSink` wiring in `spawn_acp_agent`: a durable
//! [`zeph_session::SessionEventLog`] is opened at `[serve] data_dir` (via
//! [`ServeAgentDeps::session_persistence_config`]) named after the session's own
//! [`zeph_common::SessionId`], then wrapped in a [`zeph_agent_persistence::SessionSink`] and
//! attached via `Agent::with_session_sink`. When `[session] enabled = false`, the agent still
//! works — it just persists only the `SQLite` `messages` projection, matching every other
//! channel's fallback behavior.
//!
//! Routes through the shared [`zeph_agent_persistence::hydrate_and_condense`] pipeline (D-10,
//! D-13) unconditionally — a brand-new session's log is empty, so hydration is a harmless no-op
//! there, and a *reactivated* session (D-12: `POST /sessions/:id/prompt` on an id whose actor was
//! evicted or the process restarted) gets its prior turns correctly replayed into
//! `with_preloaded_messages`, and durably condensed if over threshold, before the agent starts —
//! exactly like ACP/CLI/`/conv` resume (D-13: all four session-open paths share this one
//! pipeline). One `build_agent_factory` covers both "create" and "reactivate" without a separate
//! code path.
//!
//! **Why the async work happens here, not inside the closure**: `SessionActor::spawn` (D-8)
//! calls `build_agent(channel)` synchronously on a bare `std::thread` — *before* that thread's
//! `current_thread` Tokio runtime is entered — specifically so the `!Send` `Agent` never has to
//! cross a runtime boundary. Calling `.await` (or `Handle::current().block_on`) inside that
//! closure would panic ("no reactor running"). So [`build_agent_factory`] is itself `async`: it
//! does the `hydrate_and_condense`/`SessionStore::create` I/O once, up front, on the caller's
//! (HTTP handler's) runtime, then returns a plain synchronous closure that only captures the
//! already-built `Option<Arc<SessionSink>>` and replayed `Vec<Message>`.

use std::sync::Arc;

use zeph_core::agent::Agent;
use zeph_core::channel::LoopbackChannel;

use super::deps::ServeAgentDeps;

/// Bounds the retry-on-`AlreadyLocked` loop in [`hydrate_session_sink`] (#5487 fix 3).
///
/// `evict_loop` (`src/serve/mod.rs`) removes an idle session from `LiveSessionRegistry` and
/// cancels its actor *before* that actor's dedicated thread has necessarily finished dropping
/// its `SessionEventLog` (and releasing the actor's flock) — a fast reactivation request for the
/// same `session_id` can race the still-draining old actor and see a transient `AlreadyLocked`
/// that is not a genuine second writer, just a slow release. Retrying a few times with a short
/// backoff resolves this without restructuring `LiveSessionRegistry`/`evict_loop` to award the
/// reactivation path direct visibility into the old actor's drain-completion signal.
const REACTIVATION_LOCK_RETRY_ATTEMPTS: u32 = 5;
/// Delay between retry attempts — see [`REACTIVATION_LOCK_RETRY_ATTEMPTS`].
const REACTIVATION_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Build a `build_agent` closure for [`zeph_core::serve::SessionActor::spawn`].
///
/// Opens (and, for a reactivated session, replays) the session's durable event log — if
/// `[session] enabled = true` — before returning, so the returned closure is a plain synchronous
/// `FnOnce` — safe to call from `SessionActor::spawn`'s dedicated thread before its Tokio
/// runtime is entered. Captures only `Send`-safe values (`deps` is `Clone`,
/// `session_id`/`conversation_id` are owned, `SessionSink` is `Send + Sync`, replayed messages
/// are plain data) — the `LoopbackChannel` and the resulting `!Send` `Agent` never leave that
/// thread.
#[tracing::instrument(
    name = "serve.agent_factory.build",
    skip_all,
    level = "info",
    fields(session_id = session_id.as_str())
)]
pub(crate) async fn build_agent_factory(
    deps: ServeAgentDeps,
    session_id: zeph_common::SessionId,
    conversation_id: zeph_memory::ConversationId,
) -> impl FnOnce(LoopbackChannel) -> Agent<LoopbackChannel> + Send + 'static {
    let (session_sink, preloaded_messages) = if deps.session_persistence_config.enabled {
        hydrate_session_sink(
            &deps.session_persistence_config,
            &deps.memory,
            session_id.clone(),
            conversation_id,
            &deps.resume_condenser,
            deps.resume_token_counter.as_ref(),
            deps.session_config.budget_tokens,
        )
        .await
    } else {
        (None, Vec::new())
    };

    move |channel| {
        // Capture before apply_session_config consumes deps.session_config (mirrors
        // spawn_acp_agent's debug_config capture in src/acp.rs).
        let debug_config = deps.session_config.debug_config.clone();
        let mut agent = Agent::new_with_registry_arc(
            deps.provider,
            deps.embedding_provider,
            channel,
            deps.registry,
            deps.matcher,
            deps.max_active_skills,
            zeph_tools::DynExecutor(deps.tool_executor),
        )
        .apply_session_config(deps.session_config)
        .with_memory(
            Arc::clone(&deps.memory),
            conversation_id,
            deps.history_limit,
            deps.recall_limit,
            deps.summarization_threshold,
        )
        .with_session_sink(session_sink)
        .with_session_persistence_config(Some(deps.session_persistence_config))
        .with_provider_pool(deps.provider_pool, deps.provider_config_snapshot);
        if !preloaded_messages.is_empty() {
            agent = agent.with_preloaded_messages(preloaded_messages);
        }
        if debug_config.enabled {
            // Session-id subdirectory prefix (I2, matches spawn_acp_agent) so concurrent
            // `/sessions` agents never share the same timestamped dump directory.
            let session_dump_dir = debug_config.output_dir.join(session_id.as_str());
            match zeph_core::debug_dump::DebugDumper::new(
                session_dump_dir.as_path(),
                debug_config.format,
            ) {
                Ok(dumper) => agent = agent.with_debug_dumper(dumper),
                Err(e) => tracing::warn!(error = %e, "debug dump initialization failed"),
            }
        }
        agent
    }
}

/// Opens (and replays, per D-10) the durable event log for `session_id`, wraps it in a
/// `SessionSink`, and returns any history it recovered.
///
/// Returns `(None, Vec::new())` (logging a warning) if the log cannot be opened/read — matching
/// `spawn_acp_agent`'s fallback: the session still works, just without durable persistence.
async fn hydrate_session_sink(
    session_persistence_config: &zeph_config::SessionConfig,
    memory: &Arc<zeph_memory::semantic::SemanticMemory>,
    session_id: zeph_common::SessionId,
    conversation_id: zeph_memory::ConversationId,
    resume_condenser: &zeph_session::LlmCondenser,
    resume_token_counter: &zeph_agent_context::memory_backend::TokenCounterAdapter,
    context_window: usize,
) -> (
    Option<Arc<zeph_agent_persistence::SessionSink>>,
    Vec<zeph_llm::provider::Message>,
) {
    let data_dir = std::path::PathBuf::from(&session_persistence_config.data_dir);
    let session_path = zeph_session::session_dir(&data_dir, session_id.as_str());
    let store = zeph_session::SessionStore::new(memory.sqlite().pool().clone());

    if let Err(e) = store.create(session_id.as_str()).await {
        tracing::warn!(error = %e, "serve-sessions: failed to seed session metadata row");
    }
    // D-12: without this, `acp_sessions.conversation_id` stays NULL for every `/sessions`-created
    // session, and reactivation (`reactivate_session`, `src/serve/handlers.rs`) — which resolves
    // the conversation to replay via this exact column — can never find one. Idempotent: setting
    // the same value again on a reactivation of an already-linked session is a no-op update.
    if let Err(e) = store
        .link_conversation(session_id.as_str(), conversation_id.0)
        .await
    {
        tracing::warn!(error = %e, "serve-sessions: failed to link session to conversation");
    }
    // D-13 (spec-068 §8.1, N3): `hydrate_and_condense` folds in resume-time durable condensation
    // via the deps-level pre-built `resume_condenser`/`resume_token_counter` (see
    // `ServeAgentDeps::resume_condenser`'s doc comment for why they're built once at deps-
    // construction time, not here).
    //
    // #5487 fix 3: `hydrate_and_condense` now opens the log exclusively (INV-D2). Retry a
    // bounded number of times on `AlreadyLocked` — see `REACTIVATION_LOCK_RETRY_ATTEMPTS`'s doc
    // comment for the eviction/reactivation race this closes. A non-transient `AlreadyLocked`
    // (a genuinely still-live second writer) degrades to no persistence after the retry budget
    // is exhausted, same as any other hydration failure — this function's contract is
    // best-effort, not fail-fast, since the daemon has no synchronous caller to fail back to.
    for attempt in 1..=REACTIVATION_LOCK_RETRY_ATTEMPTS {
        match zeph_agent_persistence::hydrate_and_condense(
            &session_path,
            &store,
            session_id.as_str(),
            conversation_id,
            memory,
            None,
            resume_condenser,
            resume_token_counter,
            context_window,
        )
        .await
        {
            Ok(hydrated) => {
                let sink = Arc::new(zeph_agent_persistence::SessionSink::new(
                    hydrated.log,
                    store,
                    session_id,
                ));
                return (Some(sink), hydrated.messages);
            }
            Err(zeph_agent_persistence::PersistenceError::Session(
                zeph_session::SessionError::AlreadyLocked(lock_path),
            )) if attempt < REACTIVATION_LOCK_RETRY_ATTEMPTS => {
                tracing::debug!(
                    lock_path,
                    attempt,
                    "serve-sessions: event log still locked by a draining prior actor; retrying reactivation"
                );
                tokio::time::sleep(REACTIVATION_LOCK_RETRY_DELAY).await;
            }
            Err(e) => {
                // #5518: distinguish "retry budget exhausted against a still-live lock" from any
                // other hydration failure — only the former is the silent degrade this counter
                // exists to surface (an operator has no other way to notice it happening at
                // scale short of grepping the warn log below).
                if matches!(
                    e,
                    zeph_agent_persistence::PersistenceError::Session(
                        zeph_session::SessionError::AlreadyLocked(_)
                    )
                ) {
                    metrics::counter!("serve.session.reactivation_lock_exhausted_total")
                        .increment(1);
                }
                tracing::warn!(error = %e, "serve-sessions: session persistence disabled for this session");
                return (None, Vec::new());
            }
        }
    }
    unreachable!(
        "the loop always returns: the last attempt's AlreadyLocked falls into the generic Err(e) arm"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_agent_context::memory_backend::TokenCounterAdapter;
    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;

    async fn make_memory() -> Arc<SemanticMemory> {
        Arc::new(
            SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
                "test-model",
            )
            .await
            .unwrap(),
        )
    }

    /// D-13 fixture: a condenser whose threshold is never crossed in these hydration/linking
    /// tests — they pass `context_window: 0` too (belt-and-suspenders, since
    /// `resume_budget_fraction` returns `0.0` for a zero window), so `should_condense` never
    /// fires and cannot perturb the assertions these tests actually care about.
    fn make_test_condenser() -> (zeph_session::LlmCondenser, TokenCounterAdapter) {
        let deps = zeph_context::summarization::SummarizationDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            llm_timeout: std::time::Duration::from_secs(5),
            token_counter: std::sync::Arc::new(TokenCounterAdapter::new(std::sync::Arc::new(
                zeph_memory::TokenCounter::new(),
            ))),
            structured_summaries: true,
            on_anchored_summary: None,
        };
        let condenser = zeph_session::LlmCondenser::new(deps, 1.0, 1);
        let token_counter_adapter =
            TokenCounterAdapter::new(std::sync::Arc::new(zeph_memory::TokenCounter::new()));
        (condenser, token_counter_adapter)
    }

    /// D-12 regression: `hydrate_session_sink` must link `session_id` to `conversation_id` in
    /// `SessionStore` — without this, `reactivate_session` (`src/serve/handlers.rs`) can never
    /// resolve a conversation to replay for *any* serve-created session (the fourth bug found
    /// during the D-10/D-11/D-12 correction pass; previously proven only via a live Ollama
    /// session, not by any automated test — closing that gap here).
    #[tokio::test]
    async fn hydrate_session_sink_links_conversation_id() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = zeph_config::SessionConfig {
            enabled: true,
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let session_id = zeph_common::SessionId::new("s1");
        let (condenser, token_counter) = make_test_condenser();

        let (sink, messages) = hydrate_session_sink(
            &config,
            &memory,
            session_id.clone(),
            cid,
            &condenser,
            &token_counter,
            0,
        )
        .await;
        assert!(
            sink.is_some(),
            "session persistence enabled must produce a SessionSink"
        );
        assert!(
            messages.is_empty(),
            "a brand-new session has no history to replay"
        );

        let store = zeph_session::SessionStore::new(memory.sqlite().pool().clone());
        let meta = store.get(session_id.as_str()).await.unwrap().unwrap();
        assert_eq!(
            meta.conversation_id,
            Some(cid.0),
            "hydrate_session_sink must link the session to its conversation_id, or reactivation \
             can never find one to replay"
        );
    }

    /// Reactivation reuses this same function (`build_agent_factory` always calls it), and must
    /// pick up a *linked* session's prior history — this is what makes the D-12 reactivation
    /// path actually replay context instead of silently starting fresh.
    #[tokio::test]
    async fn hydrate_session_sink_replays_prior_history_on_reactivation() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = zeph_config::SessionConfig {
            enabled: true,
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let session_id = zeph_common::SessionId::new("s1");
        let (condenser, token_counter) = make_test_condenser();

        // First call simulates the original `POST /sessions` creation.
        let (sink, _) = hydrate_session_sink(
            &config,
            &memory,
            session_id.clone(),
            cid,
            &condenser,
            &token_counter,
            0,
        )
        .await;
        let sink = sink.expect("session persistence enabled must produce a SessionSink");
        sink.record_message(zeph_llm::provider::Role::User, "hello", &[])
            .await
            .unwrap();
        drop(sink);

        // Second call simulates D-12 reactivation after the actor ended.
        let (_, messages) = hydrate_session_sink(
            &config,
            &memory,
            session_id,
            cid,
            &condenser,
            &token_counter,
            0,
        )
        .await;
        assert_eq!(
            messages.len(),
            1,
            "reactivation must replay the session's prior turn, not start fresh"
        );
    }

    /// #5487 fix 3: `evict_loop` frees a session's registry slot before its actor's dedicated
    /// thread has necessarily finished dropping the old `SessionEventLog` (and releasing its
    /// flock) — a fast reactivation can see a transient `AlreadyLocked` that isn't a genuine
    /// second writer. Simulates that race: a background task holds the exclusive lock for
    /// longer than a couple of retry intervals, then releases it. `hydrate_session_sink` must
    /// retry past the transient failures and still succeed.
    #[tokio::test]
    async fn hydrate_session_sink_retries_transient_already_locked_and_succeeds() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = zeph_config::SessionConfig {
            enabled: true,
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let session_id = zeph_common::SessionId::new("s1");
        let session_path = zeph_session::session_dir(dir.path(), session_id.as_str());
        let (condenser, token_counter) = make_test_condenser();

        // Held past attempts 1-3 (t=0, 20, 40ms), released before attempt 4 (t=60ms).
        let blocker = zeph_session::SessionEventLog::open_exclusive(&session_path)
            .await
            .unwrap();
        let release_handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(blocker);
        });

        let (sink, _) = hydrate_session_sink(
            &config,
            &memory,
            session_id,
            cid,
            &condenser,
            &token_counter,
            0,
        )
        .await;

        release_handle.await.unwrap();
        assert!(
            sink.is_some(),
            "hydrate_session_sink must retry past a transient AlreadyLocked and eventually \
             succeed once the draining actor releases its flock"
        );
    }

    /// Counterpart to the retry-success test above: a genuinely still-live second writer (lock
    /// held for the whole call, not just a drain race) must exhaust the retry budget and
    /// gracefully degrade to no persistence, not panic or hang.
    #[tokio::test]
    async fn hydrate_session_sink_gives_up_after_retry_budget_exhausted() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = zeph_config::SessionConfig {
            enabled: true,
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let session_id = zeph_common::SessionId::new("s1");
        let session_path = zeph_session::session_dir(dir.path(), session_id.as_str());
        let (condenser, token_counter) = make_test_condenser();

        // Held for the entire call — never released, unlike the transient-race test above.
        let _blocker = zeph_session::SessionEventLog::open_exclusive(&session_path)
            .await
            .unwrap();

        let (sink, messages) = hydrate_session_sink(
            &config,
            &memory,
            session_id,
            cid,
            &condenser,
            &token_counter,
            0,
        )
        .await;

        assert!(
            sink.is_none(),
            "retry budget exhaustion against a genuinely still-locked session must degrade to \
             no persistence, not panic or hang"
        );
        assert!(messages.is_empty());
    }

    /// #5566 regression: `build_agent_factory` must wire a `DebugDumper` when `[debug] enabled =
    /// true`, the same way `spawn_acp_agent` (`src/acp.rs`) and the CLI path (`src/runner.rs`)
    /// already do. `Agent` exposes no public accessor for its internal `debug_dumper` state, so
    /// this asserts the documented, directly observable side effect instead:
    /// `DebugDumper::new` creates its timestamped subdirectory synchronously on construction.
    #[tokio::test]
    async fn build_agent_factory_wires_debug_dumper_when_enabled() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let dump_dir = tempfile::tempdir().unwrap();
        let session_id = zeph_common::SessionId::new("debug-test-session");

        let mut config = zeph_core::config::Config::default();
        config.debug.enabled = true;
        config.debug.output_dir = dump_dir.path().to_path_buf();
        config.debug.format = zeph_config::DumpFormat::Raw;

        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();

        let deps = ServeAgentDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            embedding_provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            registry: Arc::new(parking_lot::RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: None,
            max_active_skills: 5,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            memory: Arc::clone(&memory),
            history_limit: 10,
            recall_limit: 5,
            summarization_threshold: 1000,
            session_config,
            session_persistence_config: zeph_config::SessionConfig::default(),
            resume_condenser: Arc::new(condenser),
            resume_token_counter: Arc::new(token_counter),
            provider_pool: Vec::new(),
            provider_config_snapshot: zeph_core::ProviderConfigSnapshot::default(),
        };

        let build_agent = build_agent_factory(deps, session_id.clone(), cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let _agent = build_agent(channel);

        let session_dump_dir = dump_dir.path().join(session_id.as_str());
        assert!(
            session_dump_dir.is_dir(),
            "debug dump session subdirectory must be created when [debug] enabled = true"
        );
        let has_timestamped_child = std::fs::read_dir(&session_dump_dir)
            .unwrap()
            .next()
            .is_some();
        assert!(
            has_timestamped_child,
            "DebugDumper::new must create a timestamped subdirectory under the session dir"
        );
    }

    /// #5450 regression: `build_agent_factory` must call `Agent::with_provider_pool` so
    /// `resolve_background_provider` (used by e.g. `memory.graph.extract_provider`) can resolve
    /// named providers for `/sessions`-created agents — previously `ServeAgentDeps` carried the
    /// pool but `build_agent_factory`'s builder chain never consumed it, so the pool was always
    /// empty at runtime. `Agent` exposes no `pub` accessor for `provider_pool` directly, so this
    /// drives the same observable surface the real `/provider` command uses
    /// ([`zeph_commands::AgentAccess::handle_provider`] with an empty argument lists configured
    /// providers) rather than reaching into private fields from outside `zeph-core`.
    #[tokio::test]
    async fn build_agent_factory_wires_provider_pool_for_background_resolution() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let session_id = zeph_common::SessionId::new("provider-pool-test-session");

        let config = zeph_core::config::Config::default();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();

        let deps = ServeAgentDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            embedding_provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            registry: Arc::new(parking_lot::RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: None,
            max_active_skills: 5,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            memory: Arc::clone(&memory),
            history_limit: 10,
            recall_limit: 5,
            summarization_threshold: 1000,
            session_config,
            session_persistence_config: zeph_config::SessionConfig::default(),
            resume_condenser: Arc::new(condenser),
            resume_token_counter: Arc::new(token_counter),
            provider_pool: vec![zeph_core::config::ProviderEntry {
                name: Some("named-test".into()),
                model: Some("llama3.2".into()),
                ..zeph_core::config::ProviderEntry::default()
            }],
            provider_config_snapshot: zeph_core::ProviderConfigSnapshot::default(),
        };

        let build_agent = build_agent_factory(deps, session_id.clone(), cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = build_agent(channel);

        let output = agent.handle_provider("").await;
        assert!(
            output.contains("named-test"),
            "the pool entry configured on ServeAgentDeps must be visible through the built \
             Agent's provider_pool; got: {output}"
        );
    }
}
