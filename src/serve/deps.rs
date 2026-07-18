// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent-dependency assembly for `/sessions*` (spec-068 §9.4, #5343).
//!
//! Deliberately smaller than `src/acp.rs`'s `SharedAgentDeps`/`build_acp_deps`: that struct
//! carries roughly fifteen ACP-transport-specific fields (permission files, the ACP
//! model-switching provider factory, auth bearer tokens, project-rules metadata) that do not
//! apply to a plain HTTP/SSE session, so it is not reused wholesale here. [`ServeAgentDeps`]
//! covers the minimum needed for a working conversational session — provider, skills, memory,
//! and a core tool set (shell/file/web/cwd, with sandbox and audit wired the same way
//! `build_acp_deps` does for ACP sessions). `agent_factory::build_agent_factory` additionally
//! composes `skill_loader`/`invoke_skill`/`memory`/`overflow` tool executors around this base
//! per session (#6046), matching CLI/TUI/ACP/daemon's tool surface.
//!
//! **Known gap**: MCP tools, the scheduler executor, and skill/config hot-reload broadcast
//! forwarding are not wired here — a session created via `/sessions` does not see MCP-provided
//! tools or live-reload skill/config changes yet. Follow-up once the core create/prompt/events
//! path is proven out.
//!
//! **Gating (#5973/#5977)**: the trust/policy/adversarial-policy gate stack IS wired, matching
//! `runner.rs`/`acp.rs`/`daemon.rs` — see [`build_tool_executor`]'s and
//! `agent_factory::build_agent_factory`'s doc comments for the split between eager pieces here
//! and the per-session gate wrap there (required to avoid a cross-session trust-state race,
//! SEC-H1). MCP is still absent from the gated tree (the gap above), so
//! `agent_setup::register_mcp_tool_ids` is called with an empty slice for now — the seam a
//! future MCP-wiring PR must populate.
//!
//! **`[tools.policy]` compile failure (#6008)**: unlike CLI/TUI/ACP/daemon, which stay
//! fail-open (see `agent_setup::build_policy_gate_pieces`'s doc comment) since an operator
//! running those locally sees the `tracing::error!` line themselves, [`assemble_serve_deps`]
//! aborts `serve-sessions` startup on a real compile failure — an HTTP-facing entrypoint with
//! potentially remote/less-trusted callers has no other way to learn policy enforcement is
//! silently disabled.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use zeph_llm::any::AnyProvider;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::matcher::SkillMatcherBackend;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::ErasedToolExecutor;

