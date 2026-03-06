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

    /// Run in headless daemon mode (requires daemon + a2a features)
    #[cfg(all(feature = "daemon", feature = "a2a"))]
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

    /// Enable graph-based knowledge memory (experimental)
    #[arg(long)]
    pub(crate) graph_memory: bool,

    /// Override scheduler tick interval in seconds (requires scheduler feature)
    #[cfg(feature = "scheduler")]
    #[arg(long, value_name = "SECS")]
    pub(crate) scheduler_tick: Option<u64>,

    /// Disable the scheduler even if enabled in config (requires scheduler feature)
    #[cfg(feature = "scheduler")]
    #[arg(long)]
    pub(crate) scheduler_disable: bool,

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
}
