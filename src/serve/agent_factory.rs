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
use zeph_tools::ErasedToolExecutor;

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
#[allow(clippy::too_many_lines)]
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

    // SkillOrchestra: load persisted RL routing head weights, if enabled (#5921) — mirrors
    // `src/runner.rs`/`src/daemon.rs`'s cold-start/load pattern. Must happen here, in the async
    // prefix, since the returned closure is a plain synchronous `FnOnce` (see module docs) and
    // cannot `.await`. `deps.rl_embed_dim_resolved` is pre-computed once in
    // `crate::acp::build_shared_core`, shared across every session built from that core.
    // NOTE (S3, known limitation, not fixed here): `routing_head_weights` is a single global
    // row (`WHERE id = 1`, last-write-wins upsert — crates/zeph-memory/src/store/skills.rs).
    // Concurrent `/sessions` agents each load their own in-memory head from that row and
    // persist back independently, so concurrent multi-session RL routing can clobber each
    // other's learned weights. Bounded risk: `rl_routing_enabled` defaults to `false`. See
    // #5974 for per-conversation persistence keying or write serialization.
    let rl_head = if let Some(dim) = deps.rl_embed_dim_resolved {
        Some(
            crate::runner::load_rl_head(&deps.memory)
                .await
                .unwrap_or_else(|| {
                    tracing::info!(dim, "rl_head: cold start, initializing fresh routing head");
                    zeph_skills::rl_head::RoutingHead::new(dim)
                }),
        )
    } else {
        None
    };

    // SEC-H1 / R1: each session gets its own gate stack, wrapping the shared base composite —
    // see `gate_serve_session_executor`'s doc comment for the full rationale and the INVARIANT
    // that binds future serve tool-executor changes.
    let gated_executor = gate_serve_session_executor(
        deps.tool_executor,
        &deps.permission_policy,
        &deps.policy_gate_pieces,
        deps.audit_logger.as_ref(),
    );

    move |channel| {
        // Capture before apply_session_config consumes deps.session_config (mirrors
        // spawn_acp_agent's debug_config capture in src/acp.rs).
        let debug_config = deps.session_config.debug_config.clone();

        // Spec 050 F2 (#5913): `capability_scopes` wrapping (when configured) already happened
        // once in `serve::deps::assemble_serve_deps` — `deps.tool_executor` is shared/static
        // across every `/sessions*` agent (unlike ACP's per-session composite), so the
        // dead-glob outcome (FR-CG-005/NFR-CG-004) is knowable at startup and made fatal there
        // (impl-critic F1), rather than degraded per-session here.

        // Spec 050 Phase 2 (#5913): wrap with ShadowProbeExecutor when
        // shadow_sentinel.enabled = true — mirrors src/runner.rs/src/acp.rs, where
        // ScopedToolExecutor/ShadowProbeExecutor wrap OUTSIDE/ABOVE the
        // Policy/Adversarial/Trust gate stack (not the raw base composite), so shadow-probing
        // observes the same gated tree the agent actually dispatches through. Keyed by this
        // session's own `conversation_id`, since `ServeAgentDeps.tool_executor` (unlike
        // ACP's per-connection base chain) is shared across every `/sessions*` agent.
        let (final_tool_executor, shadow_sentinel_arc): (Arc<dyn ErasedToolExecutor>, _) =
            if deps.shadow_sentinel_config.enabled {
                let sentinel_cfg = &deps.shadow_sentinel_config;
                let pool = deps.memory.sqlite().pool().clone();
                let llm_probe = zeph_core::agent::shadow_sentinel::LlmSafetyProbe::new(
                    Arc::new(deps.shadow_sentinel_probe_provider.clone()),
                    sentinel_cfg.probe_timeout_ms,
                    sentinel_cfg.deny_on_timeout,
                );
                let store = zeph_core::agent::shadow_sentinel::ShadowEventStore::new(pool);
                let sentinel = Arc::new(zeph_core::agent::shadow_sentinel::ShadowSentinel::new(
                    store,
                    Box::new(llm_probe),
                    sentinel_cfg.clone(),
                    conversation_id.0.to_string(),
                ));
                let turn_number = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let risk_level = Arc::new(parking_lot::RwLock::new("calm".to_owned()));
                let probe_gate: Arc<dyn zeph_tools::ProbeGate> =
                    Arc::new(crate::runner::ShadowSentinelProbeGateAdapter {
                        sentinel: Arc::clone(&sentinel),
                    });
                let shadow_exec = zeph_tools::ShadowProbeExecutor::new(
                    zeph_tools::DynExecutor(gated_executor.0.clone()),
                    probe_gate,
                    turn_number,
                    risk_level,
                );
                (Arc::new(shadow_exec), Some(sentinel))
            } else {
                (gated_executor.0.clone(), None)
            };

        let mut agent = Agent::new_with_registry_arc(
            deps.provider,
            deps.embedding_provider,
            channel,
            deps.registry,
            deps.matcher,
            deps.max_active_skills,
            zeph_tools::DynExecutor(final_tool_executor),
        )
        .apply_session_config(deps.session_config)
        .with_skill_matching_config(
            deps.skill_disambiguation_threshold,
            deps.skill_two_stage_matching,
            deps.skill_confusability_threshold,
        )
        .with_skill_group_config(
            deps.skill_group_structured,
            deps.skill_support_similarity_threshold,
            deps.skill_min_injection_score,
        )
        .with_skill_provider_names(
            deps.skill_generation_provider,
            deps.skill_disambiguate_provider,
        )
        .with_semantic_scan(deps.semantic_scan, deps.semantic_scan_provider)
        .with_trust_config(deps.trust_config)
        .with_rl_routing(
            deps.rl_routing_enabled,
            deps.rl_learning_rate,
            deps.rl_weight,
            deps.rl_persist_interval,
            deps.rl_warmup_updates,
        )
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
        // Spec 050 Phase 2 (#5913): wire ShadowSentinel into the agent so begin_turn() calls
        // advance_turn(), matching src/runner.rs/src/acp.rs/src/daemon.rs.
        if let Some(sentinel) = shadow_sentinel_arc {
            agent = agent.with_shadow_sentinel(sentinel);
        }
        if let Some(head) = rl_head {
            agent = agent.with_rl_head(head);
        }
        if debug_config.enabled {
            // Session-id subdirectory prefix (I2, matches spawn_acp_agent) so concurrent
            // `/sessions` agents never share the same timestamped dump directory.
            let session_dump_dir = debug_config.output_dir.join(session_id.as_str());
            agent = crate::agent_setup::apply_debug_dumper(
                agent,
                session_dump_dir.as_path(),
                debug_config.format,
            )
            .0;
        }
        agent
    }
}