/// Send-safe, cloneable agent dependencies shared across all `/sessions*`-created agents.
///
/// Built once in [`build_serve_deps`] at `zeph serve-sessions` startup; each session's
/// `build_agent` factory (see `SessionActor::spawn`) clones the fields it needs — every field
/// here is cheap to clone (`Arc`, `Clone` provider handles, or a `usize`/config snapshot).
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)] // independent boolean flags; mirrors src/acp.rs's SharedAgentDeps
pub(crate) struct ServeAgentDeps {
    pub(crate) provider: AnyProvider,
    pub(crate) embedding_provider: AnyProvider,
    pub(crate) registry: Arc<RwLock<SkillRegistry>>,
    pub(crate) matcher: Option<SkillMatcherBackend>,
    pub(crate) max_active_skills: usize,
    /// `config.skills.disambiguation_threshold`/`two_stage_matching`/`confusability_threshold`,
    /// wired into `Agent::with_skill_matching_config` per session — mirrors `src/runner.rs` and
    /// `src/daemon.rs` (#5818: previously left on hardcoded builder defaults for `/sessions`).
    pub(crate) skill_disambiguation_threshold: f32,
    pub(crate) skill_two_stage_matching: bool,
    pub(crate) skill_confusability_threshold: f32,
    /// `config.skills.group_structured`/`support_similarity_threshold`/`min_injection_score`,
    /// wired into `Agent::with_skill_group_config` per session — mirrors `src/runner.rs` and
    /// `src/daemon.rs` (#5867: previously left on hardcoded builder defaults for `/sessions`).
    pub(crate) skill_group_structured: bool,
    pub(crate) skill_support_similarity_threshold: f32,
    pub(crate) skill_min_injection_score: f32,
    /// `config.skills.generation_provider`/`disambiguate_provider`, wired into
    /// `Agent::with_skill_provider_names` per session (#5818).
    pub(crate) skill_generation_provider: String,
    pub(crate) skill_disambiguate_provider: String,
    /// `config.skills.semantic_scan`/`semantic_scan_provider`, wired into
    /// `Agent::with_semantic_scan` per session — mirrors `src/runner.rs` and `src/daemon.rs`
    /// (#5827: previously left on hardcoded builder defaults for `/sessions`).
    pub(crate) semantic_scan: bool,
    pub(crate) semantic_scan_provider: String,
    /// `config.skills.trust`, wired into `Agent::with_trust_config` per session — mirrors
    /// `src/runner.rs` and `src/daemon.rs` (#5920: previously left on `TrustConfig::default()`
    /// for `/sessions` agents, silently ignoring the operator's configured trust levels).
    pub(crate) trust_config: zeph_core::config::TrustConfig,
    /// `config.skills.rl_routing_enabled`/`rl_learning_rate`/`rl_weight`/`rl_persist_interval`/
    /// `rl_warmup_updates`, wired into `Agent::with_rl_routing` per session, plus the shared RL
    /// head (`crate::acp::SharedCore::rl_head`) wired into `Agent::with_rl_head` — mirrors
    /// `src/runner.rs` and `src/daemon.rs` (#5921: previously never wired for `/sessions`
    /// agents). `rl_head` is cloned (cheap `Arc` clone) from the same `SharedCore` instance
    /// shared with `SharedAgentDeps`, fixing #5974 (concurrent `/sessions` agents previously
    /// each loaded/persisted an independent in-memory copy, clobbering each other's learned
    /// weights).
    pub(crate) rl_routing_enabled: bool,
    pub(crate) rl_learning_rate: f32,
    pub(crate) rl_weight: f32,
    pub(crate) rl_persist_interval: u32,
    pub(crate) rl_warmup_updates: u32,
    pub(crate) rl_head: Option<zeph_skills::rl_head::RoutingHead>,
    /// Base tool composite (file/shell/scrape/cwd), *not* wrapped in any gate. Per SEC-H1
    /// (concurrent `/sessions` share this one `Arc`, but `TrustGateExecutor`/`PolicyGateExecutor`
    /// carry per-turn *mutable* trust state), `agent_factory::build_agent_factory` wraps a
    /// fresh trust/policy/adversarial gate stack around this shared base **per session** — see
    /// its doc comment. This field must never be dispatched to directly without that wrap.
    pub(crate) tool_executor: Arc<dyn ErasedToolExecutor>,
    /// `[security.capability_scopes]` snapshot (#6045). `assemble_serve_deps` only *validates*
    /// this compiles against the tool registry at startup (fatal on error, matching
    /// `runner.rs`/`daemon.rs`'s precedent for this process-global config) — the actual
    /// `ScopedToolExecutor` WRAP happens fresh per session in
    /// `agent_factory::build_agent_factory`, mirroring `src/acp.rs`'s per-connection wrap, so
    /// each session's `OutOfScope` denials can feed that session's own `TrajectorySentinel`
    /// signal queue via `.with_signal_queue(...)` without leaking into other concurrent
    /// sessions sharing this same config snapshot.
    ///
    /// The startup validation and the per-session wrap must compile patterns against the SAME
    /// tool-id registry, or a pattern valid per-session can falsely abort startup (#6045/F1,
    /// caught by adversarial review): both call sites use
    /// `agent_factory::compose_session_tool_tree` to build that registry, so they can never
    /// drift apart.
    pub(crate) capability_scopes_config: zeph_config::CapabilityScopesConfig,
    /// Shared permission policy, threaded into each session's `TrustGateExecutor` wrap (via
    /// `agent_setup::apply_common_tool_gating`) in `agent_factory::build_agent_factory`.
    pub(crate) permission_policy: zeph_tools::PermissionPolicy,
    /// Shared audit logger, threaded into each session's `AdversarialPolicyGateExecutor` wrap
    /// (via `agent_setup::apply_policy_gate_chain`) in `agent_factory::build_agent_factory`.
    pub(crate) audit_logger: Option<Arc<zeph_tools::AuditLogger>>,
    /// Pre-built declarative-policy enforcer and adversarial-policy validator/LLM-client,
    /// built once eagerly at startup (`assemble_serve_deps`) via
    /// `agent_setup::build_policy_gate_pieces` — safe to share since they are immutable/
    /// read-only. `agent_factory::build_agent_factory` wraps fresh `PolicyGateExecutor`/
    /// `AdversarialPolicyGateExecutor` instances per session (via
    /// `agent_setup::apply_policy_gate_chain`) reusing these shared pieces.
    pub(crate) policy_gate_pieces: crate::agent_setup::PolicyGatePieces,
    pub(crate) memory: Arc<SemanticMemory>,
    pub(crate) history_limit: u32,
    pub(crate) recall_limit: usize,
    pub(crate) summarization_threshold: usize,
    pub(crate) session_config: zeph_core::AgentSessionConfig,
    pub(crate) session_persistence_config: zeph_config::SessionConfig,
    /// D-13 (spec-068 §8.1, N3): resume-time durable condensation, pre-built once here (where
    /// the full `Config` is still in scope) rather than per-session in
    /// `agent_factory::hydrate_session_sink`, which only receives this already-cloned
    /// deps bundle — mirrors `src/acp.rs`'s `SharedAgentDeps::resume_condenser` field. `Arc`-
    /// wrapped so `ServeAgentDeps` stays cheaply `Clone` without requiring `LlmCondenser: Clone`.
    pub(crate) resume_condenser: Arc<zeph_session::LlmCondenser>,
    pub(crate) resume_token_counter: Arc<zeph_agent_context::memory_backend::TokenCounterAdapter>,
    /// Snapshot of `[[llm.providers]]` entries, wired into each session's `Agent` via
    /// `with_provider_pool` so `resolve_background_provider` (background-provider lookups such
    /// as `memory.graph.extract_provider`) can find named providers (#5450).
    pub(crate) provider_pool: Vec<zeph_core::config::ProviderEntry>,
    pub(crate) provider_config_snapshot: zeph_core::ProviderConfigSnapshot,
    /// Spec 050 Phase 2 (#5913): `[security.shadow_sentinel]` snapshot, paired with
    /// `shadow_sentinel_probe_provider` below. `build_agent_factory` builds a fresh
    /// `ShadowSentinel`/`ShadowProbeExecutor` per session (keyed by that session's own
    /// `conversation_id`) when `enabled = true`.
    pub(crate) shadow_sentinel_config: zeph_config::ShadowSentinelConfig,
    /// Provider for `ShadowSentinel`'s `LlmSafetyProbe`, pre-resolved once at deps-assembly time
    /// (named-provider resolution + secret masking are static config work) — mirrors
    /// `src/acp.rs`'s `SharedAgentDeps::shadow_sentinel_probe_provider`.
    pub(crate) shadow_sentinel_probe_provider: AnyProvider,
    /// Spec 050 (#5958): `[security.trajectory]` snapshot. `agent_factory::build_agent_factory`
    /// builds a fresh per-session `TrajectorySentinel` risk slot/signal queue from this when
    /// wiring `Agent::with_trajectory_config`, mirroring `src/runner.rs`/`src/daemon.rs`/
    /// `src/acp.rs`.
    pub(crate) trajectory_sentinel_config: zeph_config::TrajectorySentinelConfig,
    /// #5951: pre-built `SelfCheckPipeline` (`config.quality.self_check`), shared across every
    /// `/sessions*` agent built from this `ServeAgentDeps` — provider masking is static config
    /// work, so it does not need to be rebuilt per session. `agent_factory::build_agent_factory`
    /// attaches it via `Agent::with_quality_pipeline`, mirroring `src/runner.rs`.
    pub(crate) quality_pipeline: Option<Arc<zeph_core::quality::SelfCheckPipeline>>,
    /// Safe-mode gate (#6031): `config.cli.safe_mode`, wired into every session's `Agent` via
    /// `Agent::with_safe_mode` so `check_cwd_changed`'s `/cd` (#6032) instruction-re-discovery
    /// gate is correctly set for serve-built agents, matching `runner.rs`/`daemon.rs`/`acp.rs`.
    pub(crate) safe_mode: bool,
    /// `config.tools.shell.allowed_paths` (#6032 SEC-2), wired into every session's `Agent`
    /// via `Agent::with_allowed_paths` so `/cd` is validated against the same sandbox
    /// boundary `FileExecutor`/`DiagnosticsExecutor`/`SetCwdExecutor` already enforce.
    pub(crate) allowed_paths: Vec<PathBuf>,
    /// `config.tools.enabled` (#6386), wired into every session's `Agent` via
    /// `Agent::with_tools_enabled` so `[tools] enabled = false` actually suppresses tool
    /// definitions for `/sessions`-built agents, matching `runner.rs`/`daemon.rs`/`acp.rs`.
    pub(crate) tools_enabled: bool,
}

