// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(feature = "a2a")]

use std::path::PathBuf;

use parking_lot::RwLock;

use crate::agent_setup;
use crate::bootstrap::{AppBuilder, create_mcp_registry};
#[cfg(feature = "gateway")]
use crate::gateway_spawn::spawn_gateway_server;
use tokio::sync::watch;
use zeph_core::agent::Agent;
use zeph_core::config::Config;
use zeph_llm::LlmProvider as _;

/// Build the [`zeph_a2a::types::AgentCard`] for the daemon's A2A server.
///
/// Derives capability flags from runtime-available signals so the served
/// `/.well-known/agent.json` accurately reflects what the running agent can handle:
///
/// - `images`: `provider.supports_vision()` — only the active LLM can consume image parts.
/// - `audio`: `config.llm.stt_provider_entry().is_some()` — STT presence is the
///   precondition for the agent loop to transcribe audio attachments rather than drop them.
/// - `files`: `config.a2a.advertise_files` — opt-in, because generic file attachments have
///   no built-in ingestion path; set `true` only when skills or MCP tools consume file parts.
fn build_default_card(
    config: &Config,
    public_url: &str,
    provider: &zeph_llm::any::AnyProvider,
) -> zeph_a2a::AgentCard {
    zeph_a2a::AgentCardBuilder::new(&config.agent.name, public_url, env!("CARGO_PKG_VERSION"))
        .description("Zeph AI agent")
        .streaming(true)
        .images(provider.supports_vision())
        .audio(config.llm.stt_provider_entry().is_some())
        .files(config.a2a.advertise_files)
        .build()
}

fn spawn_a2a_server(
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
    loopback_handle: zeph_core::LoopbackHandle,
    sanitizer: zeph_core::ContentSanitizer,
    // Intentionally not injected into the per-request handler tasks (those are
    // short-lived OneShot spawns managed by the A2A server internally).
    // The overflow cleanup, signal handler, and sentinel tasks in run_daemon
    // are also excluded — they are either fire-and-forget one-shots or
    // lifecycle-managed by DaemonSupervisor.
    supervisor: Option<zeph_common::TaskSupervisor>,
    provider: &zeph_llm::any::AnyProvider,
    ibct_keys: Vec<zeph_a2a::IbctKey>,
) {
    let public_url = if config.a2a.public_url.is_empty() {
        format!("http://{}:{}", config.a2a.host, config.a2a.port)
    } else {
        config.a2a.public_url.clone()
    };

    let card = build_default_card(config, &public_url, provider);

    let processor: std::sync::Arc<dyn zeph_a2a::TaskProcessor> =
        std::sync::Arc::new(AgentTaskProcessor {
            loopback_handle: std::sync::Arc::new(tokio::sync::Mutex::new(loopback_handle)),
            sanitizer,
            drain_timeout: std::time::Duration::from_millis(config.a2a.drain_timeout_ms),
        });
    let task_ttl = if config.a2a.task_ttl_secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(config.a2a.task_ttl_secs))
    };
    let a2a_server = zeph_a2a::A2aServer::new(
        card,
        processor,
        &config.a2a.host,
        config.a2a.port,
        shutdown_rx,
    )
    .with_auth(config.a2a.auth_token.clone())
    .with_require_auth(config.a2a.require_auth)
    .with_rate_limit(config.a2a.rate_limit)
    .with_max_body_size(config.a2a.max_body_size)
    .with_request_timeout(std::time::Duration::from_millis(
        config.a2a.request_timeout_ms,
    ))
    .with_task_ttl(task_ttl)
    .with_ibct_keys(ibct_keys);

    tracing::info!(
        "A2A server spawned on {}:{}",
        config.a2a.host,
        config.a2a.port
    );

    if let Some(sup) = supervisor {
        // Wrap the one-shot server in Arc<parking_lot::Mutex<Option<_>>> so the Fn factory
        // can hand it off on the first (and only) call. RunOnce tasks are never restarted,
        // so take() will be Some exactly once.
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(a2a_server)));
        sup.spawn(zeph_common::TaskDescriptor {
            name: "a2a_server",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let server = cell.lock().take();
                async move {
                    if let Some(s) = server {
                        if let Err(e) = s.serve().await {
                            tracing::error!("A2A server error: {e:#}");
                        }
                    } else {
                        tracing::warn!(
                            "a2a_server RunOnce factory called after handoff — \
                             task will not restart; this indicates a policy misconfiguration"
                        );
                    }
                }
            },
        });
    } else {
        tokio::spawn(async move {
            // EXEMPT(#5143): no-supervisor fallback branch — supervisor is None here
            if let Err(e) = a2a_server.serve().await {
                tracing::error!("A2A server error: {e:#}");
            }
        });
    }
}

/// Resolves `[a2a] ibct_keys` (inline hex) plus `ibct_signing_key_vault_ref` (vault-resolved
/// primary key) into the `Vec<zeph_a2a::IbctKey>` consumed by `A2aServer::with_ibct_keys`.
///
/// A malformed inline `key_hex` entry only drops that one key (warned, not fatal) — other
/// entries and the vault-resolved key still apply. Per the `014-a2a` spec's IBCT Key
/// Invariants ("`ibct_signing_key_vault_ref` must resolve to a vault key — startup fails if
/// the ref is set but the vault key is absent"), a *declared* `ibct_signing_key_vault_ref`
/// that fails to resolve (missing, empty, backend error, or not valid hex) fails startup
/// instead — an operator who explicitly configured the vault ref for IBCT enforcement must
/// not silently end up with enforcement disabled because the secret vanished. The
/// vault-resolved key (`key_id = "primary"`) takes precedence over an inline `ibct_keys`
/// entry sharing the same `key_id`, per `A2aServerConfig::ibct_signing_key_vault_ref`'s
/// documented precedence.
///
/// **Blast radius**: this function is called (and `?`-propagated) in `run_daemon` *before*
/// any channel/server spawns — a missing/rotated-out/non-hex vault secret aborts startup for
/// the whole daemon process (Telegram, Discord, gateway, scheduler, everything), not just the
/// A2A server. This mirrors the spec-mandated invariant above rather than scoping the failure
/// to A2A alone; if that blast radius proves too broad in practice, scoping it down (e.g.
/// disabling only the A2A server on this specific failure, matching `require_auth`'s per-
/// subsystem soft-fail elsewhere) is a reasonable follow-up, not required by this fix.
///
/// # Errors
///
/// Returns an error if `ibct_signing_key_vault_ref` is set but the vault lookup fails,
/// misses, resolves to an empty secret, or the secret is not valid hex.
async fn resolve_ibct_keys(
    config: &Config,
    vault: &dyn zeph_core::vault::VaultProvider,
) -> anyhow::Result<Vec<zeph_a2a::IbctKey>> {
    let mut keys = Vec::new();
    for entry in &config.a2a.ibct_keys {
        match zeph_a2a::IbctKey::from_hex(entry.key_id.as_str(), &entry.key_hex) {
            Ok(key) => keys.push(key),
            Err(e) => tracing::warn!(
                key_id = %entry.key_id,
                error = %e,
                "a2a.ibct_keys: invalid hex key_hex, skipping entry"
            ),
        }
    }

    if let Some(vault_key) = &config.a2a.ibct_signing_key_vault_ref {
        let hex_secret = vault
            .get_secret(vault_key)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "a2a.ibct_signing_key_vault_ref '{vault_key}': failed to resolve from vault: {e}"
                )
            })?
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "a2a.ibct_signing_key_vault_ref '{vault_key}': vault key not found or empty"
                )
            })?;
        let key = zeph_a2a::IbctKey::from_hex("primary", hex_secret.trim()).map_err(|e| {
            anyhow::anyhow!(
                "a2a.ibct_signing_key_vault_ref '{vault_key}': vault secret is not valid hex: {e}"
            )
        })?;
        keys.retain(|k| k.key_id != "primary");
        keys.insert(0, key);
    }

    Ok(keys)
}

pub(crate) struct AgentTaskProcessor {
    pub(crate) loopback_handle: std::sync::Arc<tokio::sync::Mutex<zeph_core::LoopbackHandle>>,
    pub(crate) sanitizer: zeph_core::ContentSanitizer,
    pub(crate) drain_timeout: std::time::Duration,
}

/// Derives a cross-thread store owner key (spec-080 §10 OQ-1, GitHub #6389) from an A2A
/// message's `context_id` — the A2A protocol's own conversation-scoping identifier ("shared
/// with other tasks in the same session"), `crates/zeph-a2a/src/types.rs` — so distinct A2A
/// callers/sessions land in distinct store buckets instead of every A2A message collapsing
/// into the shared `"local"` bucket alongside the CLI/TUI operator.
///
/// Like the gateway's `sender`-derived key, `context_id` is client-supplied within a single
/// shared bearer token (`AuthIdentity` has no per-caller id) — a defense-in-depth partition
/// against accidental cross-caller collisions, not a hard tenant boundary; the bearer token
/// remains the only real authentication gate on this path. A message with no `context_id`
/// still gets a distinct `a2a:default` bucket rather than falling back to `"local"`, so A2A
/// traffic never blends into the single-user CLI/TUI bucket. Capped at 256 chars to bound the
/// value written into `owner_key` (a `cross_thread_store` primary-key column), mirroring the
/// length limit the gateway already enforces on `sender` (`WebhookPayload::validate`).
fn a2a_owner_key(message: &zeph_a2a::Message) -> String {
    match &message.context_id {
        Some(cid) => format!("a2a:{}", cid.chars().take(256).collect::<String>()),
        None => "a2a:default".to_owned(),
    }
}

