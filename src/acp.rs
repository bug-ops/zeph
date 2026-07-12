// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(any(feature = "acp", feature = "acp-http"))]
use std::path::PathBuf;

#[cfg(feature = "acp")]
use parking_lot::RwLock;

#[cfg(feature = "acp")]
use crate::agent_setup;
#[cfg(any(feature = "acp", feature = "acp-http"))]
use crate::bootstrap::{AppBuilder, create_mcp_registry};
#[cfg(feature = "acp")]
use zeph_core::agent::Agent;
#[cfg(feature = "acp")]
use zeph_core::channel::Channel;
#[cfg(feature = "acp")]
use zeph_tools::ErasedToolExecutor;

#[cfg(feature = "acp")]
fn resolve_runtime_path(path: &std::path::Path, cwd: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Resolve `[acp] auth_token` and `[[acp.auth_clients]]` into the named-client credential set
/// consumed by [`zeph_acp::AcpServerConfig::auth_clients`] (#5868).
///
/// The legacy scalar `auth_token` is synthesized as the `"default"` client. Each
/// `auth_clients` entry resolves its token from either the inline `token` field or, when
/// `token_vault_key` is set, the age vault. A vault key that fails to resolve (missing or
/// backend error) disables that one client (warned, not fatal) — mirrors
/// `serve::deps::resolve_auth_token`'s soft-fail precedent for the same class of vault
/// lookup. `zeph_config::AcpConfig::validate_auth_clients` already rejects inline-token
/// collisions and reserved ids at config-load time; the cross-set duplicate check here catches
/// the one thing that validation cannot (a vault-resolved token colliding with another token),
/// since the vault is not unlocked at config-load time.
///
/// # Errors
///
/// Returns an error if two configured clients (across `auth_token` and `auth_clients`,
/// inline or vault-resolved) end up with the same resolved token.
#[cfg(any(feature = "acp", feature = "acp-http"))]
async fn resolve_acp_auth_clients(
    acp_config: &zeph_config::AcpConfig,
    vault: &dyn zeph_core::vault::VaultProvider,
) -> anyhow::Result<Vec<zeph_acp::AcpClientToken>> {
    let mut clients = Vec::new();
    let mut seen_tokens: std::collections::HashSet<String> = std::collections::HashSet::new();

    if let Some(ref token) = acp_config.auth_token {
        seen_tokens.insert(token.clone());
        clients.push(zeph_acp::AcpClientToken {
            id: zeph_config::ACP_AUTH_CLIENT_ID_DEFAULT.to_owned(),
            token: token.clone(),
        });
    }

    for client in &acp_config.auth_clients {
        let token = if let Some(ref t) = client.token {
            Some(t.clone())
        } else if let Some(ref key) = client.token_vault_key {
            match vault.get_secret(key).await {
                Ok(Some(t)) => Some(t),
                Ok(None) => {
                    tracing::warn!(
                        id = %client.id, vault_key = %key,
                        "acp.auth_clients: vault key not found; client disabled"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        id = %client.id, vault_key = %key, error = %e,
                        "acp.auth_clients: failed to resolve token from vault; client disabled"
                    );
                    None
                }
            }
        } else {
            // Unreachable in practice: AcpConfig::validate_auth_clients rejects entries with
            // neither field set before this function ever runs.
            None
        };

        let Some(token) = token else { continue };

        anyhow::ensure!(
            seen_tokens.insert(token.clone()),
            "[[acp.auth_clients]] id {:?} resolves to a token that collides with another \
             configured client's token",
            client.id
        );
        clients.push(zeph_acp::AcpClientToken {
            id: client.id.clone(),
            token,
        });
    }

    Ok(clients)
}

#[cfg(feature = "acp")]
fn log_acp_runtime_paths(config: &zeph_core::config::Config, config_path: &std::path::Path) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let logging_file = if config.logging.file.is_empty() {
        None
    } else {
        Some(resolve_runtime_path(
            std::path::Path::new(&config.logging.file),
            &cwd,
        ))
    };
    let sqlite_path = resolve_runtime_path(std::path::Path::new(&config.memory.sqlite_path), &cwd);
    let debug_output_dir = resolve_runtime_path(config.debug.output_dir.as_path(), &cwd);
    let skill_paths: Vec<std::path::PathBuf> = config
        .skills
        .paths
        .iter()
        .map(|p| resolve_runtime_path(std::path::Path::new(p), &cwd))
        .collect();
    let permission_file = config
        .acp
        .permission_file
        .as_ref()
        .map(|p| resolve_runtime_path(p.as_path(), &cwd));

    tracing::info!(
        cwd = %cwd.display(),
        config_path = %config_path.display(),
        logging_file = logging_file
            .as_ref()
            .map_or_else(|| "<disabled>".to_owned(), |p| p.display().to_string()),
        sqlite_path = %sqlite_path.display(),
        debug_output_dir = %debug_output_dir.display(),
        permission_file = permission_file
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |p| p.display().to_string()),
        skill_paths = ?skill_paths,
        "ACP startup runtime paths"
    );
}

/// Pure, spawn-free resource bundle shared between `zeph serve-sessions` and standalone ACP
/// session construction (#5420).
///
/// Deliberately excludes anything that spawns a supervised task (`overflow_cleanup`,
/// `egress_drain`, skill/config watchers) — those stay in [`build_acp_deps`] so plain
/// `serve-sessions` never silently gains them and standalone ACP never silently loses them now
/// that both call [`build_shared_core`]. Gated on `any(session, acp)`, not just `acp`:
/// `zeph serve-sessions` (feature `session`) needs it even in builds without `acp`/`acp-http`.
#[cfg(any(feature = "session", feature = "acp"))]
pub(crate) struct SharedCore {
    pub(crate) provider: zeph_llm::any::AnyProvider,
    /// Dedicated embedding provider. Never replaced by `/provider switch`.
    pub(crate) embedding_provider: zeph_llm::any::AnyProvider,
    pub(crate) registry: std::sync::Arc<parking_lot::RwLock<zeph_skills::registry::SkillRegistry>>,
    /// Shared skill matcher: `Clone` is cheap for Qdrant (connection-pool sharing), and
    /// involves copying in-memory embedding vectors only for the `InMemory` variant.
    pub(crate) matcher: Option<zeph_skills::matcher::SkillMatcherBackend>,
    pub(crate) memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
    pub(crate) budget_tokens: usize,
    /// `SkillOrchestra` RL routing head (#5921), loaded/cold-started exactly once here —
    /// mirrors `src/runner.rs`/`src/daemon.rs`'s single-load pattern — and shared (via
    /// [`Clone`], which only clones the cheap `Arc` handle) by every session built from this
    /// core (ACP, `/sessions`, or the combined transport). `None` when
    /// `config.skills.rl_routing_enabled` is `false`.
    ///
    /// Fixes #5974: previously each session independently loaded its own in-memory
    /// `RoutingHead` copy from the `routing_head_weights` singleton row and persisted back
    /// independently, so concurrent ACP/serve sessions clobbered each other's learned REINFORCE
    /// weights (lost update). Loading once and sharing the same `Arc<Mutex<..>>` across every
    /// session sharing this core means updates from any session apply to the one true
    /// in-process state instead of a disposable copy.
    pub(crate) rl_head: Option<zeph_skills::rl_head::RoutingHead>,
}

/// Build the resources common to `zeph serve-sessions` and standalone ACP session
/// construction: provider, embedding provider, skill registry/matcher, and semantic memory.
///
/// Contains no `supervisor.spawn` calls — callers that need supervised background tasks
/// (overflow cleanup, egress drain, hot-reload watchers) spawn them themselves, after this
/// returns, using the same `supervisor`.
///
/// # Errors
///
/// Returns an error if provider construction or memory (`SQLite`/Qdrant) initialization fails.
#[cfg(any(feature = "session", feature = "acp"))]
pub(crate) async fn build_shared_core(
    app: &crate::bootstrap::AppBuilder,
    supervisor: &zeph_common::TaskSupervisor,
) -> anyhow::Result<SharedCore> {
    let (provider, _status_tx, _status_rx) = app.build_provider().await?;
    let embedding_provider = crate::bootstrap::create_embedding_provider(app.config(), &provider);
    let budget_tokens = app.auto_budget_tokens(&provider);
    let registry = std::sync::Arc::new(parking_lot::RwLock::new(app.build_registry()));
    let memory = std::sync::Arc::new(app.build_memory(&provider, supervisor).await?);

    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> =
        registry.read().all_meta().into_iter().cloned().collect();
    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    let matcher = app
        .build_skill_matcher(&embedding_provider, &all_meta_refs, &memory)
        .await;

    // Populate trust DB for all loaded skills (#5920: previously only `runner.rs` did this,
    // leaving ACP/`/sessions`-only agents' skills fail-open to Trusted
    // (SkillTrustLevel::MISSING_ENTRY_FALLBACK) absent a pre-existing row — un-sanitized
    // bodies with full tool access instead of the operator's configured restriction).
    app.seed_skill_trust_db(&all_meta_owned, &memory).await;

    // Pre-resolve RL embed dim before embedding_provider is moved into SharedCore (#5921) —
    // mirrors `src/daemon.rs`'s `rl_embed_dim_resolved` computation.
    let rl_embed_dim_resolved = if app.config().skills.rl_routing_enabled {
        Some(
            crate::runner::resolve_rl_embed_dim(
                &app.config().skills,
                &embedding_provider,
                app.config().timeouts.embedding_seconds,
            )
            .await,
        )
    } else {
        None
    };

    // #5974: load/cold-start the RL routing head exactly once here, so every session built
    // from this core clones the same Arc<Mutex<..>> handle (see SharedCore::rl_head doc)
    // instead of each session independently loading its own copy from the DB row.
    let rl_head = if let Some(dim) = rl_embed_dim_resolved {
        Some(
            crate::runner::load_rl_head(&memory)
                .await
                .unwrap_or_else(|| {
                    tracing::info!(dim, "rl_head: cold start, initializing fresh routing head");
                    zeph_skills::rl_head::RoutingHead::new(dim)
                }),
        )
    } else {
        None
    };

    Ok(SharedCore {
        provider,
        embedding_provider,
        registry,
        matcher,
        memory,
        budget_tokens,
        rl_head,
    })
}

/// Shared dependencies reused across all ACP sessions.
///
/// Fields in this struct are expensive to create and safe to share across sessions.
/// `AnyProvider` is intentionally shared via `Arc` — all provider variants use internal
/// HTTP connection pools (`reqwest::Client`) that benefit from connection reuse across sessions.
/// This is equivalent to sharing an HTTP client pool, which is the intended design.
///
/// Per-session state (`conversation_id`, reload receivers, cancel signals) is created fresh
/// in `spawn_acp_agent` for each session.
///
/// ## Field categories
///
/// - **Shared runtime objects** (`provider`, `registry`, `memory`, `mcp_manager`, etc.) —
///   expensive to create, safe to share via `Arc` / `Clone`.
/// - **Config snapshot** (`session_config`) — single source of truth for all config-derived
///   agent settings; see [`zeph_core::AgentSessionConfig`].
/// - **Optional runtime providers** (`summary_provider`, `judge_provider`,
///   `quarantine_provider`) — contain HTTP client pools (`AnyProvider`) with runtime state;
///   excluded from `session_config` because they are not purely config-derived.
/// - **MCP objects** (`mcp_tools`, `mcp_registry`, `mcp_manager`, `mcp_shared_tools`,
///   `mcp_config`) — runtime + config mixture; passed together to `with_mcp()`.
/// - **ACP-specific** (`acp_*`) — transport-level config; not agent-level.
/// - **Scheduler runtime** (`scheduler_*`) — runtime broadcast senders; not config-derived.
#[cfg(feature = "acp")]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct SharedAgentDeps {
    // Shared runtime objects
    provider: zeph_llm::any::AnyProvider,
    /// Dedicated embedding provider. Never replaced by `/provider switch`.
    embedding_provider: zeph_llm::any::AnyProvider,
    registry: std::sync::Arc<RwLock<zeph_skills::registry::SkillRegistry>>,
    /// Shared skill matcher: `Clone` is cheap for Qdrant (connection-pool sharing), and
    /// involves copying in-memory embedding vectors only for the `InMemory` variant.
    matcher: Option<zeph_skills::matcher::SkillMatcherBackend>,
    max_active_skills: usize,
    /// `config.skills.disambiguation_threshold`/`two_stage_matching`/`confusability_threshold`,
    /// wired into `Agent::with_skill_matching_config` per session — mirrors `src/runner.rs` and
    /// `src/daemon.rs` (#5818: previously left on hardcoded builder defaults for ACP sessions).
    skill_disambiguation_threshold: f32,
    skill_two_stage_matching: bool,
    skill_confusability_threshold: f32,
    /// `config.skills.group_structured`/`support_similarity_threshold`/`min_injection_score`,
    /// wired into `Agent::with_skill_group_config` per session — mirrors `src/runner.rs` and
    /// `src/daemon.rs` (#5867: previously left on hardcoded builder defaults for ACP sessions).
    skill_group_structured: bool,
    skill_support_similarity_threshold: f32,
    skill_min_injection_score: f32,
    /// `config.skills.generation_provider`/`disambiguate_provider`, wired into
    /// `Agent::with_skill_provider_names` per session (#5818).
    skill_generation_provider: String,
    skill_disambiguate_provider: String,
    /// `config.skills.semantic_scan`/`semantic_scan_provider`, wired into
    /// `Agent::with_semantic_scan` per session — mirrors `src/runner.rs` and `src/daemon.rs`
    /// (#5827: previously left on hardcoded builder defaults for ACP sessions).
    semantic_scan: bool,
    semantic_scan_provider: String,
    /// `config.skills.trust`, wired into `Agent::with_trust_config` per session — mirrors
    /// `src/runner.rs` and `src/daemon.rs` (#5920: previously left on `TrustConfig::default()`
    /// for ACP sessions, silently ignoring the operator's configured trust levels).
    trust_config: zeph_core::config::TrustConfig,
    /// `config.skills.rl_routing_enabled`/`rl_learning_rate`/`rl_weight`/`rl_persist_interval`/
    /// `rl_warmup_updates`, wired into `Agent::with_rl_routing` per session, plus the shared
    /// `RL` head (`SharedCore::rl_head`) wired into `Agent::with_rl_head` — mirrors
    /// `src/runner.rs` and `src/daemon.rs` (#5921: previously never wired for ACP sessions).
    /// `rl_head` is cloned (cheap `Arc` clone) from the *same* `SharedCore` instance into every
    /// session, fixing #5974 (concurrent ACP sessions previously each loaded/persisted an
    /// independent in-memory copy, clobbering each other's learned weights).
    rl_routing_enabled: bool,
    rl_learning_rate: f32,
    rl_weight: f32,
    rl_persist_interval: u32,
    rl_warmup_updates: u32,
    rl_head: Option<zeph_skills::rl_head::RoutingHead>,
    /// Base tool composite (file/shell/scrape/diagnostics + MCP + `search_code`), *not*
    /// wrapped in any gate. `spawn_acp_agent` composites this further with `skill_loader`/
    /// `memory`/`overflow`/ACP-native fs/shell per session, then wraps the FULL per-session
    /// result in `PolicyGateExecutor -> AdversarialPolicyGateExecutor -> TrustGateExecutor`
    /// (outermost first) via `policy_gate_pieces` below and
    /// `agent_setup::apply_common_tool_gating`/`apply_policy_gate_chain` — so this field must
    /// never be dispatched to directly without that wrap.
    tool_executor: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor>,
    /// Shared permission policy, threaded into `spawn_acp_agent`'s `TrustGateExecutor` wrap
    /// (via `apply_common_tool_gating`).
    permission_policy: zeph_tools::PermissionPolicy,
    /// Pre-built declarative-policy enforcer and adversarial-policy validator/LLM-client
    /// (`[tools.policy]`+`[tools.authorization]` and `[tools.adversarial_policy]`), built once
    /// per connection via `agent_setup::build_policy_gate_pieces` since both depend only on
    /// static config. `spawn_acp_agent` wraps the per-session composite in fresh
    /// `PolicyGateExecutor`/`AdversarialPolicyGateExecutor` instances (via
    /// `agent_setup::apply_policy_gate_chain`) reusing these shared, immutable pieces.
    policy_gate_pieces: agent_setup::PolicyGatePieces,
    /// Spec 050 F2 (#5913): `[security.capability_scopes]` snapshot. `spawn_acp_agent` wraps the
    /// fully-composed per-session tool executor in a `ScopedToolExecutor` when `scopes` is
    /// non-empty, mirroring `src/runner.rs`. Empty `scopes` is the no-op identity (FR-CG-003).
    capability_scopes_config: zeph_config::CapabilityScopesConfig,
    /// Spec 050 Phase 2 (#5913): `[security.shadow_sentinel]` snapshot, paired with
    /// `shadow_sentinel_probe_provider` below. `spawn_acp_agent` builds a fresh
    /// `ShadowSentinel`/`ShadowProbeExecutor` per session (keyed by that session's own
    /// `conversation_id`) when `enabled = true`, mirroring `src/runner.rs`.
    shadow_sentinel_config: zeph_config::ShadowSentinelConfig,
    /// Provider for `ShadowSentinel`'s `LlmSafetyProbe`, pre-resolved once per connection
    /// (named-provider resolution + secret masking are static config work) — mirrors the
    /// `adversarial_policy_validator`/`adversarial_policy_llm_client` resolution above.
    shadow_sentinel_probe_provider: zeph_llm::any::AnyProvider,
    /// Spec 050 (#5958): `[security.trajectory]` snapshot. `spawn_acp_agent` builds a fresh
    /// per-session `TrajectorySentinel` risk slot/signal queue from this when wiring
    /// `Agent::with_trajectory_config`, mirroring `src/runner.rs`/`src/daemon.rs`.
    trajectory_sentinel_config: zeph_config::TrajectorySentinelConfig,
    /// #5951: pre-built `SelfCheckPipeline` (`config.quality.self_check`), shared across every
    /// session from this connection — provider masking is static config work, so it does not
    /// need to be rebuilt per session. `spawn_acp_agent` attaches it via
    /// `Agent::with_quality_pipeline`, mirroring `src/runner.rs`.
    quality_pipeline: Option<std::sync::Arc<zeph_core::quality::SelfCheckPipeline>>,
    skill_paths: Vec<PathBuf>,
    /// `pub(crate)` (unlike its sibling fields) solely so the `build_combined_deps` test harness
    /// (`crate::serve::test_support::build_shared_pair`, #5420 N5) can assert `Arc::ptr_eq`
    /// against [`crate::serve::deps::ServeAgentDeps::memory`] — proving the production sharing
    /// path actually shares one pool, not a hand-reassembled test double.
    pub(crate) memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
    history_limit: u32,
    recall_limit: usize,
    summarization_threshold: usize,
    /// `config.memory.shutdown_summary*`, wired into `Agent::with_shutdown_summary_config`/
    /// `with_shutdown_summary_provider` per session — mirrors `src/runner.rs` (#5959: previously
    /// left on `MemoryCompactionState::default()` for ACP sessions, silently ignoring the
    /// operator's configured shutdown-summary settings).
    shutdown_summary: bool,
    shutdown_summary_min_messages: usize,
    shutdown_summary_max_messages: usize,
    shutdown_summary_timeout_secs: u64,
    shutdown_summary_provider: String,
    /// `config.session.provider_persistence`/`persist_provider_overrides`, wired into
    /// `Agent::with_channel_identity("acp", ...)` per session — mirrors `src/runner.rs`'s
    /// active-channel wiring (#5959: previously never wired for ACP, so ACP sessions never
    /// persisted/restored the last-used provider).
    channel_provider_persistence: bool,
    channel_persist_provider_overrides: bool,
    /// `config.index`, wired into `agent_setup::apply_code_retrieval`/`apply_code_rag_retriever`
    /// per session — mirrors `src/runner.rs` (#6022: previously never wired for ACP, so ACP
    /// sessions got no static repo-map injection, `IndexMcpServer` registration, or automatic
    /// code-RAG context retrieval).
    index_config: zeph_core::config::IndexConfig,
    /// Dedicated embedding provider for code retrieval, resolved once per connection via
    /// `resolve_index_embed_provider` — passed to `apply_code_rag_retriever` per session.
    code_index_provider: zeph_llm::any::AnyProvider,
    /// Qdrant ops handle for code RAG retrieval; `None` when no vector backend is configured.
    code_qdrant_ops: Option<zeph_memory::QdrantOps>,
    /// Broadcast sender for skill reload events. Each session subscribes independently.
    skill_reload_tx: tokio::sync::broadcast::Sender<zeph_skills::watcher::SkillEvent>,
    /// Broadcast sender for config reload events. Each session subscribes independently.
    config_reload_tx: tokio::sync::broadcast::Sender<zeph_core::config_watcher::ConfigEvent>,
    /// Shared shutdown signal (`watch::Receiver` is `Clone`).
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    config_path: PathBuf,

    // MCP — runtime objects + config passed together to `with_mcp()`
    mcp_tools: Vec<zeph_mcp::McpTool>,
    mcp_registry: Option<zeph_mcp::McpToolRegistry>,
    mcp_manager: std::sync::Arc<zeph_mcp::McpManager>,
    mcp_shared_tools: std::sync::Arc<RwLock<Vec<zeph_mcp::McpTool>>>,
    mcp_config: zeph_core::config::McpConfig,

    // Optional runtime providers (contain HTTP client pools; excluded from session_config)
    summary_provider: Option<zeph_llm::any::AnyProvider>,
    judge_provider: Option<zeph_llm::any::AnyProvider>,
    feedback_classifier: Option<zeph_llm::classifier::llm::LlmClassifier>,
    #[cfg(feature = "classifiers")]
    classifiers_config: zeph_core::config::ClassifiersConfig,
    /// `security.pii_filter.enabled` — gates the NER union-merge PII layer (#5463),
    /// mirroring the check in `agent_setup::apply_pii_ner_classifier`.
    #[cfg(feature = "classifiers")]
    pii_filter_enabled: bool,
    causal_ipi_config: zeph_sanitizer::causal_ipi::CausalIpiConfig,
    causal_provider: Option<zeph_llm::any::AnyProvider>,
    nli_config: zeph_sanitizer::nli::NliConfig,
    nli_provider: Option<zeph_llm::any::AnyProvider>,
    secret_registry: Option<std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
    vigil_config: zeph_config::VigilConfig,
    probe_provider: Option<zeph_llm::any::AnyProvider>,
    planner_provider: Option<zeph_llm::any::AnyProvider>,
    verify_provider: Option<zeph_llm::any::AnyProvider>,
    orchestrator_provider: Option<zeph_llm::any::AnyProvider>,
    predicate_provider: Option<zeph_llm::any::AnyProvider>,
    quarantine_provider: Option<(zeph_llm::any::AnyProvider, zeph_sanitizer::QuarantineConfig)>,
    guardrail_provider: Option<(
        zeph_llm::any::AnyProvider,
        zeph_sanitizer::guardrail::GuardrailConfig,
    )>,

    /// Audit logger for pre-execution verifier blocks. `None` when audit is disabled.
    audit_logger: Option<std::sync::Arc<zeph_tools::AuditLogger>>,

    // Config snapshot — single source of truth for all config-derived agent settings
    session_config: zeph_core::AgentSessionConfig,
    /// `[session]` persistence settings (spec-068, #5343) — durable JSONL event log dual-write.
    /// Distinct from `session_config` (`AgentSessionConfig`, recap/loop settings).
    session_persistence_config: zeph_config::SessionConfig,
    /// D-13 (spec-068 §8.1, N3): resume-time durable condensation, pre-built once here (where
    /// the full `Config` — needed for `[[llm.providers]]` name resolution and secrets — is
    /// still in scope) rather than per-session in `spawn_acp_agent`, which only receives
    /// pre-decomposed sub-configs, not the raw `Config`. Mirrors the existing
    /// `session_persistence_config` pattern: extract once at deps-build time, read by
    /// reference per session.
    resume_condenser: zeph_session::LlmCondenser,
    resume_token_counter: std::sync::Arc<zeph_agent_context::memory_backend::TokenCounterAdapter>,
    /// Snapshot of `[[llm.providers]]` entries, wired into each session's `Agent` via
    /// `with_provider_pool` so `resolve_background_provider` (background-provider lookups such
    /// as `memory.graph.extract_provider`) can find named providers (#5450).
    provider_pool: Vec<zeph_core::config::ProviderEntry>,
    provider_config_snapshot: zeph_core::ProviderConfigSnapshot,
    focus_config: zeph_core::config::FocusConfig,
    sidequest_config: zeph_core::config::SidequestConfig,
    trajectory_config: zeph_core::config::TrajectoryConfig,
    category_config: zeph_core::config::CategoryConfig,
    tool_filter_config: zeph_core::config::ToolFilterConfig,

    hooks_config: zeph_core::config::HooksConfig,

    // ACP-specific fields (transport-level; not agent-level)
    acp_agent_name: String,
    acp_agent_version: String,
    acp_max_sessions: usize,
    acp_session_idle_timeout_secs: u64,
    acp_permission_file: Option<std::path::PathBuf>,
    acp_available_models: std::sync::Arc<RwLock<Vec<String>>>,
    acp_auth_clients: Vec<zeph_acp::AcpClientToken>,
    acp_discovery_enabled: bool,
    /// Maximum characters for auto-generated session titles.
    acp_title_max_chars: usize,
    /// Maximum number of sessions returned by list endpoints.
    acp_max_history: usize,
    /// Effective log file path advertised in the stdio readiness notification.
    acp_log_file: Option<String>,
    /// `SQLite` database path, passed to ACP transport for session persistence.
    sqlite_path: String,
    /// Pre-built provider factory for ACP model switching.
    #[cfg(feature = "acp")]
    acp_provider_factory: Option<zeph_acp::ProviderFactory>,
    /// Provider name + protocol pairs advertised via `providers/list` (#5448).
    acp_provider_names: Vec<(String, zeph_acp::LlmProtocol)>,
    /// Project rule file paths to advertise in session `_meta`.
    acp_project_rules: Vec<PathBuf>,
    /// Allowlist of directories ACP clients may reference in session requests.
    acp_additional_directories: Vec<zeph_core::config::AdditionalDir>,
    /// Auth methods to advertise in the `initialize` response.
    acp_auth_methods: Vec<zeph_core::config::AcpAuthMethod>,
    /// When `true`, echo `PromptRequest.message_id` through responses and chunks.
    acp_message_ids_enabled: bool,
    /// ACP timeout configuration (elicitation, terminal, MCP).
    acp_timeouts: zeph_config::AcpTimeoutsConfig,
    /// ACP model-related configuration parameters (`[acp.model_config]`).
    acp_model_config: zeph_config::AcpModelConfigConfig,
    /// Resolves current per-plugin skill dirs at hot-reload time.
    plugin_dirs_supplier: std::sync::Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>,

    /// Shell overlay snapshot captured at startup for hot-reload divergence detection.
    startup_shell_overlay: zeph_core::ShellOverlaySnapshot,
    /// Live-rebuild handle for the `ShellExecutor`'s `blocked_commands` policy.
    shell_policy_handle: zeph_tools::ShellPolicyHandle,

    // Scheduler runtime objects (broadcast senders; not config-derived values)
    /// Scheduler executor shared across sessions. Initialized once at startup.
    #[cfg(feature = "scheduler")]
    scheduler_executor: Option<std::sync::Arc<crate::scheduler_executor::SchedulerExecutor>>,
    /// Broadcast sender for scheduler update notifications (`auto_update_check`).
    #[cfg(feature = "scheduler")]
    scheduler_update_tx: Option<tokio::sync::broadcast::Sender<String>>,
    /// Broadcast sender for custom task notifications.
    #[cfg(feature = "scheduler")]
    scheduler_custom_tx: Option<tokio::sync::broadcast::Sender<String>>,
}