/// Assemble [`ServeAgentDeps`] once at `zeph serve-sessions` startup, plus the resolved bearer
/// auth token (spec §9.4's `require_auth`/`auth_token_vault_key`) as a separate value — it is
/// server-level config, not an agent-construction dependency, so it does not belong on
/// [`ServeAgentDeps`] itself.
///
/// Mirrors the early portion of `src/acp.rs`'s `build_acp_deps` (provider, embedding provider,
/// skill registry/matcher, memory, and a core shell/file/web/cwd tool set with sandbox + audit)
/// but stops before MCP, the scheduler, and every ACP-transport-only field.
///
/// # Errors
///
/// Returns an error if config loading/validation, vault resolution, provider construction, or
/// memory (`SQLite`/Qdrant) initialization fails.
pub(crate) async fn build_serve_deps(
    config_path: Option<&std::path::Path>,
    vault_backend: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
    safe_mode: bool,
) -> anyhow::Result<(ServeAgentDeps, Option<String>)> {
    use crate::bootstrap::AppBuilder;

    // Serve wires no hooks or MCP tools today (see `ServeSessionsArgs::safe_mode` doc), so
    // there is no media-passthrough surface to gate here — pass `false` unconditionally.
    let app = AppBuilder::new(
        config_path,
        vault_backend,
        vault_key,
        vault_path,
        safe_mode,
        false,
    )
    .await?;
    let auth_token = resolve_auth_token(&app).await;

    let cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);
    let core = crate::acp::build_shared_core(&app, &supervisor).await?;
    let deps = assemble_serve_deps(&app, &core, &supervisor).await?;

    Ok((deps, auth_token))
}

