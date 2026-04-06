// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

    /// Connect TUI to a remote daemon via A2A SSE (requires tui + a2a features)
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

    /// Enable Claude thinking mode: `extended:<budget_tokens>` or `adaptive` or `adaptive:<effort>`
    /// where effort is `low`, `medium`, or `high`. Overrides config.toml thinking setting.
    /// Examples: `--thinking extended:10000`  `--thinking adaptive`  `--thinking adaptive:high`
    #[arg(long, value_name = "MODE")]
    pub(crate) thinking: Option<String>,

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
    /// logging, overriding any config value. When omitted, uses the value from [logging]
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

    /// Override debug dump format: `json`, `raw`, or `trace` (`OTel` OTLP spans).
    #[arg(long = "dump-format", value_name = "FORMAT")]
    pub(crate) dump_format: Option<zeph_core::debug_dump::DumpFormat>,

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

    /// Disable pre-execution verifiers for tool calls.
    /// Use in trusted environments or when verifiers produce false positives.
    #[arg(long)]
    pub(crate) no_pre_execution_verify: bool,

    /// Enable Think-Augmented Function Calling (TAFC) for this session.
    /// Injects a reasoning step into complex tool schemas.
    /// Overrides `tools.tafc.enabled` from config.
    #[arg(long)]
    pub(crate) tafc: bool,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
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
    /// Manage ACP session history
    #[cfg(feature = "acp")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
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
}

/// Database subcommands.
#[derive(Subcommand)]
pub(crate) enum DbCommand {
    /// Run pending database migrations
    Migrate,
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
        /// Agent name (must match [a-zA-Z0-9][a-zA-Z0-9_-]{0,63})
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
    /// Show compression predictor status (sample count, weights summary)
    PredictorStatus,
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

#[cfg(feature = "acp")]
#[derive(Subcommand)]
pub(crate) enum SessionsCommand {
    /// List recent ACP sessions
    List,
    /// Resume a past session by ID (print events to stdout)
    Resume {
        /// Session ID
        id: String,
    },
    /// Delete an ACP session and its events
    Delete {
        /// Session ID
        id: String,
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
}