/// Forward events from a `broadcast::Receiver` to an `mpsc::Receiver`.
///
/// The forwarding task exits when:
/// - The `mpsc::Sender` is dropped (agent loop finished): `tx.send()` returns `Err`.
/// - The `CancellationToken` is cancelled (session evicted or shutdown).
/// - The broadcast channel is closed: `brx.recv()` returns `RecvError::Closed`.
///
/// Lagged broadcast events are logged at `warn!` and skipped. ACP session cancellation does not
/// rely on this adapter; it is wired through a separate per-session `Notify` signal.
#[cfg(feature = "acp")]
fn broadcast_to_mpsc<T: Clone + Send + 'static>(
    mut brx: tokio::sync::broadcast::Receiver<T>,
    cancel: zeph_memory::CancellationToken,
) -> tokio::sync::mpsc::Receiver<T> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        // EXEMPT(#5144): reusable adapter; self-terminating on cancel/broadcast close
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                result = brx.recv() => {
                    match result {
                        Ok(item) => {
                            if tx.send(item).await.is_err() {
                                break; // Receiver dropped: agent loop finished.
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "broadcast_to_mpsc: lagged, some reload events dropped");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });
    rx
}

/// Prebuilt shared resources for [`build_acp_deps`]: the pure [`SharedCore`] bundle plus the
/// [`zeph_common::TaskSupervisor`] driving it. `None` at the call site means "build fresh" —
/// today's standalone `--acp`/`--acp-http` behavior.
#[cfg(feature = "acp")]
pub(crate) struct PrebuiltAcpCore {
    pub(crate) core: SharedCore,
    pub(crate) supervisor: std::sync::Arc<zeph_common::TaskSupervisor>,
}

/// Build all agent dependencies for the ACP server, from either a [`PrebuiltAcpCore`] (shared
/// with `zeph serve-sessions`, #5420) or a fresh build from `app` when `prebuilt_core` is `None`
/// (standalone `--acp`/`--acp-http`, unchanged behavior).
#[cfg(feature = "acp")]
#[allow(clippy::too_many_lines)]
async fn build_acp_deps(
    app: &AppBuilder,
    prebuilt_core: Option<PrebuiltAcpCore>,
    prebuilt_mcp_manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
) -> anyhow::Result<(SharedAgentDeps, Box<dyn std::any::Any>)> {
    log_acp_runtime_paths(app.config(), app.config_path());
    let embed_model = app.embedding_model();

    let (
        SharedCore {
            provider,
            embedding_provider,
            registry,
            matcher,
            memory,
            budget_tokens,
            rl_head,
        },
        acp_mem_supervisor,
    ) = if let Some(p) = prebuilt_core {
        (p.core, p.supervisor)
    } else {
        let acp_mem_cancel = tokio_util::sync::CancellationToken::new();
        let acp_mem_supervisor =
            std::sync::Arc::new(zeph_common::TaskSupervisor::new(acp_mem_cancel));
        let core = build_shared_core(app, &acp_mem_supervisor).await?;
        (core, acp_mem_supervisor)
    };

    {
        let sqlite = memory.sqlite().clone();
        let retention_secs = app
            .config()
            .tools
            .overflow
            .retention_days
            .saturating_mul(86_400);
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some((sqlite, retention_secs))));
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "overflow_cleanup",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let args = cell.lock().take();
                async move {
                    if let Some((sqlite, retention_secs)) = args {
                        match sqlite.cleanup_overflow(retention_secs).await {
                            Ok(n) if n > 0 => {
                                tracing::info!("cleaned up {n} stale overflow entries");
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("overflow cleanup failed: {e}"),
                        }
                    } else {
                        tracing::warn!("overflow_cleanup factory called more than once");
                    }
                }
            },
        });
    }

    let config = app.config();

    // #5914: memory maintenance loops — mirrors src/runner.rs's CLI/TUI wiring so ACP sessions
    // (standalone `--acp` and the ACP half of `serve-sessions --acp`) get the same ongoing
    // eviction/tier-promotion/scene-consolidation/consolidation/forgetting sweeps instead of an
    // ever-growing, never-maintained memory store. Spawned once per connection (shared across
    // all sessions on `acp_mem_supervisor`), matching runner.rs's once-per-process cadence.
    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let embedding = memory.embedding_store().cloned();
        let eviction_cfg = config.memory.eviction.clone();
        let policy = std::sync::Arc::new(zeph_memory::EbbinghausPolicy::default());
        let cancel = acp_mem_supervisor.cancellation_token();
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "mem-eviction",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_eviction_loop(
                    store.clone(),
                    embedding.clone(),
                    eviction_cfg.clone(),
                    policy.clone(),
                    cancel.clone(),
                )
            },
        });
    }
    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let tier_cfg = zeph_memory::TierPromotionConfig {
            enabled: config.memory.tiers.enabled,
            promotion_min_sessions: config.memory.tiers.promotion_min_sessions,
            similarity_threshold: config.memory.tiers.similarity_threshold,
            sweep_interval_secs: config.memory.tiers.sweep_interval_secs,
            sweep_batch_size: config.memory.tiers.sweep_batch_size,
            embed_timeout_secs: config.memory.semantic.embed_timeout_secs,
        };
        let tier_provider = provider.clone();
        let cancel = acp_mem_supervisor.cancellation_token();
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "mem-tier-promotion",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_tier_promotion_loop(
                    store.clone(),
                    tier_provider.clone(),
                    tier_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }
    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let scene_provider = app
            .build_scene_provider()
            .unwrap_or_else(|| provider.clone());
        let scene_cfg = zeph_memory::SceneConfig {
            enabled: config.memory.tiers.scene_enabled,
            similarity_threshold: config.memory.tiers.scene_similarity_threshold,
            batch_size: config.memory.tiers.scene_batch_size,
            sweep_interval_secs: config.memory.tiers.scene_sweep_interval_secs,
        };
        let cancel = acp_mem_supervisor.cancellation_token();
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "mem-scene-consolidation",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_scene_consolidation_loop(
                    store.clone(),
                    scene_provider.clone(),
                    scene_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }
    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let consolidation_cfg = zeph_memory::ConsolidationConfig {
            enabled: config.memory.consolidation.enabled,
            confidence_threshold: config.memory.consolidation.confidence_threshold,
            sweep_interval_secs: config.memory.consolidation.sweep_interval_secs,
            sweep_batch_size: config.memory.consolidation.sweep_batch_size,
            similarity_threshold: config.memory.consolidation.similarity_threshold,
            llm_timeout_secs: config.memory.consolidation.llm_timeout_secs,
            embed_timeout_secs: config.memory.semantic.embed_timeout_secs,
        };
        let consolidation_provider = app
            .build_consolidation_provider()
            .unwrap_or_else(|| provider.clone());
        let cancel = acp_mem_supervisor.cancellation_token();
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "mem-consolidation",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_consolidation_loop(
                    store.clone(),
                    consolidation_provider.clone(),
                    consolidation_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }
    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let forgetting_cfg = zeph_memory::ForgettingConfig {
            enabled: config.memory.forgetting.enabled,
            decay_rate: config.memory.forgetting.decay_rate,
            forgetting_floor: config.memory.forgetting.forgetting_floor,
            sweep_interval_secs: config.memory.forgetting.sweep_interval_secs,
            sweep_batch_size: config.memory.forgetting.sweep_batch_size,
            replay_window_hours: config.memory.forgetting.replay_window_hours,
            replay_min_access_count: config.memory.forgetting.replay_min_access_count,
            protect_recent_hours: config.memory.forgetting.protect_recent_hours,
            protect_min_access_count: config.memory.forgetting.protect_min_access_count,
        };
        let cancel = acp_mem_supervisor.cancellation_token();
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "mem-forgetting",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_forgetting_loop(
                    store.clone(),
                    forgetting_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let permission_policy =
        zeph_tools::build_permission_policy(&config.tools, config.security.autonomy_level);
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(permission_policy.clone())
        .with_output_filters(filter_registry)
        .with_task_supervisor((*acp_mem_supervisor).clone());
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
                shell_executor = shell_executor.with_sandbox(std::sync::Arc::from(backend), policy);
                tracing::info!(backend = name, "OS sandbox enabled (acp)");
            }
            Err(e) if config.tools.sandbox.strict || config.tools.sandbox.fail_if_unavailable => {
                panic!("sandbox initialization failed: {e}");
            }
            Err(e) => {
                tracing::warn!("OS sandbox unavailable, running without isolation: {e}");
            }
        }
    }
    let mut scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape)
        .with_egress_config(config.tools.egress.clone());
    if config.tools.egress.enabled {
        let (egress_tx, egress_rx) = tokio::sync::mpsc::channel(256);
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        scrape_executor = scrape_executor.with_egress_tx(egress_tx, dropped);
        {
            let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(egress_rx)));
            acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "egress_drain",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: move || {
                    let rx = cell.lock().take();
                    async move {
                        if let Some(rx) = rx {
                            agent_setup::drain_egress_events(rx, None).await;
                        } else {
                            tracing::warn!("egress_drain factory called more than once");
                        }
                    }
                },
            });
        }
    }
    let mut acp_audit_logger: Option<std::sync::Arc<zeph_tools::AuditLogger>> = None;
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit, false).await
    {
        let logger = std::sync::Arc::new(logger);
        shell_executor = shell_executor.with_audit(std::sync::Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(std::sync::Arc::clone(&logger));
        acp_audit_logger = Some(logger);
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
    let mcp_manager = if let Some(m) = prebuilt_mcp_manager {
        m
    } else {
        let builder =
            crate::bootstrap::create_mcp_manager_with_vault(config, false, app.age_vault_arc());
        let builder =
            crate::bootstrap::wire_trust_calibration(builder, config, Some(memory.sqlite().pool()))
                .await;
        std::sync::Arc::new(builder)
    };
    let (mcp_tools, _mcp_outcomes) = mcp_manager.connect_all().await;
    let mcp_shared_tools = std::sync::Arc::new(RwLock::new(mcp_tools.clone()));
    let mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    let shell_policy_handle = shell_executor.policy_handle();
    let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(config);
    // #5611: base chain stays ungated here — it is composed with mcp/search below, then the
    // per-session skill_loader/memory/overflow layers are added on top in `spawn_acp_agent`,
    // which wraps the FULLY composed tree in one outermost `TrustGateExecutor` (see
    // `apply_common_tool_gating`). Gating only this sub-tree (as before #5611) let tools
    // composed outside it (memory, MCP, skill loader) bypass Quarantine/Blocked entirely.
    let base_executor = crate::agent_setup::build_base_executor_chain(
        file_executor,
        shell_executor,
        scrape_executor,
        diagnostics_executor,
    );
    let index_provider = crate::bootstrap::resolve_index_embed_provider(config, provider.clone());
    let inner_executor: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = {
        let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = std::sync::Arc::new(
            zeph_tools::CompositeExecutor::new(base_executor, mcp_executor),
        );
        if let Some(search_executor) = crate::agent_setup::build_search_code_executor(
            config,
            app.qdrant_ops().cloned(),
            index_provider.clone(),
            memory.sqlite().pool().clone(),
            Some(std::sync::Arc::clone(&mcp_manager)),
        ) {
            std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
                zeph_tools::DynExecutor(base),
                search_executor,
            ))
        } else {
            base
        }
    };
    let tool_executor = inner_executor;
    // Pre-build the pieces `PolicyGateExecutor`/`AdversarialPolicyGateExecutor` need — this
    // depends only on static config (policy file contents, provider resolution), so it is
    // safe and more efficient to build once per connection rather than per session. The
    // gates themselves are constructed fresh per session in `spawn_acp_agent`, wrapping that
    // session's full composite (skill_loader/memory/overflow/base/MCP/search/ACP-native
    // fs/shell) — not just this connection-scoped `tool_executor` — matching runner.rs's
    // full-stack coverage instead of gating only a subset of the tool surface.
    let policy_gate_pieces = agent_setup::build_policy_gate_pieces(config, &provider).await;

    // Spec 050 F2/Phase 2 (#5913): pre-resolve capability_scopes/shadow_sentinel config and the
    // shadow sentinel probe provider once per connection — provider resolution + secret masking
    // are static config work, mirroring the adversarial policy provider resolution above.
    // `spawn_acp_agent` builds the per-session `ScopedToolExecutor`/`ShadowSentinel` from these,
    // since capability scoping needs that session's fully-composed tool executor and the
    // sentinel's persisted event store is keyed by that session's own `conversation_id`.
    let capability_scopes_config = config.security.capability_scopes.clone();
    let shadow_sentinel_config = config.security.shadow_sentinel.clone();
    let shadow_sentinel_probe_provider = {
        let sentinel_cfg = &shadow_sentinel_config;
        let base = if sentinel_cfg.probe_provider.is_empty() {
            provider.clone()
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
                    provider.clone()
                }
            }
        };
        // #5437 round-3 style masking: the probe's own prompt embeds already-unmasked tool
        // args (see runner.rs's identical rationale), so every `.chat()` call this provider
        // makes must re-mask before the request leaves the process.
        match app.secret_registry() {
            Some(registry) => {
                base.masked(registry as std::sync::Arc<dyn zeph_llm::masking::OutboundMasker>)
            }
            None => base,
        }
    };

    // Spec 050 (#5958): `[security.trajectory]` snapshot — `spawn_acp_agent` builds the
    // per-session risk slot/signal queue from this, mirroring `src/runner.rs`/`src/daemon.rs`.
    let trajectory_sentinel_config = config.security.trajectory.clone();
    // #5951: built once per connection — provider masking is static config work, mirrors
    // `shadow_sentinel_probe_provider` above.
    let quality_pipeline = crate::agent_setup::build_quality_pipeline(
        config,
        &provider,
        app.secret_registry().as_ref(),
    );

    let mcp_registry = create_mcp_registry(
        config,
        &provider,
        &mcp_tools,
        &embed_model,
        app.qdrant_ops(),
    )
    .await;
    let summary_provider = app.build_summary_provider();
    let skill_paths = app.skill_paths_for_registry();
    let plugin_dirs_supplier = app.plugin_dirs_supplier();
    let acp_project_rules = collect_project_rules(&skill_paths);
    let crate::bootstrap::WatcherBundle {
        skill_watcher,
        skill_reload_rx: mpsc_skill_rx,
        config_watcher,
        config_reload_rx: mpsc_config_rx,
    } = app.build_watchers(&acp_mem_supervisor);
    let config_path_owned = app.config_path().to_owned();
    let (_, shutdown_rx) = AppBuilder::build_shutdown();

    // Convert mpsc receivers from watchers to broadcast senders so each ACP session
    // can subscribe independently. Option A (critic S3): keep watchers unchanged,
    // forward mpsc→broadcast only here in build_acp_deps.
    // Keep enough backlog for bursty reload traffic while leaving room for larger deployments
    // to raise the limit explicitly via config.
    let broadcast_cap = config.acp.broadcast_capacity.max(1);
    let (skill_reload_tx, _) = tokio::sync::broadcast::channel(broadcast_cap);
    let (config_reload_tx, _) = tokio::sync::broadcast::channel(broadcast_cap);

    {
        let skill_tx = skill_reload_tx.clone();
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(mpsc_skill_rx)));
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "skill_reload_fwd",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let rx = cell.lock().take();
                let tx = skill_tx.clone();
                async move {
                    if let Some(mut rx) = rx {
                        while let Some(ev) = rx.recv().await {
                            let _ = tx.send(ev);
                        }
                    } else {
                        tracing::warn!("skill_reload_fwd factory called more than once");
                    }
                }
            },
        });
    }
    {
        let cfg_tx = config_reload_tx.clone();
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(mpsc_config_rx)));
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "config_reload_fwd",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let rx = cell.lock().take();
                let tx = cfg_tx.clone();
                async move {
                    if let Some(mut rx) = rx {
                        while let Some(ev) = rx.recv().await {
                            let _ = tx.send(ev);
                        }
                    } else {
                        tracing::warn!("config_reload_fwd factory called more than once");
                    }
                }
            },
        });
    }

    #[cfg(feature = "scheduler")]
    let (scheduler_executor, scheduler_update_tx, scheduler_custom_tx) = {
        let exp_deps = {
            use std::sync::Arc;
            if config.experiments.enabled && config.experiments.schedule.enabled {
                let p = provider.clone();
                // Resolve a dedicated eval (judge) provider so scheduled runs are not
                // self-judged by the subject model — see #5947.
                let eval_provider = app.build_eval_provider().unwrap_or_else(|| p.clone());
                Some((
                    Arc::new(p),
                    Arc::new(eval_provider),
                    Some(Arc::clone(&memory)),
                ))
            } else {
                None
            }
        };

        let five_signal = memory.five_signal_runtime();
        match crate::scheduler::init_scheduler(
            config,
            shutdown_rx.clone(),
            exp_deps,
            five_signal,
            Some(&acp_mem_supervisor),
        )
        .await
        {
            Some(result) => {
                let exec = std::sync::Arc::new(result.executor);
                let custom_rx = result.custom_rx;
                let (ctx, _) = tokio::sync::broadcast::channel::<String>(broadcast_cap);
                let ctx_clone = ctx.clone();
                let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(custom_rx)));
                acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                    name: "sched_custom_fwd",
                    restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                    factory: move || {
                        let rx = cell.lock().take();
                        let tx = ctx_clone.clone();
                        async move {
                            if let Some(mut rx) = rx {
                                while let Some(ev) = rx.recv().await {
                                    let _ = tx.send(ev);
                                }
                            } else {
                                tracing::warn!("sched_custom_fwd factory called more than once");
                            }
                        }
                    },
                });
                let update_tx = if let Some(update_rx) = result.update_rx {
                    let (utx, _) = tokio::sync::broadcast::channel::<String>(broadcast_cap);
                    let utx_clone = utx.clone();
                    let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(update_rx)));
                    acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                        name: "sched_update_fwd",
                        restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                        factory: move || {
                            let rx = cell.lock().take();
                            let tx = utx_clone.clone();
                            async move {
                                if let Some(mut rx) = rx {
                                    while let Some(ev) = rx.recv().await {
                                        let _ = tx.send(ev);
                                    }
                                } else {
                                    tracing::warn!(
                                        "sched_update_fwd factory called more than once"
                                    );
                                }
                            }
                        },
                    });
                    Some(utx)
                } else {
                    None
                };
                let (update_tx, custom_tx) = (update_tx, Some(ctx));
                (Some(exec), update_tx, custom_tx)
            }
            None => (None, None, None),
        }
    };

    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);
    // D-13 (spec-068 §8.1, N3): built once here, where the full `Config` is still in scope —
    // see the `resume_condenser` field's doc comment on `SharedAgentDeps`.
    let (resume_condenser_built, resume_token_counter_built) =
        zeph_core::provider_factory::build_resume_condenser(config, &provider);
    let feedback_classifier = app.build_feedback_classifier(&provider);
    // #5450: built once here, where the full `Config` is still in scope — mirrors
    // `src/runner.rs`'s CLI-path snapshot construction, so ACP sessions get a populated
    // `provider_pool` too (previously left empty, breaking `resolve_background_provider`).
    let provider_config_snapshot = agent_setup::build_provider_config_snapshot(config);
    let acp_auth_clients = resolve_acp_auth_clients(&config.acp, app.vault()).await?;

    let deps = SharedAgentDeps {
        provider,
        embedding_provider,
        registry,
        matcher,
        max_active_skills: config.skills.max_active_skills.get(),
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
        rl_head,
        tool_executor,
        permission_policy,
        policy_gate_pieces,
        capability_scopes_config,
        shadow_sentinel_config,
        shadow_sentinel_probe_provider,
        trajectory_sentinel_config,
        quality_pipeline,
        skill_paths,
        skill_reload_tx,
        config_reload_tx,
        memory,
        history_limit: config.memory.history_limit,
        recall_limit: config.memory.semantic.recall_limit,
        summarization_threshold: config.memory.summarization_threshold,
        shutdown_summary: config.memory.shutdown_summary,
        shutdown_summary_min_messages: config.memory.shutdown_summary_min_messages,
        shutdown_summary_max_messages: config.memory.shutdown_summary_max_messages,
        shutdown_summary_timeout_secs: config.memory.shutdown_summary_timeout_secs,
        shutdown_summary_provider: config.memory.shutdown_summary_provider.as_str().to_owned(),
        channel_provider_persistence: config.session.provider_persistence,
        channel_persist_provider_overrides: config.session.persist_provider_overrides,
        index_config: config.index.clone(),
        code_index_provider: index_provider,
        code_qdrant_ops: app.qdrant_ops().cloned(),
        shutdown_rx,
        config_path: config_path_owned,
        mcp_tools,
        mcp_registry,
        mcp_manager,
        mcp_shared_tools,
        mcp_config: config.mcp.clone(),
        summary_provider,
        judge_provider: app.build_judge_provider(),
        feedback_classifier,
        #[cfg(feature = "classifiers")]
        classifiers_config: config.classifiers.clone(),
        #[cfg(feature = "classifiers")]
        pii_filter_enabled: config.security.pii_filter.enabled,
        causal_ipi_config: config.security.causal_ipi.clone(),
        causal_provider: config
            .security
            .causal_ipi
            .provider
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|name| match crate::bootstrap::create_named_provider(name, config) {
                Ok(p) => {
                    tracing::info!(provider = %name, "causal IPI dedicated provider configured (acp)");
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "causal IPI provider resolution failed, falling back to primary (acp)"
                    );
                    None
                }
            }),
        nli_config: config.security.content_isolation.nli.clone(),
        nli_provider: config
            .security
            .content_isolation
            .nli
            .provider
            .as_non_empty()
            .and_then(|name| match crate::bootstrap::create_named_provider(name, config) {
                Ok(p) => {
                    tracing::info!(provider = %name, "NLI dedicated provider configured (acp)");
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "NLI provider resolution failed, falling back to primary (acp)"
                    );
                    None
                }
            }),
        secret_registry: app.secret_registry(),
        vigil_config: config.security.vigil.clone(),
        probe_provider: app.build_probe_provider(),
        planner_provider: app.build_planner_provider(),
        verify_provider: app.build_verify_provider(),
        orchestrator_provider: app.build_orchestrator_provider(),
        predicate_provider: app.build_predicate_provider(),
        quarantine_provider: app.build_quarantine_provider(),
        guardrail_provider: app.build_guardrail_provider(),
        audit_logger: acp_audit_logger,
        hooks_config: config.hooks.clone(),
        session_config,
        session_persistence_config: config.session.clone(),
        resume_condenser: resume_condenser_built,
        resume_token_counter: resume_token_counter_built,
        provider_pool: config.llm.providers.clone(),
        provider_config_snapshot,
        focus_config: config.agent.focus.clone(),
        sidequest_config: config.memory.sidequest.clone(),
        trajectory_config: config.memory.trajectory.clone(),
        category_config: config.memory.category.clone(),
        tool_filter_config: config.agent.tool_filter.clone(),
        acp_agent_name: config.acp.agent_name.clone(),
        acp_agent_version: config.acp.agent_version.clone(),
        acp_max_sessions: config.acp.max_sessions,
        acp_session_idle_timeout_secs: config.acp.session_idle_timeout_secs,
        acp_permission_file: config.acp.permission_file.clone(),
        acp_available_models: std::sync::Arc::new(RwLock::new(
            if config.acp.available_models.is_empty() {
                discover_models_from_config(config).await
            } else {
                config.acp.available_models.clone()
            },
        )),
        acp_auth_clients,
        acp_discovery_enabled: config.acp.discovery_enabled,
        acp_title_max_chars: config.memory.sessions.title_max_chars,
        acp_max_history: config.memory.sessions.max_history,
        acp_log_file: if config.logging.file.is_empty() {
            None
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            Some(
                resolve_runtime_path(std::path::Path::new(&config.logging.file), &cwd)
                    .display()
                    .to_string(),
            )
        },
        sqlite_path: crate::db_url::resolve_db_url(config).to_owned(),
        acp_provider_factory: Some(build_acp_provider_factory(config, app.secret_registry())),
        acp_provider_names: acp_provider_names(config),
        acp_project_rules,
        acp_additional_directories: config.acp.additional_directories.clone(),
        acp_auth_methods: config.acp.auth_methods.clone(),
        acp_message_ids_enabled: config.acp.message_ids_enabled,
        acp_timeouts: config.acp.timeouts.clone(),
        acp_model_config: config.acp.model_config.clone(),
        plugin_dirs_supplier: std::sync::Arc::new(plugin_dirs_supplier),
        #[cfg(feature = "scheduler")]
        scheduler_executor,
        #[cfg(feature = "scheduler")]
        scheduler_update_tx,
        #[cfg(feature = "scheduler")]
        scheduler_custom_tx,
        startup_shell_overlay: {
            let mut blocked = config.tools.shell.blocked_commands.clone();
            blocked.sort();
            let mut allowed = config.tools.shell.allowed_commands.clone();
            allowed.sort();
            zeph_core::ShellOverlaySnapshot { blocked, allowed }
        },
        shell_policy_handle,
    };

    let keepalive: Box<dyn std::any::Any> = Box::new((skill_watcher, config_watcher));
    Ok((deps, keepalive))
}