/// Assemble [`ServeAgentDeps`] from an already-built [`crate::acp::SharedCore`] — the tail of
/// [`build_serve_deps`], extracted so `crate::acp::build_combined_deps` (#5420, `zeph
/// serve-sessions --acp`) can reuse it against a `SharedCore` shared with the ACP transport
/// instead of building a second, independent one.
///
/// # Errors
///
/// Returns an error if the core tool executor cannot be built (audit logger, sandbox
/// initialization).
#[allow(clippy::too_many_lines)]
pub(crate) async fn assemble_serve_deps(
    app: &crate::bootstrap::AppBuilder,
    core: &crate::acp::SharedCore,
    supervisor: &zeph_common::task_supervisor::TaskSupervisor,
) -> anyhow::Result<ServeAgentDeps> {
    let config = app.config();
    if config.cli.safe_mode {
        tracing::info!(
            "safe mode active: plugins and skills are disabled for every session built from \
             this serve-sessions process (serve wires no hooks or MCP tools yet)"
        );
    }
    let (tool_executor, permission_policy, audit_logger) =
        build_tool_executor(config, supervisor).await?;
    // R2 (SEC-H1): built once, eagerly, here — its outputs (`PolicyEnforcer`/`PolicyValidator`/
    // `PolicyLlmClient` Arcs) are immutable and safe to share across sessions; this also keeps
    // the fallible/logging async prep (policy-file load, provider resolution) at server
    // startup rather than per-session. The actual gate WRAP happens per session in
    // `agent_factory::build_agent_factory` — see `ServeAgentDeps::tool_executor`'s doc comment
    // for why (SEC-H1: `TrustGateExecutor`/`PolicyGateExecutor` carry per-turn mutable trust
    // state, so sharing one already-gated instance across concurrent sessions would let one
    // session's trust level clobber another's).
    let policy_gate_pieces =
        crate::agent_setup::build_policy_gate_pieces(config, &core.provider).await;
    // #6008: `PolicyEnforcer::compile` failure is intentionally fail-open at all four
    // `Agent`-construction entry points (`build_policy_gate_pieces` leaves `policy_enforcer:
    // None` and logs `tracing::error!`, per #5973/#5977/#5886) — CLI/TUI/ACP/daemon operators
    // see that log line in their own terminal. `serve-sessions` is HTTP-facing with potentially
    // remote/less-trusted callers who have no such visibility, so it diverges here: abort
    // startup rather than silently accepting connections with declarative policy unenforced.
    // `policy_configured` (true only when `[tools.policy]`/`[tools.authorization]` was actually
    // enabled) distinguishes a real compile failure from policy being legitimately absent by
    // config, which must stay fail-open (no rules configured is not a degraded state).
    if policy_gate_pieces.policy_configured && policy_gate_pieces.policy_enforcer.is_none() {
        anyhow::bail!(
            "[tools.policy]/[tools.authorization] failed to compile (see the preceding error \
             log for details) — refusing to start serve-sessions with policy enforcement \
             silently disabled; fix the policy config or disable it explicitly to proceed"
        );
    }
    // R6: startup observability log, reflecting ACTUAL compiled/config state — emitted ONCE
    // here (not inside `build_policy_gate_pieces`, which would also change daemon/acp/runner
    // startup output). `policy=on` only distinguishes "compiled" from "off/failed"; the
    // preceding `tracing::error!` inside `build_policy_gate_pieces` already carries the
    // compile-failure detail (C4: accepted as sufficient rather than threading a separate
    // compile-failed flag through `PolicyGatePieces`).
    tracing::info!(
        trust = "on",
        policy = policy_gate_pieces.policy_enforcer.is_some(),
        adversarial = policy_gate_pieces.adversarial_validator.is_some(),
        "serve-sessions: gate stack active (per-session trust/policy/adversarial wrap)"
    );

    // Spec 050 F2 (#5913): validate `capability_scopes` compiles against the tool registry —
    // mirrors src/runner.rs/src/daemon.rs's fatal-startup-error precedent for this
    // process-global config (a typo should not silently disable scoping for every
    // `/sessions*` agent). The actual `ScopedToolExecutor` WRAP now happens fresh per session
    // in `agent_factory::build_agent_factory` (#6045), mirroring `src/acp.rs`, so it can attach
    // that session's own `TrajectorySentinel` signal queue — see
    // `ServeAgentDeps::capability_scopes_config`'s doc comment. This is validation-only: the
    // compiled `ScopedToolExecutor` built here is discarded, not stored, since `tool_executor`
    // must stay the unscoped base for the per-session wrap to compose around.
    //
    // #6045/F1: the registry validated here MUST be the same tool-id surface the per-session
    // wrap will scope, not just the shared base — otherwise a scope pattern referencing a
    // #6046 tool (skill_loader/skill_invoke/memory/overflow, not present in the bare
    // `build_tool_executor` base) compiles fine per-session but fatally aborts startup here as
    // a `DeadPattern` (zero matches against the smaller base-only registry), even though the
    // pattern is valid. `agent_factory::compose_session_tool_tree` is the one function both
    // this validation and `build_agent_factory`'s real per-session tree call, so the two
    // registries can never drift apart. `conversation_id`/`memory_validation_config` only
    // affect runtime dispatch, not `tool_definitions()` — safe to use placeholder values here.
    let capability_scopes_config = config.security.capability_scopes.clone();
    if !capability_scopes_config.scopes.is_empty() {
        use zeph_tools::scope::build_scoped_executor;
        let (composed_for_validation, _trust_snapshot) =
            crate::serve::agent_factory::compose_session_tool_tree(
                Arc::clone(&tool_executor),
                &core.registry,
                &core.memory,
                zeph_memory::ConversationId(0),
                zeph_config::MemoryWriteValidationConfig::default(),
            );
        let registry_ids: std::collections::HashSet<String> = composed_for_validation
            .tool_definitions_erased()
            .into_iter()
            .map(|def| {
                let id = def.id.to_string();
                if id.contains(':') {
                    id
                } else {
                    format!("builtin:{id}")
                }
            })
            .collect();
        if let Err(e) = build_scoped_executor(
            zeph_tools::DynExecutor(composed_for_validation),
            &capability_scopes_config,
            &registry_ids,
        ) {
            anyhow::bail!("capability_scopes: {e}");
        }
    }

    // #5914/#5979/#6180: memory maintenance loops — spawned via the shared
    // `agent_setup::spawn_memory_maintenance_loops` (also used by `src/runner.rs`, `src/acp.rs`,
    // `src/daemon.rs`) so `/sessions*` agents get the same ongoing eviction/tier-promotion/
    // scene-consolidation/consolidation/forgetting/guidelines/tree-consolidation/
    // hebbian-consolidation/episodic-consolidation/optical-forgetting sweeps instead of an
    // ever-growing, never-maintained memory store. Spawned once per `supervisor` (shared across
    // all `/sessions*`-created agents); when called from `crate::acp::build_combined_deps`
    // (`serve-sessions --acp`), `build_acp_deps` spawns the same named tasks on this same
    // supervisor right after — `TaskSupervisor::spawn` aborts and replaces a same-named task, so
    // the combined-mode double-registration is a harmless no-op, not a duplicate background loop.
    crate::agent_setup::spawn_memory_maintenance_loops(
        app,
        &core.memory,
        &core.provider,
        supervisor,
        None,
        false,
        "serve",
    );

    let session_config = zeph_core::AgentSessionConfig::from_config(config, core.budget_tokens);
    let max_active_skills = config.skills.max_active_skills.get();
    let history_limit = config.memory.history_limit;
    let recall_limit = config.memory.semantic.recall_limit;
    let summarization_threshold = config.memory.summarization_threshold;
    let session_persistence_config = config.session.clone();
    // D-13 (spec-068 §8.1, N3): built once here, where the full `Config` is still in scope —
    // see `ServeAgentDeps::resume_condenser`'s doc comment.
    let (resume_condenser, resume_token_counter) =
        zeph_core::provider_factory::build_resume_condenser(config, &core.provider);
    // #5450: built once here, where the full `Config` is still in scope — mirrors
    // `src/runner.rs`'s CLI-path snapshot construction, so `/sessions`-created agents get a
    // populated `provider_pool` too (previously left empty, breaking `resolve_background_provider`).
    let provider_config_snapshot = zeph_core::ProviderConfigSnapshot {
        claude_api_key: config
            .secrets
            .claude_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        openai_api_key: config
            .secrets
            .openai_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        gemini_api_key: config
            .secrets
            .gemini_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        compatible_api_keys: config
            .secrets
            .compatible_api_keys
            .iter()
            .map(|(k, v)| (k.clone(), v.expose().to_owned()))
            .collect(),
        llm_request_timeout_secs: config.timeouts.llm_request_timeout_secs,
        embedding_model: config.llm.embedding_model.clone(),
        gonka_private_key: config
            .secrets
            .gonka_private_key
            .as_ref()
            .map(|s| zeroize::Zeroizing::new(s.expose().to_owned())),
        gonka_address: config
            .secrets
            .gonka_address
            .as_ref()
            .map(|s| s.expose().to_owned()),
        cocoon_access_hash: config
            .secrets
            .cocoon_access_hash
            .as_ref()
            .map(|s| s.expose().to_owned()),
    };

    // Spec 050 Phase 2 (#5913): pre-resolve shadow_sentinel config and its probe provider once
    // here — provider resolution + secret masking are static config work, mirroring
    // `src/acp.rs`'s `SharedAgentDeps` build. `build_agent_factory` builds the per-session
    // `ShadowSentinel` from these, since its persisted event store is keyed by that session's
    // own `conversation_id`. (`capability_scopes` wrapping already happened above, on
    // `tool_executor` itself, before any session exists.)
    let shadow_sentinel_config = config.security.shadow_sentinel.clone();
    let shadow_sentinel_probe_provider = {
        let sentinel_cfg = &shadow_sentinel_config;
        let base = if sentinel_cfg.probe_provider.is_empty() {
            core.provider.clone()
        } else {
            match crate::bootstrap::create_named_provider(
                sentinel_cfg.probe_provider.as_str(),
                config,
            ) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        provider = %sentinel_cfg.probe_provider,
                        error = %e,
                        "shadow_sentinel probe provider resolution failed, using primary"
                    );
                    core.provider.clone()
                }
            }
        };
        // #5437 round-3 style masking: the probe's own prompt embeds already-unmasked tool
        // args (see runner.rs's identical rationale), so every `.chat()` call this provider
        // makes must re-mask before the request leaves the process.
        match app.secret_registry() {
            Some(registry) => base.masked(registry as Arc<dyn zeph_llm::masking::OutboundMasker>),
            None => base,
        }
    };

    // Spec 050 (#5958): `[security.trajectory]` snapshot — `build_agent_factory` builds the
    // per-session risk slot/signal queue from this, mirroring `src/runner.rs`/`src/daemon.rs`/
    // `src/acp.rs`.
    let trajectory_sentinel_config = config.security.trajectory.clone();
    // #5951: built once here — provider masking is static config work, mirrors
    // `shadow_sentinel_probe_provider` above.
    let quality_pipeline = crate::agent_setup::build_quality_pipeline(
        config,
        &core.provider,
        app.secret_registry().as_ref(),
    );

    Ok(ServeAgentDeps {
        provider: core.provider.clone(),
        embedding_provider: core.embedding_provider.clone(),
        registry: Arc::clone(&core.registry),
        matcher: core.matcher.clone(),
        max_active_skills,
        skill_disambiguation_threshold: config.skills.disambiguation_threshold,
        skill_two_stage_matching: config.skills.two_stage_matching,
        skill_confusability_threshold: config.skills.confusability_threshold,
        skill_group_structured: config.skills.group_structured,
        skill_support_similarity_threshold: config.skills.support_similarity_threshold,
        skill_min_injection_score: config.skills.min_injection_score,
        skill_generation_provider: config.skills.generation_provider.as_str().to_owned(),
        skill_disambiguate_provider: config.skills.disambiguate_provider.as_str().to_owned(),
        semantic_scan: config.skills.semantic_scan,
        semantic_scan_provider: config.skills.semantic_scan_provider.as_str().to_owned(),
        trust_config: config.skills.trust.clone(),
        rl_routing_enabled: config.skills.rl_routing_enabled,
        rl_learning_rate: config.skills.rl_learning_rate,
        rl_weight: config.skills.rl_weight,
        rl_persist_interval: config.skills.rl_persist_interval,
        rl_warmup_updates: config.skills.rl_warmup_updates,
        rl_head: core.rl_head.clone(),
        tool_executor,
        capability_scopes_config,
        permission_policy,
        audit_logger,
        policy_gate_pieces,
        memory: Arc::clone(&core.memory),
        history_limit,
        recall_limit,
        summarization_threshold,
        session_config,
        session_persistence_config,
        resume_condenser: Arc::new(resume_condenser),
        resume_token_counter,
        provider_pool: config.llm.providers.clone(),
        provider_config_snapshot,
        shadow_sentinel_config,
        shadow_sentinel_probe_provider,
        trajectory_sentinel_config,
        quality_pipeline,
        safe_mode: config.cli.safe_mode,
        allowed_paths: config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
        tools_enabled: config.tools.enabled,
    })
}

