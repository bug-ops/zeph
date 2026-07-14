// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use zeph_memory::store::agent_sessions::SessionStatus;

#[derive(Parser)]
#[command(
    name = "zeph",
    version,
    about = "Lightweight AI agent with hybrid inference"
)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Cli {
    /// Run with TUI dashboard
    #[arg(long)]
    pub(crate) tui: bool,

    /// Override the TUI theme for this session (preset or user file name, e.g. gruvbox-dark)
    #[arg(long, value_name = "NAME")]
    pub(crate) theme: Option<String>,

    /// Run in headless daemon mode (requires a2a feature)
    #[cfg(feature = "a2a")]
    #[arg(long)]
    pub(crate) daemon: bool,

    /// Run as ACP server over stdio for IDE embedding (requires acp feature)
    #[cfg(feature = "acp")]
    #[arg(long)]
    pub(crate) acp: bool,

    /// Print ACP agent manifest JSON to stdout and exit (requires acp feature)
    #[cfg(feature = "acp")]
    #[arg(long)]
    pub(crate) acp_manifest: bool,

    /// Run as ACP server over HTTP+SSE and WebSocket (requires acp-http feature)
    #[cfg(feature = "acp-http")]
    #[arg(long)]
    pub(crate) acp_http: bool,

    /// Bind address for the ACP HTTP server (requires acp-http feature)
    #[cfg(feature = "acp-http")]
    #[arg(long, value_name = "ADDR")]
    pub(crate) acp_http_bind: Option<String>,

    /// Bearer token for ACP HTTP/WebSocket authentication (overrides `acp.auth_token` config)
    #[cfg(feature = "acp-http")]
    #[arg(long, value_name = "TOKEN")]
    pub(crate) acp_auth_token: Option<String>,

    /// Additional directory ACP clients may reference in session requests (repeatable, overrides config)
    #[cfg(feature = "acp")]
    #[arg(long = "acp-additional-dir", value_name = "PATH")]
    pub(crate) acp_additional_dir: Vec<PathBuf>,

    /// Auth method to advertise in ACP initialize response (only "agent" accepted in MVP)
    #[cfg(feature = "acp")]
    #[arg(long = "acp-auth-method", value_name = "METHOD", value_parser = ["agent"])]
    pub(crate) acp_auth_method: Vec<String>,

    /// Enable echoing of `PromptRequest.message_id` in responses and chunks
    #[cfg(feature = "acp")]
    #[arg(long = "acp-message-ids", overrides_with = "no_acp_message_ids")]
    pub(crate) acp_message_ids: bool,

    /// Disable echoing of `PromptRequest.message_id` in responses and chunks
    #[cfg(feature = "acp")]
    #[arg(long = "no-acp-message-ids", overrides_with = "acp_message_ids")]
    pub(crate) no_acp_message_ids: bool,

    /// Connect TUI to a remote daemon via A2A SSE (requires tui + a2a features).
    /// Example: `--connect http://127.0.0.1:8080/a2a/stream` — loopback targets
    /// (127.0.0.1, `::1`, localhost) always work over plain HTTP out of the box.
    /// Non-loopback targets require HTTPS by default; see the `[a2a_client]` config
    /// section (separate from `[a2a]`, which only configures this process's own
    /// A2A server) to adjust `require_tls`/`ssrf_protection`.
    #[cfg(all(feature = "tui", feature = "a2a"))]
    #[arg(long, value_name = "URL")]
    pub(crate) connect: Option<String>,

    /// Path to config file
    #[arg(long, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    /// Secrets backend: "env" or "age"
    #[arg(long, value_name = "BACKEND")]
    pub(crate) vault: Option<String>,

    /// Path to age identity (private key) file
    #[arg(long, value_name = "PATH")]
    pub(crate) vault_key: Option<PathBuf>,

    /// Path to age-encrypted secrets file
    #[arg(long, value_name = "PATH")]
    pub(crate) vault_path: Option<PathBuf>,

    /// Run the interactive configuration wizard, then exit (flag alias for the `init` subcommand).
    ///
    /// Equivalent to `zeph init` with no `--output`. To choose an output path, use the subcommand
    /// form `zeph init --output <PATH>`. If both this flag and the `init` subcommand are supplied,
    /// the flag takes precedence and the subcommand is not consulted.
    #[arg(long)]
    pub(crate) init: bool,

    /// Add missing config parameters as commented-out entries, then exit (flag alias for the
    /// `migrate-config` subcommand).
    ///
    /// Select the file with the top-level `--config <PATH>`; combine with `--in-place` / `--diff`.
    /// If both this flag and the `migrate-config` subcommand are supplied, the flag takes
    /// precedence and the subcommand's own args are not consulted.
    #[arg(long = "migrate-config")]
    pub(crate) migrate_config: bool,

    /// Write the migrated config back to the source file. Requires `--migrate-config`.
    #[arg(long, requires = "migrate_config")]
    pub(crate) in_place: bool,

    /// Show a unified diff instead of full output. Requires `--migrate-config`.
    #[arg(long, requires = "migrate_config")]
    pub(crate) diff: bool,

    /// Enable Claude thinking mode: `extended:<budget_tokens>` or `adaptive` or `adaptive:<effort>`
    /// where effort is `low`, `medium`, or `high`. Overrides config.toml thinking setting.
    /// Examples: `--thinking extended:10000`  `--thinking adaptive`  `--thinking adaptive:high`
    #[arg(long, value_name = "MODE")]
    pub(crate) thinking: Option<String>,

    /// Set the default reasoning-effort level at startup for every configured provider that
    /// supports it (Claude adaptive thinking, `OpenAI`/Compatible `reasoning_effort`, Gemini
    /// thinking level). Overrides config.toml `reasoning_effort`/`thinking_level` settings.
    /// Can also be changed at runtime with `/reasoning-effort`. There is no `--think-tokens`
    /// startup flag — `--thinking extended:N` remains the Claude-only startup token-budget
    /// mechanism; use the runtime `/think-tokens` command for other providers.
    #[arg(long, value_name = "LEVEL", value_parser = ["low", "medium", "high"])]
    pub(crate) reasoning_effort: Option<String>,

    /// Additional sub-agent definition paths (file or directory containing .md files).
    /// Can be specified multiple times. Takes highest priority over all other sources.
    #[arg(long = "agents", value_name = "PATH")]
    pub(crate) agents: Vec<PathBuf>,

    /// Enable LLM-based guardrail (prompt injection pre-screening).
    /// Overrides `security.guardrail.enabled` from config.
    #[arg(long)]
    pub(crate) guardrail: bool,

    /// Enable graph-based knowledge memory (experimental)
    #[arg(long)]
    pub(crate) graph_memory: bool,

    /// Scan skill content for injection patterns on load (overrides config `scan_on_load`).
    /// Advisory only — results are logged as warnings; does not block tool calls.
    #[arg(long)]
    pub(crate) scan_skills_on_load: bool,

    /// Enable ACON failure-driven compression guidelines for this session.
    /// Overrides `memory.compression_guidelines.enabled` from config.
    /// Requires `compression-guidelines` feature at compile time; silently
    /// ignored if the feature is not enabled.
    #[arg(long)]
    pub(crate) compression_guidelines: bool,

    /// Enable Focus Agent for this session. Overrides `agent.focus.enabled` from config.
    #[arg(long)]
    pub(crate) focus: bool,

    /// Disable Focus Agent for this session.
    #[arg(long, conflicts_with = "focus")]
    pub(crate) no_focus: bool,

    /// Enable `SideQuest` eviction for this session. Overrides `memory.sidequest.enabled` from config.
    #[arg(long)]
    pub(crate) sidequest: bool,

    /// Disable `SideQuest` eviction for this session.
    #[arg(long, conflicts_with = "sidequest")]
    pub(crate) no_sidequest: bool,

    /// Override pruning strategy: reactive, `task_aware`, mig.
    /// Overrides `memory.compression.pruning_strategy` from config.
    #[arg(long, value_name = "STRATEGY")]
    pub(crate) pruning_strategy: Option<zeph_core::config::PruningStrategy>,

    /// Enable Claude server-side context compaction (compact-2026-01-12 beta).
    /// Requires a Claude provider. Overrides `llm.cloud.server_compaction` from config.
    #[arg(long)]
    pub(crate) server_compaction: bool,

    /// Enable Claude 1M extended context window for this session.
    /// Tokens above 200K use long-context pricing. Overrides `llm.cloud.enable_extended_context`
    /// from config. Requires a Claude provider.
    #[arg(long)]
    pub(crate) extended_context: bool,

    /// Enable automatic LSP context injection (diagnostics after writes, hover on reads).
    /// Requires mcpls MCP server configured under [mcp.servers].
    #[arg(long)]
    pub(crate) lsp_context: bool,

    /// Override log file path. Use bare `--log-file` (without a value) to disable file
    /// logging, overriding any config value. When omitted, uses the value from the `logging`
    /// config section (default: .zeph/logs/zeph.log).
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    pub(crate) log_file: Option<String>,

    /// Enable debug dump: write LLM requests/responses and raw tool output to files.
    /// Omit PATH to use the default directory from config (default: .zeph/debug).
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    pub(crate) debug_dump: Option<PathBuf>,

    /// Path to external policy rules file (TOML). Overrides `tools.policy.policy_file` from config.
    #[arg(long, value_name = "PATH")]
    pub(crate) policy_file: Option<PathBuf>,

    /// Set the initial capability scope task-type for this session.
    ///
    /// Must match a scope name defined in `[security.capability_scopes]`.
    /// The agent starts with this scope active; the operator can change it at runtime
    /// via `/scope <task_type>` (Phase 2). No-op when `ScopedToolExecutor` is not wired.
    #[arg(long = "scope", value_name = "TASK_TYPE")]
    pub(crate) initial_scope: Option<String>,

    /// Override debug dump format: `json`, `raw`, or `trace` (`OTel` OTLP spans).
    #[arg(long = "dump-format", value_name = "FORMAT")]
    pub(crate) dump_format: Option<zeph_core::debug_dump::DumpFormat>,

    /// Deny network egress to this domain from sandboxed shell commands (repeatable).
    ///
    /// Merges with `[tools.sandbox].denied_domains` from config. Patterns support exact
    /// hostnames (`"pastebin.com"`) and single-level wildcards (`"*.pastebin.com"`).
    /// Has no effect when the sandbox is disabled or unavailable.
    #[arg(long = "deny-domain", value_name = "DOMAIN")]
    pub(crate) deny_domain: Vec<String>,

    /// Abort startup if an effective OS sandbox cannot be activated.
    ///
    /// Equivalent to setting `[tools.sandbox].fail_if_unavailable = true` in config.
    /// Useful when `--deny-domain` is set and the egress filter must be enforced.
    #[arg(long = "no-sandbox-fallback")]
    pub(crate) no_sandbox_fallback: bool,

    /// Override scheduler tick interval in seconds (requires scheduler feature)
    #[cfg(feature = "scheduler")]
    #[arg(long, value_name = "SECS")]
    pub(crate) scheduler_tick: Option<u64>,

    /// Disable the scheduler even if enabled in config (requires scheduler feature)
    #[cfg(feature = "scheduler")]
    #[arg(long)]
    pub(crate) scheduler_disable: bool,

    /// Run a single experiment session and exit (requires experiments feature)
    #[arg(long)]
    pub(crate) experiment_run: bool,

    /// Print experiment results summary and exit (requires experiments feature)
    #[arg(long)]
    pub(crate) experiment_report: bool,

    /// Print default configuration as TOML to stdout and exit.
    ///
    /// Useful for bootstrapping a new config file or exploring available options.
    #[arg(long)]
    pub(crate) dump_config_defaults: bool,

    /// Disable pre-execution verifiers for tool calls.
    /// Use in trusted environments or when verifiers produce false positives.
    #[arg(long)]
    pub(crate) no_pre_execution_verify: bool,

    /// Enable Think-Augmented Function Calling (TAFC) for this session.
    /// Injects a reasoning step into complex tool schemas.
    /// Overrides `tools.tafc.enabled` from config.
    #[arg(long)]
    pub(crate) tafc: bool,

    /// Bare mode: skip skill loading, memory init, MCP connections, scheduler
    /// startup, and filesystem watchers. Useful for scripting and CI pipelines.
    #[arg(long)]
    pub(crate) bare: bool,

    /// Safe mode: start this session with ZEPH.md/CLAUDE.md/AGENTS.md project
    /// instructions, plugins, skills, hooks, and MCP servers all disabled, to
    /// isolate whether a customization is causing unwanted behavior. Distinct
    /// from `--bare`, which skips memory/tool-registry/background-task overhead
    /// instead — the two flags are independent and composable. Session-scoped
    /// only; never persisted to config.toml. Equivalent to `ZEPH_SAFE_MODE=true`.
    #[arg(long)]
    pub(crate) safe_mode: bool,

    /// Emit structured JSON events to stdout (JSONL, one event per line).
    /// Safe for piping into `jq`. Forces all log output to stderr.
    /// Mutually exclusive with `--tui` and `--acp`.
    #[arg(long)]
    pub(crate) json: bool,

    /// Auto-approve trust-gate prompts. Equivalent to
    /// `[security] autonomy_level = "full"`. The adversarial policy gate (if
    /// enabled) still runs. Destructive-command blocklist still applies.
    #[arg(long = "auto", short = 'y')]
    pub(crate) auto: bool,

    /// URL of an ephemeral plugin archive to load for this session only (HTTPS required).
    ///
    /// May be repeated to load multiple plugins. Each value is either a plain URL or a
    /// `url@sha256` pair for integrity pinning, e.g.:
    ///
    ///   `--plugin-url https://example.com/p.tar.gz@abc123...`
    ///
    /// The archive is downloaded, scanned for injection patterns, and loaded into a temporary
    /// directory that is cleaned up on process exit. The plugin is never written to the
    /// permanent plugins store.
    #[arg(long, action = clap::ArgAction::Append)]
    pub(crate) plugin_url: Vec<String>,

    /// Override the `worktree.base_ref` config for this session.
    ///
    /// Accepted values: `head` (branch from local HEAD), `fresh` (fetch origin first).
    /// Ignored when `worktree.enabled = false`.
    #[arg(long, value_name = "REF")]
    pub(crate) worktree_base_ref: Option<String>,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    /// Session id to resume interactively (spec-068 D-6, #5343).
    ///
    /// Not a CLI flag — populated programmatically when dispatching `sessions resume <id>`
    /// (without `--print`) to the interactive `runner::run` path instead of the one-shot
    /// hydration-summary dump.
    #[arg(skip)]
    pub(crate) resume_session_id: Option<String>,

    /// Initial prompt pre-queued from a deep-link URI (set by `handle_url_open` before bootstrap).
    ///
    /// Not a CLI flag — populated programmatically by the `url-open` dispatch arm.
    #[cfg(feature = "deep-link")]
    #[arg(skip)]
    pub(crate) deep_link_prompt: Option<String>,

    /// URI that originated this session (emitted as a TUI status notification).
    ///
    /// Not a CLI flag — populated programmatically by the `url-open` dispatch arm.
    #[cfg(feature = "deep-link")]
    #[arg(skip)]
    pub(crate) deep_link_uri: Option<String>,
}