impl zeph_a2a::TaskProcessor for AgentTaskProcessor {
    fn process(
        &self,
        _task_id: String,
        message: zeph_a2a::Message,
        event_tx: tokio::sync::mpsc::Sender<zeph_a2a::ProcessorEvent>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), zeph_a2a::A2aError>> + Send>>
    {
        let handle = self.loopback_handle.clone();
        let sanitizer = self.sanitizer.clone();

        let drain_timeout = self.drain_timeout;

        Box::pin(async move {
            // Inbound A2A messages come from external agents and are treated as
            // ExternalUntrusted — sanitize before forwarding to the agent loop.
            // Use all_text_content() to concatenate ALL Part::Text entries; text_content()
            // returns only the first part and would silently drop subsequent text.
            let raw_text = message.all_text_content();
            let user_text = sanitizer
                .sanitize(
                    &raw_text,
                    zeph_core::ContentSource::new(zeph_core::ContentSourceKind::A2aMessage),
                )
                .body;
            let owner_key = a2a_owner_key(&message);
            let mut handle = handle.lock().await;

            handle
                .input_tx
                .send(zeph_core::ChannelMessage {
                    text: user_text,
                    attachments: vec![],
                    is_guest_context: false,
                    is_from_bot: false,
                    owner_key: Some(owner_key),
                })
                .await
                .map_err(|_| zeph_a2a::A2aError::Server("agent channel closed".to_owned()))?;

            event_tx
                .send(zeph_a2a::ProcessorEvent::StatusUpdate {
                    state: zeph_a2a::TaskState::Working,
                    is_final: false,
                })
                .await
                .map_err(|_| zeph_a2a::A2aError::Server("event channel closed".to_owned()))?;

            let mut exited_on_flush = false;
            while let Some(event) = handle.output_rx.recv().await {
                match event {
                    zeph_core::LoopbackEvent::Chunk(text) => {
                        let _ = event_tx
                            .send(zeph_a2a::ProcessorEvent::ArtifactChunk {
                                text,
                                is_final: false,
                            })
                            .await;
                    }
                    zeph_core::LoopbackEvent::Flush => {
                        let _ = event_tx
                            .send(zeph_a2a::ProcessorEvent::ArtifactChunk {
                                text: String::new(),
                                is_final: true,
                            })
                            .await;
                        exited_on_flush = true;
                        break;
                    }
                    zeph_core::LoopbackEvent::FullMessage(text) => {
                        let _ = event_tx
                            .send(zeph_a2a::ProcessorEvent::ArtifactChunk {
                                text,
                                is_final: true,
                            })
                            .await;
                        break;
                    }
                    zeph_core::LoopbackEvent::Status(_)
                    | zeph_core::LoopbackEvent::ToolStart(_)
                    | zeph_core::LoopbackEvent::ToolOutput(_)
                    | zeph_core::LoopbackEvent::Usage { .. }
                    | zeph_core::LoopbackEvent::SessionTitle(_)
                    | zeph_core::LoopbackEvent::Plan(_)
                    | zeph_core::LoopbackEvent::ThinkingChunk(_)
                    | zeph_core::LoopbackEvent::Stop(_)
                    | _ => {}
                }
            }

            // Wait for Flush — the definitive end-of-turn sentinel always emitted by the
            // agent loop after FullMessage or stop-hint paths. This prevents stale tail
            // events (e.g. the Flush that follows FullMessage, Usage, SessionTitle) from
            // leaking into the next request's recv loop.
            // A timeout guards against an agent loop panic that holds the sender Arc alive
            // without ever emitting Flush, which would otherwise block indefinitely.
            if !exited_on_flush {
                let drain = async {
                    loop {
                        match handle.output_rx.recv().await {
                            Some(zeph_core::LoopbackEvent::Flush) | None => break,
                            Some(_) => {} // discard tail events
                        }
                    }
                };
                if tokio::time::timeout(drain_timeout, drain).await.is_err() {
                    tracing::warn!(
                        timeout_ms = drain_timeout.as_millis(),
                        "A2A drain timeout: Flush not received within deadline; \
                         proceeding with degraded state"
                    );
                }
            }

            let _ = event_tx
                .send(zeph_a2a::ProcessorEvent::StatusUpdate {
                    state: zeph_a2a::TaskState::Completed,
                    is_final: true,
                })
                .await;

            Ok(())
        })
    }
}

/// Dependencies for [`build_daemon_agent`] (#5819): packages the exact inputs `run_daemon()`'s
/// `AgentBuilder` construction chain closes over, mirroring `crate::runner::build_agent`'s
/// `Deps`-taking pattern so the daemon wiring is unit-testable without running the whole
/// daemon bootstrap.
///
/// Deliberately its own struct, not merged with `crate::runner::BuildAgentDeps`: the daemon
/// path wires `with_mcp`/`with_mcp_shared_tools`/`with_provider_pool` inline (the CLI path
/// defers those to feature-gated chaining after `build_agent` returns, inside `run()`), and
/// never wires session-sink, compression, typed-pages, autosave, shutdown-summary,
/// compaction-provider, tiered-retrieval, or bare-mode config at all. Forcing both paths into
/// one struct would need `None`/default placeholders for whichever fields the other path
/// doesn't use, reintroducing the exact default-vs-omitted wiring-regression defect class this
/// issue exists to catch.
struct BuildDaemonAgentDeps<'a, F>
where
    F: Fn() -> Vec<PathBuf> + Send + Sync + 'static,
{
    config: &'a Config,
    provider: zeph_llm::any::AnyProvider,
    embedding_provider: zeph_llm::any::AnyProvider,
    registry: std::sync::Arc<RwLock<zeph_skills::registry::SkillRegistry>>,
    matcher: Option<zeph_skills::matcher::SkillMatcherBackend>,
    tool_executor: zeph_tools::DynExecutor,
    session_config: zeph_core::AgentSessionConfig,
    skill_paths: Vec<PathBuf>,
    reload_rx: tokio::sync::mpsc::Receiver<zeph_skills::watcher::SkillEvent>,
    plugin_dirs_supplier: F,
    memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
    conversation_id: zeph_memory::ConversationId,
    shutdown_rx: watch::Receiver<bool>,
    config_path: PathBuf,
    config_reload_rx: tokio::sync::mpsc::Receiver<zeph_core::config_watcher::ConfigEvent>,
    shell_policy_handle: zeph_tools::ShellPolicyHandle,
    mcp_tools: Vec<zeph_mcp::McpTool>,
    mcp_registry: Option<zeph_mcp::McpToolRegistry>,
    mcp_manager: std::sync::Arc<zeph_mcp::McpManager>,
    mcp_shared_tools: std::sync::Arc<RwLock<Vec<zeph_mcp::McpTool>>>,
    provider_config_snapshot: zeph_core::ProviderConfigSnapshot,
    /// #5975: shared with `SkillInvokeExecutor` — see `run_daemon`'s construction site.
    trust_snapshot: std::sync::Arc<
        parking_lot::RwLock<
            std::collections::HashMap<String, zeph_core::skill_invoker::SkillTrustSnapshot>,
        >,
    >,
}