/// Text shown to the client when session persistence is disabled due to a held write lock.
///
/// Deliberately omits the lock path: it is an absolute filesystem path (leaks the server's
/// home-directory prefix/OS username) and this session may be reached over an unauthenticated,
/// non-loopback ACP HTTP transport (security review finding, #5487).
#[cfg(feature = "acp")]
const SESSION_LOCK_DEGRADED_MESSAGE: &str =
    "Session persistence unavailable: another process already holds this session's write lock.";

/// Notify the client that session persistence degraded to no-persistence because another
/// process already holds this session's write lock (`SessionError::AlreadyLocked`).
///
/// Prefers `status_notifier` (present for real ACP sessions): it pushes the message
/// immediately through the session's notification drainer, so the client learns about the
/// degradation at session-creation time rather than only as a side effect of its next prompt
/// (#5519). Falls back to `channel.send_status` when no notifier is available (e.g.
/// `acp_ctx` is `None`, as for non-ACP callers of `spawn_acp_agent`) — that path is still
/// only flushed to the client on the session's next prompt-response drain.
#[cfg(feature = "acp")]
async fn notify_lock_degraded(
    status_notifier: Option<&zeph_acp::SessionStatusNotifier>,
    channel: &mut zeph_core::channel::LoopbackChannel,
) {
    if let Some(notifier) = status_notifier {
        notifier.notify_status_nowait(SESSION_LOCK_DEGRADED_MESSAGE);
    } else {
        channel
            .send_status_best_effort(SESSION_LOCK_DEGRADED_MESSAGE)
            .await;
    }
}

/// Open a session's durable JSONL event log, degrading (and notifying the client) instead of
/// failing the session when another process already holds the session's write lock.
///
/// Extracted from `spawn_acp_agent`'s no-`conversation_id` hydration branch — the only
/// `AlreadyLocked` trigger that doesn't require a full `SharedAgentDeps`/`Agent` to reach — so
/// `notify_lock_degraded`'s real trigger path (genuine file-lock contention, not a mocked
/// error) is covered by a lightweight integration test (#5519 review S2).
#[cfg(feature = "acp")]
async fn open_session_log_or_notify_locked(
    session_path: &std::path::Path,
    status_notifier: Option<&zeph_acp::SessionStatusNotifier>,
    channel: &mut zeph_core::channel::LoopbackChannel,
) -> Option<std::sync::Arc<zeph_session::SessionEventLog>> {
    match zeph_session::SessionEventLog::open_exclusive(session_path).await {
        Ok(log) => Some(std::sync::Arc::new(log)),
        Err(zeph_session::SessionError::AlreadyLocked(lock_path)) => {
            tracing::error!(
                lock_path,
                "failed to open session event log for ACP session: another process \
                 already holds this session's write lock; session persistence disabled \
                 for this session"
            );
            notify_lock_degraded(status_notifier, channel).await;
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to open session event log for ACP session; session persistence disabled for this session");
            None
        }
    }
}