#[cfg(test)]
impl Default for Cli {
    fn default() -> Self {
        use clap::Parser;
        Cli::parse_from(["zeph"])
    }
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Interactive configuration wizard
    Init {
        /// Output path for generated config
        #[arg(long, short, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Manage the age-encrypted secrets vault
    Vault {
        #[command(subcommand)]
        command: VaultCommand,
    },
    /// Manage external skills
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Manage plugins (bundled skills + MCP servers)
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Manage memory snapshots
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Ingest a document into semantic memory
    Ingest {
        /// Path to document or directory to ingest
        path: PathBuf,
        /// Chunk size in characters
        #[arg(long, default_value = "1000")]
        chunk_size: usize,
        /// Chunk overlap in characters
        #[arg(long, default_value = "100")]
        chunk_overlap: usize,
        /// Target Qdrant collection name
        #[arg(long, default_value = "zeph_documents")]
        collection: String,
    },
    /// Manage scheduled jobs
    #[cfg(feature = "scheduler")]
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// Manage conversation-session history (spec-068, #5343)
    #[cfg(any(feature = "acp", feature = "session"))]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Run as a persistent agent service exposing sessions over HTTP/SSE (spec-068 §9, #5343).
    ///
    /// Named `serve-sessions` rather than the spec's literal `serve` — `Command::Serve` is
    /// already the scheduler's foreground-daemon command (`#[cfg(all(unix, feature =
    /// "scheduler"))]`, `src/cli.rs` above); both features can be enabled simultaneously, so a
    /// second command claiming the same top-level name is not viable.
    #[cfg(feature = "session")]
    ServeSessions {
        /// HTTP/SSE API bind address (overrides `[serve] http_addr`).
        #[arg(long, value_name = "ADDR")]
        http_addr: Option<String>,
        /// Also run the ACP-HTTP protocol transport in-process, on a second listener bound to
        /// `[acp] http_bind` (#5420). Requires the `acp-http` feature (bundled in `ide`).
        ///
        /// Distinct from the standalone `zeph --acp` (stdio) and `zeph --acp-http` commands:
        /// this flag shares serve-sessions' own `SemanticMemory`/`SQLite` pool and
        /// `TaskSupervisor` with the ACP transport, rather than running a second, independent
        /// process with its own pool. ACP stdio is never used here — it has no cancellation
        /// hook and reads immediate EOF under a daemon's `StandardInput=null`, so it can't be
        /// lifecycle-managed alongside a network daemon; combined mode is HTTP-only for both
        /// transports. Fails fast if `[serve] http_addr` and `[acp] http_bind` would bind
        /// overlapping addresses on the same port.
        #[arg(long)]
        acp: bool,
        /// Maximum concurrent live sessions (overrides `[serve] max_sessions`).
        #[arg(long, value_name = "N")]
        max_sessions: Option<usize>,
    },
    /// Inspect or reset Thompson Sampling router state
    Router {
        #[command(subcommand)]
        command: RouterCommand,
    },
    /// Manage sub-agent definitions
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },
    /// Start the scheduler daemon in the background (Unix only).
    ///
    /// Acquires an exclusive pid file lock so only one instance runs per config.
    /// Use `--foreground` for systemd / launchd managed processes.
    #[cfg(all(unix, feature = "scheduler"))]
    Serve {
        /// Run in the foreground instead of detaching (useful for systemd / launchd).
        #[arg(long)]
        foreground: bool,
        /// Disable catch-up: do not replay overdue tasks on startup.
        #[arg(long)]
        no_catch_up: bool,
    },
    /// Stop the running scheduler daemon (Unix only).
    #[cfg(all(unix, feature = "scheduler"))]
    Stop {
        /// Seconds to wait for graceful shutdown before escalating to SIGKILL. Default: 10.
        #[arg(long, default_value = "10")]
        timeout_secs: u64,
    },
    /// Show scheduler daemon status and recent task runs (Unix only).
    #[cfg(all(unix, feature = "scheduler"))]
    Status {
        /// Emit output as JSON (stable schema for scripting).
        #[arg(long)]
        json: bool,
        /// Number of recent task runs to display. Default: 10.
        #[arg(long, short, default_value = "10")]
        n: usize,
    },
    /// Add missing config parameters as commented-out entries, preserving existing values
    MigrateConfig {
        /// Path to config file (default: `config/default.toml` or `ZEPH_CONFIG`)
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        /// Write the migrated config back to the source file (atomic rename, preserves permissions)
        #[arg(long)]
        in_place: bool,
        /// Show a unified diff instead of the full output
        #[arg(long)]
        diff: bool,
    },
    /// Manage ML classifier models
    Classifiers {
        #[command(subcommand)]
        command: crate::commands::classifiers::ClassifiersCommand,
    },
    /// Manage the database
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Inspect the durable execution journal (connects directly to `durable.db`)
    Durable {
        #[command(subcommand)]
        command: DurableCommand,
    },
    /// ACP sub-agent client commands
    #[cfg(feature = "acp")]
    Acp {
        #[command(subcommand)]
        command: AcpCommand,
    },
    /// Run preflight connectivity and configuration checks
    Doctor {
        /// Emit results as JSON (`schema_version` = 1)
        #[arg(long)]
        json: bool,
        /// Timeout in seconds for LLM provider probes and SQLite/Qdrant checks
        #[arg(long, default_value = "10")]
        llm_timeout_secs: u64,
        /// Timeout in seconds for MCP server connection probes
        #[arg(long, default_value = "5")]
        mcp_timeout_secs: u64,
    },
    /// Gonka network diagnostics and credential checks
    #[cfg(feature = "gonka")]
    Gonka {
        #[command(subcommand)]
        command: GonkaCommand,
    },
    /// Cocoon sidecar diagnostics
    #[cfg(feature = "cocoon")]
    Cocoon {
        #[command(subcommand)]
        command: CocoonCommand,
    },
    /// Test notification channels (sends a test notification via enabled channels)
    Notify {
        #[command(subcommand)]
        command: NotifyCommand,
    },
    /// Run agent benchmarks against standardized datasets
    #[cfg(feature = "bench")]
    Bench {
        #[command(subcommand)]
        command: zeph_bench::BenchCommand,
    },
    /// Project-level management commands
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage git worktrees used by sub-agents
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Ingest project knowledge artifacts into semantic memory (spec-067)
    Knowledge {
        #[command(subcommand)]
        command: KnowledgeCommand,
    },
    /// Open a session from a zeph:// URI dispatched by the OS scheme handler.
    #[cfg(feature = "deep-link")]
    UrlOpen {
        /// The `zeph://` URI to dispatch (e.g. `zeph://new-session?prompt=Hello`)
        uri: String,
    },
    /// Manage OS-level `zeph://` scheme registration.
    #[cfg(feature = "deep-link")]
    UrlScheme {
        #[command(subcommand)]
        command: UrlSchemeCommand,
    },
}