/// Resolves `[serve] auth_token_vault_key` from the vault. `None` when the key is empty
/// (`require_auth`'s default off-switch) or the vault lookup fails/misses — the caller
/// (`handle_serve_sessions_command`, or `run_serve_with_acp` for the combined path) decides how
/// to react (refuse to bind non-loopback, or proceed with `auth_middleware` rejecting every
/// request when `require_auth = true`).
pub(crate) async fn resolve_auth_token(app: &crate::bootstrap::AppBuilder) -> Option<String> {
    let key = &app.config().serve.auth_token_vault_key;
    if key.is_empty() {
        return None;
    }
    app.vault().get_secret(key).await.unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            key = %key,
            "serve-sessions: failed to resolve auth token from vault"
        );
        None
    })
}

/// Builds the core shell/file/web/cwd tool set (sandbox + audit wired the same way
/// `build_acp_deps` does for ACP sessions) — extracted from [`build_serve_deps`] to stay under
/// clippy's `too_many_lines`.
///
/// Returns the **un-gated** base composite alongside the `permission_policy`/`audit_logger`
/// [`assemble_serve_deps`] stores on [`ServeAgentDeps`] for the per-session gate wrap in
/// `agent_factory::build_agent_factory` (SEC-H1) — see [`ServeAgentDeps::tool_executor`]'s doc
/// comment.
///
/// INVARIANT: any *shared, session-independent* tool executor MUST be composited in HERE,
/// before this function returns, so it is part of the tree `agent_factory::build_agent_factory`
/// gates. `skill_loader`/`invoke_skill`/`memory`/`overflow` (#6046) are the one exception —
/// they depend on a per-session `conversation_id`, so `build_agent_factory` composes them
/// itself, still *before* wrapping the trust/policy/adversarial gate stack around the combined
/// tree — see its doc comment. Anything composed onto `ServeAgentDeps` downstream of that gate
/// wrap (in either function) would bypass trust/policy/adversarial enforcement entirely. See
/// #5977/#5611/#5748.
async fn build_tool_executor(
    config: &zeph_core::config::Config,
    supervisor: &zeph_common::task_supervisor::TaskSupervisor,
) -> anyhow::Result<(
    Arc<dyn ErasedToolExecutor>,
    zeph_tools::PermissionPolicy,
    Option<Arc<zeph_tools::AuditLogger>>,
)> {
    let permission_policy =
        zeph_tools::build_permission_policy(&config.tools, config.security.autonomy_level);
    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(permission_policy.clone())
        .with_output_filters(filter_registry)
        .with_task_supervisor(supervisor.clone());
    if config.tools.sandbox.enabled {
        let denied_present = !config.tools.sandbox.denied_domains.is_empty();
        match zeph_tools::sandbox::build_sandbox_with_policy(
            config.tools.sandbox.strict,
            config.tools.sandbox.fail_if_unavailable,
            denied_present,
        ) {
            Ok(backend) => {
                let name = backend.name();
                let policy = crate::agent_setup::sandbox_policy_from_config(&config.tools.sandbox);
                shell_executor = shell_executor.with_sandbox(Arc::from(backend), policy);
                tracing::info!(backend = name, "OS sandbox enabled (serve-sessions)");
            }
            Err(e) if config.tools.sandbox.strict || config.tools.sandbox.fail_if_unavailable => {
                anyhow::bail!("sandbox initialization failed: {e}");
            }
            Err(e) => {
                tracing::warn!("OS sandbox unavailable, running without isolation: {e}");
            }
        }
    }
    let mut scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape)
        .with_egress_config(config.tools.egress.clone());
    let web_search_api_key = config
        .secrets
        .web_search_api_key
        .as_ref()
        .map(|s| zeph_common::secret::Secret::new(s.expose()));
    let mut web_search_executor = zeph_tools::WebSearchExecutor::new(
        &config.tools.search,
        &config.tools.scrape,
        web_search_api_key,
    )
    .map(|w| w.with_egress_config(config.tools.egress.clone()));
    let mut audit_logger = None;
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit, false).await
    {
        let logger = Arc::new(logger);
        shell_executor = shell_executor.with_audit(Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(Arc::clone(&logger));
        if let Some(w) = web_search_executor.take() {
            web_search_executor = Some(w.with_audit(Arc::clone(&logger)));
        }
        audit_logger = Some(logger);
    }
    let file_executor = zeph_tools::FileExecutor::new(
        config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
    );
    let cwd_executor = zeph_tools::SetCwdExecutor::new(
        config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
    );
    let base: Arc<dyn ErasedToolExecutor> = Arc::new(zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(
            shell_executor,
            zeph_tools::CompositeExecutor::new(
                scrape_executor,
                zeph_tools::CompositeExecutor::new(
                    zeph_tools::OptionalExecutor(web_search_executor),
                    cwd_executor,
                ),
            ),
        ),
    ));
    Ok((base, permission_policy, audit_logger))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Regression test confirming all ten memory-maintenance loops (eviction, tier-promotion,
    /// scene-consolidation, consolidation, forgetting — #5914; plus guidelines,
    /// tree-consolidation, hebbian-consolidation, episodic-consolidation, optical-forgetting —
    /// #5979) are actually spawned by the shared `agent_setup::spawn_memory_maintenance_loops`
    /// (called from `assemble_serve_deps` at `zeph serve-sessions` startup, and by
    /// `src/runner.rs`/`src/acp.rs`/`src/daemon.rs`, #6180) on the supplied `TaskSupervisor`.
    /// The five #5979 loops are config-gated (unlike the unconditional first five), so the test
    /// config below explicitly enables each — mirrors production behavior with those settings
    /// on. `AppBuilder::for_test` lets this test call the real, shipped production code directly
    /// rather than a copy of it.
    #[tokio::test]
    async fn spawn_memory_maintenance_loops_registers_all_ten_tasks() {
        let mut config = zeph_core::config::Config::default();
        config.memory.compression_guidelines.enabled = true;
        config.memory.tree.enabled = true;
        config.memory.hebbian.enabled = true;
        config.memory.episodic_consolidation.enabled = true;
        config.memory.optical_forgetting.enabled = true;
        let app = crate::bootstrap::AppBuilder::for_test(config);
        let memory = make_memory().await;
        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);

        crate::agent_setup::spawn_memory_maintenance_loops(
            &app,
            &memory,
            &provider,
            &supervisor,
            None,
            false,
            "serve",
        );

        let names: std::collections::HashSet<String> = supervisor
            .snapshot()
            .into_iter()
            .map(|s| s.name.to_string())
            .collect();
        for expected in [
            "mem-eviction",
            "mem-tier-promotion",
            "mem-scene-consolidation",
            "mem-consolidation",
            "mem-forgetting",
            "mem-guidelines",
            "mem-tree-consolidation",
            "mem-hebbian-consolidation",
            "mem-episodic-consolidation",
            "mem-optical-forgetting",
        ] {
            assert!(
                names.contains(expected),
                "expected {expected} registered by spawn_memory_maintenance_loops, got {names:?}"
            );
        }
    }

    /// #5979 regression: with `Config::default()` (the five new `[memory.*] enabled` flags all
    /// default to `false`), the shared loop spawner must NOT register any of the five newly-gated
    /// loops, while the five unconditional loops from #5914 must still fire. Sibling of
    /// `spawn_memory_maintenance_loops_registers_all_ten_tasks` above, which only exercises the
    /// enabled-flags path — this closes the gap where an inverted or missing `if` guard would
    /// otherwise slip through undetected.
    #[tokio::test]
    async fn spawn_memory_maintenance_loops_gates_the_five_new_tasks_off_by_default() {
        let app = crate::bootstrap::AppBuilder::for_test(zeph_core::config::Config::default());
        let memory = make_memory().await;
        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);

        crate::agent_setup::spawn_memory_maintenance_loops(
            &app,
            &memory,
            &provider,
            &supervisor,
            None,
            false,
            "serve",
        );

        let names: std::collections::HashSet<String> = supervisor
            .snapshot()
            .into_iter()
            .map(|s| s.name.to_string())
            .collect();
        for expected in [
            "mem-eviction",
            "mem-tier-promotion",
            "mem-scene-consolidation",
            "mem-consolidation",
            "mem-forgetting",
        ] {
            assert!(
                names.contains(expected),
                "expected {expected} registered by spawn_memory_maintenance_loops even with \
                 Config::default(), got {names:?}"
            );
        }
        for absent in [
            "mem-guidelines",
            "mem-tree-consolidation",
            "mem-hebbian-consolidation",
            "mem-episodic-consolidation",
            "mem-optical-forgetting",
        ] {
            assert!(
                !names.contains(absent),
                "expected {absent} NOT registered by spawn_memory_maintenance_loops under \
                 Config::default() (all five new loops default to disabled), got {names:?}"
            );
        }
    }

    fn make_test_core(memory: Arc<SemanticMemory>) -> crate::acp::SharedCore {
        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        crate::acp::SharedCore {
            provider: provider.clone(),
            embedding_provider: provider,
            registry: Arc::new(RwLock::new(zeph_skills::registry::SkillRegistry::empty())),
            matcher: None,
            memory,
            budget_tokens: 4096,
            rl_head: None,
        }
    }

    /// #6045 regression: `assemble_serve_deps` must *validate* `[security.capability_scopes]`
    /// at startup (fatal on a bad config, mirroring `src/runner.rs`/`src/daemon.rs`'s
    /// precedent for this process-global setting) but must NOT bake a `ScopedToolExecutor`
    /// wrap into `ServeAgentDeps.tool_executor` — that field must stay the unscoped base so
    /// `agent_factory::build_agent_factory` can wrap it fresh per session (see
    /// `ServeAgentDeps::capability_scopes_config`'s doc comment for why: a shared, eagerly
    /// wrapped instance cannot receive a per-session `TrajectorySentinel` signal queue without
    /// leaking one session's `OutOfScope` signals into every other concurrent session).
    #[tokio::test]
    async fn assemble_serve_deps_validates_but_does_not_wrap_capability_scopes() {
        let mut config = zeph_core::config::Config::default();
        config.security.capability_scopes = zeph_config::CapabilityScopesConfig {
            default_scope: "narrow".to_owned(),
            scopes: std::collections::HashMap::from([(
                "narrow".to_owned(),
                zeph_config::ScopeConfig {
                    patterns: vec!["builtin:read".to_owned()],
                },
            )]),
            ..Default::default()
        };
        let app = crate::bootstrap::AppBuilder::for_test(config);
        let memory = make_memory().await;
        let core = make_test_core(memory);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);

        let deps = assemble_serve_deps(&app, &core, &supervisor)
            .await
            .expect("assemble_serve_deps must succeed with a valid single-pattern scope");

        assert_eq!(
            deps.capability_scopes_config.scopes.len(),
            1,
            "the validated config must be carried forward on ServeAgentDeps for the \
             per-session wrap"
        );

        // "bash" is outside the configured scope, but `deps.tool_executor` must still be the
        // UNSCOPED base — `ScopedToolExecutor` no longer wraps it here — so this call must NOT
        // be rejected with OutOfScope (it fails for an unrelated reason: no shell config).
        let result = deps
            .tool_executor
            .execute_tool_call_erased(&zeph_tools::ToolCall {
                tool_id: "bash".into(),
                params: serde_json::Map::new(),
                caller_id: None,
                context: None,
                tool_call_id: String::new(),
                skill_name: None,
            })
            .await;
        assert!(
            !matches!(result, Err(zeph_tools::ToolError::OutOfScope { .. })),
            "deps.tool_executor must stay unscoped after assemble_serve_deps — the scope wrap \
             now happens per session in agent_factory::build_agent_factory, got {result:?}"
        );
    }

    /// #6045/F1 regression (critic finding): before this fix, `assemble_serve_deps`'s startup
    /// capability-scope validation compiled patterns against `build_tool_executor`'s base-only
    /// registry (file/shell/scrape/cwd), which does NOT include the #6046 tools
    /// (`skill_loader`/`invoke_skill`/`memory`/`overflow`). A scope pattern referencing a
    /// #6046 tool — e.g. `builtin:invoke_skill`, a strict namespace under the default
    /// `PatternStrictness::ProvisionalForDynamicNamespaces` — matched zero ids in that smaller
    /// registry and hit `ScopeError::DeadPattern`, aborting `serve-sessions` startup entirely
    /// even though the pattern is valid against the real per-session registry
    /// `agent_factory::wrap_capability_scope` uses. `assemble_serve_deps` must now validate
    /// against the SAME composed registry (`agent_factory::compose_session_tool_tree`) the
    /// per-session wrap uses, so this must succeed, not abort.
    #[tokio::test]
    async fn assemble_serve_deps_succeeds_with_capability_scope_on_6046_tool() {
        let mut config = zeph_core::config::Config::default();
        config.security.capability_scopes = zeph_config::CapabilityScopesConfig {
            default_scope: "skills-only".to_owned(),
            scopes: std::collections::HashMap::from([(
                "skills-only".to_owned(),
                zeph_config::ScopeConfig {
                    patterns: vec!["builtin:invoke_skill".to_owned()],
                },
            )]),
            ..Default::default()
        };
        let app = crate::bootstrap::AppBuilder::for_test(config);
        let memory = make_memory().await;
        let core = make_test_core(memory);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);

        let result = assemble_serve_deps(&app, &core, &supervisor).await;
        assert!(
            result.is_ok(),
            "a [security.capability_scopes] pattern targeting a #6046 tool (\"builtin:invoke_skill\") \
             must NOT abort serve-sessions startup — it is valid against the real per-session \
             registry, got err: {:?}",
            result.err().map(|e| e.to_string())
        );
    }

    /// #6045: a `[security.capability_scopes]` pattern that fails to compile against the tool
    /// registry must still abort `serve-sessions` startup — mirrors the pre-#6045 fatal-error
    /// behavior, just moved to a validation-only build rather than the (now removed) eager wrap.
    #[tokio::test]
    async fn assemble_serve_deps_fails_startup_on_invalid_capability_scope_pattern() {
        let mut config = zeph_core::config::Config::default();
        config.security.capability_scopes = zeph_config::CapabilityScopesConfig {
            default_scope: "broken".to_owned(),
            scopes: std::collections::HashMap::from([(
                "broken".to_owned(),
                zeph_config::ScopeConfig {
                    patterns: vec!["[".to_owned()],
                },
            )]),
            ..Default::default()
        };
        let app = crate::bootstrap::AppBuilder::for_test(config);
        let memory = make_memory().await;
        let core = make_test_core(memory);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);

        let result = assemble_serve_deps(&app, &core, &supervisor).await;
        assert!(
            result.is_err(),
            "an invalid capability_scopes glob pattern must abort serve-sessions startup"
        );
    }

    /// #6008 regression: unlike CLI/TUI/ACP/daemon (which stay fail-open on a
    /// `[tools.policy]` compile failure, per #5973/#5977/#5886), `assemble_serve_deps` must
    /// abort `serve-sessions` startup instead of silently leaving `policy_enforcer: None` for
    /// an HTTP-facing entrypoint. Uses an invalid `args_match` regex (`(` is unterminated) to
    /// make `PolicyEnforcer::compile` fail without touching the filesystem.
    #[tokio::test]
    async fn assemble_serve_deps_fails_startup_on_policy_compile_failure() {
        let mut config = zeph_core::config::Config::default();
        config.tools.policy = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: vec![zeph_tools::PolicyRuleConfig {
                effect: zeph_tools::PolicyEffect::Deny,
                tool: "shell".into(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: Some("(".to_owned()),
                capabilities: vec![],
            }],
            ..Default::default()
        };
        let app = crate::bootstrap::AppBuilder::for_test(config);
        let memory = make_memory().await;
        let core = make_test_core(memory);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);

        let result = assemble_serve_deps(&app, &core, &supervisor).await;
        assert!(
            result.is_err(),
            "a [tools.policy] compile failure must abort serve-sessions startup (fail-closed), \
             not silently disable policy enforcement"
        );
    }

    /// Counterpart to the failure test above: `[tools.policy]` being absent/disabled entirely
    /// is a legitimate, non-degraded state and must stay fail-open — `assemble_serve_deps` must
    /// still succeed.
    #[tokio::test]
    async fn assemble_serve_deps_succeeds_when_policy_not_configured() {
        let config = zeph_core::config::Config::default();
        let app = crate::bootstrap::AppBuilder::for_test(config);
        let memory = make_memory().await;
        let core = make_test_core(memory);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);

        let deps = assemble_serve_deps(&app, &core, &supervisor).await.expect(
            "[tools.policy] absent by config is legitimate and must stay fail-open, not \
                 abort startup",
        );
        assert!(deps.policy_gate_pieces.policy_enforcer.is_none());
    }
}