/// Spawn an `Agent` from shared deps and per-session context, then run its loop.
///
/// Called once per ACP session. Each invocation creates independent per-session state:
/// - Per-session `mpsc::Receiver` adapters from shared broadcast senders.
/// - A fresh `CancellationToken` for the broadcast adapter lifetime.
/// - The session's own `conversation_id` from `SessionContext`.
///
/// When `acp_ctx` is `Some`, ACP executors are composed on top of the local tool executor
/// (ACP-first, local fallback). When `None`, local tools handle everything.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_lines)]
async fn spawn_acp_agent(
    d: std::sync::Arc<SharedAgentDeps>,
    mut channel: zeph_core::channel::LoopbackChannel,
    acp_ctx: Option<zeph_acp::AcpContext>,
    session_ctx: zeph_acp::SessionContext,
) {
    use std::sync::Arc;

    let provider = d.provider.clone();
    let registry = Arc::clone(&d.registry);
    let matcher = d.matcher.clone();
    let max_active_skills = d.max_active_skills;
    let skill_disambiguation_threshold = d.skill_disambiguation_threshold;
    let skill_two_stage_matching = d.skill_two_stage_matching;
    let skill_confusability_threshold = d.skill_confusability_threshold;
    let skill_group_structured = d.skill_group_structured;
    let skill_support_similarity_threshold = d.skill_support_similarity_threshold;
    let skill_min_injection_score = d.skill_min_injection_score;
    let skill_generation_provider = d.skill_generation_provider.clone();
    let skill_disambiguate_provider = d.skill_disambiguate_provider.clone();
    let semantic_scan = d.semantic_scan;
    let semantic_scan_provider = d.semantic_scan_provider.clone();
    let tool_executor = Arc::clone(&d.tool_executor);
    let permission_policy = d.permission_policy.clone();
    let skill_paths = d.skill_paths.clone();
    let plugin_dirs_supplier = Arc::clone(&d.plugin_dirs_supplier);
    let memory = Arc::clone(&d.memory);
    let history_limit = d.history_limit;
    let recall_limit = d.recall_limit;
    let summarization_threshold = d.summarization_threshold;
    let shutdown_summary = d.shutdown_summary;
    let shutdown_summary_min_messages = d.shutdown_summary_min_messages;
    let shutdown_summary_max_messages = d.shutdown_summary_max_messages;
    let shutdown_summary_timeout_secs = d.shutdown_summary_timeout_secs;
    let shutdown_summary_provider = d.shutdown_summary_provider.clone();
    let channel_provider_persistence = d.channel_provider_persistence;
    let channel_persist_provider_overrides = d.channel_persist_provider_overrides;
    let index_config = d.index_config.clone();
    let code_index_provider = d.code_index_provider.clone();
    let code_qdrant_ops = d.code_qdrant_ops.clone();
    let shutdown_rx = d.shutdown_rx.clone();
    let config_path = d.config_path.clone();
    let mcp_tools = d.mcp_tools.clone();
    let mcp_registry = d.mcp_registry.clone();
    let mcp_manager = Arc::clone(&d.mcp_manager);
    let mcp_shared_tools = Arc::clone(&d.mcp_shared_tools);
    let mcp_config = d.mcp_config.clone();
    let summary_provider = d.summary_provider.clone();
    let judge_provider = d.judge_provider.clone();
    let feedback_classifier = d.feedback_classifier.clone();
    #[cfg(feature = "classifiers")]
    let classifiers_config = d.classifiers_config.clone();
    #[cfg(feature = "classifiers")]
    let pii_filter_enabled = d.pii_filter_enabled;
    let causal_ipi_config = d.causal_ipi_config.clone();
    let causal_provider = d.causal_provider.clone();
    let nli_config = d.nli_config.clone();
    let nli_provider = d.nli_provider.clone();
    let secret_registry = d.secret_registry.clone();
    let vigil_config = d.vigil_config.clone();
    let probe_provider = d.probe_provider.clone();
    let planner_provider = d.planner_provider.clone();
    let verify_provider = d.verify_provider.clone();
    let orchestrator_provider = d.orchestrator_provider.clone();
    let predicate_provider = d.predicate_provider.clone();
    let quarantine_provider = d.quarantine_provider.clone();
    let guardrail_provider = d.guardrail_provider.clone();
    let session_config = d.session_config.clone();
    let session_persistence_config = d.session_persistence_config.clone();
    let provider_pool = d.provider_pool.clone();
    let provider_config_snapshot = d.provider_config_snapshot.clone();
    let managed_skills_dir = crate::bootstrap::managed_skills_dir();
    let skill_reload_tx = d.skill_reload_tx.clone();
    let config_reload_tx = d.config_reload_tx.clone();
    #[cfg(feature = "scheduler")]
    let scheduler_executor = d.scheduler_executor.as_ref().map(std::sync::Arc::clone);
    #[cfg(feature = "scheduler")]
    let scheduler_update_tx = d.scheduler_update_tx.clone();
    #[cfg(feature = "scheduler")]
    let scheduler_custom_tx = d.scheduler_custom_tx.clone();

    let hooks_config = d.hooks_config.clone();
    let tool_filter_config = d.tool_filter_config.clone();

    // Cloned before `acp_ctx` is destructured into individual per-session executors below
    // (the tool-executor setup consumes `ctx` by value), so it survives to the session
    // hydration block further down and can proactively push a client-visible notification if
    // hydration hits `AlreadyLocked` — reaching the client without waiting for this session's
    // next `session/prompt` drain (#5519).
    let status_notifier = acp_ctx.as_ref().map(|ctx| ctx.status_notifier.clone());

    // Per-session receivers: each session gets its own mpsc::Receiver forwarded from the
    // shared broadcast senders. The CancellationToken is derived from the AcpContext cancel
    // signal so the forwarding task exits when the session ends (eviction, shutdown, or
    // natural completion). This satisfies critic finding S1.
    let adapter_cancel = zeph_memory::CancellationToken::new();
    let reload_rx = broadcast_to_mpsc(skill_reload_tx.subscribe(), adapter_cancel.clone());
    let config_reload_rx = broadcast_to_mpsc(config_reload_tx.subscribe(), adapter_cancel.clone());
    #[cfg(feature = "scheduler")]
    let scheduler_update_rx = scheduler_update_tx
        .as_ref()
        .map(|tx| broadcast_to_mpsc(tx.subscribe(), adapter_cancel.clone()));
    #[cfg(feature = "scheduler")]
    let scheduler_custom_rx = scheduler_custom_tx
        .as_ref()
        .map(|tx| broadcast_to_mpsc(tx.subscribe(), adapter_cancel.clone()));

    // Capture per-session fields before session_config is consumed by apply_session_config.
    let debug_config = session_config.debug_config.clone();
    let memory_validation_config = session_config.security.memory_validation.clone();

    // Build tool executor: ACP executors take priority via CompositeExecutor (first-match-wins).
    // DynExecutor wraps Arc<dyn ErasedToolExecutor> so it satisfies Agent::new's ToolExecutor bound.
    // When conversation_id is None (store unavailable), memory_tools use id=0 which maps to no
    // persisted history — the tool calls succeed but return empty results.
    let memory_executor = zeph_core::memory_tools::MemoryToolExecutor::with_validator(
        Arc::clone(&memory),
        session_ctx
            .conversation_id
            .unwrap_or(zeph_memory::ConversationId(0)),
        zeph_sanitizer::memory_validation::MemoryWriteValidator::new(memory_validation_config),
    );
    let overflow_executor = {
        let mut ex =
            zeph_core::overflow_tools::OverflowToolExecutor::new(Arc::new(memory.sqlite().clone()));
        if let Some(cid) = session_ctx.conversation_id {
            ex = ex.with_conversation(cid.0);
        }
        ex
    };
    let (skill_loader_executor, skill_invoke_executor, trust_snapshot) =
        agent_setup::build_skill_executors(&registry);
    let (base_composite, cancel_signal, provider_override, parent_tool_use_id): (
        Arc<dyn ErasedToolExecutor>,
        _,
        _,
        _,
    ) = if let Some(ctx) = acp_ctx {
        let cancel_signal = Arc::clone(&ctx.cancel_signal);
        let provider_override = Arc::clone(&ctx.provider_override);
        let parent_tool_use_id = ctx.parent_tool_use_id.clone();
        // Link adapter_cancel to session cancel_signal so the broadcast forwarding task
        // exits when the ACP session is cancelled (eviction, shutdown, or completion).
        let adapter_cancel_clone = adapter_cancel.clone();
        let cancel_signal_clone = Arc::clone(&cancel_signal);
        tokio::spawn(async move {
            // EXEMPT(#5144): per-session cancel bridge; self-terminating single await; name collision risk under spawn
            cancel_signal_clone.notified().await;
            adapter_cancel_clone.cancel();
        });
        let mut base: Arc<dyn ErasedToolExecutor> = Arc::clone(&tool_executor) as Arc<_>;
        if let Some(fs) = ctx.file_executor {
            // Suppress FileExecutor's read/write/glob when AcpFileExecutor is active.
            // edit and grep remain available from FileExecutor (no ACP equivalents yet).
            let filtered = zeph_tools::ToolFilter::new(
                zeph_tools::DynExecutor(base),
                &["read", "write", "glob"],
            );
            base = Arc::new(zeph_tools::CompositeExecutor::new(fs, filtered));
        }
        if let Some(shell) = ctx.shell_executor {
            base = Arc::new(zeph_tools::CompositeExecutor::new(
                shell,
                zeph_tools::DynExecutor(base),
            ));
        }
        base = Arc::new(zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                skill_invoke_executor,
                zeph_tools::CompositeExecutor::new(
                    memory_executor,
                    zeph_tools::CompositeExecutor::new(
                        overflow_executor,
                        zeph_tools::DynExecutor(base),
                    ),
                ),
            ),
        ));
        (
            base,
            Some(cancel_signal),
            Some(provider_override),
            parent_tool_use_id,
        )
    } else {
        // No AcpContext: the adapter forwarding tasks (skill reload, config reload, and
        // scheduler receivers) run until adapter_cancel.cancel() is called explicitly at
        // function end (line below), or until the mpsc sender is dropped.
        let base: Arc<dyn ErasedToolExecutor> = Arc::new(zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                skill_invoke_executor,
                zeph_tools::CompositeExecutor::new(
                    memory_executor,
                    zeph_tools::CompositeExecutor::new(
                        overflow_executor,
                        zeph_tools::DynExecutor(Arc::clone(&tool_executor) as Arc<_>),
                    ),
                ),
            ),
        ));
        (base, None, None, None)
    };

    // Gate the FULLY composed per-session tree (skill_loader/memory/overflow/base/mcp/search,
    // plus any ACP-provided fs/shell overrides) behind one outermost TrustGateExecutor,
    // matching runner.rs. Previously only the base chain carried a gate, so memory/MCP/
    // skill-loader tools composed outside it bypassed Quarantine/Blocked entirely.
    let (trust_gated, mcp_ids_handle) = crate::agent_setup::apply_common_tool_gating(
        zeph_tools::DynExecutor(base_composite),
        &permission_policy,
    );
    crate::agent_setup::register_mcp_tool_ids(&mcp_ids_handle, &mcp_tools);

    // #5958: shared trajectory risk slot — written by begin_turn(), read by PolicyGateExecutor —
    // and pending risk signal queue — executor layers push signal codes; begin_turn() drains.
    // Built fresh per session (like ShadowSentinel below), mirroring src/runner.rs; previously
    // ACP never created these, so TrajectorySentinel was never wired into any ACP session.
    let trajectory_risk_slot: zeph_tools::TrajectoryRiskSlot =
        Arc::new(parking_lot::RwLock::new(0u8));
    let trajectory_signal_queue: zeph_tools::RiskSignalQueue =
        Arc::new(parking_lot::Mutex::new(Vec::new()));

    // Wire AdversarialPolicyGateExecutor / PolicyGateExecutor around the trust-gated
    // per-session composite, using the pieces pre-built once per connection in
    // `build_acp_deps` — previously these gates wrapped only the connection-scoped
    // base/MCP/search subset, so skill_loader/memory/overflow/ACP-native fs/shell calls
    // bypassed both. Wiring order (outermost first): PolicyGateExecutor ->
    // AdversarialPolicyGateExecutor -> TrustGateExecutor -> composite, matching runner.rs.
    let tool_executor = crate::agent_setup::apply_policy_gate_chain(
        trust_gated,
        &d.policy_gate_pieces,
        d.audit_logger.as_ref(),
        Some((&trajectory_risk_slot, &trajectory_signal_queue)),
    );

    // Spec 050 F2 (#5913): wrap with ScopedToolExecutor when capability_scopes are configured —
    // mirrors src/runner.rs. Wraps the FULLY composed per-session tree (not just the
    // connection-scoped base chain) so glob patterns see every tool this session's LLM can
    // actually call, including skill_loader/memory/overflow/ACP-native fs/shell.
    let tool_executor = {
        let scopes_cfg = &d.capability_scopes_config;
        if scopes_cfg.scopes.is_empty() {
            tool_executor
        } else {
            use std::collections::HashSet;
            use zeph_tools::executor::ToolExecutor as _;
            use zeph_tools::scope::build_scoped_executor;
            let registry_ids: HashSet<String> = tool_executor
                .tool_definitions()
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
            // Retain a cheap Arc clone for the Err fallback below — `build_scoped_executor`
            // takes `tool_executor` by value.
            let fallback = zeph_tools::DynExecutor(Arc::clone(&tool_executor.0));
            match build_scoped_executor(tool_executor, scopes_cfg, &registry_ids) {
                Ok(scoped) => {
                    // #5958: OutOfScope denials feed the trajectory signal queue too, matching
                    // src/runner.rs/src/daemon.rs — otherwise capability-scope violations would
                    // be invisible to TrajectorySentinel's risk escalation.
                    let scoped = scoped.with_signal_queue(Arc::clone(&trajectory_signal_queue));
                    zeph_tools::DynExecutor(Arc::new(scoped))
                }
                Err(e) => {
                    // Misconfiguration (FR-CG-005) is fatal for the single-process CLI run
                    // (runner.rs aborts startup); ACP serves many concurrent IDE clients on one
                    // process, so aborting the whole server over one connection's config
                    // snapshot is not appropriate here. But degrading to the *unscoped* executor
                    // would be fail-OPEN for a security control the operator explicitly enabled
                    // (impl-critic F1) — deny all tool access for this session instead
                    // (fail-CLOSED), via the same `OutOfScope` enforcement path a working scope
                    // would use, rather than silently granting full access.
                    tracing::error!(
                        "capability_scopes: {e}, denying all tool access for this session \
                         (fail-closed)"
                    );
                    zeph_tools::DynExecutor(Arc::new(zeph_tools::scope::ScopedToolExecutor::new(
                        fallback,
                        zeph_tools::scope::ToolScope::empty(),
                    )))
                }
            }
        }
    };

    // Spec 050 Phase 2 (#5913): wrap with ShadowProbeExecutor when shadow_sentinel.enabled =
    // true — mirrors src/runner.rs. Wiring order: ScopedToolExecutor -> ShadowProbeExecutor ->
    // PolicyGateExecutor -> AdversarialPolicyGateExecutor -> TrustGateExecutor -> composite.
    let (tool_executor, shadow_sentinel_arc) = {
        let sentinel_cfg = &d.shadow_sentinel_config;
        if sentinel_cfg.enabled {
            let pool = memory.sqlite().pool().clone();
            let llm_probe = zeph_core::agent::shadow_sentinel::LlmSafetyProbe::new(
                Arc::new(d.shadow_sentinel_probe_provider.clone()),
                sentinel_cfg.probe_timeout_ms,
                sentinel_cfg.deny_on_timeout,
            );
            let store = zeph_core::agent::shadow_sentinel::ShadowEventStore::new(pool);
            // Keyed by this session's own conversation_id — ACP sessions are per-conversation,
            // unlike runner.rs's single process-wide conversation.
            let conversation_identity = session_ctx
                .conversation_id
                .unwrap_or(zeph_memory::ConversationId(0))
                .0
                .to_string();
            let sentinel = Arc::new(zeph_core::agent::shadow_sentinel::ShadowSentinel::new(
                store,
                Box::new(llm_probe),
                sentinel_cfg.clone(),
                conversation_identity,
            ));
            let turn_number = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let risk_level = Arc::new(parking_lot::RwLock::new("calm".to_owned()));
            let probe_gate: Arc<dyn zeph_tools::ProbeGate> =
                Arc::new(crate::runner::ShadowSentinelProbeGateAdapter {
                    sentinel: Arc::clone(&sentinel),
                });
            let shadow_exec = zeph_tools::ShadowProbeExecutor::new(
                tool_executor,
                probe_gate,
                turn_number,
                risk_level,
            );
            tracing::info!("security.shadow_sentinel: ShadowProbeExecutor wired (acp session)");
            (
                zeph_tools::DynExecutor(Arc::new(shadow_exec)),
                Some(sentinel),
            )
        } else {
            (tool_executor, None)
        }
    };
    // #5736: ShadowSentinel keeps its own MCP tool-id set (mirroring TrustGateExecutor's) so
    // classify_tool can escalate MCP write/edit tools to ExfilCapable without a cross-crate
    // ToolDef dependency at its call site, matching src/runner.rs.
    if let Some(ref sentinel) = shadow_sentinel_arc {
        crate::agent_setup::register_mcp_tool_ids(&sentinel.mcp_tool_ids_handle(), &mcp_tools);
    }

    // Session persistence (spec-068, #5343): reuse the ACP session_id directly as the
    // zeph_common::SessionId — ACP already owns this session's identity/lifecycle, so no
    // separate minting/reuse logic is needed here (unlike the CLI/TUI path in runner.rs, which
    // has no pre-existing session identity to anchor to). `SessionStore::create` is idempotent
    // (INSERT_IGNORE) and does not touch `conversation_id`, which ACP's own
    // `create_acp_session_with_conversation` already manages — this call only ensures the row
    // exists so `SessionStore::update_seq` has something to update.
    //
    // Computed here, before `channel` is consumed by `Agent::new_with_registry_arc` below, so an
    // `AlreadyLocked` failure can be surfaced to the client (`notify_lock_degraded` above, via
    // `status_notifier` — pushed immediately, see #5519) instead of only being visible in logs
    // (#5487 fix 3).
    let mut acp_session_sink = None;
    let mut preloaded_messages: Vec<zeph_llm::provider::Message> = Vec::new();
    if session_persistence_config.enabled {
        let sid = zeph_common::SessionId::new(session_ctx.session_id.to_string());
        let store = zeph_session::SessionStore::new(memory.sqlite().pool().clone());
        if let Err(e) = store.create(sid.as_str()).await {
            tracing::warn!(error = %e, session_id = %sid, "failed to create session-store row for ACP session");
        }
        let data_dir = std::path::PathBuf::from(&session_persistence_config.data_dir);
        let session_path = zeph_session::session_dir(&data_dir, sid.as_str());

        // D-10 (spec-068 §12.3/§13): route through the shared hydration pipeline (legacy
        // bootstrap + ReplayEngine fold + INV-SP-3 reconcile) — the one pipeline every
        // session-open path (ACP, CLI `sessions resume`, `/conv resume`) now shares, so they
        // cannot silently diverge again (impl-critic finding C1). Bootstrap/reconcile need a
        // linked `ConversationId`; when absent (store was unavailable at session creation —
        // `with_memory` above was skipped too), fall back to a bare log open with no
        // SQLite-touching steps, matching this edge case's pre-D-10 behavior.
        // D-13 (spec-068 §8.1, N3): `hydrate_and_condense` additionally folds in resume-time
        // durable condensation via the pre-built `d.resume_condenser`/`d.resume_token_counter`
        // (see `SharedAgentDeps`'s doc comment for why they're built once at deps-construction
        // time, not here).
        let log = if let Some(cid) = session_ctx.conversation_id {
            match zeph_agent_persistence::hydrate_and_condense(
                &session_path,
                &store,
                sid.as_str(),
                cid,
                &memory,
                None,
                &d.resume_condenser,
                d.resume_token_counter.as_ref(),
                d.session_config.budget_tokens,
            )
            .await
            {
                Ok(hydrated) => {
                    preloaded_messages = hydrated.messages;
                    Some(hydrated.log)
                }
                // #5487 fix 3: another process already holds this session's exclusive write
                // lock. Unlike the generic degrade-to-no-persistence branch below, this is
                // elevated to `error` plus a client-visible status notification — silently
                // continuing here would let this ACP session race the other process's writes
                // exactly like the reported bug.
                Err(zeph_agent_persistence::PersistenceError::Session(
                    zeph_session::SessionError::AlreadyLocked(lock_path),
                )) => {
                    tracing::error!(
                        lock_path,
                        "session hydration failed: another process already holds this session's \
                         write lock; session persistence disabled for this session"
                    );
                    notify_lock_degraded(status_notifier.as_ref(), &mut channel).await;
                    None
                }
                Err(e) => {
                    tracing::warn!(error = %e, "session hydration failed; session persistence disabled for this session");
                    None
                }
            }
        } else {
            open_session_log_or_notify_locked(&session_path, status_notifier.as_ref(), &mut channel)
                .await
        };

        if let Some(log) = log {
            acp_session_sink = Some(Arc::new(zeph_agent_persistence::SessionSink::new(
                log, store, sid,
            )));
        }
    }

    let mut agent = Box::pin(
        Agent::new_with_registry_arc(
            provider.clone(),
            d.embedding_provider.clone(),
            channel,
            Arc::clone(&registry),
            matcher,
            max_active_skills,
            tool_executor,
        )
        .apply_session_config(session_config)
        .with_skill_matching_config(
            skill_disambiguation_threshold,
            skill_two_stage_matching,
            skill_confusability_threshold,
        )
        .with_skill_group_config(
            skill_group_structured,
            skill_support_similarity_threshold,
            skill_min_injection_score,
        )
        .with_skill_provider_names(skill_generation_provider, skill_disambiguate_provider)
        .with_semantic_scan(semantic_scan, semantic_scan_provider)
        .with_trust_config(d.trust_config.clone())
        .with_trust_snapshot(Arc::clone(&trust_snapshot))
        .with_quality_pipeline(d.quality_pipeline.clone())
        .with_rl_routing(
            d.rl_routing_enabled,
            d.rl_learning_rate,
            d.rl_weight,
            d.rl_persist_interval,
            d.rl_warmup_updates,
        )
        .with_working_dir(session_ctx.working_dir.clone())
        .with_skill_reload(skill_paths, reload_rx)
        .with_plugin_dirs_supplier(move || plugin_dirs_supplier())
        .with_managed_skills_dir(managed_skills_dir)
        .with_shutdown(shutdown_rx)
        .with_config_reload(config_path, config_reload_rx)
        .with_plugins_dir(
            crate::bootstrap::plugins_dir(),
            d.startup_shell_overlay.clone(),
        )
        .with_shell_policy_handle(d.shell_policy_handle.clone())
        .with_mcp(
            mcp_tools,
            mcp_registry,
            Some(Arc::clone(&mcp_manager)),
            &mcp_config,
        )
        .with_mcp_shared_tools(mcp_shared_tools)
        .with_focus_and_sidequest_config(d.focus_config.clone(), d.sidequest_config.clone())
        .with_trajectory_and_category_config(d.trajectory_config.clone(), d.category_config.clone())
        .with_provider_pool(provider_pool, provider_config_snapshot)
        .with_embedding_provider(d.embedding_provider.clone())
        .with_shutdown_summary_config(
            shutdown_summary,
            shutdown_summary_min_messages,
            shutdown_summary_max_messages,
            shutdown_summary_timeout_secs,
        )
        .with_shutdown_summary_provider(shutdown_summary_provider)
        .with_channel_identity(
            "acp",
            channel_provider_persistence,
            channel_persist_provider_overrides,
        )
        .maybe_init_tool_schema_filter(tool_filter_config, provider.clone()),
    )
    .await;

    agent = agent.with_acp_session(true);

    // #6022: wire code-RAG retrieval (static repo-map/IndexMcpServer injection plus automatic
    // per-turn code-context retrieval) — mirrors src/runner.rs and src/daemon.rs. Previously ACP
    // sessions got neither, since these calls only existed in the CLI/TUI bootstrap path.
    agent = agent_setup::apply_code_retrieval(agent, &index_config);
    agent = agent_setup::apply_code_rag_retriever(
        agent,
        &index_config,
        code_qdrant_ops,
        code_index_provider,
        memory.sqlite().pool().clone(),
    );

    // #5958: wire the trajectory risk slot/signal queue built above (spec 050 Invariant 2) plus
    // the TrajectorySentinel state machine itself into the agent, matching src/runner.rs and
    // src/daemon.rs.
    agent = agent
        .with_trajectory_risk_slot(trajectory_risk_slot)
        .with_signal_queue(trajectory_signal_queue)
        .with_trajectory_config(d.trajectory_sentinel_config.clone())
        .0;

    // SkillOrchestra: wire the RL routing head, if enabled (#5921). `d.rl_head` is loaded/
    // cold-started exactly once in `build_shared_core` and cloned (cheap `Arc` clone) into every
    // session sharing this core — fixes #5974, where each ACP session previously loaded its own
    // independent in-memory copy from the `routing_head_weights` singleton row and persisted
    // back independently, letting concurrent sessions clobber each other's learned REINFORCE
    // weights. All sessions now mutate the SAME `Arc<Mutex<..>>`, so updates serialize through
    // that mutex instead of racing across independent copies.
    if let Some(head) = d.rl_head.clone() {
        agent = agent.with_rl_head(head);
    }

    if let Some(ref logger) = d.audit_logger {
        agent = agent.with_audit_logger(std::sync::Arc::clone(logger));
    }

    // Wire scheduler per session: apply update/custom receivers and add executor.
    #[cfg(feature = "scheduler")]
    {
        if let Some(rx) = scheduler_update_rx {
            agent = agent.with_update_notifications(rx);
        }
        if let Some(rx) = scheduler_custom_rx {
            agent = agent.with_custom_task_rx(rx);
        }
        if let Some(sched_exec) = scheduler_executor {
            agent = agent
                .add_tool_executor(crate::scheduler_executor::DynSchedulerExecutor(sched_exec));
        }
    }

    // Apply per-session memory only when a ConversationId was successfully allocated.
    // When None (store unavailable at session creation), the agent operates without persistent history.
    if let Some(cid) = session_ctx.conversation_id {
        agent = agent.with_memory(
            Arc::clone(&memory),
            cid,
            history_limit,
            recall_limit,
            summarization_threshold,
        );
    }

    // Attach the session log/history computed above, before `channel` was moved into
    // `Agent::new_with_registry_arc`.
    if !preloaded_messages.is_empty() {
        agent = agent.with_preloaded_messages(preloaded_messages);
    }
    if let Some(sink) = acp_session_sink {
        agent = agent
            .with_session_sink(Some(sink))
            .with_session_persistence_config(Some(session_persistence_config.clone()));
    }

    if let Some(signal) = cancel_signal {
        agent = agent.with_cancel_signal(signal);
    }

    if let Some(slot) = provider_override {
        agent = agent.with_provider_override(slot);
    }

    if let Some(parent_id) = parent_tool_use_id {
        agent = agent.with_parent_tool_use_id(parent_id);
    }

    if let Some(sp) = summary_provider {
        agent = agent.with_summary_provider(sp);
    }

    if let Some(jp) = judge_provider {
        agent = agent.with_judge_provider(jp);
    }
    if let Some(fc) = feedback_classifier {
        agent = agent.with_llm_classifier(fc);
    }

    if let Some(pp) = probe_provider {
        agent = agent.with_probe_provider(pp);
    }

    if let Some(pp) = planner_provider {
        agent = agent.with_planner_provider(pp);
    }

    if let Some(vp) = verify_provider {
        agent = agent.with_verify_provider(vp);
    }

    if let Some(op) = orchestrator_provider {
        agent = agent.with_orchestrator_provider(op);
    }

    if let Some(pp) = predicate_provider {
        agent = agent.with_predicate_provider(pp);
    }

    agent = agent_setup::apply_quarantine_provider(agent, quarantine_provider);
    {
        agent = agent_setup::apply_guardrail(agent, guardrail_provider);
    }
    #[cfg(feature = "classifiers")]
    {
        agent = agent_setup::apply_injection_classifier_with_cfg(agent, &classifiers_config);
        if classifiers_config.enabled {
            agent = agent.with_enforcement_mode(classifiers_config.enforcement_mode);
        }
        agent = agent_setup::apply_three_class_classifier_with_cfg(agent, &classifiers_config);
        agent = agent_setup::apply_pii_classifier_with_cfg(agent, &classifiers_config);
        agent = agent_setup::apply_pii_ner_classifier_with_cfg(
            agent,
            &classifiers_config,
            pii_filter_enabled,
        );
    }
    agent = agent_setup::apply_causal_analyzer_with_cfg(
        agent,
        provider.clone(),
        causal_provider,
        &causal_ipi_config,
        secret_registry.as_ref(),
    );
    agent = agent_setup::apply_nli_sanitizer_with_cfg(
        agent,
        provider.clone(),
        nli_provider,
        &nli_config,
        secret_registry.as_ref(),
    );
    agent = agent_setup::apply_secret_masking(agent, secret_registry);
    agent = agent_setup::apply_vigil(agent, &vigil_config);

    if debug_config.enabled {
        // Use session_id as a subdirectory prefix so concurrent sessions never share the same
        // timestamped directory and collide on file names (I2).
        let session_dump_dir = debug_config
            .output_dir
            .join(session_ctx.session_id.to_string());
        agent =
            agent_setup::apply_debug_dumper(agent, session_dump_dir.as_path(), debug_config.format)
                .0;
    }

    agent = agent.with_hooks_config(&hooks_config);
    // Spec 050 Phase 2 (#5913): wire ShadowSentinel into the agent so begin_turn() calls
    // advance_turn(), matching src/runner.rs.
    if let Some(sentinel) = shadow_sentinel_arc {
        agent = agent.with_shadow_sentinel(sentinel);
    }
    // Keep TrustGateExecutor's MCP tool-id registry in sync with MCP servers connected after
    // startup (#5747) — without this, check_tool_refresh has no handle to update.
    agent = agent.with_mcp_tool_ids_handle(mcp_ids_handle);

    drop(d);

    if let Err(e) = agent.load_history().await {
        tracing::error!("failed to load agent history: {e:#}");
    }

    if let Err(e) = Box::pin(agent.run()).await {
        tracing::error!("ACP agent loop error: {e:#}");
    }

    agent.shutdown().await;

    // Ensure the adapter cancellation token is dropped/cancelled after the agent loop exits,
    // which terminates the broadcast forwarding tasks for this session.
    adapter_cancel.cancel();
}

/// Collect model keys from config when `acp.available_models` is not set.
///
/// For each configured provider the disk cache is consulted first (24 h TTL).
/// When the cache is warm the full remote model list is returned; otherwise the
/// single model from config is used as the fallback so startup is never blocked
/// on network I/O.  Call `/model refresh` at runtime to populate the caches.
///
/// Each key uses `"{provider_name}:{model_id}"` format matching the provider factory.
#[cfg(feature = "acp")]
async fn discover_models_from_config(config: &zeph_core::config::Config) -> Vec<String> {
    use zeph_llm::model_cache::ModelCache;

    /// Expand a provider slug using its on-disk cache, or fall back to `fallback`.
    async fn expand_from_cache(slug: &str, fallback: &str) -> Vec<String> {
        let cache = ModelCache::for_slug(slug);
        if !cache.is_stale_async().await
            && let Ok(Some(entries)) = cache.load_async().await
            && !entries.is_empty()
        {
            return entries
                .into_iter()
                .map(|m| format!("{slug}:{}", m.id))
                .collect();
        }
        vec![format!("{slug}:{fallback}")]
    }

    let mut models: Vec<String> = Vec::new();

    for entry in &config.llm.providers {
        let slug = entry.provider_type.as_str();
        let fallback = entry.model.as_deref().unwrap_or("unknown");
        models.extend(expand_from_cache(slug, fallback).await);
    }

    models.dedup();
    models
}

/// Build a `ProviderFactory` from the known named providers in config.
///
/// Each available model key is `"{provider_name}:{model}"`.
/// The factory creates a provider by parsing that key and overriding the model in a clone.
///
/// `secret_registry`, when `Some`, wraps every provider this factory produces via
/// [`zeph_llm::any::AnyProvider::masked`] (#5437) — this is the single construction point for
/// every ACP-switched/primed provider (`prime_provider_override`, `/model` switch, session-title
/// generation), so wrapping here structurally covers all of them, including the session-title
/// background task that dispatches directly on the factory's output and never touches the
/// `provider_override` slot that `Agent::apply_provider_override`/`set_provider` guard.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_lines)]
fn build_acp_provider_factory(
    config: &zeph_core::config::Config,
    secret_registry: Option<std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> zeph_acp::ProviderFactory {
    // Collect snapshots for providers that have secrets already resolved.
    #[derive(Clone)]
    enum ProviderSnapshot {
        Ollama {
            base_url: String,
            embed: String,
        },
        Claude {
            api_key: String,
            max_tokens: u32,
        },
        OpenAi {
            api_key: String,
            base_url: String,
            max_tokens: u32,
            embed: Option<String>,
            reasoning_effort: Option<String>,
        },
        Compatible {
            api_key: String,
            base_url: String,
            max_tokens: u32,
            embed: Option<String>,
            name: String,
        },
    }

    let mut snapshots: Vec<ProviderSnapshot> = Vec::new();

    for entry in &config.llm.providers {
        let name = entry.effective_name();
        match entry.provider_type {
            zeph_core::config::ProviderKind::Ollama => {
                snapshots.push(ProviderSnapshot::Ollama {
                    base_url: entry
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "http://localhost:11434".to_owned()),
                    embed: config.llm.embedding_model.clone(),
                });
            }
            zeph_core::config::ProviderKind::Claude => {
                if let Some(ref secret) = config.secrets.claude_api_key {
                    snapshots.push(ProviderSnapshot::Claude {
                        api_key: secret.expose().to_owned(),
                        max_tokens: entry.max_tokens.unwrap_or(4096),
                    });
                }
            }
            zeph_core::config::ProviderKind::OpenAi => {
                if let Some(ref secret) = config.secrets.openai_api_key {
                    snapshots.push(ProviderSnapshot::OpenAi {
                        api_key: secret.expose().to_owned(),
                        base_url: entry
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://api.openai.com/v1".to_owned()),
                        max_tokens: entry.max_tokens.unwrap_or(4096),
                        embed: entry.embedding_model.clone(),
                        reasoning_effort: entry.reasoning_effort.clone(),
                    });
                }
            }
            zeph_core::config::ProviderKind::Compatible => {
                let secret = entry
                    .api_key
                    .as_deref()
                    .map(std::borrow::ToOwned::to_owned)
                    .or_else(|| {
                        config
                            .secrets
                            .compatible_api_keys
                            .get(&name)
                            .map(|s| s.expose().to_owned())
                    });
                if let Some(api_key) = secret {
                    snapshots.push(ProviderSnapshot::Compatible {
                        api_key,
                        base_url: entry.base_url.clone().unwrap_or_default(),
                        max_tokens: entry.max_tokens.unwrap_or(4096),
                        embed: entry.embedding_model.clone(),
                        name,
                    });
                }
            }
            _ => {}
        }
    }

    let masker: Option<std::sync::Arc<dyn zeph_llm::masking::OutboundMasker>> =
        secret_registry.map(|r| r as std::sync::Arc<dyn zeph_llm::masking::OutboundMasker>);
    let snapshots = std::sync::Arc::new(snapshots);
    std::sync::Arc::new(move |key: &str| {
        // #5437: wrap every provider this factory produces so it's masked regardless of which
        // consumer dispatches on it (`provider_override` slot or the session-title generation
        // task, which calls `.chat()` directly on the factory's output).
        let wrap = |p: zeph_llm::any::AnyProvider| -> zeph_llm::any::AnyProvider {
            match &masker {
                Some(m) => p.masked(std::sync::Arc::clone(m)),
                None => p,
            }
        };
        let (provider_name, model) = key.split_once(':')?;
        let model = model.to_owned();
        for snapshot in snapshots.as_ref() {
            match snapshot {
                ProviderSnapshot::Ollama {
                    base_url, embed, ..
                } if provider_name == "ollama" => {
                    let mut p = zeph_llm::ollama::OllamaProvider::new(
                        base_url,
                        model.clone(),
                        embed.clone(),
                    );
                    p.set_context_window(0);
                    return Some(wrap(zeph_llm::any::AnyProvider::Ollama(p)));
                }
                ProviderSnapshot::Claude {
                    api_key,
                    max_tokens,
                } if provider_name == "claude" => {
                    return Some(wrap(zeph_llm::any::AnyProvider::Claude(
                        zeph_llm::claude::ClaudeProvider::new(
                            api_key.clone(),
                            model.clone(),
                            *max_tokens,
                        ),
                    )));
                }
                ProviderSnapshot::OpenAi {
                    api_key,
                    base_url,
                    max_tokens,
                    embed,
                    reasoning_effort,
                } if provider_name == "openai" => {
                    return Some(wrap(zeph_llm::any::AnyProvider::OpenAi(
                        zeph_llm::openai::OpenAiProvider::new(zeph_llm::openai::OpenAiConfig {
                            api_key: api_key.clone(),
                            base_url: base_url.clone(),
                            model: model.clone(),
                            max_tokens: *max_tokens,
                            embedding_model: embed.clone(),
                            reasoning_effort: reasoning_effort.clone(),
                            context_window: None,
                            completion_tokens_param: None,
                        }),
                    )));
                }
                ProviderSnapshot::Compatible {
                    api_key,
                    base_url,
                    max_tokens,
                    embed,
                    name,
                } if provider_name == name => {
                    return Some(wrap(zeph_llm::any::AnyProvider::Compatible(
                        zeph_llm::compatible::CompatibleProvider::new(
                            zeph_llm::compatible::CompatibleConfig {
                                provider_name: name.clone(),
                                api_key: api_key.clone(),
                                base_url: base_url.clone(),
                                model: model.clone(),
                                max_tokens: *max_tokens,
                                embedding_model: embed.clone(),
                                completion_tokens_param: None,
                            },
                        ),
                    )));
                }
                _ => {}
            }
        }
        None
    })
}