#[derive(Subcommand)]
pub(crate) enum WorktreeCommand {
    /// List active and stale worktrees for the current repository
    List,
    /// Remove stale worktrees that exist on disk but are not tracked in-session
    Clean {
        /// Also remove worktrees whose directory still exists and is not
        /// marked `prunable` by git — i.e. worktrees that may belong to
        /// another, currently running zeph session. Requires this explicit
        /// flag; never applied automatically.
        #[arg(long)]
        force: bool,
    },
}

/// Project-knowledge ingest subcommands (spec-067).
#[derive(Subcommand)]
pub(crate) enum KnowledgeCommand {
    /// Load project artifacts into semantic memory via the notes sink
    Ingest {
        /// Artifact sources to process (may be specified multiple times or comma-separated)
        #[arg(long = "source", value_name = "SRC", required = true, num_args = 1..)]
        sources: Vec<KnowledgeSource>,
        /// Preview only: report files / chunks / estimated tokens without writing anything
        #[arg(long)]
        dry_run: bool,
        /// Maximum number of documents to process; `0` uses the config default (unlimited)
        #[arg(long, default_value = "0")]
        max_documents: usize,
        /// Provider name override from `[[llm.providers]]`
        #[arg(long)]
        provider: Option<String>,
        /// Skip any confirmation prompts (reserved for Phase 2 graph writes)
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Roll back a previous import batch: removes all graph edges, entities, and ledger rows
    Rollback {
        /// `import_batch_id` to delete from the graph and ledger
        #[arg(long)]
        batch_id: String,
        /// Skip the confirmation prompt (non-interactive / scripted use)
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List import batches and ledger summary
    Status,
}

/// Artifact sources eligible for `zeph knowledge ingest`.
///
/// Corresponds to the `--source` flag. Clap enums are purely additive — new variants
/// do not break existing callers. Phase-3 external-agent sources (`claude-code`,
/// `codex`) require an explicit `--yes` flag and trigger a confirmation gate.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Eq)]
pub(crate) enum KnowledgeSource {
    /// Specification documents under `specs/**/*.md`
    Specs,
    /// Top-level `CHANGELOG.md`
    Changelog,
    /// Agent handoff files under `.local/handoff/**/*.md`
    Handoff,
    /// Coverage-status file at `.local/testing/coverage-status.md`
    Coverage,
    /// Recent git log (captured in-memory; bounded by `--max-documents`)
    #[value(name = "git-log")]
    GitLog,
    /// Zeph subagent transcripts of the current project (graph sink, spec-067 Phase 2)
    Subagents,
    /// Claude Code session transcripts for the current project
    ///
    /// Reads `~/.claude/projects/<project-slug>/*.jsonl` (current project only).
    /// Requires `--yes` confirmation. Writes to the knowledge graph with
    /// `origin='external-agent'`.
    #[value(name = "claude-code")]
    ClaudeCode,
    /// `OpenAI` Codex CLI session transcripts for the current project
    ///
    /// Reads `~/.codex/archived_sessions/*.jsonl`, filtered to sessions whose
    /// `cwd` matches the current project root. Requires `--yes` confirmation.
    /// Writes to the knowledge graph with `origin='external-agent'`.
    Codex,
}

/// Project management subcommands.
#[derive(Subcommand)]
pub(crate) enum ProjectCommand {
    /// Remove all project data: memory, database, skill outcomes, and debug artifacts
    Purge {
        /// Path to config file (overrides default resolution)
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Show what would be removed without deleting anything
        #[arg(long)]
        dry_run: bool,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
}

/// ACP sub-agent client subcommands.
#[cfg(feature = "acp")]
#[derive(Subcommand)]
pub(crate) enum AcpCommand {
    /// Run a one-shot prompt against an ACP sub-agent and print the response
    RunAgent {
        /// Shell command to spawn the sub-agent (e.g. "cargo run -- --acp")
        #[arg(long, short)]
        command: String,

        /// Prompt text to send
        #[arg(long, short)]
        prompt: Option<String>,

        /// Working directory for the subprocess (sets both `process_cwd` and `session_cwd`)
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,

        /// Handshake + session timeout in seconds
        #[arg(long, default_value = "600")]
        timeout: u64,
    },
    /// Sub-agent preset management
    Subagent {
        #[command(subcommand)]
        command: AcpSubagentCommand,
    },
    /// Model-related configuration parameters (`[acp.model_config]`)
    ModelConfig {
        #[command(subcommand)]
        command: AcpModelConfigCommand,
    },
}

/// Sub-agent preset subcommands.
#[cfg(feature = "acp")]
#[derive(Subcommand)]
pub(crate) enum AcpSubagentCommand {
    /// List configured sub-agent presets
    List,
}

/// Model-config subcommands.
#[cfg(feature = "acp")]
#[derive(Subcommand)]
pub(crate) enum AcpModelConfigCommand {
    /// Show available sampling-temperature presets and the configured default
    Show,
}

/// Database subcommands.
#[derive(Subcommand)]
pub(crate) enum DbCommand {
    /// Run pending database migrations
    Migrate,
}

/// Durable execution journal subcommands.
///
/// All subcommands connect directly to `durable.db`; no running agent process is required. Default
/// output is redacted (INV-5) — payload bytes and resolver tokens are shown only with `--reveal`.
#[derive(Subcommand)]
pub(crate) enum DurableCommand {
    /// List durable executions, newest first
    List {
        /// Filter by status: running | completed | failed | aborted
        #[arg(long)]
        status: Option<String>,
        /// Filter by execution kind (e.g. `agent_turn`, `dag_run`, `scheduled_job`, `subagent_session`)
        #[arg(long)]
        kind: Option<String>,
        /// Maximum number of executions to display
        #[arg(long, default_value = "50")]
        limit: i64,
        /// Emit structured JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Show the journal entries of one execution (payload redacted by default)
    Show {
        /// Execution id (UUID)
        id: String,
        /// Decrypt and print payload bytes (prints a warning first; requires `ZEPH_DURABLE_KEY`)
        #[arg(long, conflicts_with = "json")]
        reveal: bool,
        /// Emit structured JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Inspect a single journal step entry
    Inspect {
        /// Execution id (UUID)
        id: String,
        /// Step index to inspect
        #[arg(long)]
        step: u32,
        /// Decrypt and print payload bytes (prints a warning first; requires `ZEPH_DURABLE_KEY`)
        #[arg(long, conflicts_with = "json")]
        reveal: bool,
        /// Emit structured JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Force the crash-orphan sweep, then a retention sweep over terminal executions past their TTL
    Prune {
        /// Report how many executions would be aborted/pruned without mutating anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Trigger a manual replay of a supported execution
    Resume {
        /// Execution id (UUID)
        id: String,
    },
}

/// Typed session status filter for the `agents fleet` sub-command.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum FleetStatus {
    Active,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl From<FleetStatus> for SessionStatus {
    fn from(s: FleetStatus) -> Self {
        match s {
            FleetStatus::Active => SessionStatus::Active,
            FleetStatus::Completed => SessionStatus::Completed,
            FleetStatus::Failed => SessionStatus::Failed,
            FleetStatus::Cancelled => SessionStatus::Cancelled,
            FleetStatus::Unknown => SessionStatus::Unknown,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum AgentsCommand {
    /// List all available sub-agent definitions
    List,
    /// Show full definition of a sub-agent
    Show {
        /// Agent name
        name: String,
    },
    /// Create a new sub-agent definition
    Create {
        /// Agent name (must match `[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}`)
        name: String,
        /// Short description
        #[arg(long, short)]
        description: String,
        /// Target directory (default: .zeph/agents)
        #[arg(long, default_value = ".zeph/agents")]
        dir: std::path::PathBuf,
        /// Model to use (optional, inherits from parent config)
        #[arg(long)]
        model: Option<String>,
    },
    /// Edit a sub-agent definition in $VISUAL or $EDITOR
    Edit {
        /// Agent name
        name: String,
    },
    /// Delete a sub-agent definition
    Delete {
        /// Agent name
        name: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// List agent sessions recorded in the fleet database
    Fleet {
        /// Filter by session status
        #[arg(long, short)]
        status: Option<FleetStatus>,
        /// Maximum number of sessions to show
        #[arg(long, default_value = "20")]
        limit: u32,
    },
}

#[derive(Subcommand)]
pub(crate) enum MemoryCommand {
    /// Export memory to a JSON snapshot file
    Export {
        /// Output file path
        path: PathBuf,
    },
    /// Import memory from a JSON snapshot file
    Import {
        /// Input file path
        path: PathBuf,
    },
    /// Run the `SleepGate` forgetting sweep once and print the result
    ForgettingSweep,
    /// Show trajectory memory statistics (entry count by kind)
    Trajectory,
    /// Show memory tree statistics (node count by level)
    Tree,
}

#[derive(Subcommand)]
pub(crate) enum SkillCommand {
    /// Install a skill from a git URL or local path
    Install {
        /// Git URL or local directory path
        source: String,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name
        name: String,
    },
    /// List installed skills
    List,
    /// Verify skill integrity (blake3 hash check)
    Verify {
        /// Skill name (omit to verify all)
        name: Option<String>,
    },
    /// Set trust level for a skill
    Trust {
        /// Skill name
        name: String,
        /// Trust level: trusted, verified, quarantined, blocked
        level: String,
        /// Enable per-invocation blake3 integrity re-check: re-hash SKILL.md before every
        /// dispatch and demote to quarantined on mismatch (#4293, #6080). Forces the check on
        /// regardless of `[skills.trust] require_integrity_check_on_promote`.
        #[arg(long, conflicts_with = "no_require_check")]
        require_check: bool,
        /// Skip arming the per-invocation integrity re-check even if
        /// `[skills.trust] require_integrity_check_on_promote` defaults it on (#6087)
        #[arg(long)]
        no_require_check: bool,
    },
    /// Block a skill
    Block {
        /// Skill name
        name: String,
    },
    /// Unblock a skill (sets to quarantined)
    Unblock {
        /// Skill name
        name: String,
    },
    /// Preview a skill body with trust-aware sanitization (same pipeline as the agent)
    Invoke {
        /// Skill name
        name: String,
        /// Optional arguments appended as <args>…</args> block
        #[arg(long)]
        args: Option<String>,
    },
    /// Trigger heuristic promotion evaluation manually (`AutoSkill A6`)
    PromoteHeuristics {
        /// Evaluate only this skill (omit to evaluate all qualifying skills)
        #[arg(long)]
        skill: Option<String>,
    },
    /// Search the configured external skill registry by keyword (spec-045, #5869)
    ///
    /// Requires `[skills.registry] enabled = true` in config.toml; see `zeph --init` or
    /// `--migrate-config`. Distinct from `install`, which installs from a git URL/local path.
    Search {
        /// Search query (min 2 characters)
        query: String,
    },
    /// Install a skill by registry ID returned by `skill search` (spec-045, #5869)
    ///
    /// Named `get` (not `add`) to stay unambiguous with `install`, which installs from a git
    /// URL/local path. Requires `[skills.registry] enabled = true` in config.toml. Fetched
    /// packages route through the same frontmatter validation, injection scan, and
    /// Quarantined-trust upsert as `skill install`.
    Get {
        /// Registry-assigned skill ID (as printed by `skill search`)
        registry_id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum PluginCommand {
    /// List installed plugins, optionally showing the active config overlay
    List {
        /// Show plugin overlay: which plugins contributed to the active config and which
        /// were skipped (with reasons). Values shown against `Config::default()` — use
        /// --config for live intersection.
        #[arg(long)]
        overlay: bool,
    },
    /// Install a plugin from a local directory path
    Add {
        /// Local directory path to the plugin root (must contain plugin.toml)
        source: String,
    },
    /// Remove an installed plugin
    Remove {
        /// Plugin name
        name: String,
    },
    /// Search the configured external skill/plugin registry by keyword (spec-045, #5869)
    ///
    /// Requires `[skills.registry] enabled = true` in config.toml; see `zeph --init` or
    /// `--migrate-config`.
    Search {
        /// Search query (min 2 characters)
        query: String,
    },
    /// Install a plugin by registry ID returned by `plugin search` (spec-045, #5869)
    ///
    /// Named `get` (not `add`) to stay unambiguous with `plugin add <local-path>` above.
    /// Requires `[skills.registry] enabled = true`. Fails with a pointer to `skill get` when
    /// the fetched package has no `plugin.toml` (i.e. it is a bare skill package).
    Get {
        /// Registry-assigned plugin ID (as printed by `plugin search`)
        registry_id: String,
    },
}

#[cfg(feature = "scheduler")]
#[derive(Subcommand)]
pub(crate) enum ScheduleCommand {
    /// List all active scheduled jobs
    List,
    /// Add a new periodic cron job
    Add {
        /// Cron expression (5 or 6 fields, e.g. "0 * * * *")
        cron: String,
        /// Task prompt to execute on each trigger
        prompt: String,
        /// Job name (auto-generated from prompt if omitted)
        #[arg(long)]
        name: Option<String>,
        /// Task kind (default: "custom")
        #[arg(long, default_value = "custom")]
        kind: String,
    },
    /// Remove a scheduled job by name
    Remove {
        /// Job name to remove
        name: String,
    },
    /// Show details of a scheduled job
    Show {
        /// Job name to inspect
        name: String,
    },
}

#[cfg(any(feature = "acp", feature = "session"))]
#[derive(Subcommand)]
pub(crate) enum SessionsCommand {
    /// List recent conversation-sessions
    List,
    /// Resume a session as a live interactive agent, replaying its event log to reconstruct
    /// history (spec-068 D-6, #5343)
    Resume {
        /// Session ID
        id: String,
        /// Dump the session's raw JSONL events to stdout instead of resuming interactively
        /// (the pre-#5343 one-shot dump behavior)
        #[arg(long)]
        print: bool,
    },
    /// Show metadata (and optionally events) for a single session
    Show {
        /// Session ID
        id: String,
        /// Only include events with `seq >= FROM`
        #[arg(long, value_name = "SEQ")]
        from: Option<u64>,
        /// Only include events with `seq < TO`
        #[arg(long, value_name = "SEQ")]
        to: Option<u64>,
        /// Also print the session's events (JSONL, one per line)
        #[arg(long)]
        events: bool,
    },
    /// Delete a session and its event log
    Delete {
        /// Session ID
        id: String,
    },
    /// Fork a session into a new, independent child session
    Fork {
        /// Session ID to fork from
        id: String,
        /// Fork at this event `seq` (exclusive upper bound); omit to fork at the current end of
        /// the log (copies everything)
        #[arg(long, value_name = "SEQ")]
        at: Option<u64>,
    },
    /// Export a session's event log to a JSONL file
    Export {
        /// Session ID to export
        id: String,
        /// Destination path for the exported JSONL file
        path: std::path::PathBuf,
    },
    /// Import a session event log from a JSONL file as a new session
    Import {
        /// Source JSONL file (as produced by `sessions export`)
        path: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
pub(crate) enum RouterCommand {
    /// Show current Thompson Sampling alpha/beta per provider
    Stats {
        /// Path to Thompson state file (default: `~/.zeph/router_thompson_state.json`)
        #[arg(long, value_name = "PATH")]
        state_path: Option<std::path::PathBuf>,
    },
    /// Delete the Thompson state file (resets to uniform priors)
    Reset {
        /// Path to Thompson state file (default: `~/.zeph/router_thompson_state.json`)
        #[arg(long, value_name = "PATH")]
        state_path: Option<std::path::PathBuf>,
    },
}

/// Gonka network subcommands.
#[cfg(feature = "gonka")]
#[derive(Subcommand)]
pub(crate) enum GonkaCommand {
    /// Run Gonka connectivity and credential diagnostics
    Doctor {
        /// Emit results as JSON (`schema_version` = 1)
        #[arg(long)]
        json: bool,
        /// Timeout in seconds for node probe requests
        #[arg(long, default_value = "10")]
        timeout_secs: u64,
    },
}

/// Cocoon sidecar subcommands.
#[cfg(feature = "cocoon")]
#[derive(Subcommand)]
pub(crate) enum CocoonCommand {
    /// Run Cocoon sidecar connectivity and configuration diagnostics
    Doctor {
        /// Emit results as JSON (`schema_version` = 1)
        #[arg(long)]
        json: bool,
        /// Timeout in seconds for HTTP checks (default 5)
        #[arg(long, default_value = "5")]
        timeout_secs: u64,
    },
}

/// Notification management subcommands.
#[derive(Subcommand)]
pub(crate) enum NotifyCommand {
    /// Send a test notification via all configured channels
    Test,
}

/// Subcommands for `zeph url-scheme`.
#[cfg(feature = "deep-link")]
#[derive(Subcommand)]
pub(crate) enum UrlSchemeCommand {
    /// Register the `zeph://` URI scheme with the OS.
    Register,
    /// Remove the OS `zeph://` URI scheme registration.
    Unregister,
    /// Show the current registration status.
    ///
    /// With `--check`, exits non-zero when the scheme is not registered or the registered
    /// binary path does not match the current executable (stale registration).
    Status {
        /// Exit non-zero if the scheme is not registered or registration is stale.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum VaultCommand {
    /// Generate age keypair and empty encrypted vault
    Init,

    /// Encrypt and store a secret.
    /// Note: VALUE is visible in process listing (ps/history). For sensitive values
    /// prefer setting the variable in the shell and passing via env instead.
    Set {
        #[arg()]
        key: String,
        #[arg()]
        value: String,
        /// Overwrite an existing key. Without this flag, `vault set` refuses to replace
        /// a key that is already present in the vault.
        #[arg(long)]
        force: bool,
    },
    /// Decrypt and print a secret value
    Get {
        #[arg()]
        key: String,
    },
    /// List stored secret keys (no values)
    List,
    /// Remove a secret
    Rm {
        #[arg()]
        key: String,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[cfg(feature = "acp")]
    #[test]
    fn cli_parses_acp_model_config_show() {
        use super::{AcpCommand, AcpModelConfigCommand, Command};
        let cli = Cli::try_parse_from(["zeph", "acp", "model-config", "show"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Acp {
                command: AcpCommand::ModelConfig {
                    command: AcpModelConfigCommand::Show
                }
            })
        ));
    }

    #[cfg(feature = "scheduler")]
    #[test]
    fn cli_parses_schedule_list() {
        use super::{Command, ScheduleCommand};
        let cli = Cli::try_parse_from(["zeph", "schedule", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                command: ScheduleCommand::List
            })
        ));
    }

    #[cfg(feature = "scheduler")]
    #[test]
    fn cli_parses_schedule_add() {
        use super::{Command, ScheduleCommand};
        let cli =
            Cli::try_parse_from(["zeph", "schedule", "add", "0 * * * *", "run report"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                command: ScheduleCommand::Add { .. }
            })
        ));
    }

    #[cfg(feature = "scheduler")]
    #[test]
    fn cli_parses_schedule_remove() {
        use super::{Command, ScheduleCommand};
        let cli = Cli::try_parse_from(["zeph", "schedule", "remove", "my-job"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                command: ScheduleCommand::Remove { .. }
            })
        ));
    }

    #[cfg(feature = "scheduler")]
    #[test]
    fn cli_parses_schedule_show() {
        use super::{Command, ScheduleCommand};
        let cli = Cli::try_parse_from(["zeph", "schedule", "show", "my-job"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                command: ScheduleCommand::Show { .. }
            })
        ));
    }

    #[test]
    fn cli_parses_extended_context_flag() {
        let cli = Cli::try_parse_from(["zeph", "--extended-context"]).unwrap();
        assert!(cli.extended_context);
    }

    #[test]
    fn cli_extended_context_defaults_to_false() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(!cli.extended_context);
    }

    #[test]
    fn cli_parses_graph_memory_flag() {
        let cli = Cli::try_parse_from(["zeph", "--graph-memory"]).unwrap();
        assert!(cli.graph_memory);
    }

    #[test]
    fn cli_graph_memory_flag_defaults_to_false() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(!cli.graph_memory);
    }

    #[test]
    fn cli_parses_compression_guidelines_flag() {
        let cli = Cli::try_parse_from(["zeph", "--compression-guidelines"]).unwrap();
        assert!(cli.compression_guidelines);
    }

    #[test]
    fn cli_compression_guidelines_defaults_to_false() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(!cli.compression_guidelines);
    }

    #[test]
    fn cli_parses_scan_skills_on_load_flag() {
        let cli = Cli::try_parse_from(["zeph", "--scan-skills-on-load"]).unwrap();
        assert!(cli.scan_skills_on_load);
    }

    #[test]
    fn cli_scan_skills_on_load_defaults_to_false() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(!cli.scan_skills_on_load);
    }
    #[test]
    fn cli_parses_experiment_run_flag() {
        let cli = Cli::try_parse_from(["zeph", "--experiment-run"]).unwrap();
        assert!(cli.experiment_run);
    }
    #[test]
    fn cli_parses_experiment_report_flag() {
        let cli = Cli::try_parse_from(["zeph", "--experiment-report"]).unwrap();
        assert!(cli.experiment_report);
    }
    #[test]
    fn cli_experiment_flags_default_to_false() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(!cli.experiment_run);
        assert!(!cli.experiment_report);
    }

    #[test]
    fn cli_parses_log_file_flag() {
        let cli = Cli::try_parse_from(["zeph", "--log-file", "/tmp/test.log"]).unwrap();
        assert_eq!(cli.log_file.as_deref(), Some("/tmp/test.log"));
    }

    #[test]
    fn cli_log_file_defaults_to_none() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(cli.log_file.is_none());
    }

    #[test]
    fn cli_log_file_bare_flag_disables_logging() {
        let cli = Cli::try_parse_from(["zeph", "--log-file"]).unwrap();
        assert_eq!(cli.log_file.as_deref(), Some(""));
    }

    #[test]
    fn cli_dump_format_defaults_to_none() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(cli.dump_format.is_none());
    }

    #[test]
    fn cli_dump_format_parses_trace() {
        let cli = Cli::try_parse_from(["zeph", "--dump-format", "trace"]).unwrap();
        assert_eq!(
            cli.dump_format,
            Some(zeph_core::debug_dump::DumpFormat::Trace)
        );
    }

    #[test]
    fn cli_dump_format_parses_raw() {
        let cli = Cli::try_parse_from(["zeph", "--dump-format", "raw"]).unwrap();
        assert_eq!(
            cli.dump_format,
            Some(zeph_core::debug_dump::DumpFormat::Raw)
        );
    }

    #[test]
    fn cli_parses_focus_flag() {
        let cli = Cli::try_parse_from(["zeph", "--focus"]).unwrap();
        assert!(cli.focus);
    }

    #[test]
    fn cli_parses_no_focus_flag() {
        let cli = Cli::try_parse_from(["zeph", "--no-focus"]).unwrap();
        assert!(cli.no_focus);
    }

    #[test]
    fn cli_parses_sidequest_flag() {
        let cli = Cli::try_parse_from(["zeph", "--sidequest"]).unwrap();
        assert!(cli.sidequest);
    }

    #[test]
    fn cli_parses_no_sidequest_flag() {
        let cli = Cli::try_parse_from(["zeph", "--no-sidequest"]).unwrap();
        assert!(cli.no_sidequest);
    }

    #[test]
    fn cli_parses_pruning_strategy_task_aware() {
        let cli = Cli::try_parse_from(["zeph", "--pruning-strategy", "task_aware"]).unwrap();
        assert_eq!(
            cli.pruning_strategy,
            Some(zeph_core::config::PruningStrategy::TaskAware)
        );
    }

    #[test]
    fn cli_parses_pruning_strategy_mig() {
        let cli = Cli::try_parse_from(["zeph", "--pruning-strategy", "mig"]).unwrap();
        assert_eq!(
            cli.pruning_strategy,
            Some(zeph_core::config::PruningStrategy::Mig)
        );
    }

    #[test]
    fn cli_pruning_strategy_task_aware_mig_falls_back_to_reactive() {
        // task_aware_mig was removed; FromStr now returns Reactive with a warning.
        let parsed: zeph_core::config::PruningStrategy = "task_aware_mig".parse().unwrap();
        assert_eq!(parsed, zeph_core::config::PruningStrategy::Reactive);
    }

    #[test]
    fn cli_focus_and_no_focus_conflict() {
        assert!(Cli::try_parse_from(["zeph", "--focus", "--no-focus"]).is_err());
    }

    #[test]
    fn cli_sidequest_and_no_sidequest_conflict() {
        assert!(Cli::try_parse_from(["zeph", "--sidequest", "--no-sidequest"]).is_err());
    }

    #[test]
    fn cli_defaults_compression_flags_to_false() {
        let cli = Cli::try_parse_from(["zeph"]).unwrap();
        assert!(!cli.focus);
        assert!(!cli.no_focus);
        assert!(!cli.sidequest);
        assert!(!cli.no_sidequest);
        assert!(cli.pruning_strategy.is_none());
    }

    #[test]
    fn cli_parses_pruning_strategy_task_aware_kebab() {
        let cli = Cli::try_parse_from(["zeph", "--pruning-strategy", "task-aware"]).unwrap();
        assert_eq!(
            cli.pruning_strategy,
            Some(zeph_core::config::PruningStrategy::TaskAware)
        );
    }

    #[test]
    fn cli_parses_project_purge() {
        use super::{Command, ProjectCommand};
        let cli = Cli::try_parse_from(["zeph", "project", "purge"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Project {
                command: ProjectCommand::Purge {
                    dry_run: false,
                    yes: false,
                    ..
                }
            })
        ));
    }