/// Build the `Agent` from the `AgentBuilder` construction chain used by the daemon (A2A)
/// bootstrap path (`run_daemon()`), extracted verbatim so it is unit-testable without running
/// the whole daemon bootstrap (#5819). Mirrors `crate::runner::build_agent`'s `Deps`-taking
/// shape — see [`BuildDaemonAgentDeps`] for why the field set is not shared.
///
/// Only the core `Agent::new_with_registry_arc(...)...await` wiring lives here — feature-gated
/// post-processing (audit logger, RL head, tool dependency graph, provider setters, debug
/// dumper, etc.) stays in `run_daemon`, matching where `build_agent`'s scope ends in `run()`.
async fn build_daemon_agent<C, F>(deps: BuildDaemonAgentDeps<'_, F>, channel: C) -> Agent<C>
where
    C: zeph_core::channel::Channel,
    F: Fn() -> Vec<PathBuf> + Send + Sync + 'static,
{
    let config = deps.config;
    Agent::new_with_registry_arc(
        deps.provider.clone(),
        deps.embedding_provider.clone(),
        channel,
        deps.registry,
        deps.matcher,
        config.skills.max_active_skills.get(),
        deps.tool_executor,
    )
    .apply_session_config(deps.session_config)
    .with_skill_config(zeph_core::SkillConfigParams::from(&config.skills))
    .with_skill_coldstart(
        deps.skill_paths,
        deps.reload_rx,
        deps.plugin_dirs_supplier,
        crate::bootstrap::managed_skills_dir(),
    )
    .with_trust_config(config.skills.trust.clone())
    .with_trust_snapshot(deps.trust_snapshot)
    .with_memory(
        deps.memory,
        deps.conversation_id,
        config.memory.history_limit,
        config.memory.semantic.recall_limit,
        config.memory.summarization_threshold,
    )
    .with_shutdown(deps.shutdown_rx)
    .with_config_reload(deps.config_path, deps.config_reload_rx)
    .with_plugins_dir(crate::bootstrap::plugins_dir(), {
        let mut blocked = config.tools.shell.blocked_commands.clone();
        blocked.sort();
        let mut allowed = config.tools.shell.allowed_commands.clone();
        allowed.sort();
        zeph_core::ShellOverlaySnapshot { blocked, allowed }
    })
    .with_shell_policy_handle(deps.shell_policy_handle)
    .with_mcp(
        deps.mcp_tools,
        deps.mcp_registry,
        Some(deps.mcp_manager),
        &config.mcp,
    )
    .with_mcp_shared_tools(deps.mcp_shared_tools)
    .with_hybrid_search(config.skills.hybrid_search)
    .with_rl_routing(
        config.skills.rl_routing_enabled,
        config.skills.rl_learning_rate,
        config.skills.rl_weight,
        config.skills.rl_persist_interval,
        config.skills.rl_warmup_updates,
    )
    .with_focus_and_sidequest_config(config.agent.focus.clone(), config.memory.sidequest.clone())
    .with_trajectory_and_category_config(
        config.memory.trajectory.clone(),
        config.memory.category.clone(),
    )
    .with_embedding_provider(deps.embedding_provider.clone())
    .with_provider_pool(config.llm.providers.clone(), deps.provider_config_snapshot)
    .with_safe_mode(config.cli.safe_mode)
    .with_allowed_paths(
        config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
    )
    .with_tools_enabled(config.tools.enabled)
    .maybe_init_tool_schema_filter(config.agent.tool_filter.clone(), deps.embedding_provider)
    .await
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_daemon(
    config_path: Option<&std::path::Path>,
    vault: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
    safe_mode: bool,
    no_mcp_media: bool,
) -> anyhow::Result<()> {
    use zeph_core::daemon::{ComponentHandle, DaemonSupervisor, PidGuard};

    let app = AppBuilder::new(
        config_path,
        vault,
        vault_key,
        vault_path,
        safe_mode,
        no_mcp_media,
    )
    .await?;
    let config = app.config();

    // Atomically acquire the daemon pid file lock — fails fast with `AlreadyRunning` if another
    // instance already holds it, instead of racing it via a separate check-then-write sequence.
    let pid_guard = PidGuard::acquire(&config.daemon.pid_file)
        .map_err(|e| anyhow::anyhow!("failed to acquire daemon pid file lock: {e}"))?;
    tracing::info!(pid_file = %config.daemon.pid_file, "daemon started");
    if config.cli.safe_mode {
        tracing::info!(
            "safe mode active: ZEPH.md/CLAUDE.md/AGENTS.md, plugins, skills, hooks, and MCP \
             servers are disabled for this session"
        );
    }

    let (provider, status_tx, _status_rx) = app.build_provider().await?;
    let embed_model = app.embedding_model();
    let embedding_provider = crate::bootstrap::create_embedding_provider(app.config(), &provider);
    let budget_tokens = app.auto_budget_tokens(&provider);

    // Safe-mode gate (#6031): empty registry with no matching disables skill loading/matching,
    // mirroring `runner.rs`'s `exec_mode.bare` gate and `acp::build_shared_core`'s safe-mode gate.
    let registry = std::sync::Arc::new(RwLock::new(if config.cli.safe_mode {
        zeph_skills::registry::SkillRegistry::empty()
    } else {
        app.build_registry()
    }));
    let mem_cancel = tokio_util::sync::CancellationToken::new();
    let mem_supervisor = zeph_common::TaskSupervisor::new(mem_cancel.clone());
    let memory = std::sync::Arc::new(app.build_memory(&provider, &mem_supervisor).await?);
    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> =
        registry.read().all_meta().into_iter().cloned().collect();
    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    let matcher = app
        .build_skill_matcher(&embedding_provider, &all_meta_refs, &memory)
        .await;
    let skill_count = all_meta_owned.len();
    tracing::info!("skills loaded: {skill_count}");

    // Populate trust DB for all loaded skills (#5920: daemon.rs previously never called this,
    // leaving every skill without a pre-existing trust row fail-open to Trusted
    // (SkillTrustLevel::MISSING_ENTRY_FALLBACK) — un-sanitized bodies with full tool access
    // instead of the operator's configured restriction).
    app.seed_skill_trust_db(&all_meta_owned, &memory).await;

    let conversation_id = match memory.sqlite().latest_conversation_id().await? {
        Some(id) => id,
        None => memory.sqlite().create_conversation().await?,
    };

    {
        let sqlite = memory.sqlite().clone();
        let retention_secs = config.tools.overflow.retention_days.saturating_mul(86_400);
        let fut = async move {
            match sqlite.cleanup_overflow(retention_secs).await {
                Ok(n) if n > 0 => tracing::info!("cleaned up {n} stale overflow entries"),
                Ok(_) => {}
                Err(e) => tracing::warn!("overflow cleanup failed: {e}"),
            }
        };
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        mem_supervisor.spawn(zeph_common::TaskDescriptor {
            name: "overflow_cleanup",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    }

    // #5914/#5979/#6180: memory maintenance loops, via the shared
    // `agent_setup::spawn_memory_maintenance_loops` (also used by `src/runner.rs`, `src/acp.rs`,
    // `src/serve/deps.rs`) so the daemon (A2A) entry point gets the same ongoing
    // eviction/tier-promotion/scene-consolidation/consolidation/forgetting/guidelines/
    // tree-consolidation/hebbian-consolidation/episodic-consolidation/optical-forgetting sweeps
    // instead of an ever-growing, never-maintained memory store.
    agent_setup::spawn_memory_maintenance_loops(
        &app,
        &memory,
        &provider,
        &mem_supervisor,
        Some(&status_tx),
        false,
        "daemon",
    );

    let (shutdown_tx, shutdown_rx) = AppBuilder::build_shutdown();

    // Wire shutdown to mem_supervisor (created before build_memory for retrieval-failure-logger).
    {
        let mut rx = shutdown_rx.clone();
        let cancel = mem_cancel.clone();
        let fut = async move {
            let _ = rx.changed().await;
            cancel.cancel();
        };
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        mem_supervisor.spawn(zeph_common::TaskDescriptor {
            name: "mem_shutdown_bridge",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    }

    let daemon_cancel = tokio_util::sync::CancellationToken::new();
    let task_supervisor = zeph_common::TaskSupervisor::new(daemon_cancel.clone());
    {
        let mut rx = shutdown_rx.clone();
        let cancel = daemon_cancel;
        let fut = async move {
            let _ = rx.changed().await;
            cancel.cancel();
        };
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        task_supervisor.spawn(zeph_common::TaskDescriptor {
            name: "daemon_shutdown_bridge",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    }

    let daemon_runtime_ctx = zeph_core::RuntimeContext {
        tui_mode: false,
        daemon_mode: true,
    };

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
        .with_task_supervisor(task_supervisor.clone());
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
                tracing::info!(backend = name, "OS sandbox enabled (daemon)");
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
        let fut = agent_setup::drain_egress_events(egress_rx, None);
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        task_supervisor.spawn(zeph_common::TaskDescriptor {
            name: "egress_drain",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    }
    let mut daemon_audit_logger: Option<std::sync::Arc<zeph_tools::AuditLogger>> = None;
    if config.tools.audit.enabled
        && let Ok(logger) =
            zeph_tools::AuditLogger::from_config(&config.tools.audit, daemon_runtime_ctx.tui_mode)
                .await
    {
        let logger = std::sync::Arc::new(logger);
        shell_executor = shell_executor.with_audit(std::sync::Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(std::sync::Arc::clone(&logger));
        daemon_audit_logger = Some(logger);
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
    let mcp_manager_builder = crate::bootstrap::create_mcp_manager_with_vault(
        config,
        daemon_runtime_ctx.suppress_stderr(),
        app.age_vault_arc(),
    )
    .with_status_tx(status_tx.clone());
    let mcp_manager_builder = crate::bootstrap::wire_trust_calibration(
        mcp_manager_builder,
        config,
        Some(memory.sqlite().pool()),
    )
    .await;
    let mcp_manager = std::sync::Arc::new(mcp_manager_builder);
    // Safe-mode gate (#6031): daemon had no prior bare-mode gate on MCP connections either —
    // this is a fresh gate, mirroring `agent_setup::build_tool_setup`'s runner-path gate.
    let (mcp_tools, _mcp_outcomes) = if config.cli.safe_mode {
        (Vec::new(), Vec::new())
    } else {
        mcp_manager.connect_all().await
    };
    // Retain a reference for explicit pre-shutdown so child processes are killed while the
    // tokio runtime is still live (fixes #2693: ChildWithCleanup::drop races with shutdown).
    let shutdown_mcp_manager = std::sync::Arc::clone(&mcp_manager);
    let mcp_shared_tools = std::sync::Arc::new(RwLock::new(mcp_tools.clone()));
    let mut mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    if config.cli.no_mcp_media {
        tracing::info!("--no-mcp-media: MCP image passthrough disabled for this session");
    } else {
        mcp_executor = mcp_executor
            .with_media(
                std::sync::Arc::new(zeph_sanitizer::MediaSanitizer::new(&config.mcp.media)),
                config.mcp.media.max_images_per_result,
            )
            .with_status_tx(status_tx.clone());
    }
    if let Some(ref logger) = daemon_audit_logger {
        mcp_executor = mcp_executor.with_audit(std::sync::Arc::clone(logger));
    }
    let shell_policy_handle = shell_executor.policy_handle();
    let diagnostics_executor = agent_setup::build_diagnostics_executor(config);
    // #5611: base chain stays ungated here; it is composed with mcp/search/skill_loader/
    // memory/overflow below, then the FULLY composed tree is wrapped in one outermost
    // TrustGateExecutor via `apply_common_tool_gating`, matching runner.rs. Gating only this
    // sub-tree (as before #5611) let tools composed outside it bypass Quarantine/Blocked.
    let base_executor = agent_setup::build_base_executor_chain(
        file_executor,
        shell_executor,
        scrape_executor,
        diagnostics_executor,
        config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
    );
    let memory_executor = zeph_core::memory_tools::MemoryToolExecutor::with_validator(
        std::sync::Arc::clone(&memory),
        conversation_id,
        zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
            config.security.memory_validation.clone(),
        ),
    );
    let overflow_executor = zeph_core::overflow_tools::OverflowToolExecutor::new(
        std::sync::Arc::new(memory.sqlite().clone()),
    )
    .with_conversation(conversation_id.0);
    let (skill_loader_executor, skill_invoke_executor, trust_snapshot) =
        agent_setup::build_skill_executors(&registry);
    // Hoisted out of the composite-executor block below (rather than resolved twice) so it can
    // also be passed to `apply_code_rag_retriever` once the agent exists (#6022: previously the
    // daemon never wired code-RAG retrieval, only the on-demand `search_code` tool).
    let index_provider = crate::bootstrap::resolve_index_embed_provider(config, provider.clone());
    let base_tool: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = {
        let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = std::sync::Arc::new(
            zeph_tools::CompositeExecutor::new(base_executor, mcp_executor),
        );
        if let Some(search_executor) = agent_setup::build_search_code_executor(
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
    let inner_executor =
        zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                skill_invoke_executor,
                zeph_tools::CompositeExecutor::new(
                    memory_executor,
                    zeph_tools::CompositeExecutor::new(
                        overflow_executor,
                        zeph_tools::DynExecutor(base_tool),
                    ),
                ),
            ),
        )));
    // Gate the FULLY composed tree (base chain + mcp + search + skill loader + skill invoke +
    // memory + overflow) behind one outermost TrustGateExecutor, matching runner.rs and ACP
    // (`src/acp.rs`). Previously only the base chain was gated here, so a Quarantined skill
    // could still reach `memory_save` and any MCP-sourced tool.
    let (trust_gated, mcp_ids_handle) =
        agent_setup::apply_common_tool_gating(inner_executor, &permission_policy);
    agent_setup::register_mcp_tool_ids(&mcp_ids_handle, &mcp_tools);
    // #5958: shared trajectory risk slot — written by begin_turn(), read by PolicyGateExecutor —
    // and pending risk signal queue — executor layers push signal codes; begin_turn() drains.
    // Mirrors src/runner.rs; previously the daemon never created these, so TrajectorySentinel
    // was never wired into the daemon's tool-gate chain or the Agent itself (see below).
    let trajectory_risk_slot: zeph_tools::TrajectoryRiskSlot =
        std::sync::Arc::new(parking_lot::RwLock::new(0u8));
    let trajectory_signal_queue: zeph_tools::RiskSignalQueue =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    // Wire the same PolicyGateExecutor / AdversarialPolicyGateExecutor stack the CLI
    // path (src/runner.rs) applies around its full tool composite, so declarative
    // (`[tools.policy]`/`[tools.authorization]`) and LLM-based (`[tools.adversarial_policy]`)
    // enforcement covers the daemon (A2A) entry point too — previously tool calls dispatched
    // through the daemon bypassed both gates entirely regardless of config. Wiring order
    // (outermost first): PolicyGateExecutor -> AdversarialPolicyGateExecutor -> TrustGateExecutor
    // -> inner_executor.
    let policy_gate_pieces = agent_setup::build_policy_gate_pieces(config, &provider).await;
    let tool_executor = agent_setup::apply_policy_gate_chain(
        trust_gated,
        &policy_gate_pieces,
        daemon_audit_logger.as_ref(),
        Some((&trajectory_risk_slot, &trajectory_signal_queue)),
    );

    // Spec 050 F2 (#5913): wrap with ScopedToolExecutor when capability_scopes are configured —
    // mirrors src/runner.rs. The daemon builds one static Agent (no per-session composition
    // like ACP), so `tool_executor` here is already the fully composed tree, and the dead-glob
    // outcome (FR-CG-005/NFR-CG-004) is knowable at startup, before any live session exists —
    // same as runner.rs's CLI process, so this stays a fatal startup error (`return Err`)
    // rather than degrading to the unscoped executor (impl-critic F1: that degradation is
    // fail-OPEN for a security control the operator explicitly enabled, and `capability_scopes`
    // is process-global — a config typo would silently disable scoping for every session).
    let tool_executor = {
        let scopes_cfg = config.security.capability_scopes.clone();
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
            match build_scoped_executor(tool_executor, &scopes_cfg, &registry_ids) {
                Ok(scoped) => {
                    // #5958: OutOfScope denials feed the trajectory signal queue too, matching
                    // src/runner.rs — otherwise capability-scope violations would be invisible
                    // to TrajectorySentinel's risk escalation.
                    let scoped =
                        scoped.with_signal_queue(std::sync::Arc::clone(&trajectory_signal_queue));
                    zeph_tools::DynExecutor(std::sync::Arc::new(scoped))
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("capability_scopes: {e}"));
                }
            }
        }
    };

    // Spec 050 Phase 2 (#5913): wrap with ShadowProbeExecutor when shadow_sentinel.enabled =
    // true — mirrors src/runner.rs. Wiring order: ScopedToolExecutor -> ShadowProbeExecutor ->
    // PolicyGateExecutor -> AdversarialPolicyGateExecutor -> TrustGateExecutor -> composite.
    let (tool_executor, shadow_sentinel_arc) = {
        let sentinel_cfg = &config.security.shadow_sentinel;
        if sentinel_cfg.enabled {
            let pool = memory.sqlite().pool().clone();
            let probe_provider = if sentinel_cfg.probe_provider.is_empty() {
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
            let probe_provider = match app.secret_registry() {
                Some(registry) => probe_provider
                    .masked(registry as std::sync::Arc<dyn zeph_llm::masking::OutboundMasker>),
                None => probe_provider,
            };
            let llm_probe = zeph_core::agent::shadow_sentinel::LlmSafetyProbe::new(
                std::sync::Arc::new(probe_provider),
                sentinel_cfg.probe_timeout_ms,
                sentinel_cfg.deny_on_timeout,
            );
            let store = zeph_core::agent::shadow_sentinel::ShadowEventStore::new(pool);
            let sentinel =
                std::sync::Arc::new(zeph_core::agent::shadow_sentinel::ShadowSentinel::new(
                    store,
                    Box::new(llm_probe),
                    sentinel_cfg.clone(),
                    conversation_id.0.to_string(),
                ));
            let turn_number = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let risk_level = std::sync::Arc::new(parking_lot::RwLock::new("calm".to_owned()));
            let probe_gate: std::sync::Arc<dyn zeph_tools::ProbeGate> =
                std::sync::Arc::new(crate::runner::ShadowSentinelProbeGateAdapter {
                    sentinel: std::sync::Arc::clone(&sentinel),
                });
            let shadow_exec = zeph_tools::ShadowProbeExecutor::new(
                tool_executor,
                probe_gate,
                turn_number,
                risk_level,
            );
            tracing::info!("security.shadow_sentinel: ShadowProbeExecutor wired (daemon)");
            (
                zeph_tools::DynExecutor(std::sync::Arc::new(shadow_exec)),
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
        agent_setup::register_mcp_tool_ids(&sentinel.mcp_tool_ids_handle(), &mcp_tools);
    }

    let mcp_embed_provider = {
        let discovery = &config.mcp.tool_discovery;
        if discovery.embedding_provider.is_empty() {
            provider.clone()
        } else {
            match crate::bootstrap::create_named_provider(&discovery.embedding_provider, config) {
                Ok(p) => {
                    tracing::info!(
                        provider = %discovery.embedding_provider,
                        "Using dedicated embed provider for MCP registry"
                    );
                    p
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %discovery.embedding_provider,
                        "MCP registry embedding_provider resolution failed, using main provider: {e:#}"
                    );
                    provider.clone()
                }
            }
        }
    };
    let mcp_registry = create_mcp_registry(
        config,
        &mcp_embed_provider,
        &mcp_tools,
        &embed_model,
        app.qdrant_ops(),
    )
    .await;

    let watchers = {
        let sup_arc = std::sync::Arc::new(task_supervisor.clone());
        app.build_watchers(&sup_arc)
    };
    let _skill_watcher = watchers.skill_watcher;
    let reload_rx = watchers.skill_reload_rx.into_inner();
    let _config_watcher = watchers.config_watcher;
    let config_reload_rx = watchers.config_reload_rx.into_inner();
    let skill_paths = app.skill_paths_for_registry();
    let plugin_dirs_supplier = app.plugin_dirs_supplier();
    let config_path_owned = app.config_path().to_owned();
    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);
    // #5450: built here, where the full `Config` is still in scope — mirrors
    // `src/runner.rs`'s CLI-path snapshot construction, so the daemon's agent gets a populated
    // `provider_pool` too (previously left empty, breaking `resolve_background_provider`).
    let provider_config_snapshot = agent_setup::build_provider_config_snapshot(config);

    let (loopback_channel, loopback_handle) = zeph_core::LoopbackChannel::pair(64);

    // Pre-resolve RL embed dim before embedding_provider is moved into the agent builder.
    let rl_embed_dim_resolved = if config.skills.rl_routing_enabled {
        Some(
            crate::runner::resolve_rl_embed_dim(
                &config.skills,
                &embedding_provider,
                config.timeouts.embedding_seconds,
            )
            .await,
        )
    } else {
        None
    };

    let deps = BuildDaemonAgentDeps {
        config,
        provider: provider.clone(),
        embedding_provider,
        registry,
        matcher,
        tool_executor,
        session_config,
        skill_paths,
        reload_rx,
        plugin_dirs_supplier,
        memory: std::sync::Arc::clone(&memory),
        conversation_id,
        shutdown_rx: shutdown_rx.clone(),
        config_path: config_path_owned,
        config_reload_rx,
        shell_policy_handle,
        mcp_tools,
        mcp_registry,
        mcp_manager,
        mcp_shared_tools,
        provider_config_snapshot,
        trust_snapshot,
    };
    let agent = Box::pin(build_daemon_agent(deps, loopback_channel)).await;

    // #6022: wire code-RAG retrieval (static repo-map/IndexMcpServer injection plus automatic
    // per-turn code-context retrieval) — mirrors src/runner.rs. Previously the daemon only
    // wired the on-demand `search_code` tool (above), never the automatic injection path.
    let agent = agent_setup::apply_code_retrieval(agent, &config.index);
    let agent = agent_setup::apply_code_rag_retriever(
        agent,
        &config.index,
        app.qdrant_ops().cloned(),
        index_provider,
        memory.sqlite().pool().clone(),
    );

    let agent = if let Some(logger) = daemon_audit_logger {
        agent.with_audit_logger(logger)
    } else {
        agent
    };

    // SkillOrchestra: load persisted RL routing head weights if enabled.
    let agent = if let Some(dim) = rl_embed_dim_resolved {
        let head = crate::runner::load_rl_head(&memory)
            .await
            .unwrap_or_else(|| {
                // Cold start: no persisted weights yet, initialize a fresh head.
                tracing::info!(dim, "rl_head: cold start, initializing fresh routing head");
                zeph_skills::rl_head::RoutingHead::new(dim)
            });
        agent.with_rl_head(head)
    } else {
        agent
    };

    // Wire tool dependency graph if enabled (#2024).
    let agent = if config.tools.dependencies.enabled && !config.tools.dependencies.rules.is_empty()
    {
        let graph = zeph_tools::ToolDependencyGraph::new(config.tools.dependencies.rules.clone());
        let always_on: std::collections::HashSet<String> =
            config.agent.tool_filter.always_on.iter().cloned().collect();
        agent
            .with_tool_dependency_graph(graph, always_on)
            .with_dependency_config(config.tools.dependencies.clone())
    } else {
        agent
    };

    let summary_provider = app.build_summary_provider();
    let agent = if let Some(sp) = summary_provider {
        agent.with_summary_provider(sp)
    } else {
        agent
    };
    let probe_provider = app.build_probe_provider();
    let agent = if let Some(pp) = probe_provider {
        agent.with_probe_provider(pp)
    } else {
        agent
    };
    let planner_provider = app.build_planner_provider();
    let agent = if let Some(pp) = planner_provider {
        agent.with_planner_provider(pp)
    } else {
        agent
    };
    let verify_provider = app.build_verify_provider();
    let agent = if let Some(vp) = verify_provider {
        agent.with_verify_provider(vp)
    } else {
        agent
    };
    let ensemble_members = app.build_ensemble_members();
    let agent = agent.with_ensemble_members(ensemble_members);
    let orchestrator_provider = app.build_orchestrator_provider();
    let agent = if let Some(op) = orchestrator_provider {
        agent.with_orchestrator_provider(op)
    } else {
        agent
    };
    let predicate_provider = app.build_predicate_provider();
    let agent = if let Some(pp) = predicate_provider {
        agent.with_predicate_provider(pp)
    } else {
        agent
    };
    let agent = agent_setup::apply_quarantine_provider(agent, app.build_quarantine_provider());
    let agent = agent_setup::apply_guardrail(agent, app.build_guardrail_provider());
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_injection_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_enforcement_mode(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_three_class_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_pii_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_pii_ner_classifier(agent, config);
    let agent = agent_setup::apply_causal_analyzer(
        agent,
        provider.clone(),
        config,
        app.secret_registry().as_ref(),
    );
    let agent = agent_setup::apply_nli_sanitizer(
        agent,
        provider.clone(),
        config,
        app.secret_registry().as_ref(),
    );
    let agent = agent_setup::apply_vigil(agent, &config.security.vigil);

    let judge_provider = app.build_judge_provider();
    let agent = if let Some(jp) = judge_provider {
        agent.with_judge_provider(jp)
    } else {
        agent
    };
    // #5437 round-3: apply_secret_masking must run after every with_*_provider call above —
    // it retroactively wraps each already-set AnyProvider field via AnyProvider::masked so
    // masking is structural, not per-call-site. judge_provider is the last provider setter in
    // this chain, so secret masking is wired here, not earlier alongside the other classifiers.
    let agent = agent_setup::apply_secret_masking(agent, app.secret_registry());
    let agent = if let Some(fc) = app.build_feedback_classifier(&provider) {
        agent.with_llm_classifier(fc)
    } else {
        agent
    };

    let agent = agent_setup::apply_cost_tracker(agent, config);

    let agent = if config.tools.anomaly.enabled {
        agent.with_anomaly_detector(zeph_tools::AnomalyDetector::new(
            config.tools.anomaly.window_size,
            config.tools.anomaly.error_threshold,
            config.tools.anomaly.critical_threshold,
        ))
    } else {
        agent
    };

    let agent = agent.with_document_config(config.memory.documents.clone());
    // Safe-mode gate (#6031): daemon had no prior bare-mode gate on hooks (unlike
    // `runner.rs`'s `exec_mode.bare`) — this is a fresh gate, safe-mode only.
    let agent = if config.cli.safe_mode {
        agent
    } else {
        agent.with_hooks_config(&config.hooks)
    };
    // Keep TrustGateExecutor's MCP tool-id registry in sync with MCP servers connected
    // after startup (#5747) — without this, check_tool_refresh has no handle to update.
    let mut agent = agent.with_mcp_tool_ids_handle(mcp_ids_handle);

    // Spec 050 Phase 2 (#5913): wire ShadowSentinel into the agent so begin_turn() calls
    // advance_turn(), matching src/runner.rs and src/acp.rs.
    if let Some(sentinel) = shadow_sentinel_arc {
        agent = agent.with_shadow_sentinel(sentinel);
    }

    // #5958: wire the trajectory risk slot/signal queue built above (spec 050 Invariant 2) plus
    // the TrajectorySentinel state machine itself into the agent, matching src/runner.rs.
    agent = agent
        .with_trajectory_risk_slot(trajectory_risk_slot)
        .with_signal_queue(trajectory_signal_queue)
        .with_trajectory_config(config.security.trajectory.clone())
        .0;

    // #5951: wire the self-check quality pipeline, matching src/runner.rs.
    agent = agent.with_quality_pipeline(agent_setup::build_quality_pipeline(
        config,
        &provider,
        app.secret_registry().as_ref(),
    ));

    // #5566: daemon mode (the process that actually serves `[gateway]` long-lived, since
    // `spawn_gateway_server` only forwards webhooks into an already-built agent) never wired
    // `[debug]`, unlike the CLI (src/runner.rs) and ACP (src/acp.rs) agent-construction paths.
    if config.debug.enabled {
        agent = agent_setup::apply_debug_dumper(
            agent,
            config.debug.output_dir.as_path(),
            config.debug.format,
            config.debug.include_raw_images,
        )
        .0;
    }

    agent.load_history().await?;
    agent
        .check_vector_store_health(config.memory.vector_backend.as_str())
        .await;

    let a2a_sanitizer = zeph_core::ContentSanitizer::new(&config.security.content_isolation);
    // Clone input_tx before consuming loopback_handle so the gateway can also inject
    // messages into the agent loop after the A2A server takes ownership of the handle.
    #[cfg(feature = "gateway")]
    let gateway_input_tx = loopback_handle.input_tx.clone();
    let ibct_keys = resolve_ibct_keys(config, app.vault()).await?;
    spawn_a2a_server(
        config,
        shutdown_rx.clone(),
        loopback_handle,
        a2a_sanitizer,
        Some(task_supervisor.clone()),
        &provider,
        ibct_keys,
    );

    #[cfg(feature = "gateway")]
    if config.gateway.enabled {
        spawn_gateway_server(
            config,
            shutdown_rx.clone(),
            gateway_input_tx,
            // Daemon mode has no MetricsSnapshot watch channel — skip Prometheus sync.
            #[cfg(feature = "prometheus")]
            None,
            Some(&task_supervisor),
        );
    }

    let mut supervisor = DaemonSupervisor::new(&config.daemon, shutdown_rx.clone());

    let shutdown_tx_signal = shutdown_tx.clone();
    let signal_fut = async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to register SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received Ctrl-C, initiating daemon shutdown");
                }
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM, initiating daemon shutdown");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("received Ctrl-C, initiating daemon shutdown");
        }
        let _ = shutdown_tx_signal.send(true);
    };
    let signal_cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(signal_fut)));
    task_supervisor.spawn(zeph_common::TaskDescriptor {
        name: "signal_handler",
        restart: zeph_common::RestartPolicy::RunOnce,
        factory: move || {
            let f = signal_cell.lock().take();
            async move {
                if let Some(f) = f {
                    f.await;
                }
            }
        },
    });

    // Spawn a sentinel task for the supervisor to track; agent runs in current task.

    let mut sentinel_rx = shutdown_rx.clone();
    let sentinel = tokio::spawn(async move {
        // EXEMPT(#5143): DaemonSupervisor::add_component requires JoinHandle by API contract
        let _ = sentinel_rx.changed().await;
        Ok(())
    });
    supervisor.add_component(ComponentHandle::new("agent-sentinel", sentinel));

    tokio::select! {
        result = agent.run() => {
            if let Err(e) = result {
                tracing::error!("agent exited with error: {e:#}");
            }
        }
        () = supervisor.run() => {}
    }

    // Explicitly shut down MCP connections before agent.shutdown() so that child processes
    // are killed while the tokio runtime is still active (#2693).
    shutdown_mcp_manager.shutdown_all_shared().await;
    agent.shutdown().await;

    // Dropping the guard releases the flock and unlinks the pid file.
    drop(pid_guard);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::channels::A2aServerConfig;
    use zeph_config::providers::{ProviderEntry, ProviderKind, SttConfig};
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    fn mock_provider() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default())
    }

    fn config_with_a2a(advertise_files: bool) -> Config {
        Config {
            a2a: A2aServerConfig {
                advertise_files,
                ..A2aServerConfig::default()
            },
            ..Config::default()
        }
    }

    /// Build a config that has an STT provider entry wired up, so `stt_provider_entry()` returns `Some`.
    fn config_with_stt(advertise_files: bool) -> Config {
        let mut cfg = config_with_a2a(advertise_files);
        cfg.llm.providers = vec![ProviderEntry {
            name: Some("stt-provider".into()),
            provider_type: ProviderKind::Ollama,
            stt_model: Some("whisper".into()),
            ..ProviderEntry::default()
        }];
        cfg.llm.stt = Some(SttConfig {
            provider: "stt-provider".into(),
            language: "en".into(),
        });
        cfg
    }

    // --- build_daemon_agent (#5819) ---

    async fn make_daemon_test_memory() -> std::sync::Arc<zeph_memory::semantic::SemanticMemory> {
        std::sync::Arc::new(
            zeph_memory::semantic::SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                mock_provider(),
                "test-model",
            )
            .await
            .unwrap(),
        )
    }

    fn build_daemon_agent_test_embed_fn(text: &str) -> zeph_skills::matcher::EmbedFuture {
        let _ = text;
        Box::pin(async { Ok(vec![1.0_f32, 0.0]) })
    }

    /// #5819 regression: `build_daemon_agent` must call `Agent::with_skill_matching_config` so
    /// `config.skills.confusability_threshold` reaches the real, constructed `Agent` via the
    /// same `AgentBuilder` chain `run_daemon()` actually uses at startup — the daemon-path
    /// counterpart to `build_agent_wires_skill_matching_config` (`src/runner.rs`, #5831), for
    /// the `BuildDaemonAgentDeps`/`build_daemon_agent` seam extracted from `run_daemon` (#5813,
    /// #5610, #5818). Asserts the *exact* threshold value echoed by `ConfusabilityReport`'s
    /// `Display` output, not just "non-default", so a swapped argument in
    /// `with_skill_matching_config` would also be caught.
    #[tokio::test]
    async fn build_daemon_agent_wires_skill_matching_config() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_daemon_test_memory().await;
        let conversation_id = memory.sqlite().create_conversation().await.unwrap();

        let mut config = Config::default();
        config.skills.disambiguation_threshold = 0.77;
        config.skills.two_stage_matching = true;
        config.skills.confusability_threshold = 0.42;

        let skill_meta = zeph_skills::loader::SkillMeta {
            name: "solo-skill".to_owned(),
            description: "a lone skill with no confusable sibling".to_owned(),
            ..Default::default()
        };
        let inner_matcher = zeph_skills::matcher::SkillMatcher::new(
            &[&skill_meta],
            build_daemon_agent_test_embed_fn,
        )
        .await
        .expect("single-skill matcher construction must succeed with a constant embed_fn");

        let (_reload_tx, reload_rx) = tokio::sync::mpsc::channel(1);
        let (_config_reload_tx, config_reload_rx) = tokio::sync::mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let shell_policy_handle =
            zeph_tools::ShellExecutor::new(&zeph_tools::ShellConfig::default()).policy_handle();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let provider_config_snapshot = crate::agent_setup::build_provider_config_snapshot(&config);
        let mcp_manager = std::sync::Arc::new(crate::bootstrap::create_mcp_manager_with_vault(
            &config, false, None,
        ));

        let deps = BuildDaemonAgentDeps {
            config: &config,
            provider: mock_provider(),
            embedding_provider: mock_provider(),
            registry: std::sync::Arc::new(RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: Some(zeph_skills::matcher::SkillMatcherBackend::InMemory(
                inner_matcher,
            )),
            tool_executor: zeph_tools::DynExecutor(std::sync::Arc::new(
                zeph_tools::SetCwdExecutor::new(vec![]),
            )),
            session_config,
            skill_paths: Vec::new(),
            reload_rx,
            plugin_dirs_supplier: || Vec::<PathBuf>::new(),
            memory: std::sync::Arc::clone(&memory),
            conversation_id,
            shutdown_rx,
            config_path: PathBuf::new(),
            config_reload_rx,
            shell_policy_handle,
            mcp_tools: Vec::new(),
            mcp_registry: None,
            mcp_manager,
            mcp_shared_tools: std::sync::Arc::new(RwLock::new(Vec::new())),
            provider_config_snapshot,
            trust_snapshot: std::sync::Arc::new(parking_lot::RwLock::new(
                std::collections::HashMap::new(),
            )),
        };

        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = Box::pin(build_daemon_agent(deps, channel)).await;

        let output = agent
            .handle_skills("confusability")
            .await
            .expect("handle_skills(\"confusability\") must not error");
        assert!(
            output.contains("above 0.42"),
            "config.skills.confusability_threshold = 0.42 must reach the built Agent's \
             ConfusabilityReport exactly (not e.g. 0.77, disambiguation_threshold's value, from a \
             swapped with_skill_matching_config argument); got: {output}"
        );
    }

    /// #5867 regression: `build_daemon_agent` must call `Agent::with_skill_group_config` so
    /// `config.skills.group_structured`/`support_similarity_threshold`/`min_injection_score`
    /// reach the real, constructed `Agent` via the same `AgentBuilder` chain `run_daemon()`
    /// actually uses at startup — the daemon-path counterpart to
    /// `build_agent_wires_skill_group_config` (`src/runner.rs`), for the same class of
    /// cold-start wiring gap previously found for `confusability_threshold` et al. (#5819).
    /// Asserts the exact values echoed by `/skills injection`'s `Display` output, not just
    /// "non-default", so a swapped argument in `with_skill_group_config` would also be caught.
    #[tokio::test]
    async fn build_daemon_agent_wires_skill_group_config() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_daemon_test_memory().await;
        let conversation_id = memory.sqlite().create_conversation().await.unwrap();

        let mut config = Config::default();
        config.skills.group_structured = true;
        config.skills.support_similarity_threshold = 0.73;
        config.skills.min_injection_score = 0.35;

        let skill_meta = zeph_skills::loader::SkillMeta {
            name: "solo-skill".to_owned(),
            description: "a lone skill with no confusable sibling".to_owned(),
            ..Default::default()
        };
        let inner_matcher = zeph_skills::matcher::SkillMatcher::new(
            &[&skill_meta],
            build_daemon_agent_test_embed_fn,
        )
        .await
        .expect("single-skill matcher construction must succeed with a constant embed_fn");

        let (_reload_tx, reload_rx) = tokio::sync::mpsc::channel(1);
        let (_config_reload_tx, config_reload_rx) = tokio::sync::mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let shell_policy_handle =
            zeph_tools::ShellExecutor::new(&zeph_tools::ShellConfig::default()).policy_handle();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let provider_config_snapshot = crate::agent_setup::build_provider_config_snapshot(&config);
        let mcp_manager = std::sync::Arc::new(crate::bootstrap::create_mcp_manager_with_vault(
            &config, false, None,
        ));

        let deps = BuildDaemonAgentDeps {
            config: &config,
            provider: mock_provider(),
            embedding_provider: mock_provider(),
            registry: std::sync::Arc::new(RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: Some(zeph_skills::matcher::SkillMatcherBackend::InMemory(
                inner_matcher,
            )),
            tool_executor: zeph_tools::DynExecutor(std::sync::Arc::new(
                zeph_tools::SetCwdExecutor::new(vec![]),
            )),
            session_config,
            skill_paths: Vec::new(),
            reload_rx,
            plugin_dirs_supplier: || Vec::<PathBuf>::new(),
            memory: std::sync::Arc::clone(&memory),
            conversation_id,
            shutdown_rx,
            config_path: PathBuf::new(),
            config_reload_rx,
            shell_policy_handle,
            mcp_tools: Vec::new(),
            mcp_registry: None,
            mcp_manager,
            mcp_shared_tools: std::sync::Arc::new(RwLock::new(Vec::new())),
            provider_config_snapshot,
            trust_snapshot: std::sync::Arc::new(parking_lot::RwLock::new(
                std::collections::HashMap::new(),
            )),
        };

        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = Box::pin(build_daemon_agent(deps, channel)).await;

        let output = agent
            .handle_skills("injection")
            .await
            .expect("handle_skills(\"injection\") must not error");
        assert_eq!(
            output,
            "Skill injection config: group_structured=true, support_similarity_threshold=0.73, \
             min_injection_score=0.35",
            "config.skills.group_structured/support_similarity_threshold/min_injection_score \
             must reach the built Agent exactly via with_skill_group_config; got: {output}"
        );
    }

    /// #5920/#5921 regression: `build_daemon_agent` must call `Agent::with_trust_config` and
    /// `Agent::with_rl_routing` so `config.skills.trust.*`/`rl_routing_enabled` reach the real,
    /// constructed `Agent` via the same `AgentBuilder` chain `run_daemon()` actually uses at
    /// startup — the same wire-X-into-ACP/serve/daemon defect class as
    /// `build_daemon_agent_wires_skill_group_config` above (#5867), applied to skill trust
    /// config and the `SkillOrchestra` RL routing head this time. Asserts the exact `/skills
    /// trust` `Display` output, not just "non-default", so a swapped argument or a dropped
    /// `.with_trust_config(...)`/`.with_rl_routing(...)` call would also be caught.
    #[tokio::test]
    async fn build_daemon_agent_wires_trust_and_rl_config() {
        use zeph_commands::traits::agent::AgentAccess as _;

        let memory = make_daemon_test_memory().await;
        let conversation_id = memory.sqlite().create_conversation().await.unwrap();

        let mut config = Config::default();
        config.skills.trust.default_level = zeph_common::SkillTrustLevel::Quarantined;
        config.skills.trust.local_level = zeph_common::SkillTrustLevel::Trusted;
        config.skills.trust.bundled_level = zeph_common::SkillTrustLevel::Verified;
        config.skills.trust.hash_mismatch_level = zeph_common::SkillTrustLevel::Blocked;
        config.skills.rl_routing_enabled = true;

        let (_reload_tx, reload_rx) = tokio::sync::mpsc::channel(1);
        let (_config_reload_tx, config_reload_rx) = tokio::sync::mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let shell_policy_handle =
            zeph_tools::ShellExecutor::new(&zeph_tools::ShellConfig::default()).policy_handle();
        let session_config = zeph_core::AgentSessionConfig::from_config(&config, 4096);
        let provider_config_snapshot = crate::agent_setup::build_provider_config_snapshot(&config);
        let mcp_manager = std::sync::Arc::new(crate::bootstrap::create_mcp_manager_with_vault(
            &config, false, None,
        ));

        let deps = BuildDaemonAgentDeps {
            config: &config,
            provider: mock_provider(),
            embedding_provider: mock_provider(),
            registry: std::sync::Arc::new(RwLock::new(
                zeph_skills::registry::SkillRegistry::empty(),
            )),
            matcher: None,
            tool_executor: zeph_tools::DynExecutor(std::sync::Arc::new(
                zeph_tools::SetCwdExecutor::new(vec![]),
            )),
            session_config,
            skill_paths: Vec::new(),
            reload_rx,
            plugin_dirs_supplier: || Vec::<PathBuf>::new(),
            memory: std::sync::Arc::clone(&memory),
            conversation_id,
            shutdown_rx,
            config_path: PathBuf::new(),
            config_reload_rx,
            shell_policy_handle,
            mcp_tools: Vec::new(),
            mcp_registry: None,
            mcp_manager,
            mcp_shared_tools: std::sync::Arc::new(RwLock::new(Vec::new())),
            provider_config_snapshot,
            trust_snapshot: std::sync::Arc::new(parking_lot::RwLock::new(
                std::collections::HashMap::new(),
            )),
        };

        let (channel, _handle) = zeph_core::LoopbackChannel::pair(8);
        let mut agent = Box::pin(build_daemon_agent(deps, channel)).await;

        let output = agent
            .handle_skills("trust")
            .await
            .expect("handle_skills(\"trust\") must not error");
        assert_eq!(
            output,
            "Skill trust config: default_level=Quarantined, local_level=Trusted, \
             bundled_level=Verified, hash_mismatch_level=Blocked | RL routing: enabled=true, \
             rl_head_loaded=false",
            "config.skills.trust.*/rl_routing_enabled must reach the built Agent exactly via \
             with_trust_config/with_rl_routing; got: {output}"
        );
    }

    /// Regression test for #5578 (dispatch-level companion to #5433's reachability
    /// test): calls the same `agent_setup::build_base_executor_chain` helper used by
    /// `run_daemon` above, wrapped in the same `TrustGateExecutor` (see #5575), and
    /// asserts a `diagnostics` `ToolCall` actually reaches `DiagnosticsExecutor` — not
    /// just that it appears in `tool_definitions()`. `Full` autonomy bypasses the
    /// trust gate's confirmation prompt so the call proceeds to the inner executor,
    /// exercising the trust gate's pass-through path rather than #5575's Ask path.
    #[tokio::test]
    async fn diagnostics_tool_call_dispatches_through_daemon_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

        let config = Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = agent_setup::build_diagnostics_executor(&config);
        let base_executor = agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
            vec![],
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

    /// Regression test for #5575's daemon gap found in review: `run_daemon` built the
    /// base chain with NO `TrustGateExecutor` at all, so `diagnostics` (and any other
    /// unconfigured, non-MCP/non-readonly tool) reached `LoopbackChannel::confirm` —
    /// which unconditionally returns `Ok(true)` — instead of ever producing
    /// `ConfirmationRequired`. Now that `run_daemon` wraps the chain in
    /// `TrustGateExecutor` (mirroring `runner.rs`), the default `Supervised` autonomy
    /// must require confirmation for `diagnostics` here too.
    #[tokio::test]
    async fn diagnostics_requires_confirmation_in_daemon_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

        let config = Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = agent_setup::build_diagnostics_executor(&config);
        let base_executor = agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
            vec![],
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
    struct DaemonTaggedMock(&'static str);

    impl zeph_tools::executor::ToolExecutor for DaemonTaggedMock {
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
                ..Default::default()
            }))
        }
        zeph_tools::tool_executor_no_inner_defaults!();
    }

    fn daemon_test_call(tool_id: &str) -> zeph_tools::ToolCall {
        zeph_tools::ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    /// Regression test for #5611: `run_daemon` composes `skill_loader -> memory -> overflow
    /// -> (base_chain -> mcp)` into one tree and gates the WHOLE thing via
    /// `agent_setup::apply_common_tool_gating`. Before the fix, only the base chain carried
    /// a `TrustGateExecutor`, so tools composed outside it (memory, mcp, skill loader) never
    /// reached `check_trust` at all. This mirrors that exact nesting order with lightweight
    /// mocks standing in for the real `MemoryToolExecutor`/`McpToolExecutor` (which need a
    /// live `SemanticMemory`/`McpManager`) and asserts Quarantine now reaches all of them.
    #[tokio::test]
    async fn quarantine_blocks_memory_and_mcp_in_daemon_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

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
        // mirroring `CompositeExecutor::new(base_executor, mcp_executor)` in `run_daemon`.
        let base_tool = zeph_tools::CompositeExecutor::new(
            DaemonTaggedMock("read"),
            DaemonTaggedMock("mcp_write_file"),
        );
        let inner_executor =
            zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
                DaemonTaggedMock("load_skill"),
                zeph_tools::CompositeExecutor::new(
                    DaemonTaggedMock("memory_save"),
                    zeph_tools::CompositeExecutor::new(
                        DaemonTaggedMock("overflow_flush"),
                        base_tool,
                    ),
                ),
            )));
        let (gated, mcp_ids_handle) = agent_setup::apply_common_tool_gating(
            inner_executor,
            &zeph_tools::PermissionPolicy::default(),
        );
        agent_setup::register_mcp_tool_ids(&mcp_ids_handle, std::slice::from_ref(&mcp_tool));
        gated.set_effective_trust(zeph_common::SkillTrustLevel::Quarantined);

        let memory_result = gated
            .execute_tool_call(&daemon_test_call("memory_save"))
            .await;
        assert!(
            matches!(memory_result, Err(zeph_tools::ToolError::Blocked { .. })),
            "memory_save must be denied under Quarantine, got {memory_result:?}"
        );

        let mcp_result = gated
            .execute_tool_call(&daemon_test_call(&mcp_tool_id))
            .await;
        assert!(
            matches!(mcp_result, Err(zeph_tools::ToolError::Blocked { .. })),
            "MCP-sourced tool must be denied under Quarantine, got {mcp_result:?}"
        );

        let skill_load_result = gated
            .execute_tool_call(&daemon_test_call("load_skill"))
            .await;
        assert!(
            matches!(
                skill_load_result,
                Err(zeph_tools::ToolError::Blocked { .. })
            ),
            "load_skill must be denied under Quarantine, got {skill_load_result:?}"
        );

        let read_result = gated.execute_tool_call(&daemon_test_call("read")).await;
        assert!(
            read_result.is_ok(),
            "readonly native tool must remain reachable under Quarantine, got {read_result:?}"
        );
    }

    /// Regression test confirming `PolicyGateExecutor` is reachable through the daemon
    /// composite chain: `run_daemon` previously built its full tool composite (skill
    /// loader/memory/overflow/base+MCP+search) with no declarative policy gate wired in at
    /// all, unlike the CLI path (`src/runner.rs`), so a configured `[tools.policy]` deny
    /// rule was silently ignored for every daemon-dispatched tool call. Reconstructs the
    /// same `base_executor` chain `run_daemon` builds (file/shell/scrape/diagnostics,
    /// wrapped in `TrustGateExecutor`) and layers `PolicyGateExecutor` on top exactly as
    /// `run_daemon` now does, asserting a deny rule for `diagnostics` is actually enforced.
    #[tokio::test]
    async fn policy_gate_denies_tool_in_daemon_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

        let config = Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = agent_setup::build_diagnostics_executor(&config);
        let base_executor = agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
            vec![],
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

    /// Combined regression test proving `PolicyGateExecutor` and `TrustGateExecutor`
    /// (`Quarantine` enforcement via `apply_common_tool_gating`) both enforce independently
    /// in the same composite chain: reconstructs the full production wiring order (outermost
    /// first) `PolicyGateExecutor -> TrustGateExecutor -> composite` and asserts that a
    /// declarative policy deny rule AND `TrustGateExecutor`'s Quarantine enforcement both
    /// survive being stacked together — neither gate silently swallows or bypasses the
    /// other, and a tool denied by neither still dispatches normally.
    #[tokio::test]
    async fn policy_and_quarantine_trust_gate_both_enforce_in_daemon_composite_chain() {
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
            DaemonTaggedMock("read"),
            DaemonTaggedMock("mcp_write_file"),
        );
        let inner_executor =
            zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
                DaemonTaggedMock("load_skill"),
                zeph_tools::CompositeExecutor::new(
                    DaemonTaggedMock("memory_save"),
                    zeph_tools::CompositeExecutor::new(
                        DaemonTaggedMock("overflow_flush"),
                        base_tool,
                    ),
                ),
            )));

        // TrustGateExecutor (innermost gate), Quarantined trust.
        let (trust_gated, mcp_ids_handle) = agent_setup::apply_common_tool_gating(
            inner_executor,
            &zeph_tools::PermissionPolicy::default(),
        );
        agent_setup::register_mcp_tool_ids(&mcp_ids_handle, std::slice::from_ref(&mcp_tool));
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
            .execute_tool_call(&daemon_test_call("overflow_flush"))
            .await;
        assert!(
            matches!(policy_denied, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected Blocked from PolicyGateExecutor's own deny rule, got {policy_denied:?}"
        );

        // Quarantine-denied tool (policy allows it by default): must still be blocked by
        // TrustGateExecutor's Quarantine check — proves TrustGate isn't shadowed by the
        // outer PolicyGate.
        let quarantine_denied = gated
            .execute_tool_call(&daemon_test_call("load_skill"))
            .await;
        assert!(
            matches!(
                quarantine_denied,
                Err(zeph_tools::ToolError::Blocked { .. })
            ),
            "expected Blocked from TrustGateExecutor's Quarantine enforcement, got {quarantine_denied:?}"
        );

        // Neither gate denies "read": must still dispatch successfully through the full
        // merged stack.
        let allowed = gated.execute_tool_call(&daemon_test_call("read")).await;
        assert!(
            allowed.is_ok(),
            "expected read to dispatch normally through the merged gate stack, got {allowed:?}"
        );
    }

    /// Regression test confirming `AdversarialPolicyGateExecutor` is reachable through the
    /// daemon composite chain: `run_daemon` never wired this gate in either, so
    /// `[tools.adversarial_policy]` (LLM-based tool review) had no effect on daemon-dispatched
    /// calls even when enabled. Same reconstructed `base_executor` chain as the sibling test
    /// above, layered with `AdversarialPolicyGateExecutor` driven by a fake `PolicyLlmClient`
    /// that always returns `DENY`, asserting the deny path is reached.
    #[tokio::test]
    async fn adversarial_policy_gate_denies_tool_in_daemon_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

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

        let config = Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = agent_setup::build_diagnostics_executor(&config);
        let base_executor = agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
            vec![],
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

    /// Regression test confirming `ScopedToolExecutor` (Spec 050 F2, #5913) is reachable
    /// through the daemon composite chain: `run_daemon` previously wrapped no capability-
    /// scope gate at all, so a configured `[security.capability_scopes]` scope was silently
    /// ignored for every daemon-dispatched tool call. Reconstructs the same `base_executor`
    /// chain the sibling `policy_gate_*` tests use and layers `ScopedToolExecutor` on top via
    /// `zeph_tools::scope::build_scoped_executor` exactly as `run_daemon` now does, asserting
    /// a tool outside the configured scope is rejected while an in-scope tool still dispatches.
    #[tokio::test]
    async fn capability_scopes_denies_tool_outside_scope_in_daemon_composite_chain() {
        use std::collections::HashSet;
        use zeph_tools::executor::ToolExecutor;
        use zeph_tools::scope::build_scoped_executor;

        let config = Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = agent_setup::build_diagnostics_executor(&config);
        let base_executor = agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
            vec![],
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
            .execute_tool_call(&daemon_test_call("diagnostics"))
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
        let allowed = scoped.execute_tool_call(&daemon_test_call("read")).await;
        assert!(
            !matches!(allowed, Err(zeph_tools::ToolError::OutOfScope { .. })),
            "expected read to reach past ScopedToolExecutor since it matches the active \
             scope's pattern, got {allowed:?}"
        );
    }

    /// Regression test confirming `PolicyGateExecutor`'s trajectory-risk signal queue (#5958,
    /// spec 050) is wired through `agent_setup::apply_policy_gate_chain` when called from
    /// `run_daemon`: previously `None` was passed for the `trajectory` parameter, so a
    /// declarative-policy denial never reached `TrajectorySentinel` for daemon-dispatched
    /// calls. Reconstructs the same trust-gated base chain the sibling
    /// `policy_gate_denies_tool_in_daemon_composite_chain` test uses, wraps it via
    /// `apply_policy_gate_chain` with a deny rule and
    /// `Some((&trajectory_risk_slot, &trajectory_signal_queue))` exactly as `run_daemon` now
    /// does, and asserts the signal queue receives the `PolicyDeny` code (`1`) after a denied
    /// call. This is the observable wiring surface reachable from this crate — the downstream
    /// `trajectory_risk_slot` mutation happens inside `Agent::begin_turn` (private to
    /// `zeph-core`, already covered by that crate's own `agent/trajectory.rs` unit tests).
    #[tokio::test]
    async fn trajectory_signal_queue_receives_policy_denial_in_daemon_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

        let config = Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = agent_setup::build_diagnostics_executor(&config);
        let base_executor = agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
            vec![],
        );
        let (trust_gated, _mcp_ids_handle) = agent_setup::apply_common_tool_gating(
            zeph_tools::DynExecutor(std::sync::Arc::new(base_executor)),
            &zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full),
        );

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
        let pieces = agent_setup::PolicyGatePieces {
            policy_enforcer: Some(std::sync::Arc::new(enforcer)),
            adversarial_validator: None,
            adversarial_llm_client: None,
            adv_policy_info: None,
            policy_configured: true,
        };

        let trajectory_risk_slot: zeph_tools::TrajectoryRiskSlot =
            std::sync::Arc::new(parking_lot::RwLock::new(0u8));
        let trajectory_signal_queue: zeph_tools::RiskSignalQueue =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

        let gated = agent_setup::apply_policy_gate_chain(
            trust_gated,
            &pieces,
            None,
            Some((&trajectory_risk_slot, &trajectory_signal_queue)),
        );

        let denied = gated
            .execute_tool_call(&daemon_test_call("diagnostics"))
            .await;
        assert!(
            matches!(denied, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected Blocked from PolicyGateExecutor's deny rule, got {denied:?}"
        );
        assert_eq!(
            *trajectory_signal_queue.lock(),
            vec![1u8],
            "expected the PolicyDeny signal code (1) to be pushed into the shared trajectory \
             signal queue after a denied tool call, proving apply_policy_gate_chain's daemon \
             call site actually wires PolicyGateExecutor::with_signal_queue instead of passing \
             None"
        );
    }

    /// Companion to `trajectory_signal_queue_receives_policy_denial_in_daemon_composite_chain`
    /// covering the other #5958 signal source `run_daemon` wires: `ScopedToolExecutor`
    /// (`[security.capability_scopes]`) `OutOfScope` denials. Before this PR, the daemon's
    /// `ScopedToolExecutor` was never given a signal queue, so capability-scope violations
    /// were invisible to `TrajectorySentinel`'s risk escalation. Reconstructs the same scope
    /// wrap the sibling `capability_scopes_denies_tool_outside_scope_in_daemon_composite_chain`
    /// test uses, adding `.with_signal_queue(...)` exactly as `run_daemon` now does, and
    /// asserts the queue receives the `OutOfScope` signal code (`3`).
    #[tokio::test]
    async fn trajectory_signal_queue_receives_scope_denial_in_daemon_composite_chain() {
        use std::collections::HashSet;
        use zeph_tools::executor::ToolExecutor;
        use zeph_tools::scope::build_scoped_executor;

        let config = Config::default();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = agent_setup::build_diagnostics_executor(&config);
        let base_executor = agent_setup::build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
            vec![],
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
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let scoped = scoped.with_signal_queue(std::sync::Arc::clone(&trajectory_signal_queue));

        let denied = scoped
            .execute_tool_call(&daemon_test_call("diagnostics"))
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
             signal queue after a scope-denied tool call, proving run_daemon's new \
             `.with_signal_queue(...)` call on the ScopedToolExecutor branch is reachable"
        );
    }

    /// Regression test confirming `SkillInvokeExecutor` (#5975) is reachable through the
    /// daemon composite chain: before this PR, `run_daemon` never constructed
    /// `SkillInvokeExecutor` at all, so `invoke_skill` calls fell through to
    /// memory/overflow/base (none of which handle that tool id) and would have surfaced as
    /// `ToolError::NotFound` instead of a skill body/summary. Reconstructs `run_daemon`'s
    /// `skill_loader -> skill_invoke -> ...` nesting order with the real production executor
    /// types (a lightweight `DaemonTaggedMock` stands in for memory, since only ordering and
    /// `SkillInvokeExecutor`'s own reachability are under test) and calls `invoke_skill` for a
    /// name absent from the (empty) registry — only `SkillInvokeExecutor` produces the
    /// `"skill not found: {name}"` summary text.
    #[tokio::test]
    async fn skill_invoke_executor_reachable_in_daemon_composite_chain() {
        use zeph_tools::executor::ToolExecutor;

        let registry =
            std::sync::Arc::new(RwLock::new(zeph_skills::registry::SkillRegistry::empty()));
        let (skill_loader_executor, skill_invoke_executor, _trust_snapshot) =
            agent_setup::build_skill_executors(&registry);

        let composite = zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                skill_invoke_executor,
                DaemonTaggedMock("memory_save"),
            ),
        );

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
        let output = composite
            .execute_tool_call(&call)
            .await
            .expect("invoke_skill must dispatch successfully through SkillInvokeExecutor")
            .expect("SkillInvokeExecutor must always return Some(ToolOutput) for invoke_skill");
        assert!(
            output
                .summary
                .contains("skill not found: nonexistent-skill"),
            "expected the \"skill not found: ...\" summary that only SkillInvokeExecutor \
             produces, proving invoke_skill reaches it in the daemon composite chain instead \
             of falling through to memory/overflow/base, got: {output:?}"
        );
    }

    /// Regression test confirming `ShadowProbeExecutor` (Spec 050 Phase 2, #5913) is reachable
    /// through the daemon composite chain: `run_daemon` previously never constructed a
    /// `ShadowSentinel`/`ShadowProbeExecutor` at all, so `[security.shadow_sentinel]` had no
    /// effect on daemon-dispatched calls even when enabled. Drives a real tool call through
    /// `ShadowProbeExecutor -> ShadowSentinelProbeGateAdapter -> ShadowSentinel::record_tool_event`
    /// using the same adapter type `run_daemon` now reuses from `src/runner.rs`
    /// (`crate::runner::ShadowSentinelProbeGateAdapter`, promoted to `pub(crate)` for this
    /// reuse), asserting the event is actually persisted — mirrors runner.rs's own precedent
    /// test but proves the daemon's own wiring block reaches the identical production chain.
    #[tokio::test]
    async fn shadow_probe_executor_reaches_shadow_sentinel_in_daemon_composite_chain() {
        use zeph_core::agent::shadow_sentinel::{
            ProbeVerdict, SafetyProbe, SentinelEvent, ShadowEventStore, ShadowSentinel,
        };
        use zeph_tools::executor::ToolExecutor;
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
                    ..Default::default()
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
            "daemon-conversation-42",
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
            .execute_tool_call(&daemon_test_call("builtin:shell"))
            .await;
        assert!(result.unwrap().is_some(), "tool call must succeed");
        // record_tool_event is fire-and-forget; drain before querying the store.
        sentinel.drain_pending().await;

        let events = ShadowEventStore::new(pool)
            .get_trajectory("daemon-conversation-42", 10)
            .await
            .expect("get_trajectory must succeed against the in-memory pool");
        assert!(
            events.iter().any(|e| e.event_type == "tool_call"
                && e.context_summary.as_deref() == Some("command completed")),
            "expected the ShadowProbeExecutor-driven tool_call event to be persisted via the \
             daemon's reused ShadowSentinelProbeGateAdapter, got: {events:?}"
        );
    }

    /// Regression test confirming all ten memory-maintenance loops (eviction, tier-promotion,
    /// scene-consolidation, consolidation, forgetting — #5914; plus guidelines,
    /// tree-consolidation, hebbian-consolidation, episodic-consolidation, optical-forgetting —
    /// #5979) are actually registered on the daemon's own `TaskSupervisor` by the shared
    /// `agent_setup::spawn_memory_maintenance_loops` (also called by `run_daemon` in production,
    /// and by `src/runner.rs`/`src/acp.rs`/`src/serve/deps.rs`, #6180) — asserts every expected
    /// task name is present in the daemon's memory supervisor snapshot. The five #5979 loops are
    /// config-gated, so the config below explicitly enables each.
    #[tokio::test]
    async fn daemon_memory_maintenance_loops_registered_on_mem_supervisor() {
        let mock_provider = mock_provider();
        let memory = make_daemon_test_memory().await;
        let mut config = Config::default();
        config.memory.compression_guidelines.enabled = true;
        config.memory.tree.enabled = true;
        config.memory.hebbian.enabled = true;
        config.memory.episodic_consolidation.enabled = true;
        config.memory.optical_forgetting.enabled = true;
        let app = AppBuilder::for_test(config);
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = zeph_common::TaskSupervisor::new(cancel);

        agent_setup::spawn_memory_maintenance_loops(
            &app,
            &memory,
            &mock_provider,
            &supervisor,
            None,
            false,
            "daemon",
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
                "expected {expected} registered on the daemon's memory supervisor, got {names:?}"
            );
        }
    }

    #[test]
    fn build_default_card_no_capabilities_by_default() {
        let cfg = config_with_a2a(false);
        let provider = mock_provider();
        // MockProvider::supports_vision() returns false; no STT; advertise_files=false
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        assert!(
            !card.capabilities.images,
            "images must be false without vision support"
        );
        assert!(!card.capabilities.audio, "audio must be false without STT");
        assert!(
            !card.capabilities.files,
            "files must be false when advertise_files=false"
        );
        assert!(card.capabilities.streaming, "streaming must always be true");
    }

    #[test]
    fn build_default_card_audio_from_stt_config() {
        let cfg = config_with_stt(false);
        let provider = mock_provider();
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        assert!(
            card.capabilities.audio,
            "audio must be true when STT provider is configured"
        );
        assert!(!card.capabilities.images);
        assert!(!card.capabilities.files);
    }

    #[test]
    fn build_default_card_files_from_advertise_files_flag() {
        let cfg = config_with_a2a(true);
        let provider = mock_provider();
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        assert!(
            card.capabilities.files,
            "files must be true when advertise_files=true"
        );
        assert!(!card.capabilities.images);
        assert!(!card.capabilities.audio);
    }

    #[test]
    fn build_default_card_audio_and_files_without_images() {
        let cfg = config_with_stt(true);
        let provider = mock_provider();
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        // images is still false because MockProvider::supports_vision() returns false
        assert!(!card.capabilities.images);
        assert!(card.capabilities.audio);
        assert!(card.capabilities.files);
        assert!(card.capabilities.streaming);
    }

    // --- a2a_owner_key (#6389) ---

    /// #6389 regression: distinct `context_id`s must derive distinct owner keys, so two A2A
    /// callers/sessions sharing one bearer token land in distinct cross-thread store buckets
    /// instead of both collapsing into `"local"`.
    #[test]
    fn a2a_owner_key_distinct_per_context_id() {
        let mut alice = zeph_a2a::Message::user_text("hi");
        alice.context_id = Some("alice-session".into());
        let mut bob = zeph_a2a::Message::user_text("hi");
        bob.context_id = Some("bob-session".into());

        assert_ne!(a2a_owner_key(&alice), a2a_owner_key(&bob));
        assert_eq!(a2a_owner_key(&alice), "a2a:alice-session");
    }

    /// A message with no `context_id` still gets a distinct, non-`"local"` bucket rather than
    /// silently falling back to the CLI/TUI default.
    #[test]
    fn a2a_owner_key_missing_context_id_uses_distinct_default() {
        let message = zeph_a2a::Message::user_text("hi");
        assert_eq!(a2a_owner_key(&message), "a2a:default");
        assert_ne!(a2a_owner_key(&message), "local");
    }

    /// A `context_id` longer than 256 chars must be truncated, bounding the value written
    /// into the `cross_thread_store` `owner_key` primary-key column.
    #[test]
    fn a2a_owner_key_truncates_long_context_id() {
        let mut message = zeph_a2a::Message::user_text("hi");
        message.context_id = Some("x".repeat(500));
        let key = a2a_owner_key(&message);
        assert_eq!(key, format!("a2a:{}", "x".repeat(256)));
    }
}