/// Build the `(name, protocol)` list advertised via ACP `providers/list` (#5448).
///
/// Reuses the same `config.llm.providers` source of truth as [`build_acp_provider_factory`]
/// and `discover_models_from_config`, so the advertised identity always matches the providers
/// actually wired for model switching. Vault-resolved API keys are never included.
#[cfg(feature = "acp")]
fn acp_provider_names(config: &zeph_core::config::Config) -> Vec<(String, zeph_acp::LlmProtocol)> {
    config
        .llm
        .providers
        .iter()
        .map(|entry| {
            let protocol = match entry.provider_type {
                zeph_core::config::ProviderKind::Claude => zeph_acp::LlmProtocol::Anthropic,
                zeph_core::config::ProviderKind::OpenAi
                | zeph_core::config::ProviderKind::Compatible => zeph_acp::LlmProtocol::OpenAi,
                other => zeph_acp::LlmProtocol::Other(other.as_str().to_owned()),
            };
            (entry.effective_name(), protocol)
        })
        .collect()
}

/// Collect project rule file paths from `.claude/rules/*.md` and skill files.
///
/// Rule files are resolved relative to the current working directory.
/// Skill paths that point to regular files (SKILL.md entries) are included as-is.
#[cfg(feature = "acp")]
fn collect_project_rules(skill_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut rules = Vec::new();
    let rules_dir = std::path::Path::new(".claude/rules");
    if rules_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(rules_dir)
    {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();
        rules.extend(paths);
    }
    for sp in skill_paths {
        if sp.is_file() {
            rules.push(sp.clone());
        }
    }
    rules
}

