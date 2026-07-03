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
//! `build_acp_deps` does for ACP sessions).
//!
//! **Known gap**: MCP tools, the scheduler executor, and skill/config hot-reload broadcast
//! forwarding are not wired here — a session created via `/sessions` does not see MCP-provided
//! tools or live-reload skill/config changes yet. Follow-up once the core create/prompt/events
//! path is proven out.

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
pub(crate) struct ServeAgentDeps {
    pub(crate) provider: AnyProvider,
    pub(crate) embedding_provider: AnyProvider,
    pub(crate) registry: Arc<RwLock<SkillRegistry>>,
    pub(crate) matcher: Option<SkillMatcherBackend>,
    pub(crate) max_active_skills: usize,
    pub(crate) tool_executor: Arc<dyn ErasedToolExecutor>,
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
) -> anyhow::Result<(ServeAgentDeps, Option<String>)> {
    use crate::bootstrap::AppBuilder;

    let app = AppBuilder::new(config_path, vault_backend, vault_key, vault_path).await?;
    let auth_token = resolve_auth_token(&app).await;

    let (provider, _status_tx, _status_rx) = app.build_provider().await?;
    let embedding_provider = crate::bootstrap::create_embedding_provider(app.config(), &provider);
    let budget_tokens = app.auto_budget_tokens(&provider);
    let registry = Arc::new(RwLock::new(app.build_registry()));

    let cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = zeph_common::task_supervisor::TaskSupervisor::new(cancel);
    let memory = Arc::new(app.build_memory(&provider, &supervisor).await?);

    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> =
        registry.read().all_meta().into_iter().cloned().collect();
    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    let matcher = app
        .build_skill_matcher(&embedding_provider, &all_meta_refs, &memory)
        .await;

    let config = app.config();
    let tool_executor = build_tool_executor(config, &supervisor).await?;

    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);
    let max_active_skills = config.skills.max_active_skills.get();
    let history_limit = config.memory.history_limit;
    let recall_limit = config.memory.semantic.recall_limit;
    let summarization_threshold = config.memory.summarization_threshold;
    let session_persistence_config = config.session.clone();
    // D-13 (spec-068 §8.1, N3): built once here, where the full `Config` is still in scope —
    // see `ServeAgentDeps::resume_condenser`'s doc comment.
    let (resume_condenser, resume_token_counter) =
        zeph_core::provider_factory::build_resume_condenser(config, &provider);
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

    Ok((
        ServeAgentDeps {
            provider,
            embedding_provider,
            registry,
            matcher,
            max_active_skills,
            tool_executor,
            memory,
            history_limit,
            recall_limit,
            summarization_threshold,
            session_config,
            session_persistence_config,
            resume_condenser: Arc::new(resume_condenser),
            resume_token_counter,
            provider_pool: config.llm.providers.clone(),
            provider_config_snapshot,
        },
        auth_token,
    ))
}

/// Resolves `[serve] auth_token_vault_key` from the vault. `None` when the key is empty
/// (`require_auth`'s default off-switch) or the vault lookup fails/misses — the caller
/// (`handle_serve_sessions_command`) decides how to react (refuse to bind non-loopback, or
/// proceed with `auth_middleware` rejecting every request when `require_auth = true`).
async fn resolve_auth_token(app: &crate::bootstrap::AppBuilder) -> Option<String> {
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
async fn build_tool_executor(
    config: &zeph_core::config::Config,
    supervisor: &zeph_common::task_supervisor::TaskSupervisor,
) -> anyhow::Result<Arc<dyn ErasedToolExecutor>> {
    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(zeph_tools::build_permission_policy(
            &config.tools,
            config.security.autonomy_level,
        ))
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
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit, false).await
    {
        let logger = Arc::new(logger);
        shell_executor = shell_executor.with_audit(Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(logger);
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
    let cwd_executor = zeph_tools::SetCwdExecutor;
    Ok(Arc::new(zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(
            shell_executor,
            zeph_tools::CompositeExecutor::new(scrape_executor, cwd_executor),
        ),
    )))
}