    #[test]
    fn cli_parses_project_purge_dry_run() {
        use super::{Command, ProjectCommand};
        let cli = Cli::try_parse_from(["zeph", "project", "purge", "--dry-run"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Project {
                command: ProjectCommand::Purge {
                    dry_run: true,
                    yes: false,
                    ..
                }
            })
        ));
    }

    #[test]
    fn cli_parses_project_purge_yes() {
        use super::{Command, ProjectCommand};
        let cli = Cli::try_parse_from(["zeph", "project", "purge", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Project {
                command: ProjectCommand::Purge {
                    dry_run: false,
                    yes: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn cli_parses_project_purge_with_config() {
        use super::{Command, ProjectCommand};
        let cli = Cli::try_parse_from(["zeph", "project", "purge", "--config", "/tmp/test.toml"])
            .unwrap();
        if let Some(Command::Project {
            command: ProjectCommand::Purge { config, .. },
        }) = cli.command
        {
            assert_eq!(config, Some(std::path::PathBuf::from("/tmp/test.toml")));
        } else {
            panic!("unexpected command variant");
        }
    }

    #[cfg(feature = "deep-link")]
    #[test]
    fn cli_parses_url_open() {
        use super::Command;
        let cli =
            Cli::try_parse_from(["zeph", "url-open", "zeph://new-session?prompt=Hello"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::UrlOpen { ref uri }) if uri == "zeph://new-session?prompt=Hello"
        ));
    }

    #[cfg(feature = "deep-link")]
    #[test]
    fn cli_parses_url_scheme_register() {
        use super::{Command, UrlSchemeCommand};
        let cli = Cli::try_parse_from(["zeph", "url-scheme", "register"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::UrlScheme {
                command: UrlSchemeCommand::Register
            })
        ));
    }

    #[cfg(feature = "deep-link")]
    #[test]
    fn cli_parses_url_scheme_status() {
        use super::{Command, UrlSchemeCommand};
        let cli = Cli::try_parse_from(["zeph", "url-scheme", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::UrlScheme {
                command: UrlSchemeCommand::Status { check: false }
            })
        ));
    }

    #[cfg(feature = "deep-link")]
    #[test]
    fn cli_parses_url_scheme_status_check_flag() {
        use super::{Command, UrlSchemeCommand};
        let cli = Cli::try_parse_from(["zeph", "url-scheme", "status", "--check"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::UrlScheme {
                command: UrlSchemeCommand::Status { check: true }
            })
        ));
    }

    // ── skill/plugin registry marketplace CLI parsing (spec-045, #5869) ─────

    #[test]
    fn cli_parses_skill_search() {
        use super::{Command, SkillCommand};
        let cli = Cli::try_parse_from(["zeph", "skill", "search", "pdf tools"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Skill {
                command: SkillCommand::Search { query }
            }) if query == "pdf tools"
        ));
    }

    #[test]
    fn cli_parses_skill_get() {
        use super::{Command, SkillCommand};
        let cli = Cli::try_parse_from(["zeph", "skill", "get", "acme/pdf-tools"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Skill {
                command: SkillCommand::Get { registry_id }
            }) if registry_id == "acme/pdf-tools"
        ));
    }

    #[test]
    fn cli_skill_add_no_longer_exists_as_registry_verb() {
        // S1 rename: the registry-install verb is `get`, not `add` — `add` is not a valid
        // `skill` subcommand at all (the local-path installer is `install`).
        let err = Cli::try_parse_from(["zeph", "skill", "add", "acme/pdf-tools"])
            .err()
            .unwrap();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    // ── skill trust auto-activation CLI parsing (#6087) ──────────────────────

    #[test]
    fn cli_parses_skill_trust_with_no_flags() {
        use super::{Command, SkillCommand};
        let cli = Cli::try_parse_from(["zeph", "skill", "trust", "git", "trusted"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Skill {
                command: SkillCommand::Trust {
                    require_check: false,
                    no_require_check: false,
                    ..
                }
            })
        ));
    }

    #[test]
    fn cli_parses_skill_trust_require_check() {
        use super::{Command, SkillCommand};
        let cli = Cli::try_parse_from([
            "zeph",
            "skill",
            "trust",
            "git",
            "trusted",
            "--require-check",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Skill {
                command: SkillCommand::Trust {
                    require_check: true,
                    no_require_check: false,
                    ..
                }
            })
        ));
    }

    #[test]
    fn cli_parses_skill_trust_no_require_check() {
        use super::{Command, SkillCommand};
        let cli = Cli::try_parse_from([
            "zeph",
            "skill",
            "trust",
            "git",
            "trusted",
            "--no-require-check",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Skill {
                command: SkillCommand::Trust {
                    require_check: false,
                    no_require_check: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn cli_skill_trust_require_check_and_no_require_check_conflict() {
        let err = Cli::try_parse_from([
            "zeph",
            "skill",
            "trust",
            "git",
            "trusted",
            "--require-check",
            "--no-require-check",
        ])
        .err()
        .unwrap();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn cli_parses_plugin_search() {
        use super::{Command, PluginCommand};
        let cli = Cli::try_parse_from(["zeph", "plugin", "search", "pdf tools"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Search { query }
            }) if query == "pdf tools"
        ));
    }

    #[test]
    fn cli_parses_plugin_get() {
        use super::{Command, PluginCommand};
        let cli = Cli::try_parse_from(["zeph", "plugin", "get", "acme/full-plugin"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Get { registry_id }
            }) if registry_id == "acme/full-plugin"
        ));
    }

    #[test]
    fn cli_parses_plugin_add_as_local_path_install_unaffected_by_rename() {
        use super::{Command, PluginCommand};
        let cli = Cli::try_parse_from(["zeph", "plugin", "add", "/tmp/my-plugin"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::Add { source }
            }) if source == "/tmp/my-plugin"
        ));
    }
}