/// Wraps `tool_executor` in the per-session trust/policy/adversarial gate stack (SEC-H1 / R1):
/// each session gets its own `TrustGateExecutor`/`AdversarialPolicyGateExecutor`/
/// `PolicyGateExecutor` instance (own mutable per-turn trust-state atomic/context), even though
/// the underlying base composite and the immutable `policy_gate_pieces` are shared across every
/// `/sessions` agent built from the same [`ServeAgentDeps`]. Gating this eagerly, once, at
/// `assemble_serve_deps` startup would let one concurrent session's `set_effective_trust`
/// clobber another's on a shared atomic (serve runs each session on its own thread) — see
/// [`crate::agent_setup::apply_common_tool_gating`]'s doc comment and
/// [`ServeAgentDeps::tool_executor`]'s doc comment for the full rationale.
///
/// INVARIANT: `tool_executor` must already contain serve's ENTIRE tool surface (today: the base
/// file/shell/scrape/cwd chain only — see `deps.rs::build_tool_executor`'s doc comment) — any
/// tool executor added to `ServeAgentDeps` outside that base, or composed onto the result of
/// this wrap, bypasses trust/policy/adversarial enforcement entirely. See #5977/#5611/#5748.
fn gate_serve_session_executor(
    tool_executor: Arc<dyn zeph_tools::ErasedToolExecutor>,
    permission_policy: &zeph_tools::PermissionPolicy,
    policy_gate_pieces: &crate::agent_setup::PolicyGatePieces,
    audit_logger: Option<&Arc<zeph_tools::AuditLogger>>,
) -> zeph_tools::DynExecutor {
    let (trust_gated, mcp_ids_handle) = crate::agent_setup::apply_common_tool_gating(
        zeph_tools::DynExecutor(tool_executor),
        permission_policy,
    );
    // R7: serve has no MCP-provided tools yet (deps.rs's "Known gap") — this empty-slice call
    // is the exact seam a future MCP-wiring PR must populate with the connected tool list.
    crate::agent_setup::register_mcp_tool_ids(&mcp_ids_handle, &[]);
    crate::agent_setup::apply_policy_gate_chain(
        trust_gated,
        policy_gate_pieces,
        audit_logger,
        None, // R8: serve has no TrajectorySentinel wiring yet; #5958 seam, mirrors daemon/acp.
    )
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
            skill_disambiguation_threshold: 0.2,
            skill_two_stage_matching: false,
            skill_confusability_threshold: 0.0,
            skill_group_structured: false,
            skill_support_similarity_threshold: 0.50,
            skill_min_injection_score: 0.20,
            skill_generation_provider: String::new(),
            skill_disambiguate_provider: String::new(),
            semantic_scan: false,
            semantic_scan_provider: String::new(),
            trust_config: zeph_core::config::TrustConfig::default(),
            rl_routing_enabled: false,
            rl_learning_rate: 0.0,
            rl_weight: 0.0,
            rl_persist_interval: 0,
            rl_warmup_updates: 0,
            rl_embed_dim_resolved: None,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            audit_logger: None,
            policy_gate_pieces: crate::agent_setup::PolicyGatePieces::default(),
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
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
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
            skill_disambiguation_threshold: 0.2,
            skill_two_stage_matching: false,
            skill_confusability_threshold: 0.0,
            skill_group_structured: false,
            skill_support_similarity_threshold: 0.50,
            skill_min_injection_score: 0.20,
            skill_generation_provider: String::new(),
            skill_disambiguate_provider: String::new(),
            semantic_scan: false,
            semantic_scan_provider: String::new(),
            trust_config: zeph_core::config::TrustConfig::default(),
            rl_routing_enabled: false,
            rl_learning_rate: 0.0,
            rl_weight: 0.0,
            rl_persist_interval: 0,
            rl_warmup_updates: 0,
            rl_embed_dim_resolved: None,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            audit_logger: None,
            policy_gate_pieces: crate::agent_setup::PolicyGatePieces::default(),
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
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
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

    fn embed_fn_constant(text: &str) -> zeph_skills::matcher::EmbedFuture {
        let _ = text;
        Box::pin(async { Ok(vec![1.0_f32, 0.0]) })
    }

    /// #5818 regression: `build_agent_factory` must call `Agent::with_skill_matching_config`/
    /// `with_skill_provider_names` so `config.skills.*` reaches the built `Agent` — previously
    /// `ServeAgentDeps` had no such fields and the builder chain never called either method, so
    /// every `/sessions`-created agent silently ran skill matching on hardcoded builder defaults
    /// regardless of config.
    ///
    /// Critic finding (round 1, S1): the 4 pre-existing `ServeAgentDeps` test literals this fix
    /// touched all use `0.2, false, 0.0, "", ""` for these fields — byte-identical to the
    /// builder's pre-fix defaults (`crates/zeph-core/src/agent/state/mod.rs`) — so those tests
    /// cannot distinguish wired-vs-unwired. This test uses distinct non-default values for all 5
    /// fields and, since `Agent` exposes no `pub` accessor for them directly, drives the same
    /// observable surface the real `/skills confusability` command uses
    /// ([`zeph_commands::AgentAccess::handle_skills`]) — but asserts the *exact* threshold value
    /// echoed in [`zeph_skills::matcher::ConfusabilityReport`]'s `Display` output (`"above
    /// {threshold:.2}"`), not just "non-default", so a swap between `disambiguation_threshold`
    /// and `confusability_threshold` in the `with_skill_matching_config` call — both `f32`,
    /// unlike `two_stage_matching`'s `bool` — would also be caught. A real `SkillMatcherBackend`
    /// with one skill is needed for `handle_skills("confusability")` to reach the
    /// threshold-printing branch instead of its "matcher not available" short-circuit.
    #[tokio::test]
    async fn build_agent_factory_wires_skill_matching_config() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let session_id = zeph_common::SessionId::new("skill-matching-config-test-session");

        let config = zeph_core::config::Config::default();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();

        let skill_meta = zeph_skills::loader::SkillMeta {
            name: "solo-skill".to_owned(),
            description: "a lone skill with no confusable sibling".to_owned(),
            ..Default::default()
        };
        let inner_matcher =
            zeph_skills::matcher::SkillMatcher::new(&[&skill_meta], embed_fn_constant)
                .await
                .expect("single-skill matcher construction must succeed with a constant embed_fn");

        let deps = ServeAgentDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            embedding_provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            registry: Arc::new(parking_lot::RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: Some(zeph_skills::matcher::SkillMatcherBackend::InMemory(
                inner_matcher,
            )),
            max_active_skills: 5,
            // Non-default values for all 5 fields (critic S1): the builder's pre-fix defaults
            // are 0.2 / false / 0.0 / "" / "", so none of these match.
            skill_disambiguation_threshold: 0.77,
            skill_two_stage_matching: true,
            skill_confusability_threshold: 0.42,
            skill_group_structured: false,
            skill_support_similarity_threshold: 0.50,
            skill_min_injection_score: 0.20,
            skill_generation_provider: "gen".to_owned(),
            skill_disambiguate_provider: "dis".to_owned(),
            semantic_scan: false,
            semantic_scan_provider: String::new(),
            trust_config: zeph_core::config::TrustConfig::default(),
            rl_routing_enabled: false,
            rl_learning_rate: 0.0,
            rl_weight: 0.0,
            rl_persist_interval: 0,
            rl_warmup_updates: 0,
            rl_embed_dim_resolved: None,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            audit_logger: None,
            policy_gate_pieces: crate::agent_setup::PolicyGatePieces::default(),
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
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
        };

        let build_agent = build_agent_factory(deps, session_id.clone(), cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = build_agent(channel);

        let output = agent
            .handle_skills("confusability")
            .await
            .expect("handle_skills(\"confusability\") must not error");
        assert!(
            output.contains("above 0.42"),
            "ServeAgentDeps::skill_confusability_threshold = 0.42 must reach the built Agent's \
             ConfusabilityReport exactly (not e.g. 0.77, disambiguation_threshold's value, from a \
             swapped with_skill_matching_config argument); got: {output}"
        );
    }

    /// #5867 regression: `build_agent_factory` must call `Agent::with_skill_group_config` so
    /// `ServeAgentDeps::skill_group_structured`/`skill_support_similarity_threshold`/
    /// `skill_min_injection_score` reach the built `Agent` — previously these three fields were
    /// only ever applied on the hot-reload path (`Agent::reload_config`), never at the
    /// `/sessions` cold-start path built here. Mirrors
    /// `build_agent_factory_wires_skill_matching_config` above, using non-default values distinct
    /// from the builder's pre-fix defaults (`false`/`0.50`/`0.20`, matching `SkillState::new()`),
    /// and asserts the exact values echoed by `/skills injection`'s `Display` output, not just
    /// "non-default", so a swapped argument in `with_skill_group_config` would also be caught.
    #[tokio::test]
    async fn build_agent_factory_wires_skill_group_config() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let session_id = zeph_common::SessionId::new("skill-group-config-test-session");

        let config = zeph_core::config::Config::default();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();

        let skill_meta = zeph_skills::loader::SkillMeta {
            name: "solo-skill".to_owned(),
            description: "a lone skill with no confusable sibling".to_owned(),
            ..Default::default()
        };
        let inner_matcher =
            zeph_skills::matcher::SkillMatcher::new(&[&skill_meta], embed_fn_constant)
                .await
                .expect("single-skill matcher construction must succeed with a constant embed_fn");

        let deps = ServeAgentDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            embedding_provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            registry: Arc::new(parking_lot::RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: Some(zeph_skills::matcher::SkillMatcherBackend::InMemory(
                inner_matcher,
            )),
            max_active_skills: 5,
            skill_disambiguation_threshold: 0.2,
            skill_two_stage_matching: false,
            skill_confusability_threshold: 0.0,
            // Non-default values for all 3 fields under test: the builder's pre-fix defaults
            // are false / 0.50 / 0.20, matching SkillState::new()'s hardcoded fallback.
            skill_group_structured: true,
            skill_support_similarity_threshold: 0.73,
            skill_min_injection_score: 0.35,
            skill_generation_provider: String::new(),
            skill_disambiguate_provider: String::new(),
            semantic_scan: false,
            semantic_scan_provider: String::new(),
            trust_config: zeph_core::config::TrustConfig::default(),
            rl_routing_enabled: false,
            rl_learning_rate: 0.0,
            rl_weight: 0.0,
            rl_persist_interval: 0,
            rl_warmup_updates: 0,
            rl_embed_dim_resolved: None,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            audit_logger: None,
            policy_gate_pieces: crate::agent_setup::PolicyGatePieces::default(),
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
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
        };

        let build_agent = build_agent_factory(deps, session_id.clone(), cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = build_agent(channel);

        let output = agent
            .handle_skills("injection")
            .await
            .expect("handle_skills(\"injection\") must not error");
        assert_eq!(
            output,
            "Skill injection config: group_structured=true, support_similarity_threshold=0.73, \
             min_injection_score=0.35",
            "ServeAgentDeps::skill_group_structured/skill_support_similarity_threshold/\
             skill_min_injection_score must reach the built Agent exactly via \
             with_skill_group_config; got: {output}"
        );
    }

    /// #5920/#5921 regression: `build_agent_factory` must call `Agent::with_trust_config` and
    /// `Agent::with_rl_routing`/`with_rl_head` so `ServeAgentDeps::trust_config`/
    /// `rl_routing_enabled`/`rl_embed_dim_resolved` reach the built `Agent` — previously these
    /// fields did not exist on `ServeAgentDeps` at all, so every `/sessions`-created agent
    /// silently ran skill trust classification and `SkillOrchestra` RL routing on hardcoded
    /// builder defaults regardless of config. Mirrors
    /// `build_agent_factory_wires_skill_group_config` above for the same wire-X-into-
    /// ACP/serve/daemon defect class, applied to trust config and RL routing this time. Asserts
    /// the exact `/skills trust` `Display` output, not just "non-default", so a swapped
    /// argument or a dropped `.with_trust_config(...)`/`.with_rl_routing(...)` call would also
    /// be caught. `rl_embed_dim_resolved: Some(8)` also exercises the RL-head cold-start path
    /// (`build_agent_factory`'s async prefix, since the returned closure cannot `.await`).
    #[tokio::test]
    async fn build_agent_factory_wires_trust_and_rl_config() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let session_id = zeph_common::SessionId::new("trust-rl-config-test-session");

        let config = zeph_core::config::Config::default();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();

        let trust_config = zeph_core::config::TrustConfig {
            default_level: zeph_common::SkillTrustLevel::Quarantined,
            local_level: zeph_common::SkillTrustLevel::Trusted,
            bundled_level: zeph_common::SkillTrustLevel::Verified,
            hash_mismatch_level: zeph_common::SkillTrustLevel::Blocked,
            ..Default::default()
        };

        let deps = ServeAgentDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            embedding_provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            registry: Arc::new(parking_lot::RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: None,
            max_active_skills: 5,
            skill_disambiguation_threshold: 0.2,
            skill_two_stage_matching: false,
            skill_confusability_threshold: 0.0,
            skill_group_structured: false,
            skill_support_similarity_threshold: 0.50,
            skill_min_injection_score: 0.20,
            skill_generation_provider: String::new(),
            skill_disambiguate_provider: String::new(),
            semantic_scan: false,
            semantic_scan_provider: String::new(),
            trust_config,
            rl_routing_enabled: true,
            rl_learning_rate: 0.05,
            rl_weight: 0.3,
            rl_persist_interval: 5,
            rl_warmup_updates: 3,
            rl_embed_dim_resolved: Some(8),
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            audit_logger: None,
            policy_gate_pieces: crate::agent_setup::PolicyGatePieces::default(),
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
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
        };

        let build_agent = build_agent_factory(deps, session_id.clone(), cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = build_agent(channel);

        let output = agent
            .handle_skills("trust")
            .await
            .expect("handle_skills(\"trust\") must not error");
        assert_eq!(
            output,
            "Skill trust config: default_level=Quarantined, local_level=Trusted, \
             bundled_level=Verified, hash_mismatch_level=Blocked | RL routing: enabled=true, \
             rl_head_loaded=true",
            "ServeAgentDeps::trust_config/rl_routing_enabled/rl_embed_dim_resolved must reach \
             the built Agent exactly via with_trust_config/with_rl_routing/with_rl_head; got: \
             {output}"
        );
    }

    /// #5827 regression: `build_agent_factory` must call `Agent::with_semantic_scan` so
    /// `config.skills.semantic_scan`/`semantic_scan_provider` reach the built `Agent` —
    /// previously `ServeAgentDeps` had no such fields and the builder chain never called
    /// `with_semantic_scan`, so every `/sessions`-created agent silently ran Stage-2 skill
    /// semantic-scanning on hardcoded builder defaults (`semantic_scan: false`) regardless of
    /// config.
    ///
    /// `Agent` exposes no `pub` accessor for `semantic_scan`/`semantic_scan_provider` directly,
    /// so this drives the same observable surface the real `/plugins add` command uses
    /// ([`zeph_commands::AgentAccess::handle_plugins`]): with an empty `provider_pool`,
    /// `handle_plugins("add <path>")` fails closed on the *unknown provider* branch
    /// (`crates/zeph-core/src/agent/agent_access_impl.rs`) whenever `semantic_scan` is enabled,
    /// and the error message echoes the exact configured provider name — so a dropped or
    /// swapped `with_semantic_scan` argument would also be caught.
    #[tokio::test]
    async fn build_agent_factory_wires_semantic_scan_config() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let session_id = zeph_common::SessionId::new("semantic-scan-config-test-session");

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
            skill_disambiguation_threshold: 0.2,
            skill_two_stage_matching: false,
            skill_confusability_threshold: 0.0,
            skill_group_structured: false,
            skill_support_similarity_threshold: 0.50,
            skill_min_injection_score: 0.20,
            skill_generation_provider: String::new(),
            skill_disambiguate_provider: String::new(),
            // Non-default values (pre-fix builder default is `false`/`""`, see
            // `crates/zeph-core/src/agent/state/mod.rs`): proves both fields are wired, not just
            // left on their hardcoded defaults.
            semantic_scan: true,
            semantic_scan_provider: "scan-test-provider".to_owned(),
            trust_config: zeph_core::config::TrustConfig::default(),
            rl_routing_enabled: false,
            rl_learning_rate: 0.0,
            rl_weight: 0.0,
            rl_persist_interval: 0,
            rl_warmup_updates: 0,
            rl_embed_dim_resolved: None,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            audit_logger: None,
            policy_gate_pieces: crate::agent_setup::PolicyGatePieces::default(),
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
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
        };

        let build_agent = build_agent_factory(deps, session_id.clone(), cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = build_agent(channel);

        let err = agent
            .handle_plugins("add /nonexistent/plugin/path")
            .await
            .expect_err(
                "handle_plugins(\"add\") must fail closed when semantic_scan is enabled and \
                 semantic_scan_provider is not in the (empty) provider pool",
            );
        let msg = err.to_string();
        assert!(
            msg.contains("semantic_scan_provider") && msg.contains("scan-test-provider"),
            "ServeAgentDeps::semantic_scan/semantic_scan_provider = true/\"scan-test-provider\" \
             must reach the built Agent's fail-closed plugin-add check exactly; got: {msg}"
        );
    }

    /// Builds a minimal [`ServeAgentDeps`] shared by the R3/R4 gate-wiring regression tests
    /// below — `Full` autonomy so [`zeph_tools::TrustGateExecutor`]'s permission-policy
    /// fallback never interferes with the trust/policy assertions under test.
    fn make_gate_test_deps(
        memory: Arc<zeph_memory::semantic::SemanticMemory>,
        session_config: zeph_core::AgentSessionConfig,
        resume_condenser: zeph_session::LlmCondenser,
        resume_token_counter: zeph_agent_context::memory_backend::TokenCounterAdapter,
        policy_gate_pieces: crate::agent_setup::PolicyGatePieces,
    ) -> ServeAgentDeps {
        ServeAgentDeps {
            provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            embedding_provider: AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            registry: Arc::new(parking_lot::RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: None,
            max_active_skills: 5,
            skill_disambiguation_threshold: 0.2,
            skill_two_stage_matching: false,
            skill_confusability_threshold: 0.0,
            skill_group_structured: false,
            skill_support_similarity_threshold: 0.50,
            skill_min_injection_score: 0.20,
            skill_generation_provider: String::new(),
            skill_disambiguate_provider: String::new(),
            semantic_scan: false,
            semantic_scan_provider: String::new(),
            trust_config: zeph_core::config::TrustConfig::default(),
            rl_routing_enabled: false,
            rl_learning_rate: 0.0,
            rl_weight: 0.0,
            rl_persist_interval: 0,
            rl_warmup_updates: 0,
            rl_embed_dim_resolved: None,
            tool_executor: Arc::new(zeph_tools::SetCwdExecutor),
            permission_policy: zeph_tools::PermissionPolicy::default()
                .with_autonomy(zeph_tools::AutonomyLevel::Full),
            audit_logger: None,
            policy_gate_pieces,
            memory,
            history_limit: 10,
            recall_limit: 5,
            summarization_threshold: 1000,
            session_config,
            session_persistence_config: zeph_config::SessionConfig::default(),
            resume_condenser: Arc::new(resume_condenser),
            resume_token_counter: Arc::new(resume_token_counter),
            provider_pool: Vec::new(),
            provider_config_snapshot: zeph_core::ProviderConfigSnapshot::default(),
            shadow_sentinel_config: zeph_config::ShadowSentinelConfig::default(),
            shadow_sentinel_probe_provider: AnyProvider::Mock(
                zeph_llm::mock::MockProvider::default(),
            ),
        }
    }

    fn make_tool_call(tool_id: &str) -> zeph_tools::ToolCall {
        zeph_tools::ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    /// R3 (SEC-H1 guardrail, #5973/#5977): `build_agent_factory` must give each session its
    /// OWN `TrustGateExecutor` trust-state instance — not share one gated executor (and its
    /// single mutable `effective_trust` atomic) across every `/sessions` agent built from the
    /// same `ServeAgentDeps`. Builds ONE `ServeAgentDeps`, spawns TWO agents from it, sets
    /// session A's trust to `Quarantined` then session B's to `Trusted` (the order that would
    /// fail under eager/shared gating: B's write would clobber a shared atomic written after
    /// A's), and asserts A's `bash` (`QUARANTINE_DENIED`) call is STILL Blocked. This test FAILS
    /// if `ServeAgentDeps::tool_executor`/`build_agent_factory` reverts to gating once, eagerly,
    /// in `assemble_serve_deps` instead of per session.
    #[tokio::test]
    async fn build_agent_factory_gates_trust_state_independently_per_session() {
        let memory = make_memory().await;
        let cid_a = memory.sqlite().create_conversation().await.unwrap();
        let cid_b = memory.sqlite().create_conversation().await.unwrap();
        let session_id_a = zeph_common::SessionId::new("trust-race-session-a");
        let session_id_b = zeph_common::SessionId::new("trust-race-session-b");

        let config = zeph_core::config::Config::default();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();
        let deps = make_gate_test_deps(
            Arc::clone(&memory),
            session_config,
            condenser,
            token_counter,
            crate::agent_setup::PolicyGatePieces::default(),
        );

        let build_agent_a = build_agent_factory(deps.clone(), session_id_a, cid_a).await;
        let build_agent_b = build_agent_factory(deps, session_id_b, cid_b).await;
        let (channel_a, _handle_a) = zeph_core::LoopbackChannel::pair(8);
        let (channel_b, _handle_b) = zeph_core::LoopbackChannel::pair(8);
        let agent_a = build_agent_a(channel_a);
        let agent_b = build_agent_b(channel_b);

        let executor_a = agent_a.tool_executor_arc();
        let executor_b = agent_b.tool_executor_arc();

        // Order matters: B's Trusted write happens AFTER A's Quarantined write — under eager
        // (shared) gating this would clobber the single shared atomic and A's subsequent call
        // would incorrectly read Trusted.
        executor_a.set_effective_trust(zeph_common::SkillTrustLevel::Quarantined);
        executor_b.set_effective_trust(zeph_common::SkillTrustLevel::Trusted);

        let result = executor_a
            .execute_tool_call_erased(&make_tool_call("bash"))
            .await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "session A's Quarantined trust must still block a QUARANTINE_DENIED tool call \
             (\"bash\") even after session B's concurrent set_effective_trust(Trusted) — a \
             shared (eager) gate instance would let B's write clobber A's, allowing this call \
             through; got {result:?}"
        );
    }

    /// R4 (#5973 regression): a `[tools.policy]` deny rule must block a tool call dispatched
    /// through a `/sessions`-built (serve) agent's executor — proving `build_agent_factory`'s
    /// per-session `apply_policy_gate_chain` wrap is actually reachable, not just constructed.
    /// Before this PR, `serve/deps.rs::build_tool_executor` never wrapped its executor in
    /// `PolicyGateExecutor` at all, so this call would have gone through unblocked.
    #[tokio::test]
    async fn build_agent_factory_wires_policy_gate_deny_rule() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let session_id = zeph_common::SessionId::new("policy-deny-test-session");

        let config = zeph_core::config::Config::default();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();

        let policy_config = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: vec![zeph_tools::PolicyRuleConfig {
                effect: zeph_tools::PolicyEffect::Deny,
                tool: "shell".into(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            ..Default::default()
        };
        let enforcer = Arc::new(zeph_tools::PolicyEnforcer::compile(&policy_config).unwrap());
        let policy_gate_pieces = crate::agent_setup::PolicyGatePieces {
            policy_enforcer: Some(enforcer),
            adversarial_validator: None,
            adversarial_llm_client: None,
            adv_policy_info: None,
        };
        let deps = make_gate_test_deps(
            memory,
            session_config,
            condenser,
            token_counter,
            policy_gate_pieces,
        );

        let build_agent = build_agent_factory(deps, session_id, cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let agent = build_agent(channel);

        let result = agent
            .tool_executor_arc()
            .execute_tool_call_erased(&make_tool_call("shell"))
            .await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "a [tools.policy] deny rule for \"shell\" must block the call through a \
             /sessions-built agent's executor, got {result:?}"
        );
    }

    /// R4 (#5977 regression): a `Quarantined` trust level must deny a `QUARANTINE_DENIED` tool
    /// call dispatched through a `/sessions`-built (serve) agent's executor — proving
    /// `build_agent_factory`'s per-session `apply_common_tool_gating` wrap is reachable. Before
    /// this PR, `serve/deps.rs::build_tool_executor` never called `apply_common_tool_gating` at
    /// all, so serve had zero trust/quarantine enforcement regardless of `[skills.trust]`.
    #[tokio::test]
    async fn build_agent_factory_wires_trust_gate_quarantine_deny() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let session_id = zeph_common::SessionId::new("trust-quarantine-test-session");

        let config = zeph_core::config::Config::default();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let (condenser, token_counter) = make_test_condenser();
        let deps = make_gate_test_deps(
            memory,
            session_config,
            condenser,
            token_counter,
            crate::agent_setup::PolicyGatePieces::default(),
        );

        let build_agent = build_agent_factory(deps, session_id, cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let agent = build_agent(channel);

        let executor = agent.tool_executor_arc();
        executor.set_effective_trust(zeph_common::SkillTrustLevel::Quarantined);
        let result = executor
            .execute_tool_call_erased(&make_tool_call("bash"))
            .await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "Quarantined trust must block a QUARANTINE_DENIED tool call (\"bash\") through a \
             /sessions-built agent's executor, got {result:?}"
        );
    }

    /// Coverage gap closed post-review: `crate::acp::build_combined_deps` (`serve --acp`
    /// combined mode) claims to inherit the SEC-H1 per-session gate fix "for free" because it
    /// reuses `deps::assemble_serve_deps` under the hood, but no test previously exercised that
    /// claim end-to-end — `build_combined_deps_wires_skill_matching_config_from_config`
    /// (`src/acp.rs`) only asserts skill/RL config fields flow into the two deps structs, never
    /// `tool_executor`/`policy_gate_pieces`. Drives the full production pipeline
    /// (`build_combined_deps` -> `ServeAgentDeps` -> [`build_agent_factory`] -> the per-session
    /// gated executor) with a `[tools.policy]` deny rule configured, and asserts a `shell` call
    /// is blocked — proving combined mode's `/sessions` agents get the same gate wrap as
    /// standalone serve (this test), not merely a code-sharing assertion in a doc comment.
    #[cfg(all(feature = "acp-http", feature = "session"))]
    #[tokio::test]
    async fn build_combined_deps_wires_policy_gate_through_to_session_agent() {
        let mut config =
            zeph_core::config::Config::load(std::path::Path::new("/nonexistent")).unwrap();
        config.llm.providers = vec![zeph_core::config::ProviderEntry {
            provider_type: zeph_core::config::ProviderKind::Ollama,
            base_url: Some("http://127.0.0.1:1".to_owned()),
            model: Some("test-model".to_owned()),
            ..Default::default()
        }];
        config.memory.sqlite_path = ":memory:".to_owned();
        config.tools.policy = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: vec![zeph_tools::PolicyRuleConfig {
                effect: zeph_tools::PolicyEffect::Deny,
                tool: "shell".into(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            ..Default::default()
        };

        let app = crate::bootstrap::AppBuilder::for_test(config);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = std::sync::Arc::new(zeph_common::TaskSupervisor::new(cancel));

        let (serve_deps, _acp_deps, _keepalive) =
            crate::acp::build_combined_deps(&app, &supervisor)
                .await
                .expect("build_combined_deps must succeed against a mock-provider AppBuilder");

        assert!(
            serve_deps.policy_gate_pieces.policy_enforcer.is_some(),
            "the [tools.policy] deny rule must compile into ServeAgentDeps::policy_gate_pieces \
             via assemble_serve_deps, same as the standalone serve path"
        );

        let session_id = zeph_common::SessionId::new("combined-mode-policy-test-session");
        let cid = serve_deps
            .memory
            .sqlite()
            .create_conversation()
            .await
            .unwrap();
        let build_agent = build_agent_factory(serve_deps, session_id, cid).await;
        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let agent = build_agent(channel);

        let result = agent
            .tool_executor_arc()
            .execute_tool_call_erased(&make_tool_call("shell"))
            .await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "a [tools.policy] deny rule for \"shell\" must block the call through a \
             combined-mode (`serve --acp`) /sessions-built agent's executor — the deps-level \
             assertion above proves construction, this proves the wrap is actually reachable; \
             got {result:?}"
        );
    }
}