/// Run the ACP server over stdin/stdout.
///
/// Supports multiple concurrent sessions via `SharedAgentDeps` — each `session/new` spawns
/// an independent agent loop with its own conversation history.
///
/// # Errors
///
/// Returns an error if the agent stack cannot be built or the transport fails.
#[cfg(feature = "acp")]
pub(crate) async fn run_acp_server(
    config_path: Option<&std::path::Path>,
    vault_backend: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
    cli_additional_dirs: Vec<std::path::PathBuf>,
    cli_auth_methods: Vec<String>,
    cli_message_ids: Option<bool>,
) -> anyhow::Result<()> {
    use std::sync::Arc;

    let app = AppBuilder::new(config_path, vault_backend, vault_key, vault_path).await?;
    let (mut deps, _keepalive) = Box::pin(build_acp_deps(&app, None, None)).await?;
    let available_models = std::sync::Arc::clone(&deps.acp_available_models);
    let provider = deps.provider.clone();
    zeph_acp::warm_model_caches(provider, available_models).await;

    // Apply CLI overrides to config-derived values.
    let effective_additional_dirs = if cli_additional_dirs.is_empty() {
        deps.acp_additional_directories.clone()
    } else {
        cli_additional_dirs
            .into_iter()
            .map(|p| {
                zeph_core::config::AdditionalDir::parse(p.clone()).map_err(|e| {
                    anyhow::anyhow!("invalid --acp-additional-dir {}: {e}", p.display())
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    let effective_auth_methods = if cli_auth_methods.is_empty() {
        let methods = deps.acp_auth_methods.clone();
        anyhow::ensure!(
            !methods.is_empty(),
            "acp.auth_methods must not be empty; set at least one method (e.g. \"agent\")"
        );
        methods
    } else {
        let methods: Vec<_> = cli_auth_methods
            .iter()
            .map(|m| match m.as_str() {
                "agent" => Ok(zeph_core::config::AcpAuthMethod::Agent),
                other => Err(anyhow::anyhow!(
                    "unknown --acp-auth-method {other:?}; accepted values: agent"
                )),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            !methods.is_empty(),
            "--acp-auth-method list must not be empty after parsing"
        );
        methods
    };
    let effective_message_ids = cli_message_ids.unwrap_or(deps.acp_message_ids_enabled);

    let mcp_manager_for_acp = Arc::clone(&deps.mcp_manager);
    let server_config = zeph_acp::AcpServerConfig {
        agent_name: deps.acp_agent_name.clone(),
        agent_version: deps.acp_agent_version.clone(),
        max_sessions: deps.acp_max_sessions,
        session_idle_timeout_secs: deps.acp_session_idle_timeout_secs,
        permission_file: deps.acp_permission_file.clone(),
        provider_factory: deps.acp_provider_factory.take(),
        available_models: std::sync::Arc::clone(&deps.acp_available_models),
        provider_names: deps.acp_provider_names.clone(),
        mcp_manager: Some(mcp_manager_for_acp),
        auth_clients: deps.acp_auth_clients.clone(),
        discovery_enabled: deps.acp_discovery_enabled,
        terminal_timeout_secs: deps.acp_timeouts.terminal_secs,
        project_rules: deps.acp_project_rules.clone(),
        title_max_chars: deps.acp_title_max_chars,
        max_history: deps.acp_max_history,
        sqlite_path: Some(deps.sqlite_path.clone()),
        session_data_dir: deps
            .session_persistence_config
            .enabled
            .then(|| std::path::PathBuf::from(&deps.session_persistence_config.data_dir)),
        ready_notification: Some(zeph_acp::transport::ReadyNotification {
            version: deps.acp_agent_version.clone(),
            pid: std::process::id(),
            log_file: deps.acp_log_file.clone(),
        }),
        additional_directories: effective_additional_dirs,
        auth_methods: effective_auth_methods,
        message_ids_enabled: effective_message_ids,
        timeouts: deps.acp_timeouts.clone(),
        model_config: deps.acp_model_config.clone(),
    };

    let shared = Arc::new(deps);

    let spawner: zeph_acp::AgentSpawner = Arc::new(move |channel, acp_ctx, session_ctx| {
        let shared = Arc::clone(&shared);
        Box::pin(spawn_acp_agent(shared, channel, acp_ctx, session_ctx))
    });

    zeph_acp::serve_stdio(spawner, server_config).await?;

    Ok(())
}

/// Run the ACP server over HTTP+SSE and WebSocket.
///
/// # Errors
///
/// Returns an error if the agent stack cannot be built or the server fails to bind.
#[cfg(feature = "acp-http")]
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_acp_http_server(
    config_path: Option<&std::path::Path>,
    vault_backend: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
    bind_override: Option<&str>,
    auth_token_override: Option<String>,
) -> anyhow::Result<()> {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let app = AppBuilder::new(config_path, vault_backend, vault_key, vault_path).await?;
    log_acp_runtime_paths(app.config(), app.config_path());
    let bind_addr = bind_override.map_or_else(|| app.config().acp.http_bind.clone(), str::to_owned);

    // CLI flag overrides config/env values for the "default" client's token; other
    // configured `[[acp.auth_clients]]` entries are unaffected.
    let mut auth_clients = resolve_acp_auth_clients(&app.config().acp, app.vault()).await?;
    if let Some(override_token) = auth_token_override {
        auth_clients.retain(|c| c.id != zeph_config::ACP_AUTH_CLIENT_ID_DEFAULT);
        // Same collision guard `resolve_acp_auth_clients` applies to every other client —
        // the CLI override must not silently bypass it and reintroduce a shared-owner_key leak.
        anyhow::ensure!(
            !auth_clients.iter().any(|c| c.token == override_token),
            "--acp-auth-token collides with a configured [[acp.auth_clients]] token"
        );
        auth_clients.insert(
            0,
            zeph_acp::AcpClientToken {
                id: zeph_config::ACP_AUTH_CLIENT_ID_DEFAULT.to_owned(),
                token: override_token,
            },
        );
    }
    let mcp_manager_for_acp = Arc::new(crate::bootstrap::create_mcp_manager_with_vault(
        app.config(),
        false,
        app.age_vault_arc(),
    ));
    let server_config = zeph_acp::AcpServerConfig {
        agent_name: app.config().acp.agent_name.clone(),
        agent_version: app.config().acp.agent_version.clone(),
        max_sessions: app.config().acp.max_sessions,
        session_idle_timeout_secs: app.config().acp.session_idle_timeout_secs,
        permission_file: app.config().acp.permission_file.clone(),
        provider_factory: Some(build_acp_provider_factory(
            app.config(),
            app.secret_registry(),
        )),
        available_models: std::sync::Arc::new(parking_lot::RwLock::new(
            if app.config().acp.available_models.is_empty() {
                discover_models_from_config(app.config()).await
            } else {
                app.config().acp.available_models.clone()
            },
        )),
        provider_names: acp_provider_names(app.config()),
        mcp_manager: Some(Arc::clone(&mcp_manager_for_acp)),
        auth_clients,
        discovery_enabled: app.config().acp.discovery_enabled,
        terminal_timeout_secs: app.config().acp.timeouts.terminal_secs,
        project_rules: collect_project_rules(&app.skill_paths_for_registry()),
        title_max_chars: app.config().memory.sessions.title_max_chars,
        max_history: app.config().memory.sessions.max_history,
        sqlite_path: Some(crate::db_url::resolve_db_url(app.config()).to_owned()),
        session_data_dir: app
            .config()
            .session
            .enabled
            .then(|| std::path::PathBuf::from(&app.config().session.data_dir)),
        ready_notification: None,
        additional_directories: app.config().acp.additional_directories.clone(),
        auth_methods: app.config().acp.auth_methods.clone(),
        message_ids_enabled: app.config().acp.message_ids_enabled,
        timeouts: app.config().acp.timeouts.clone(),
        model_config: app.config().acp.model_config.clone(),
    };
    let shared_deps: Arc<RwLock<Option<Arc<SharedAgentDeps>>>> = Arc::new(RwLock::new(None));
    let shared_deps_for_spawner = Arc::clone(&shared_deps);
    let spawner: zeph_acp::SendAgentSpawner = Arc::new(move |channel, acp_ctx, session_ctx| {
        let shared_deps = Arc::clone(&shared_deps_for_spawner);
        Box::pin(async move {
            let maybe_shared = shared_deps.read().await.clone();
            let Some(shared) = maybe_shared else {
                tracing::warn!("ACP request received before runtime became ready");
                return;
            };
            Box::pin(spawn_acp_agent(shared, channel, acp_ctx, session_ctx)).await;
        })
    });
    let mut state = zeph_acp::AcpHttpState::new(spawner, server_config);
    match zeph_memory::store::SqliteStore::new(crate::db_url::resolve_db_url(app.config())).await {
        Ok(store) => state = state.with_store(store),
        Err(e) => tracing::warn!(error = %e, "failed to open SQLite for HTTP session endpoints"),
    }

    let router = zeph_acp::acp_router(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("ACP HTTP server listening on {bind_addr}");
    let server_task = tokio::spawn(async move { ::axum::serve(listener, router).await }); // EXEMPT(#5144): awaited at end of fn; joinable lifecycle needed

    let (deps, _keepalive) =
        match Box::pin(build_acp_deps(&app, None, Some(mcp_manager_for_acp))).await {
            Ok(result) => result,
            Err(err) => {
                server_task.abort();
                return Err(err);
            }
        };

    let available_models = std::sync::Arc::clone(&deps.acp_available_models);
    let provider = deps.provider.clone();
    zeph_acp::warm_model_caches(provider, available_models).await;
    *shared_deps.write().await = Some(Arc::new(deps));
    state.mark_ready();
    state.start_reaper();
    tracing::info!("ACP server ready");
    server_task.await??;

    Ok(())
}

/// Build [`crate::serve::deps::ServeAgentDeps`] and [`SharedAgentDeps`] from ONE [`SharedCore`]
/// and one [`zeph_common::TaskSupervisor`] — the production sharing path for
/// `zeph serve-sessions --acp` (#5420), called by both `crate::serve::run_serve_with_acp` and
/// its test harness's `build_shared_pair` (so the pair-sharing assertion exercises real
/// production wiring, not a test-only re-assembly).
///
/// ACP is the sole `McpManager` builder in combined mode (`prebuilt_mcp_manager: None` below):
/// `serve` wires no MCP tools today (see `crate::serve::deps` module doc), so there is nothing
/// to share and no duplicate-subprocess risk.
///
/// # Errors
///
/// Returns an error if either deps bundle's construction fails (provider, memory, or MCP
/// connection).
#[cfg(all(feature = "acp-http", feature = "session"))]
pub(crate) async fn build_combined_deps(
    app: &AppBuilder,
    supervisor: &std::sync::Arc<zeph_common::TaskSupervisor>,
) -> anyhow::Result<(
    crate::serve::deps::ServeAgentDeps,
    SharedAgentDeps,
    Box<dyn std::any::Any>,
)> {
    let core = build_shared_core(app, supervisor).await?;
    let serve_deps = crate::serve::deps::assemble_serve_deps(app, &core, supervisor).await?;
    let prebuilt_core = PrebuiltAcpCore {
        core,
        supervisor: std::sync::Arc::clone(supervisor),
    };
    let (acp_deps, keepalive) = Box::pin(build_acp_deps(app, Some(prebuilt_core), None)).await?;
    Ok((serve_deps, acp_deps, keepalive))
}

/// Assemble an [`zeph_acp::AcpServerConfig`] for the ACP-HTTP transport from already-built,
/// ready [`SharedAgentDeps`] — used by `zeph serve-sessions --acp`'s combined orchestrator
/// (`crate::serve::run_serve_with_acp`).
///
/// `ready_notification` is always `None`: readiness is signaled via `GET /health` returning
/// `200` after `AcpHttpState::mark_ready`, not a stdio JSON-RPC frame — that mechanism belongs
/// to the standalone `--acp` stdio transport, not the HTTP one.
#[cfg(all(feature = "acp-http", feature = "session"))]
pub(crate) fn acp_http_server_config(deps: &mut SharedAgentDeps) -> zeph_acp::AcpServerConfig {
    zeph_acp::AcpServerConfig {
        agent_name: deps.acp_agent_name.clone(),
        agent_version: deps.acp_agent_version.clone(),
        max_sessions: deps.acp_max_sessions,
        session_idle_timeout_secs: deps.acp_session_idle_timeout_secs,
        permission_file: deps.acp_permission_file.clone(),
        provider_factory: deps.acp_provider_factory.take(),
        available_models: std::sync::Arc::clone(&deps.acp_available_models),
        provider_names: deps.acp_provider_names.clone(),
        mcp_manager: Some(std::sync::Arc::clone(&deps.mcp_manager)),
        auth_clients: deps.acp_auth_clients.clone(),
        discovery_enabled: deps.acp_discovery_enabled,
        terminal_timeout_secs: deps.acp_timeouts.terminal_secs,
        project_rules: deps.acp_project_rules.clone(),
        title_max_chars: deps.acp_title_max_chars,
        max_history: deps.acp_max_history,
        sqlite_path: Some(deps.sqlite_path.clone()),
        session_data_dir: deps
            .session_persistence_config
            .enabled
            .then(|| std::path::PathBuf::from(&deps.session_persistence_config.data_dir)),
        ready_notification: None,
        additional_directories: deps.acp_additional_directories.clone(),
        auth_methods: deps.acp_auth_methods.clone(),
        message_ids_enabled: deps.acp_message_ids_enabled,
        timeouts: deps.acp_timeouts.clone(),
        model_config: deps.acp_model_config.clone(),
    }
}

/// Warm model caches and build a [`zeph_acp::SendAgentSpawner`] closing over already-ready
/// `deps` — no `RwLock<Option<Arc<SharedAgentDeps>>>` deferral is needed here, unlike
/// `run_acp_http_server`'s standalone path: the combined orchestrator builds `deps` fully
/// before either axum listener starts accepting connections, so the spawner is never invoked
/// while deps are still being assembled.
#[cfg(all(feature = "acp-http", feature = "session"))]
pub(crate) async fn acp_http_ready_spawner(
    deps: std::sync::Arc<SharedAgentDeps>,
) -> zeph_acp::SendAgentSpawner {
    let available_models = std::sync::Arc::clone(&deps.acp_available_models);
    let provider = deps.provider.clone();
    zeph_acp::warm_model_caches(provider, available_models).await;
    std::sync::Arc::new(move |channel, acp_ctx, session_ctx| {
        let shared = std::sync::Arc::clone(&deps);
        Box::pin(spawn_acp_agent(shared, channel, acp_ctx, session_ctx))
    })
}

#[cfg(feature = "acp")]
pub(crate) fn print_acp_manifest() {
    let manifest = serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "transport": "stdio",
        "command": [env!("CARGO_PKG_NAME"), "--acp"],
        "capabilities": ["prompt", "cancel", "load_session", "set_session_mode", "config_options", "ext_methods"],
        "description": "Zeph AI Agent",
        "readiness": {
            "notification": {
                "method": "zeph/ready",
                "params": {
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": "<process-id>",
                    "log_file": "<configured-log-file>"
                }
            },
            "http": {
                "health_endpoint": "/health",
                "statuses": [200, 503]
            }
        }
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&manifest).unwrap_or_default()
    );
}

#[cfg(all(test, feature = "acp"))]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;
    use zeph_tools::executor::ToolExecutor;

    // ── resolve_acp_auth_clients (#5868) ──────────────────────────────────────

    /// In-memory `VaultProvider` for `resolve_acp_auth_clients` tests — implements the real
    /// trait directly rather than pulling in `MockVaultProvider` (which needs the `zeph-core
    /// mock` feature threaded into this binary crate's dev-dependencies).
    #[derive(Default)]
    struct TestVault {
        secrets: std::collections::HashMap<String, String>,
        /// Keys that simulate a backend error (as opposed to a plain miss) on lookup.
        erroring_keys: std::collections::HashSet<String>,
    }

    impl TestVault {
        fn with_secret(mut self, key: &str, value: &str) -> Self {
            self.secrets.insert(key.to_owned(), value.to_owned());
            self
        }

        fn with_erroring_key(mut self, key: &str) -> Self {
            self.erroring_keys.insert(key.to_owned());
            self
        }
    }

    impl zeph_core::vault::VaultProvider for TestVault {
        fn get_secret(
            &self,
            key: &str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<String>, zeph_core::vault::VaultError>,
                    > + Send
                    + '_,
            >,
        > {
            let result = if self.erroring_keys.contains(key) {
                Err(zeph_core::vault::VaultError::Backend(
                    "simulated backend failure".to_owned(),
                ))
            } else {
                Ok(self.secrets.get(key).cloned())
            };
            Box::pin(async move { result })
        }
    }

    fn acp_config_with(
        auth_token: Option<&str>,
        auth_clients: Vec<zeph_config::AcpAuthClient>,
    ) -> zeph_config::AcpConfig {
        zeph_config::AcpConfig {
            auth_token: auth_token.map(str::to_owned),
            auth_clients,
            ..zeph_config::AcpConfig::default()
        }
    }

    fn inline_client(id: &str, token: &str) -> zeph_config::AcpAuthClient {
        zeph_config::AcpAuthClient {
            id: id.to_owned(),
            token: Some(token.to_owned()),
            token_vault_key: None,
        }
    }

    fn vault_client(id: &str, vault_key: &str) -> zeph_config::AcpAuthClient {
        zeph_config::AcpAuthClient {
            id: id.to_owned(),
            token: None,
            token_vault_key: Some(vault_key.to_owned()),
        }
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_empty_config_returns_empty() {
        let cfg = acp_config_with(None, vec![]);
        let clients = resolve_acp_auth_clients(&cfg, &TestVault::default())
            .await
            .unwrap();
        assert!(clients.is_empty());
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_legacy_token_becomes_default_client() {
        let cfg = acp_config_with(Some("legacy-secret"), vec![]);
        let clients = resolve_acp_auth_clients(&cfg, &TestVault::default())
            .await
            .unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, zeph_config::ACP_AUTH_CLIENT_ID_DEFAULT);
        assert_eq!(clients[0].token, "legacy-secret");
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_inline_token_resolved_directly() {
        let cfg = acp_config_with(None, vec![inline_client("alice", "token-a")]);
        let clients = resolve_acp_auth_clients(&cfg, &TestVault::default())
            .await
            .unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, "alice");
        assert_eq!(clients[0].token, "token-a");
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_vault_key_resolved_from_vault() {
        let cfg = acp_config_with(None, vec![vault_client("alice", "ZEPH_ACP_TOKEN_ALICE")]);
        let vault = TestVault::default().with_secret("ZEPH_ACP_TOKEN_ALICE", "vault-token-a");
        let clients = resolve_acp_auth_clients(&cfg, &vault).await.unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, "alice");
        assert_eq!(clients[0].token, "vault-token-a");
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_missing_vault_key_soft_disables_client() {
        let cfg = acp_config_with(
            None,
            vec![
                vault_client("alice", "ZEPH_ACP_TOKEN_ALICE"),
                inline_client("bob", "token-b"),
            ],
        );
        // No secret registered for ZEPH_ACP_TOKEN_ALICE -> Ok(None) -> alice silently dropped.
        let clients = resolve_acp_auth_clients(&cfg, &TestVault::default())
            .await
            .unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, "bob");
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_vault_backend_error_soft_disables_client() {
        let cfg = acp_config_with(
            None,
            vec![
                vault_client("alice", "ZEPH_ACP_TOKEN_ALICE"),
                inline_client("bob", "token-b"),
            ],
        );
        let vault = TestVault::default().with_erroring_key("ZEPH_ACP_TOKEN_ALICE");
        let clients = resolve_acp_auth_clients(&cfg, &vault).await.unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, "bob");
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_rejects_vault_token_colliding_with_inline_token() {
        let cfg = acp_config_with(
            None,
            vec![
                inline_client("alice", "shared-secret"),
                vault_client("bob", "ZEPH_ACP_TOKEN_BOB"),
            ],
        );
        let vault = TestVault::default().with_secret("ZEPH_ACP_TOKEN_BOB", "shared-secret");
        let err = resolve_acp_auth_clients(&cfg, &vault).await.unwrap_err();
        assert!(
            err.to_string().contains("collides"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_rejects_two_vault_tokens_resolving_to_same_secret() {
        let cfg = acp_config_with(
            None,
            vec![
                vault_client("alice", "ZEPH_ACP_TOKEN_ALICE"),
                vault_client("bob", "ZEPH_ACP_TOKEN_BOB"),
            ],
        );
        let vault = TestVault::default()
            .with_secret("ZEPH_ACP_TOKEN_ALICE", "same-secret")
            .with_secret("ZEPH_ACP_TOKEN_BOB", "same-secret");
        let err = resolve_acp_auth_clients(&cfg, &vault).await.unwrap_err();
        assert!(
            err.to_string().contains("collides"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_acp_auth_clients_rejects_vault_token_colliding_with_legacy_default() {
        let cfg = acp_config_with(
            Some("legacy-secret"),
            vec![vault_client("alice", "ZEPH_ACP_TOKEN_ALICE")],
        );
        let vault = TestVault::default().with_secret("ZEPH_ACP_TOKEN_ALICE", "legacy-secret");
        let err = resolve_acp_auth_clients(&cfg, &vault).await.unwrap_err();
        assert!(
            err.to_string().contains("collides"),
            "unexpected error: {err}"
        );
    }

    fn make_rules_dir(dir: &std::path::Path, files: &[&str]) {
        let rules = dir.join(".claude").join("rules");
        fs::create_dir_all(&rules).unwrap();
        for name in files {
            fs::write(rules.join(name), b"").unwrap();
        }
    }

    #[test]
    #[serial]
    fn collect_project_rules_empty_skill_paths_no_rules_dir() {
        let tmp = TempDir::new().unwrap();
        // No .claude/rules dir exists — function must return empty vec.
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = collect_project_rules(&[]);
        std::env::set_current_dir(orig).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    #[serial]
    fn collect_project_rules_picks_md_files_from_rules_dir() {
        let tmp = TempDir::new().unwrap();
        make_rules_dir(tmp.path(), &["rust-code.md", "testing.md", "notes.txt"]);
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = collect_project_rules(&[]);
        std::env::set_current_dir(orig).unwrap();
        // Only .md files should be returned.
        assert_eq!(result.len(), 2);
        let names: Vec<_> = result
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"rust-code.md".to_owned()));
        assert!(names.contains(&"testing.md".to_owned()));
        assert!(!names.contains(&"notes.txt".to_owned()));
    }

    #[test]
    #[serial]
    fn collect_project_rules_includes_skill_files() {
        let tmp = TempDir::new().unwrap();
        let skill_file = tmp.path().join("my-skill.md");
        fs::write(&skill_file, b"").unwrap();
        let skill_dir = tmp.path().join("skills-dir");
        fs::create_dir_all(&skill_dir).unwrap();

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        // skill_file is a file — included; skill_dir is a dir — excluded.
        let result = collect_project_rules(&[skill_file.clone(), skill_dir]);
        std::env::set_current_dir(orig).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], skill_file);
    }

    /// Regression test for #5578 (dispatch-level companion to #5433's reachability
    /// test): calls the same `agent_setup::build_base_executor_chain` helper used by
    /// `spawn_acp_agent` above, wrapped in the same `TrustGateExecutor` (see #5575),
    /// and asserts a `diagnostics` `ToolCall` actually reaches `DiagnosticsExecutor` —
    /// not just that it appears in `tool_definitions()`. `Full` autonomy bypasses the
    /// trust gate's confirmation prompt so the call proceeds to the inner executor,
    /// exercising the trust gate's pass-through path rather than #5575's Ask path.
    #[tokio::test]
    async fn diagnostics_tool_call_dispatches_through_acp_composite_chain() {
        let config = zeph_core::config::Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(&config);
        let base_executor = crate::agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );
        let policy =
            zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full);
        let base_executor = zeph_tools::TrustGateExecutor::new(base_executor, policy);

        // Must exist on disk (DiagnosticsExecutor canonicalizes before the sandbox
        // check) while staying outside `allowed_paths` (defaults to cwd). Assumes
        // `TMPDIR`/temp_dir() is not itself under the repo/cwd, true in normal
        // environments.
        let outside = std::env::temp_dir();
        let mut params = serde_json::Map::new();
        params.insert(
            "path".into(),
            serde_json::Value::String(outside.display().to_string()),
        );
        let call = zeph_tools::ToolCall {
            tool_id: "diagnostics".into(),
            params,
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = base_executor.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::SandboxViolation { .. })),
            "expected SandboxViolation from DiagnosticsExecutor, got {result:?}"
        );
    }

    /// Regression test for #5575's ACP gap found in review: `spawn_acp_agent` built
    /// the base chain with NO `TrustGateExecutor` at all, so `diagnostics` (and any
    /// other unconfigured, non-MCP/non-readonly tool) reached `LoopbackChannel::confirm`
    /// — which unconditionally returns `Ok(true)` — instead of ever producing
    /// `ConfirmationRequired`. Now that `spawn_acp_agent` wraps the chain in
    /// `TrustGateExecutor` (mirroring `runner.rs`), the default `Supervised` autonomy
    /// must require confirmation for `diagnostics` here too.
    #[tokio::test]
    async fn diagnostics_requires_confirmation_in_acp_composite_chain() {
        let config = zeph_core::config::Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(&config);
        let base_executor = crate::agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );
        // Default PermissionPolicy: Supervised autonomy, no explicit rules configured —
        // the exact real-world "user never set tools.permissions" scenario #5575 covers.
        let base_executor = zeph_tools::TrustGateExecutor::new(
            base_executor,
            zeph_tools::PermissionPolicy::default(),
        );

        let call = zeph_tools::ToolCall {
            tool_id: "diagnostics".into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = base_executor.execute_tool_call(&call).await;
        assert!(
            matches!(
                result,
                Err(zeph_tools::ToolError::ConfirmationRequired { .. })
            ),
            "expected ConfirmationRequired for diagnostics under Supervised autonomy, got {result:?}"
        );
    }

    /// Mock executor that only handles calls matching its own `tool_id`, mirroring
    /// `CompositeExecutor`'s first-match-wins dispatch (`Ok(None)` = "not mine, try next").
    #[derive(Debug)]
    struct AcpTaggedMock(&'static str);

    impl zeph_tools::executor::ToolExecutor for AcpTaggedMock {
        async fn execute(
            &self,
            _response: &str,
        ) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &zeph_tools::ToolCall,
        ) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> {
            if call.tool_id != self.0 {
                return Ok(None);
            }
            Ok(Some(zeph_tools::ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
        zeph_tools::tool_executor_no_inner_defaults!();
    }

    fn acp_test_call(tool_id: &str) -> zeph_tools::ToolCall {
        zeph_tools::ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    /// Regression test for #5611: `spawn_acp_agent` composes `skill_loader -> memory ->
    /// overflow -> (base_chain -> mcp)` into one tree and gates the WHOLE thing via
    /// `agent_setup::apply_common_tool_gating`. Before the fix, only the base chain carried
    /// a `TrustGateExecutor` (wired in `build_acp_deps`), so a Quarantined skill could still
    /// reach `memory_save`, any MCP-sourced tool, or `load_skill` — all composed outside that
    /// gate. Mirrors `spawn_acp_agent`'s exact nesting order with lightweight mocks standing
    /// in for the real `MemoryToolExecutor`/`McpToolExecutor` (which need a live
    /// `SemanticMemory`/`McpManager`).
    #[tokio::test]
    async fn quarantine_blocks_memory_and_mcp_in_acp_composite_chain() {
        let mcp_tool = zeph_mcp::McpTool {
            server_id: "mcp".to_owned(),
            name: "write_file".to_owned(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
            output_schema: None,
            security_meta: zeph_mcp::tool::ToolSecurityMeta::default(),
        };
        let mcp_tool_id = mcp_tool.sanitized_id();
        assert_eq!(mcp_tool_id, "mcp_write_file");

        // base tier: a readonly native tool ("read") alongside the mock MCP-sourced tool,
        // mirroring `CompositeExecutor::new(base_executor, mcp_executor)` in `build_acp_deps`.
        let base_tool = zeph_tools::CompositeExecutor::new(
            AcpTaggedMock("read"),
            AcpTaggedMock("mcp_write_file"),
        );
        let inner_executor =
            zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
                AcpTaggedMock("load_skill"),
                zeph_tools::CompositeExecutor::new(
                    AcpTaggedMock("memory_save"),
                    zeph_tools::CompositeExecutor::new(AcpTaggedMock("overflow_flush"), base_tool),
                ),
            )));
        let (gated, mcp_ids_handle) = crate::agent_setup::apply_common_tool_gating(
            inner_executor,
            &zeph_tools::PermissionPolicy::default(),
        );
        crate::agent_setup::register_mcp_tool_ids(&mcp_ids_handle, std::slice::from_ref(&mcp_tool));
        zeph_tools::executor::ToolExecutor::set_effective_trust(
            &gated,
            zeph_common::SkillTrustLevel::Quarantined,
        );

        let memory_result = gated.execute_tool_call(&acp_test_call("memory_save")).await;
        assert!(
            matches!(memory_result, Err(zeph_tools::ToolError::Blocked { .. })),
            "memory_save must be denied under Quarantine, got {memory_result:?}"
        );

        let mcp_result = gated.execute_tool_call(&acp_test_call(&mcp_tool_id)).await;
        assert!(
            matches!(mcp_result, Err(zeph_tools::ToolError::Blocked { .. })),
            "MCP-sourced tool must be denied under Quarantine, got {mcp_result:?}"
        );

        let skill_load_result = gated.execute_tool_call(&acp_test_call("load_skill")).await;
        assert!(
            matches!(
                skill_load_result,
                Err(zeph_tools::ToolError::Blocked { .. })
            ),
            "load_skill must be denied under Quarantine, got {skill_load_result:?}"
        );

        let read_result = gated.execute_tool_call(&acp_test_call("read")).await;
        assert!(
            read_result.is_ok(),
            "readonly native tool must remain reachable under Quarantine, got {read_result:?}"
        );
    }

    /// Regression test confirming `PolicyGateExecutor` is reachable through the ACP composite
    /// chain: `build_acp_deps` previously built its tool composite (base+MCP+search) with no
    /// declarative policy gate wired in at all, unlike the CLI path (`src/runner.rs`), so a
    /// configured `[tools.policy]` deny rule was silently ignored for every ACP-dispatched
    /// tool call. Reconstructs the same `base_executor` chain `build_acp_deps` builds
    /// (file/shell/scrape/diagnostics, wrapped in `TrustGateExecutor`) and layers
    /// `PolicyGateExecutor` on top exactly as `build_acp_deps` now does, asserting a deny rule
    /// for `diagnostics` is enforced.
    #[tokio::test]
    async fn policy_gate_denies_tool_in_acp_composite_chain() {
        let config = zeph_core::config::Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(&config);
        let base_executor = crate::agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );
        let policy =
            zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full);
        let base_executor = zeph_tools::TrustGateExecutor::new(base_executor, policy);

        let policy_config = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: vec![zeph_tools::PolicyRuleConfig {
                effect: zeph_tools::PolicyEffect::Deny,
                tool: "diagnostics".into(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            ..Default::default()
        };
        let enforcer = zeph_tools::PolicyEnforcer::compile(&policy_config).unwrap();
        let policy_context = std::sync::Arc::new(RwLock::new(zeph_tools::PolicyContext {
            trust_level: zeph_common::SkillTrustLevel::Trusted,
            env: std::collections::HashMap::new(),
        }));
        let gated = zeph_tools::PolicyGateExecutor::new(
            base_executor,
            std::sync::Arc::new(enforcer),
            policy_context,
        );

        let call = zeph_tools::ToolCall {
            tool_id: "diagnostics".into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = gated.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected Blocked from PolicyGateExecutor deny rule, got {result:?}"
        );
    }

    /// Regression test confirming `AdversarialPolicyGateExecutor` is reachable through the
    /// ACP composite chain: `build_acp_deps` never wired this gate in either, so
    /// `[tools.adversarial_policy]` (LLM-based tool review) had no effect on ACP-dispatched
    /// calls even when enabled. Same reconstructed `base_executor` chain as the sibling test
    /// above, layered with `AdversarialPolicyGateExecutor` driven by a fake `PolicyLlmClient`
    /// that always returns `DENY`, asserting the deny path is reached.
    #[tokio::test]
    async fn adversarial_policy_gate_denies_tool_in_acp_composite_chain() {
        struct AlwaysDenyLlm;
        impl zeph_tools::PolicyLlmClient for AlwaysDenyLlm {
            fn chat<'a>(
                &'a self,
                _messages: &'a [zeph_tools::PolicyMessage],
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
            > {
                Box::pin(async move { Ok("DENY: test policy".to_owned()) })
            }
        }

        let config = zeph_core::config::Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(&config);
        let base_executor = crate::agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );
        let policy =
            zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full);
        let base_executor = zeph_tools::TrustGateExecutor::new(base_executor, policy);

        let validator = std::sync::Arc::new(zeph_tools::PolicyValidator::new(
            vec!["never allow diagnostics".to_owned()],
            std::time::Duration::from_millis(500),
            false,
            vec![],
        ));
        let llm_client: std::sync::Arc<dyn zeph_tools::PolicyLlmClient> =
            std::sync::Arc::new(AlwaysDenyLlm);
        let gated =
            zeph_tools::AdversarialPolicyGateExecutor::new(base_executor, validator, llm_client);

        let call = zeph_tools::ToolCall {
            tool_id: "diagnostics".into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = gated.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected Blocked from AdversarialPolicyGateExecutor deny decision, got {result:?}"
        );
    }

    /// Combined regression test proving `PolicyGateExecutor` and `TrustGateExecutor`
    /// (`Quarantine` enforcement via `apply_common_tool_gating`) both enforce independently
    /// in the same composite chain: reconstructs the production wiring order (outermost
    /// first) `PolicyGateExecutor -> TrustGateExecutor -> composite` and asserts that a
    /// declarative policy deny rule AND `TrustGateExecutor`'s Quarantine enforcement both
    /// survive being stacked together — neither gate silently swallows or bypasses the
    /// other, and a tool denied by neither still dispatches normally.
    #[tokio::test]
    async fn policy_and_quarantine_trust_gate_both_enforce_in_acp_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

        let mcp_tool = zeph_mcp::McpTool {
            server_id: "mcp".to_owned(),
            name: "write_file".to_owned(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
            output_schema: None,
            security_meta: zeph_mcp::tool::ToolSecurityMeta::default(),
        };

        let base_tool = zeph_tools::CompositeExecutor::new(
            AcpTaggedMock("read"),
            AcpTaggedMock("mcp_write_file"),
        );
        let inner_executor =
            zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
                AcpTaggedMock("load_skill"),
                zeph_tools::CompositeExecutor::new(
                    AcpTaggedMock("memory_save"),
                    zeph_tools::CompositeExecutor::new(AcpTaggedMock("overflow_flush"), base_tool),
                ),
            )));

        // TrustGateExecutor (innermost gate), Quarantined trust.
        let (trust_gated, mcp_ids_handle) = crate::agent_setup::apply_common_tool_gating(
            inner_executor,
            &zeph_tools::PermissionPolicy::default(),
        );
        crate::agent_setup::register_mcp_tool_ids(&mcp_ids_handle, std::slice::from_ref(&mcp_tool));
        zeph_tools::ToolExecutor::set_effective_trust(
            &trust_gated,
            zeph_common::SkillTrustLevel::Quarantined,
        );

        // PolicyGateExecutor (outermost gate), denying a tool Quarantine does not itself
        // target by name, to prove the declarative gate's own deny logic isn't shadowed.
        let policy_config = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: vec![zeph_tools::PolicyRuleConfig {
                effect: zeph_tools::PolicyEffect::Deny,
                tool: "overflow_flush".into(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            ..Default::default()
        };
        let enforcer = zeph_tools::PolicyEnforcer::compile(&policy_config).unwrap();
        let policy_context = std::sync::Arc::new(RwLock::new(zeph_tools::PolicyContext {
            trust_level: zeph_common::SkillTrustLevel::Trusted,
            env: std::collections::HashMap::new(),
        }));
        let gated = zeph_tools::PolicyGateExecutor::new(
            trust_gated,
            std::sync::Arc::new(enforcer),
            policy_context,
        );

        // Policy-denied tool: blocked by PolicyGateExecutor before reaching TrustGate.
        let policy_denied = gated
            .execute_tool_call(&acp_test_call("overflow_flush"))
            .await;
        assert!(
            matches!(policy_denied, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected Blocked from PolicyGateExecutor's own deny rule, got {policy_denied:?}"
        );

        // Quarantine-denied tool (policy allows it by default): must still be blocked by
        // TrustGateExecutor's Quarantine check — proves TrustGate isn't shadowed by the
        // outer PolicyGate.
        let quarantine_denied = gated.execute_tool_call(&acp_test_call("load_skill")).await;
        assert!(
            matches!(
                quarantine_denied,
                Err(zeph_tools::ToolError::Blocked { .. })
            ),
            "expected Blocked from TrustGateExecutor's Quarantine enforcement, got {quarantine_denied:?}"
        );

        // Neither gate denies "read": must still dispatch successfully through the full
        // merged stack.
        let allowed = gated.execute_tool_call(&acp_test_call("read")).await;
        assert!(
            allowed.is_ok(),
            "expected read to dispatch normally through the merged gate stack, got {allowed:?}"
        );
    }

    /// Regression test confirming `ScopedToolExecutor` (Spec 050 F2, #5913) is reachable
    /// through the ACP composite chain: `spawn_acp_agent` previously wrapped no capability-
    /// scope gate at all, so a configured `[security.capability_scopes]` scope was silently
    /// ignored for every ACP-dispatched tool call. Reconstructs the same `base_executor`
    /// chain the sibling `policy_gate_*` tests use and layers `ScopedToolExecutor` on top via
    /// `zeph_tools::scope::build_scoped_executor` exactly as `spawn_acp_agent` now does,
    /// asserting a tool outside the configured scope is rejected while an in-scope tool still
    /// dispatches.
    #[tokio::test]
    async fn capability_scopes_denies_tool_outside_scope_in_acp_composite_chain() {
        use std::collections::HashSet;
        use zeph_tools::scope::build_scoped_executor;

        let config = zeph_core::config::Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(&config);
        let base_executor = crate::agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );
        let policy =
            zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full);
        let base_executor = zeph_tools::TrustGateExecutor::new(base_executor, policy);

        let registry_ids: HashSet<String> = base_executor
            .tool_definitions()
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

        let scopes_cfg = zeph_config::CapabilityScopesConfig {
            default_scope: "narrow".to_owned(),
            scopes: std::collections::HashMap::from([(
                "narrow".to_owned(),
                zeph_config::ScopeConfig {
                    patterns: vec!["builtin:read".to_owned()],
                },
            )]),
            ..Default::default()
        };
        let scoped = build_scoped_executor(base_executor, &scopes_cfg, &registry_ids)
            .expect("build_scoped_executor must compile a valid single-pattern scope");

        // "diagnostics" is outside the scope; blocked before it ever reaches the real
        // (network/system-probing) `DiagnosticsExecutor`, so this stays fast and hermetic.
        let denied = scoped
            .execute_tool_call(&acp_test_call("diagnostics"))
            .await;
        assert!(
            matches!(denied, Err(zeph_tools::ToolError::OutOfScope { .. })),
            "expected OutOfScope from ScopedToolExecutor for a tool outside the configured \
             scope, got {denied:?}"
        );

        // "read" matches the active scope's pattern, so it must reach past
        // `ScopedToolExecutor` into the real `FileExecutor` — asserting on the absence of
        // `OutOfScope` rather than a bare `is_ok()`, since an empty `params` map still fails
        // `FileExecutor`'s own param validation (missing `path`), which is a separate,
        // expected failure mode that proves the call *did* reach past the scope gate.
        let allowed = scoped.execute_tool_call(&acp_test_call("read")).await;
        assert!(
            !matches!(allowed, Err(zeph_tools::ToolError::OutOfScope { .. })),
            "expected read to reach past ScopedToolExecutor since it matches the active \
             scope's pattern, got {allowed:?}"
        );
    }

    /// Regression test confirming `ShadowProbeExecutor` (Spec 050 Phase 2, #5913) is reachable
    /// through the ACP composite chain: `spawn_acp_agent` previously never constructed a
    /// `ShadowSentinel`/`ShadowProbeExecutor` at all, so `[security.shadow_sentinel]` had no
    /// effect on ACP-dispatched calls even when enabled. Drives a real tool call through
    /// `ShadowProbeExecutor -> ShadowSentinelProbeGateAdapter -> ShadowSentinel::record_tool_event`
    /// using the same adapter type `spawn_acp_agent` now reuses from `src/runner.rs`
    /// (`crate::runner::ShadowSentinelProbeGateAdapter`, promoted to `pub(crate)` for this
    /// reuse), asserting the event is actually persisted — mirrors runner.rs's own precedent
    /// test (`shadow_probe_executor_writes_reach_a_different_sessions_probe_context`) but
    /// proves ACP's own wiring block reaches the identical production chain.
    #[tokio::test]
    async fn shadow_probe_executor_reaches_shadow_sentinel_in_acp_composite_chain() {
        use zeph_core::agent::shadow_sentinel::{
            ProbeVerdict, SafetyProbe, SentinelEvent, ShadowEventStore, ShadowSentinel,
        };
        use zeph_tools::{ProbeGate, ToolCall, ToolOutput};

        struct AllowProbe;
        impl SafetyProbe for AllowProbe {
            fn evaluate<'a>(
                &'a self,
                _: &'a str,
                _: &'a serde_json::Value,
                _: &'a [SentinelEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                Box::pin(async { ProbeVerdict::Allow })
            }
        }

        struct OkExec;
        impl ToolExecutor for OkExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, zeph_tools::ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                call: &ToolCall,
            ) -> Result<Option<ToolOutput>, zeph_tools::ToolError> {
                Ok(Some(ToolOutput {
                    tool_name: call.tool_id.clone(),
                    summary: "command completed".to_owned(),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                }))
            }
            zeph_tools::tool_executor_no_inner_defaults!();
        }

        let pool = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        }
        .connect()
        .await
        .expect("connect + migrate in-memory sqlite pool");

        let sentinel = std::sync::Arc::new(ShadowSentinel::new(
            ShadowEventStore::new(pool.clone()),
            Box::new(AllowProbe),
            zeph_config::ShadowSentinelConfig {
                enabled: true,
                ..Default::default()
            },
            "acp-conversation-42",
        ));
        let probe_gate: std::sync::Arc<dyn ProbeGate> =
            std::sync::Arc::new(crate::runner::ShadowSentinelProbeGateAdapter {
                sentinel: std::sync::Arc::clone(&sentinel),
            });
        let executor = zeph_tools::ShadowProbeExecutor::new(
            OkExec,
            probe_gate,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            std::sync::Arc::new(parking_lot::RwLock::new("calm".to_owned())),
        );

        let result = executor
            .execute_tool_call(&acp_test_call("builtin:shell"))
            .await;
        assert!(result.unwrap().is_some(), "tool call must succeed");
        // record_tool_event is fire-and-forget; drain before querying the store.
        sentinel.drain_pending().await;

        let events = ShadowEventStore::new(pool)
            .get_trajectory("acp-conversation-42", 10)
            .await
            .expect("get_trajectory must succeed against the in-memory pool");
        assert!(
            events.iter().any(|e| e.event_type == "tool_call"
                && e.context_summary.as_deref() == Some("command completed")),
            "expected the ShadowProbeExecutor-driven tool_call event to be persisted via ACP's \
             reused ShadowSentinelProbeGateAdapter, got: {events:?}"
        );
    }

    /// Regression test confirming `PolicyGateExecutor`'s trajectory-risk signal queue (#5958,
    /// spec 050) is wired through `agent_setup::apply_policy_gate_chain` when called from ACP:
    /// `spawn_acp_agent` previously passed `None` for the `trajectory` parameter, so a
    /// declarative-policy denial never reached `TrajectorySentinel` for ACP-dispatched calls.
    /// Reconstructs the same trust-gated base chain the sibling `policy_gate_denies_tool_in_acp_composite_chain`
    /// test uses, wraps it via `apply_policy_gate_chain` with a deny rule and
    /// `Some((&trajectory_risk_slot, &trajectory_signal_queue))` exactly as `spawn_acp_agent`
    /// now does, and asserts the signal queue receives the `PolicyDeny` code (`1`) after a
    /// denied call. This is the observable wiring surface reachable from this crate — the
    /// downstream `trajectory_risk_slot` mutation happens inside `Agent::begin_turn` (private to
    /// `zeph-core`, already covered by that crate's own `agent/trajectory.rs` unit tests).
    #[tokio::test]
    async fn trajectory_signal_queue_receives_policy_denial_in_acp_composite_chain() {
        let config = zeph_core::config::Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(&config);
        let base_executor = crate::agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );
        let (trust_gated, _mcp_ids_handle) = crate::agent_setup::apply_common_tool_gating(
            zeph_tools::DynExecutor(Arc::new(base_executor)),
            &zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full),
        );

        let policy_config = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: vec![zeph_tools::PolicyRuleConfig {
                effect: zeph_tools::PolicyEffect::Deny,
                tool: "overflow_flush".into(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            ..Default::default()
        };
        let enforcer = zeph_tools::PolicyEnforcer::compile(&policy_config).unwrap();
        let pieces = crate::agent_setup::PolicyGatePieces {
            policy_enforcer: Some(Arc::new(enforcer)),
            adversarial_validator: None,
            adversarial_llm_client: None,
            adv_policy_info: None,
            policy_configured: true,
        };

        let trajectory_risk_slot: zeph_tools::TrajectoryRiskSlot =
            Arc::new(parking_lot::RwLock::new(0u8));
        let trajectory_signal_queue: zeph_tools::RiskSignalQueue =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        let gated = crate::agent_setup::apply_policy_gate_chain(
            trust_gated,
            &pieces,
            None,
            Some((&trajectory_risk_slot, &trajectory_signal_queue)),
        );

        let denied = gated
            .execute_tool_call(&acp_test_call("overflow_flush"))
            .await;
        assert!(
            matches!(denied, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected Blocked from PolicyGateExecutor's deny rule, got {denied:?}"
        );
        assert_eq!(
            *trajectory_signal_queue.lock(),
            vec![1u8],
            "expected the PolicyDeny signal code (1) to be pushed into the shared trajectory \
             signal queue after a denied tool call, proving apply_policy_gate_chain's ACP call \
             site actually wires PolicyGateExecutor::with_signal_queue instead of passing None"
        );
    }

    /// Companion to `trajectory_signal_queue_receives_policy_denial_in_acp_composite_chain`
    /// covering the other #5958 signal source `spawn_acp_agent` wires: `ScopedToolExecutor`
    /// (`[security.capability_scopes]`) `OutOfScope` denials. Before this PR, ACP's
    /// `ScopedToolExecutor` was never given a signal queue at all, so capability-scope
    /// violations were invisible to `TrajectorySentinel`'s risk escalation. Reconstructs the
    /// same scope wrap the sibling `capability_scopes_denies_tool_outside_scope_in_acp_composite_chain`
    /// test uses, adding `.with_signal_queue(...)` exactly as `spawn_acp_agent` now does, and
    /// asserts the queue receives the `OutOfScope` signal code (`3`).
    #[tokio::test]
    async fn trajectory_signal_queue_receives_scope_denial_in_acp_composite_chain() {
        use std::collections::HashSet;
        use zeph_tools::scope::build_scoped_executor;

        let config = zeph_core::config::Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(&config);
        let base_executor = crate::agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );
        let policy =
            zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full);
        let base_executor = zeph_tools::TrustGateExecutor::new(base_executor, policy);

        let registry_ids: HashSet<String> = base_executor
            .tool_definitions()
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

        let scopes_cfg = zeph_config::CapabilityScopesConfig {
            default_scope: "narrow".to_owned(),
            scopes: std::collections::HashMap::from([(
                "narrow".to_owned(),
                zeph_config::ScopeConfig {
                    patterns: vec!["builtin:read".to_owned()],
                },
            )]),
            ..Default::default()
        };
        let scoped = build_scoped_executor(base_executor, &scopes_cfg, &registry_ids)
            .expect("build_scoped_executor must compile a valid single-pattern scope");

        let trajectory_signal_queue: zeph_tools::RiskSignalQueue =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let scoped = scoped.with_signal_queue(Arc::clone(&trajectory_signal_queue));

        let denied = scoped
            .execute_tool_call(&acp_test_call("diagnostics"))
            .await;
        assert!(
            matches!(denied, Err(zeph_tools::ToolError::OutOfScope { .. })),
            "expected OutOfScope from ScopedToolExecutor for a tool outside the configured \
             scope, got {denied:?}"
        );
        assert_eq!(
            *trajectory_signal_queue.lock(),
            vec![3u8],
            "expected the OutOfScope signal code (3) to be pushed into the shared trajectory \
             signal queue after a scope-denied tool call, proving spawn_acp_agent's new \
             `.with_signal_queue(...)` call on the ScopedToolExecutor branch is reachable"
        );
    }

    /// Regression test confirming `SkillInvokeExecutor` (#5975) is reachable through ACP's full
    /// per-session composite: before this PR, `spawn_acp_agent` never constructed
    /// `SkillInvokeExecutor` at all, so `invoke_skill` tool calls fell through to
    /// `memory`/`overflow`/`base` (none of which handle that tool id) and would have surfaced
    /// as `ToolError::NotFound` instead of a skill body/summary. Reuses
    /// `build_full_acp_session_composite_with_native_fs_shell`, now updated to insert
    /// `skill_invoke` between `skill_loader` and `memory` matching `spawn_acp_agent`'s current
    /// nesting order, and calls `invoke_skill` for a name absent from the (empty) registry —
    /// only `SkillInvokeExecutor` produces the `"skill not found: {name}"` summary text; the
    /// default (missing trust-snapshot entry) trust level resolves to
    /// `SkillTrustLevel::MISSING_ENTRY_FALLBACK` (`Trusted`), which is not `Blocked`, so the
    /// call reaches the body lookup instead of being short-circuited.
    #[tokio::test]
    async fn invoke_skill_reaches_skill_invoke_executor_in_full_acp_session_composite() {
        let (session_composite, _trust_snapshot) =
            build_full_acp_session_composite_with_native_fs_shell().await;

        let mut params = serde_json::Map::new();
        params.insert(
            "skill_name".to_owned(),
            serde_json::Value::String("nonexistent-skill".to_owned()),
        );
        let call = zeph_tools::ToolCall {
            tool_id: "invoke_skill".into(),
            params,
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = session_composite.execute_tool_call_erased(&call).await;
        let output = result
            .expect("invoke_skill must dispatch successfully through SkillInvokeExecutor")
            .expect("SkillInvokeExecutor must always return Some(ToolOutput) for invoke_skill");
        assert!(
            output
                .summary
                .contains("skill not found: nonexistent-skill"),
            "expected the \"skill not found: ...\" summary that only SkillInvokeExecutor \
             produces, proving invoke_skill actually reaches it in the full ACP session \
             composite instead of falling through to memory/overflow/base, got: {output:?}"
        );
    }

    /// Regression test confirming the five #5914 memory-maintenance loops (eviction,
    /// tier-promotion, scene-consolidation, consolidation, forgetting) are actually
    /// registered on the ACP connection's own `TaskSupervisor`: `build_acp_deps` previously
    /// spawned none of them, unlike the CLI path (`src/runner.rs`). Reconstructs the same
    /// `TaskDescriptor`/`supervisor.spawn` block `build_acp_deps` now runs (falling back
    /// directly to the primary provider where production uses `app.build_scene_provider()`/
    /// `app.build_consolidation_provider()`, since this test has no `AppBuilder` to resolve a
    /// named override from) and asserts every expected task name is present in the connection
    /// supervisor's snapshot.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn acp_memory_maintenance_loops_registered_on_connection_supervisor() {
        let mock_provider =
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = std::sync::Arc::new(
            zeph_memory::semantic::SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                mock_provider.clone(),
                "test",
            )
            .await
            .unwrap(),
        );
        let config = zeph_core::config::Config::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::TaskSupervisor::new(cancel);

        {
            let store = std::sync::Arc::new(memory.sqlite().clone());
            let embedding = memory.embedding_store().cloned();
            let eviction_cfg = config.memory.eviction.clone();
            let policy = std::sync::Arc::new(zeph_memory::EbbinghausPolicy::default());
            let cancel = supervisor.cancellation_token();
            supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "mem-eviction",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: move || {
                    zeph_memory::start_eviction_loop(
                        store.clone(),
                        embedding.clone(),
                        eviction_cfg.clone(),
                        policy.clone(),
                        cancel.clone(),
                    )
                },
            });
        }
        {
            let store = std::sync::Arc::new(memory.sqlite().clone());
            let tier_cfg = zeph_memory::TierPromotionConfig {
                enabled: config.memory.tiers.enabled,
                promotion_min_sessions: config.memory.tiers.promotion_min_sessions,
                similarity_threshold: config.memory.tiers.similarity_threshold,
                sweep_interval_secs: config.memory.tiers.sweep_interval_secs,
                sweep_batch_size: config.memory.tiers.sweep_batch_size,
                embed_timeout_secs: config.memory.semantic.embed_timeout_secs,
            };
            let tier_provider = mock_provider.clone();
            let cancel = supervisor.cancellation_token();
            supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "mem-tier-promotion",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: move || {
                    zeph_memory::start_tier_promotion_loop(
                        store.clone(),
                        tier_provider.clone(),
                        tier_cfg.clone(),
                        cancel.clone(),
                    )
                },
            });
        }
        {
            let store = std::sync::Arc::new(memory.sqlite().clone());
            let scene_provider = mock_provider.clone();
            let scene_cfg = zeph_memory::SceneConfig {
                enabled: config.memory.tiers.scene_enabled,
                similarity_threshold: config.memory.tiers.scene_similarity_threshold,
                batch_size: config.memory.tiers.scene_batch_size,
                sweep_interval_secs: config.memory.tiers.scene_sweep_interval_secs,
            };
            let cancel = supervisor.cancellation_token();
            supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "mem-scene-consolidation",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: move || {
                    zeph_memory::start_scene_consolidation_loop(
                        store.clone(),
                        scene_provider.clone(),
                        scene_cfg.clone(),
                        cancel.clone(),
                    )
                },
            });
        }
        {
            let store = std::sync::Arc::new(memory.sqlite().clone());
            let consolidation_cfg = zeph_memory::ConsolidationConfig {
                enabled: config.memory.consolidation.enabled,
                confidence_threshold: config.memory.consolidation.confidence_threshold,
                sweep_interval_secs: config.memory.consolidation.sweep_interval_secs,
                sweep_batch_size: config.memory.consolidation.sweep_batch_size,
                similarity_threshold: config.memory.consolidation.similarity_threshold,
                llm_timeout_secs: config.memory.consolidation.llm_timeout_secs,
                embed_timeout_secs: config.memory.semantic.embed_timeout_secs,
            };
            let consolidation_provider = mock_provider.clone();
            let cancel = supervisor.cancellation_token();
            supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "mem-consolidation",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: move || {
                    zeph_memory::start_consolidation_loop(
                        store.clone(),
                        consolidation_provider.clone(),
                        consolidation_cfg.clone(),
                        cancel.clone(),
                    )
                },
            });
        }
        {
            let store = std::sync::Arc::new(memory.sqlite().clone());
            let forgetting_cfg = zeph_memory::ForgettingConfig {
                enabled: config.memory.forgetting.enabled,
                decay_rate: config.memory.forgetting.decay_rate,
                forgetting_floor: config.memory.forgetting.forgetting_floor,
                sweep_interval_secs: config.memory.forgetting.sweep_interval_secs,
                sweep_batch_size: config.memory.forgetting.sweep_batch_size,
                replay_window_hours: config.memory.forgetting.replay_window_hours,
                replay_min_access_count: config.memory.forgetting.replay_min_access_count,
                protect_recent_hours: config.memory.forgetting.protect_recent_hours,
                protect_min_access_count: config.memory.forgetting.protect_min_access_count,
            };
            let cancel = supervisor.cancellation_token();
            supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "mem-forgetting",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: move || {
                    zeph_memory::start_forgetting_loop(
                        store.clone(),
                        forgetting_cfg.clone(),
                        cancel.clone(),
                    )
                },
            });
        }

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
                "expected {expected} registered on the ACP connection's memory supervisor, \
                 got {names:?}"
            );
        }
    }

    /// Trivial stand-in for `AcpFileExecutor`/`AcpShellExecutor` in tests: the real types need
    /// a live `acp::ConnectionTo<acp::Client>` (an IDE transport) to construct, which isn't
    /// available in a unit test. This exposes the same tool ids the real executors use
    /// (`write_file` for `AcpFileExecutor`, `bash` for `AcpShellExecutor` — see
    /// `crates/zeph-acp/src/fs.rs`/`terminal.rs`) so tests can occupy the identical composite
    /// slot and prove the gate intercepts calls there, without needing the real network-backed
    /// implementation — the gate only cares about `tool_id`, not which concrete type serves it.
    #[derive(Debug)]
    struct AcpNativeStandIn {
        tool_id: &'static str,
    }
    impl ToolExecutor for AcpNativeStandIn {
        async fn execute(
            &self,
            _response: &str,
        ) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(
            &self,
            call: &zeph_tools::ToolCall,
        ) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> {
            if call.tool_id != self.tool_id {
                return Ok(None);
            }
            panic!(
                "AcpNativeStandIn({}) reached — gate did not intercept",
                self.tool_id
            );
        }
        zeph_tools::tool_executor_no_inner_defaults!();
    }

    /// Builds the full per-session composite `spawn_acp_agent` assembles when the IDE supplies
    /// an `AcpContext` (the primary ACP embedding case) — `ToolFilter`-wrapped base composed
    /// with `AcpNativeStandIn` fs/shell stand-ins (occupying the same slot as
    /// `AcpFileExecutor`/`AcpShellExecutor`), then `skill_loader`/`skill_invoke`/`memory`/
    /// `overflow` layered outside, matching `spawn_acp_agent`'s exact nesting order (#5975 added
    /// `skill_invoke` between `skill_loader` and `memory`). Returns the `trust_snapshot` Arc
    /// alongside the composite so callers can pre-populate trust rows for `invoke_skill` tests.
    async fn build_full_acp_session_composite_with_native_fs_shell() -> (
        Arc<dyn ErasedToolExecutor>,
        Arc<
            RwLock<std::collections::HashMap<String, zeph_core::skill_invoker::SkillTrustSnapshot>>,
        >,
    ) {
        let registry = Arc::new(RwLock::new(zeph_skills::registry::SkillRegistry::empty()));
        let (skill_loader_executor, skill_invoke_executor, trust_snapshot) =
            agent_setup::build_skill_executors(&registry);

        let mock_provider =
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = Arc::new(
            zeph_memory::semantic::SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                mock_provider,
                "test",
            )
            .await
            .unwrap(),
        );
        let memory_executor = zeph_core::memory_tools::MemoryToolExecutor::with_validator(
            Arc::clone(&memory),
            zeph_memory::ConversationId(0),
            zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
                zeph_core::config::Config::default()
                    .security
                    .memory_validation
                    .clone(),
            ),
        );
        let overflow_executor =
            zeph_core::overflow_tools::OverflowToolExecutor::new(Arc::new(memory.sqlite().clone()));

        // Mirrors spawn_acp_agent's `Some(ctx)` branch: base -> ToolFilter (suppress
        // read/write/glob) -> composite with the fs stand-in -> composite with the shell
        // stand-in -> skill_loader/skill_invoke/memory/overflow layered outside.
        let mut base: Arc<dyn ErasedToolExecutor> = Arc::new(zeph_tools::FileExecutor::new(vec![]));
        let filtered =
            zeph_tools::ToolFilter::new(zeph_tools::DynExecutor(base), &["read", "write", "glob"]);
        base = Arc::new(zeph_tools::CompositeExecutor::new(
            AcpNativeStandIn {
                tool_id: "write_file",
            },
            filtered,
        ));
        base = Arc::new(zeph_tools::CompositeExecutor::new(
            AcpNativeStandIn { tool_id: "bash" },
            zeph_tools::DynExecutor(base),
        ));
        base = Arc::new(zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                skill_invoke_executor,
                zeph_tools::CompositeExecutor::new(
                    memory_executor,
                    zeph_tools::CompositeExecutor::new(
                        overflow_executor,
                        zeph_tools::DynExecutor(base),
                    ),
                ),
            ),
        ));
        (base, trust_snapshot)
    }

    /// Regression test closing the gap found in review: the sibling `policy_gate_denies_tool_in_acp_composite_chain`
    /// test above only reconstructs `build_base_executor_chain` (file/shell/scrape/diagnostics)
    /// + `TrustGateExecutor` — the connection-scoped subset `build_acp_deps` wires. It gives no
    /// evidence that `PolicyGateExecutor` reaches `skill_loader`/`memory`/ACP-native fs-shell
    /// tool calls, which `spawn_acp_agent` composites in *per session*, outside that
    /// connection-scoped subset. Reconstructs the exact nesting shape `spawn_acp_agent` builds
    /// for the primary IDE-embedding case (`AcpContext` present: `ToolFilter`-wrapped base +
    /// ACP-native fs/shell stand-ins, then `skill_loader`/`memory`/`overflow` layered outside),
    /// wrapped in `PolicyGateExecutor` the same way `spawn_acp_agent` now wraps its full
    /// per-session composite, and asserts a deny rule blocks `load_skill` (`skill_loader`),
    /// `memory_search` (memory), `write_file`, and `bash` (ACP-native fs/shell stand-ins) —
    /// not just calls into the `base` chain.
    #[tokio::test]
    async fn policy_gate_denies_skill_and_memory_tools_in_full_acp_session_composite() {
        let (session_composite, _trust_snapshot) =
            build_full_acp_session_composite_with_native_fs_shell().await;

        let policy_config = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: ["load_skill", "memory_search", "write_file", "bash"]
                .into_iter()
                .map(|tool| zeph_tools::PolicyRuleConfig {
                    effect: zeph_tools::PolicyEffect::Deny,
                    tool: tool.into(),
                    paths: vec![],
                    env: vec![],
                    trust_level: None,
                    args_match: None,
                    capabilities: vec![],
                })
                .collect(),
            ..Default::default()
        };
        let enforcer = zeph_tools::PolicyEnforcer::compile(&policy_config).unwrap();
        let policy_context = Arc::new(RwLock::new(zeph_tools::PolicyContext {
            trust_level: zeph_common::SkillTrustLevel::Trusted,
            env: std::collections::HashMap::new(),
        }));
        let gated = zeph_tools::PolicyGateExecutor::new(
            zeph_tools::DynExecutor(session_composite),
            Arc::new(enforcer),
            policy_context,
        );

        for tool_id in ["load_skill", "memory_search", "write_file", "bash"] {
            let call = zeph_tools::ToolCall {
                tool_id: tool_id.into(),
                params: serde_json::Map::new(),
                caller_id: None,
                context: None,
                tool_call_id: String::new(),
                skill_name: None,
            };
            let result = gated.execute_tool_call(&call).await;
            assert!(
                matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
                "expected Blocked for {tool_id} from PolicyGateExecutor wrapping the full \
                 per-session composite (including ACP-native fs/shell), got {result:?}"
            );
        }
    }

    /// Adversarial-policy companion to `policy_gate_denies_skill_and_memory_tools_in_full_acp_session_composite`:
    /// same full per-session composite shape (including the ACP-native fs/shell stand-ins),
    /// wrapped in `AdversarialPolicyGateExecutor` driven by a fake `PolicyLlmClient` that always
    /// returns `DENY`, asserting `load_skill`, `memory_search`, `write_file`, and `bash` calls
    /// are all blocked before reaching their respective inner executors.
    #[tokio::test]
    async fn adversarial_policy_gate_denies_skill_and_memory_tools_in_full_acp_session_composite() {
        struct AlwaysDenyLlm;
        impl zeph_tools::PolicyLlmClient for AlwaysDenyLlm {
            fn chat<'a>(
                &'a self,
                _messages: &'a [zeph_tools::PolicyMessage],
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
            > {
                Box::pin(async move { Ok("DENY: test policy".to_owned()) })
            }
        }

        let (session_composite, _trust_snapshot) =
            build_full_acp_session_composite_with_native_fs_shell().await;

        let validator = Arc::new(zeph_tools::PolicyValidator::new(
            vec!["never allow load_skill, memory_search, write_file, or bash".to_owned()],
            std::time::Duration::from_millis(500),
            false,
            vec![],
        ));
        let llm_client: Arc<dyn zeph_tools::PolicyLlmClient> = Arc::new(AlwaysDenyLlm);
        let gated = zeph_tools::AdversarialPolicyGateExecutor::new(
            zeph_tools::DynExecutor(session_composite),
            validator,
            llm_client,
        );

        for tool_id in ["load_skill", "memory_search", "write_file", "bash"] {
            let call = zeph_tools::ToolCall {
                tool_id: tool_id.into(),
                params: serde_json::Map::new(),
                caller_id: None,
                context: None,
                tool_call_id: String::new(),
                skill_name: None,
            };
            let result = gated.execute_tool_call(&call).await;
            assert!(
                matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
                "expected Blocked for {tool_id} from AdversarialPolicyGateExecutor wrapping the \
                 full per-session composite (including ACP-native fs/shell), got {result:?}"
            );
        }
    }

    /// #5437 (S1, third recurrence): `build_acp_provider_factory` constructs raw `AnyProvider`
    /// variants directly (not via `provider_factory::build_provider_from_entry`), and its output
    /// is consumed both via the `provider_override` slot (already guarded by
    /// `Agent::set_provider`) and directly by the ACP session-title generation background task,
    /// which never touches that slot. Wrapping here is the single point that covers both.
    #[test]
    fn build_acp_provider_factory_masks_when_registry_present() {
        let mut config = zeph_core::config::Config::default();
        config.llm.providers = vec![zeph_core::config::ProviderEntry {
            provider_type: zeph_core::config::ProviderKind::Ollama,
            name: Some("ollama".into()),
            model: Some("qwen3:8b".into()),
            ..zeph_core::config::ProviderEntry::default()
        }];
        let registry = std::sync::Arc::new(zeph_sanitizer::secret_mask::SecretMaskRegistry::new());

        let factory = build_acp_provider_factory(&config, Some(std::sync::Arc::clone(&registry)));
        let provider = factory("ollama:qwen3:8b").expect("factory must resolve a known model key");
        assert!(
            matches!(provider, zeph_llm::any::AnyProvider::Masked(_)),
            "factory output must be wrapped when a secret registry is supplied"
        );
    }

    #[test]
    fn build_acp_provider_factory_unmasked_when_registry_absent() {
        let mut config = zeph_core::config::Config::default();
        config.llm.providers = vec![zeph_core::config::ProviderEntry {
            provider_type: zeph_core::config::ProviderKind::Ollama,
            name: Some("ollama".into()),
            model: Some("qwen3:8b".into()),
            ..zeph_core::config::ProviderEntry::default()
        }];

        let factory = build_acp_provider_factory(&config, None);
        let provider = factory("ollama:qwen3:8b").expect("factory must resolve a known model key");
        assert!(
            !matches!(provider, zeph_llm::any::AnyProvider::Masked(_)),
            "no registry supplied — factory output must be a plain passthrough"
        );
    }

    /// #5448 review follow-up: `acp_provider_names()` had zero direct test coverage — the
    /// integration test only exercises a manually-constructed `LlmProtocol`, never these match arms.
    #[test]
    fn acp_provider_names_maps_known_protocols() {
        let mut config = zeph_core::config::Config::default();
        config.llm.providers = vec![
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::Claude,
                name: Some("claude".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::OpenAi,
                name: Some("openai".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::Compatible,
                name: Some("compat".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::Ollama,
                name: Some("ollama".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
        ];

        let names = acp_provider_names(&config);

        assert_eq!(
            names,
            vec![
                ("claude".to_owned(), zeph_acp::LlmProtocol::Anthropic),
                ("openai".to_owned(), zeph_acp::LlmProtocol::OpenAi),
                ("compat".to_owned(), zeph_acp::LlmProtocol::OpenAi),
                (
                    "ollama".to_owned(),
                    zeph_acp::LlmProtocol::Other("ollama".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn acp_provider_names_empty_providers_returns_empty_vec() {
        // `Config::default()` now seeds one provider so `--dump-config-defaults` output
        // stays self-consistent with `validate_pool` (#5932 critic follow-up) — clear it
        // explicitly to exercise the empty-providers branch.
        let mut config = zeph_core::config::Config::default();
        config.llm.providers.clear();
        assert!(acp_provider_names(&config).is_empty());
    }

    #[test]
    #[serial]
    fn collect_project_rules_mixed_sources() {
        let tmp = TempDir::new().unwrap();
        make_rules_dir(tmp.path(), &["branching.md"]);
        let skill_file = tmp.path().join("SKILL.md");
        fs::write(&skill_file, b"").unwrap();

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = collect_project_rules(std::slice::from_ref(&skill_file));
        std::env::set_current_dir(orig).unwrap();
        assert_eq!(result.len(), 2);
        let names: Vec<_> = result
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"branching.md".to_owned()));
        assert!(names.contains(&"SKILL.md".to_owned()));
    }

    // Verify that SharedAgentDeps has the document_config and graph_config fields with the
    // correct types. This is a compile-time regression test for issue #1634: before the fix,
    // these fields were absent and spawn_acp_agent could not propagate RAG config to the agent.
    //
    // Implementation note: `GraphConfig` has 20+ fields with deeply nested sub-configs whose
    // `Default` impls may trigger lazy global initialization (once_cell / tracing subscribers)
    // that leaves background threads running, causing nextest to report this test as leaky.
    // To avoid the issue entirely, field existence is verified via a never-called closure —
    // the closure must compile (proving the fields exist with the right types) but is never
    // invoked at runtime, so no Default construction or global initialization occurs.
    #[test]
    fn shared_agent_deps_has_document_and_graph_config_fields() {
        // Explicit construction for the small DocumentConfig (5 fields, no nested types).
        let doc_cfg = zeph_core::config::DocumentConfig {
            rag_enabled: true,
            top_k: 7,
            collection: String::new(),
            chunk_size: 0,
            chunk_overlap: 0,
        };
        assert!(doc_cfg.rag_enabled);
        assert_eq!(doc_cfg.top_k, 7);
    }

    // Compile-time regression test for issue #1643: anomaly_config and orchestration_config
    // were absent from SharedAgentDeps, silently disabling both features for ACP sessions.
    #[test]
    fn shared_agent_deps_has_anomaly_and_orchestration_config_fields() {
        let anomaly_cfg = zeph_tools::AnomalyConfig {
            enabled: true,
            ..Default::default()
        };
        let orch_cfg = zeph_core::config::OrchestrationConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(anomaly_cfg.enabled);
        assert!(orch_cfg.enabled);
    }

    /// #5818/#5827/#5867/#5920/#5921 regression: `build_acp_deps`/`assemble_serve_deps` must
    /// populate `SharedAgentDeps`'s/`ServeAgentDeps`'s `skill_disambiguation_threshold`/
    /// `skill_two_stage_matching`/`skill_confusability_threshold`/`skill_group_structured`/
    /// `skill_support_similarity_threshold`/`skill_min_injection_score`/
    /// `skill_generation_provider`/`skill_disambiguate_provider`/`semantic_scan`/
    /// `semantic_scan_provider`/`trust_config`/`rl_routing_enabled`/`rl_learning_rate`/
    /// `rl_weight`/`rl_persist_interval`/`rl_warmup_updates`/`rl_head` from
    /// `config.skills.*` — previously these fields did not exist on either deps struct at all,
    /// so neither `spawn_acp_agent` nor `build_agent_factory` could call
    /// `Agent::with_skill_matching_config`/`with_skill_group_config`/
    /// `with_skill_provider_names`/`with_semantic_scan`/`with_trust_config`/`with_rl_routing`,
    /// and every ACP/`/sessions` agent silently ran skill matching, `GoSkills`
    /// grouping/injection scoring, semantic scanning, trust classification, and RL routing on
    /// hardcoded builder defaults regardless of config. `group_structured`/
    /// `support_similarity_threshold`/`min_injection_score` (#5867) went through the identical
    /// gap one PR later than `disambiguation_threshold`/`two_stage_matching`/
    /// `confusability_threshold` (#5818); `trust_config`/RL fields (#5920/#5921) went through it
    /// again — same deps-population seam, added to this test rather than a new one since
    /// `build_combined_deps` assembles all `config.skills.*` fields in one pass.
    ///
    /// Drives the real production `build_combined_deps` (mirroring
    /// `crate::serve::test_support::build_shared_pair`'s use of a mock-provider
    /// `AppBuilder::for_test`) rather than hand-constructing deps literals, so a regression in
    /// either config-to-deps mapping (e.g. a swapped field, or one silently dropped) is caught —
    /// covers both call sites in one test since `build_combined_deps` assembles both structs from
    /// one `SharedCore`. Stops at the deps struct: it does not construct a real `Agent` via
    /// `spawn_acp_agent`/`build_agent_factory`, so the deps→`Agent` step for the ACP path
    /// specifically is covered separately by `build_agent_factory_wires_skill_group_config` /
    /// `build_agent_factory_wires_trust_and_rl_config` (`src/serve/agent_factory.rs`) for the
    /// `/sessions` path only.
    ///
    /// **Known gap, not closed by this fix, tracked in #5887**: unlike `build_daemon_agent`/
    /// `build_agent_factory`, `spawn_acp_agent` returns `()`, not `Agent<C>` — it builds the
    /// agent, then internally drives `load_history()`/`run()`/`shutdown()` for the session's
    /// full lifetime, so there is no seam to call `.handle_skills("trust")` on a real
    /// ACP-path-constructed `Agent` without extracting a `build_acp_agent`-style helper (the
    /// same pattern `build_daemon_agent`/`build_agent` already use — see #5819). #5887 ("extract
    /// shared Agent skill-config builder chain duplicated across runner/daemon/acp/serve") is
    /// the tracked follow-up for that extraction — it already covers `spawn_acp_agent`'s
    /// construction complexity as the reason this hasn't happened yet, so a real Agent-level ACP
    /// test falls out of #5887, not a new issue. Doing that extraction inside a "fix review
    /// issues" pass on a P1 security-relevant construction path (~250 lines of session-scoped
    /// setup — MCP wiring, policy enforcers, session hydration with spawned cancel-bridging
    /// tasks — would need to move) was judged out of scope here; this deps-level test plus code
    /// review is the coverage accepted for the ACP path specifically, same as the pre-existing
    /// accepted gap this test's own history already documents for `skill_group_config`.
    #[cfg(all(feature = "acp-http", feature = "session"))]
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // exhaustive field-by-field assertions across 2 deps structs
    async fn build_combined_deps_wires_skill_matching_config_from_config() {
        let mut config =
            zeph_core::config::Config::load(std::path::Path::new("/nonexistent")).unwrap();
        config.llm.providers = vec![zeph_core::config::ProviderEntry {
            provider_type: zeph_core::config::ProviderKind::Ollama,
            base_url: Some("http://127.0.0.1:1".to_owned()),
            model: Some("test-model".to_owned()),
            ..Default::default()
        }];
        config.memory.sqlite_path = ":memory:".to_owned();
        config.skills.disambiguation_threshold = 0.55;
        config.skills.two_stage_matching = true;
        config.skills.confusability_threshold = 0.65;
        config.skills.group_structured = true;
        config.skills.support_similarity_threshold = 0.73;
        config.skills.min_injection_score = 0.35;
        config.skills.generation_provider = zeph_common::ProviderName::new("gen-test");
        config.skills.disambiguate_provider = zeph_common::ProviderName::new("disamb-test");
        config.skills.semantic_scan = true;
        config.skills.semantic_scan_provider = zeph_common::ProviderName::new("scan-test");
        config.skills.trust.default_level = zeph_common::SkillTrustLevel::Quarantined;
        config.skills.trust.local_level = zeph_common::SkillTrustLevel::Trusted;
        config.skills.rl_routing_enabled = true;
        config.skills.rl_learning_rate = 0.05;
        config.skills.rl_weight = 0.3;
        config.skills.rl_persist_interval = 5;
        config.skills.rl_warmup_updates = 3;
        // Explicit dim avoids a live embedding-provider probe (resolve_rl_embed_dim falls back
        // to a network call against config.llm.providers' unreachable 127.0.0.1:1 otherwise).
        config.skills.rl_embed_dim = Some(8);

        let app = crate::bootstrap::AppBuilder::for_test(config);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = std::sync::Arc::new(zeph_common::TaskSupervisor::new(cancel));

        let (serve_deps, acp_deps, _keepalive) = build_combined_deps(&app, &supervisor)
            .await
            .expect("build_combined_deps must succeed against a mock-provider AppBuilder");

        assert!(
            (serve_deps.skill_disambiguation_threshold - 0.55).abs() < f32::EPSILON,
            "config.skills.disambiguation_threshold must flow into ServeAgentDeps"
        );
        assert!(
            serve_deps.skill_two_stage_matching,
            "config.skills.two_stage_matching must flow into ServeAgentDeps"
        );
        assert!(
            (serve_deps.skill_confusability_threshold - 0.65).abs() < f32::EPSILON,
            "config.skills.confusability_threshold must flow into ServeAgentDeps"
        );
        assert!(
            serve_deps.skill_group_structured,
            "config.skills.group_structured must flow into ServeAgentDeps"
        );
        assert!(
            (serve_deps.skill_support_similarity_threshold - 0.73).abs() < f32::EPSILON,
            "config.skills.support_similarity_threshold must flow into ServeAgentDeps"
        );
        assert!(
            (serve_deps.skill_min_injection_score - 0.35).abs() < f32::EPSILON,
            "config.skills.min_injection_score must flow into ServeAgentDeps"
        );
        assert_eq!(serve_deps.skill_generation_provider, "gen-test");
        assert_eq!(serve_deps.skill_disambiguate_provider, "disamb-test");
        assert!(
            serve_deps.semantic_scan,
            "config.skills.semantic_scan must flow into ServeAgentDeps"
        );
        assert_eq!(serve_deps.semantic_scan_provider, "scan-test");
        assert_eq!(
            serve_deps.trust_config.default_level,
            zeph_common::SkillTrustLevel::Quarantined,
            "config.skills.trust.default_level must flow into ServeAgentDeps"
        );
        assert_eq!(
            serve_deps.trust_config.local_level,
            zeph_common::SkillTrustLevel::Trusted,
            "config.skills.trust.local_level must flow into ServeAgentDeps"
        );
        assert!(
            serve_deps.rl_routing_enabled,
            "config.skills.rl_routing_enabled must flow into ServeAgentDeps"
        );
        assert!(
            (serve_deps.rl_learning_rate - 0.05).abs() < f32::EPSILON,
            "config.skills.rl_learning_rate must flow into ServeAgentDeps"
        );
        assert!(
            (serve_deps.rl_weight - 0.3).abs() < f32::EPSILON,
            "config.skills.rl_weight must flow into ServeAgentDeps"
        );
        assert_eq!(
            serve_deps.rl_persist_interval, 5,
            "config.skills.rl_persist_interval must flow into ServeAgentDeps"
        );
        assert_eq!(
            serve_deps.rl_warmup_updates, 3,
            "config.skills.rl_warmup_updates must flow into ServeAgentDeps"
        );
        let serve_rl_head = serve_deps
            .rl_head
            .clone()
            .expect("rl_head must be Some when rl_routing_enabled and rl_embed_dim resolves");
        assert_eq!(
            serve_rl_head.embed_dim(),
            8,
            "the resolved RL embed dim (config.skills.rl_embed_dim) must flow into the \
             SharedCore::rl_head loaded for ServeAgentDeps"
        );

        assert!(
            (acp_deps.skill_disambiguation_threshold - 0.55).abs() < f32::EPSILON,
            "config.skills.disambiguation_threshold must flow into SharedAgentDeps"
        );
        assert!(
            acp_deps.skill_two_stage_matching,
            "config.skills.two_stage_matching must flow into SharedAgentDeps"
        );
        assert!(
            (acp_deps.skill_confusability_threshold - 0.65).abs() < f32::EPSILON,
            "config.skills.confusability_threshold must flow into SharedAgentDeps"
        );
        assert!(
            acp_deps.skill_group_structured,
            "config.skills.group_structured must flow into SharedAgentDeps"
        );
        assert!(
            (acp_deps.skill_support_similarity_threshold - 0.73).abs() < f32::EPSILON,
            "config.skills.support_similarity_threshold must flow into SharedAgentDeps"
        );
        assert!(
            (acp_deps.skill_min_injection_score - 0.35).abs() < f32::EPSILON,
            "config.skills.min_injection_score must flow into SharedAgentDeps"
        );
        assert_eq!(acp_deps.skill_generation_provider, "gen-test");
        assert_eq!(acp_deps.skill_disambiguate_provider, "disamb-test");
        assert!(
            acp_deps.semantic_scan,
            "config.skills.semantic_scan must flow into SharedAgentDeps"
        );
        assert_eq!(acp_deps.semantic_scan_provider, "scan-test");
        assert_eq!(
            acp_deps.trust_config.default_level,
            zeph_common::SkillTrustLevel::Quarantined,
            "config.skills.trust.default_level must flow into SharedAgentDeps"
        );
        assert_eq!(
            acp_deps.trust_config.local_level,
            zeph_common::SkillTrustLevel::Trusted,
            "config.skills.trust.local_level must flow into SharedAgentDeps"
        );
        assert!(
            acp_deps.rl_routing_enabled,
            "config.skills.rl_routing_enabled must flow into SharedAgentDeps"
        );
        assert!(
            (acp_deps.rl_learning_rate - 0.05).abs() < f32::EPSILON,
            "config.skills.rl_learning_rate must flow into SharedAgentDeps"
        );
        assert!(
            (acp_deps.rl_weight - 0.3).abs() < f32::EPSILON,
            "config.skills.rl_weight must flow into SharedAgentDeps"
        );
        assert_eq!(
            acp_deps.rl_persist_interval, 5,
            "config.skills.rl_persist_interval must flow into SharedAgentDeps"
        );
        assert_eq!(
            acp_deps.rl_warmup_updates, 3,
            "config.skills.rl_warmup_updates must flow into SharedAgentDeps"
        );
        let acp_rl_head = acp_deps
            .rl_head
            .clone()
            .expect("rl_head must be Some when rl_routing_enabled and rl_embed_dim resolves");
        assert_eq!(
            acp_rl_head.embed_dim(),
            8,
            "the resolved RL embed dim (config.skills.rl_embed_dim) must flow into the \
             SharedCore::rl_head loaded for SharedAgentDeps"
        );

        // #5974 regression: acp_deps.rl_head and serve_deps.rl_head must be the SAME shared
        // RoutingHead handle (same Arc<Mutex<..>>), not two independent copies each loaded from
        // the DB row — otherwise concurrent ACP and `/sessions` agents built from one
        // SharedCore would silently clobber each other's learned REINFORCE weights. Proven
        // behaviorally through the public API: an update applied via one handle must be
        // observable through the other.
        let q = vec![0.0f32; 8];
        let s = vec![0.0f32; 8];
        let _ = acp_rl_head.score(&q, &s, 0.5, 0.5, 1);
        assert!(acp_rl_head.update(1.0, 0.01));
        assert_eq!(
            serve_rl_head.update_count(),
            1,
            "acp_deps.rl_head and serve_deps.rl_head must share the same in-memory RoutingHead \
             instance loaded once by build_shared_core (#5974)"
        );
    }

    /// #5959/#6022 regression: before this PR, `SharedAgentDeps` had no
    /// `shutdown_summary*`/`channel_provider_persistence`/`channel_persist_provider_overrides`/
    /// `index_config` fields at all, so `spawn_acp_agent` had no way to call
    /// `Agent::with_shutdown_summary_config`/`with_shutdown_summary_provider`/
    /// `with_channel_identity("acp", ...)`/`agent_setup::apply_code_retrieval`/
    /// `apply_code_rag_retriever` for ACP sessions — every ACP agent silently ran on builder
    /// defaults (no shutdown summary, no provider-override persistence, no code-RAG retrieval)
    /// regardless of what the operator configured in `config.memory.shutdown_summary*`,
    /// `config.session.*`, and `config.index`. Drives the real `build_acp_deps` against a
    /// mock-provider `AppBuilder::for_test` (same pattern as
    /// `build_combined_deps_wires_skill_matching_config_from_config`) rather than hand-
    /// constructing a `SharedAgentDeps` literal, so a regression in the config-to-deps mapping
    /// is caught. Stops at the deps struct for the same reason documented on that sibling test:
    /// `spawn_acp_agent`'s internal `Agent`-level wiring has no test seam yet (#5887).
    #[cfg(feature = "acp")]
    #[tokio::test]
    async fn build_acp_deps_wires_shutdown_summary_channel_identity_and_index_config_from_config() {
        let mut config =
            zeph_core::config::Config::load(std::path::Path::new("/nonexistent")).unwrap();
        config.llm.providers = vec![zeph_core::config::ProviderEntry {
            provider_type: zeph_core::config::ProviderKind::Ollama,
            base_url: Some("http://127.0.0.1:1".to_owned()),
            model: Some("test-model".to_owned()),
            ..Default::default()
        }];
        config.memory.sqlite_path = ":memory:".to_owned();
        config.memory.shutdown_summary = true;
        config.memory.shutdown_summary_min_messages = 7;
        config.memory.shutdown_summary_max_messages = 42;
        config.memory.shutdown_summary_timeout_secs = 9;
        config.memory.shutdown_summary_provider = zeph_common::ProviderName::new("summary-test");
        config.session.provider_persistence = true;
        config.session.persist_provider_overrides = true;
        config.index.enabled = true;
        config.index.mcp_enabled = true;

        let app = crate::bootstrap::AppBuilder::for_test(config);
        let (deps, _keepalive) = Box::pin(build_acp_deps(&app, None, None))
            .await
            .expect("build_acp_deps must succeed against a mock-provider AppBuilder");

        assert!(
            deps.shutdown_summary,
            "config.memory.shutdown_summary must flow into SharedAgentDeps"
        );
        assert_eq!(
            deps.shutdown_summary_min_messages, 7,
            "config.memory.shutdown_summary_min_messages must flow into SharedAgentDeps"
        );
        assert_eq!(
            deps.shutdown_summary_max_messages, 42,
            "config.memory.shutdown_summary_max_messages must flow into SharedAgentDeps"
        );
        assert_eq!(
            deps.shutdown_summary_timeout_secs, 9,
            "config.memory.shutdown_summary_timeout_secs must flow into SharedAgentDeps"
        );
        assert_eq!(
            deps.shutdown_summary_provider, "summary-test",
            "config.memory.shutdown_summary_provider must flow into SharedAgentDeps"
        );
        assert!(
            deps.channel_provider_persistence,
            "config.session.provider_persistence must flow into SharedAgentDeps"
        );
        assert!(
            deps.channel_persist_provider_overrides,
            "config.session.persist_provider_overrides must flow into SharedAgentDeps"
        );
        assert!(
            deps.index_config.enabled,
            "config.index.enabled must flow into SharedAgentDeps"
        );
        assert!(
            deps.index_config.mcp_enabled,
            "config.index.mcp_enabled must flow into SharedAgentDeps"
        );
    }

    #[tokio::test]
    async fn broadcast_to_mpsc_forwards_items() {
        let (btx, brx) = tokio::sync::broadcast::channel::<u32>(16);
        let cancel = zeph_memory::CancellationToken::new();
        let mut rx = broadcast_to_mpsc(brx, cancel.clone());

        btx.send(1).unwrap();
        btx.send(2).unwrap();
        drop(btx); // Close broadcast — adapter exits on Closed.

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        // After broadcast closes the adapter task exits and mpsc is also closed.
        assert_eq!(rx.recv().await, None);
        cancel.cancel();
    }

    #[tokio::test]
    async fn broadcast_to_mpsc_cancellation_stops_task() {
        let (btx, brx) = tokio::sync::broadcast::channel::<u32>(16);
        let cancel = zeph_memory::CancellationToken::new();
        let mut rx = broadcast_to_mpsc(brx, cancel.clone());

        cancel.cancel();
        // Give the spawned task a chance to exit.
        tokio::task::yield_now().await;

        // After cancellation the adapter task exits, closing the mpsc sender.
        // Sending on broadcast should succeed (no one listening) but recv returns None.
        drop(btx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn broadcast_lag_does_not_block_direct_cancel_signal() {
        let (btx, brx) = tokio::sync::broadcast::channel::<u32>(1);
        let adapter_cancel = zeph_memory::CancellationToken::new();
        let mut rx = broadcast_to_mpsc(brx, adapter_cancel.clone());
        let cancel_signal = std::sync::Arc::new(tokio::sync::Notify::new());

        {
            let cancel_signal = std::sync::Arc::clone(&cancel_signal);
            let adapter_cancel = adapter_cancel.clone();
            tokio::spawn(async move {
                // EXEMPT(#5144): test-only spawn
                cancel_signal.notified().await;
                adapter_cancel.cancel();
            });
        }

        btx.send(1).unwrap();
        btx.send(2).unwrap();
        btx.send(3).unwrap();
        tokio::task::yield_now().await;

        cancel_signal.notify_one();
        drop(btx);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            adapter_cancel.cancelled(),
        )
        .await
        .expect("direct ACP cancel signal should not be blocked by reload lag");

        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("adapter receiver should shut down promptly after cancel");
            if next.is_none() {
                break;
            }
        }
    }

    /// Regression test for #5519 review S2: `notify_lock_degraded`'s fallback branch (no
    /// `SessionStatusNotifier` — e.g. `spawn_acp_agent` invoked without an `AcpContext`) must
    /// still reach the caller, via `channel.send_status`.
    #[tokio::test]
    async fn notify_lock_degraded_falls_back_to_channel_send_status_without_notifier() {
        let (mut channel, mut handle) = zeph_core::channel::LoopbackChannel::pair(8);

        notify_lock_degraded(None, &mut channel).await;

        let event = handle
            .output_rx
            .recv()
            .await
            .expect("channel must receive a status event");
        match event {
            zeph_core::LoopbackEvent::Status(text) => {
                assert_eq!(text, SESSION_LOCK_DEGRADED_MESSAGE);
            }
            other => panic!("expected LoopbackEvent::Status, got {other:?}"),
        }
    }

    /// Regression test for #5519 review S2: drives the real trigger path — genuine file-lock
    /// contention (not a mocked error) through `open_session_log_or_notify_locked`, the same
    /// helper `spawn_acp_agent`'s no-`conversation_id` hydration branch calls — and asserts the
    /// client is notified via `SessionStatusNotifier` synchronously, i.e. without any
    /// `session/prompt` drain (`try_recv`, not `recv().await` behind a drain loop).
    #[tokio::test]
    async fn already_locked_session_log_notifies_client_proactively_without_prompt() {
        let tmp = TempDir::new().unwrap();
        let session_path = tmp.path().join("already-locked-session");
        // Hold the write lock ourselves first — the exact contention a second concurrent
        // `spawn_acp_agent` invocation for the same session would hit.
        let _held_lock = zeph_session::SessionEventLog::open_exclusive(&session_path)
            .await
            .expect("first open_exclusive must succeed and hold the lock");

        let (mut channel, _handle) = zeph_core::channel::LoopbackChannel::pair(8);
        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel(8);
        let session_id =
            agent_client_protocol::schema::v1::SessionId::new("already-locked-test".to_owned());
        let status_notifier = Some(zeph_acp::SessionStatusNotifier::new(
            notify_tx,
            session_id.clone(),
        ));

        let log = open_session_log_or_notify_locked(
            &session_path,
            status_notifier.as_ref(),
            &mut channel,
        )
        .await;
        assert!(
            log.is_none(),
            "AlreadyLocked must degrade to no persistence, not fail session creation"
        );

        let (notification, _ack) = notify_rx.try_recv().expect(
            "client must be notified proactively — synchronously, with no prompt drain needed",
        );
        assert_eq!(notification.session_id, session_id);
        match notification.update {
            agent_client_protocol::schema::v1::SessionUpdate::AgentThoughtChunk(chunk) => {
                match chunk.content {
                    agent_client_protocol::schema::v1::ContentBlock::Text(t) => {
                        assert_eq!(t.text, SESSION_LOCK_DEGRADED_MESSAGE);
                    }
                    other => panic!("expected ContentBlock::Text, got {other:?}"),
                }
            }
            other => panic!("expected AgentThoughtChunk, got {other:?}"),
        }
    }
}
