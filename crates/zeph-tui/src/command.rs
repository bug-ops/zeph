// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Commands dispatched from the TUI command palette to the agent loop.
///
/// Each variant corresponds to a slash-command or keybinding action that the
/// TUI can trigger. The agent loop receives these via an `mpsc` channel and
/// produces a [`crate::event::AgentEvent::CommandResult`] response.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::TuiCommand;
///
/// let cmd = TuiCommand::SkillList;
/// assert_eq!(cmd, TuiCommand::SkillList);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiCommand {
    // Existing view commands
    SkillList,
    McpList,
    MemoryStats,
    ViewCost,
    ViewTools,
    ViewConfig,
    ViewAutonomy,
    ViewLatency,
    // New action commands
    Quit,
    Help,
    NewSession,
    ToggleTheme,
    // Session history browser (H keybind)
    SessionBrowser,
    // Daemon / remote connection commands
    DaemonConnect,
    DaemonDisconnect,
    DaemonStatus,
    // Filter inspection
    ViewFilters,
    // Document ingestion
    Ingest,
    // Gateway
    GatewayStatus,
    // Scheduler
    SchedulerList,
    // Sub-agents (runtime)
    AgentList,
    AgentStatus,
    AgentCancelPrompt,
    AgentSpawnPrompt,
    // Router
    RouterStats,
    // Sub-agent definitions (CRUD)
    AgentsShow,
    AgentsCreate,
    AgentsEdit,
    AgentsDelete,
    // Security
    SecurityEvents,
    // Plan / orchestration
    PlanStatus,
    PlanConfirm,
    PlanCancel,
    PlanList,
    PlanToggleView,
    // Graph memory
    GraphStats,
    GraphEntities,
    GraphFactsPrompt,
    GraphCommunities,
    GraphBackfillPrompt,
    // Experiments
    ExperimentStart,
    ExperimentStop,
    ExperimentStatus,
    ExperimentReport,
    ExperimentBest,
    // LSP context injection
    LspStatus,
    // Log file
    ViewLog,
    // Config migration
    MigrateConfig,
    // Server-side compaction
    ServerCompactionStatus,
    // Compression guidelines
    ViewGuidelines,
    // Think-Augmented Function Calling
    TafcStatus,
    // SleepGate forgetting sweep
    ForgettingSweep,
    // Trajectory-informed memory (#2498)
    TrajectoryStats,
    // TiMem memory tree (#2262)
    MemoryTreeStats,
    // Task registry panel (#2962)
    TaskPanel,
    // Plugin management (#2806)
    PluginList,
    PluginAdd,
    PluginRemove,
    // Multi-session management (#3130, phase-1)
    SessionSwitchNext,
    SessionSwitchPrev,
    SessionClose,
    // Plugin overlay status (#3147)
    PluginListOverlay,
    // ACP read-only inspection (#3270)
    AcpDirsList,
    AcpAuthMethodsView,
    AcpStatus,
    // ACP sub-agent delegation (#3272)
    SubagentSpawn {
        command: String,
    },
    // Sandbox egress status (#3294)
    SandboxStatus,
    // Cocoon sidecar inspection (#3673)
    CocoonStatus,
    CocoonModels,
    // Clipboard (#3685)
    CopyLastAssistant,
    /// Copy the Nth visible code block from the last assistant message (1-indexed).
    /// When `n` is 0, copies the last (most recent) block.
    CopyLastCodeBlock(usize),
    // Fleet session overview (#3884)
    FleetPanel,
    // Durable execution journal (spec-064, #4949)
    DurablePanel,
    // Worktree subsystem (#4679)
    WorktreeList,
    WorktreeClean,
    // Undo/redo checkpoint commands (#4990)
    Undo,
    Redo,
    // Knowledge ingest management (#5019, #5020)
    KnowledgeStatus,
    KnowledgeRollbackPrompt,
    KnowledgeIngestPrompt,
    // Theme runtime switching (#5090)
    /// List all available theme presets.
    ListThemes,
    /// Switch to the named theme preset or user file.
    SetTheme(String),
    // Motion control (#5096)
    /// Set the TUI animation budget at runtime (`full`, `minimal`, or `off`).
    SetMotion(zeph_config::Motion),
    // Mouse mode (#5103)
    /// Enable or disable opt-in mouse capture (`/mouse on|off`).
    SetMouse(bool),
    /// Toggle the current mouse capture state.
    ToggleMouse,
    /// Toggle the compact equalizer widget in the busy separator row.
    ToggleEqualizer,
    // SubAgent sidebar navigation (used by decode_normal_key → Action::Dispatch)
    /// Move the subagent list selection down by one.
    SubagentSidebarDown,
    /// Move the subagent list selection up by one.
    SubagentSidebarUp,
    /// Send `/clear-queue` to the agent input channel (Ctrl+K in Insert mode).
    SendClearQueue,
    /// Send an arbitrary slash command's bare text verbatim to the agent input channel.
    ///
    /// Used for [`zeph_commands::COMMANDS`] entries that have no dedicated hand-authored
    /// `TuiCommand` variant (#5875): every such command gets autocomplete for free instead
    /// of requiring a matching enum variant and reducer arm to be added by hand. Only used
    /// for commands whose bare (no-argument) form is a valid, useful default — see
    /// [`crate::command::zeph_commands_entries`].
    SendVerbatim(String),
    /// Fill the input box with an arbitrary slash command's text without submitting it.
    ///
    /// Counterpart to [`TuiCommand::SendVerbatim`] for [`zeph_commands::COMMANDS`] entries
    /// whose argument is required (e.g. `/image <path>`) — submitting the bare command would
    /// just produce a usage error, so this instead prefills the input for the user to
    /// complete, mirroring the existing `*Prompt` variants' `prefill_input` behavior (#5875
    /// F1).
    PrefillVerbatim(String),
}

/// Metadata for a single entry in the command palette.
///
/// Used for both display (label, category, shortcut hint) and fuzzy-matching
/// (id + label are scored by [`filter_commands`]).
///
/// # Examples
///
/// ```rust
/// use zeph_tui::command::{command_registry, CommandEntry};
///
/// let registry = command_registry();
/// let quit = registry.iter().find(|e| e.id == "app:quit").unwrap();
/// assert_eq!(quit.shortcut, Some("q"));
/// ```
pub struct CommandEntry {
    /// Stable identifier used in fuzzy search and slash-command routing (e.g. `"skill:list"`).
    pub id: &'static str,
    /// Human-readable label shown in the command palette list.
    pub label: &'static str,
    /// Logical group for categorised display (e.g. `"memory"`, `"agent"`).
    pub category: &'static str,
    /// Optional keyboard shortcut hint (e.g. `"q"`, `"?"`).
    pub shortcut: Option<&'static str>,
    /// The [`TuiCommand`] dispatched when this entry is selected.
    pub command: TuiCommand,
}

/// Returns the static registry of core TUI commands.
///
/// This includes navigation, session management, view toggles, and app-level
/// actions. Extended commands (agent, plan, graph, experiment, infra) are in
/// [`extra_command_registry`] and daemon commands in [`daemon_command_registry`].
///
/// Lazily initialised on first call and then shared for the process lifetime.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::command::command_registry;
///
/// let registry = command_registry();
/// assert!(!registry.is_empty());
/// assert!(registry.iter().any(|e| e.id == "app:quit"));
/// ```
#[must_use]
pub fn command_registry() -> &'static [CommandEntry] {
    static COMMANDS: std::sync::OnceLock<Vec<CommandEntry>> = std::sync::OnceLock::new();
    COMMANDS.get_or_init(build_core_commands)
}

fn build_view_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "skill:list",
            label: "List loaded skills",
            category: "skill",
            shortcut: None,
            command: TuiCommand::SkillList,
        },
        CommandEntry {
            id: "mcp:list",
            label: "List MCP servers and tools",
            category: "mcp",
            shortcut: None,
            command: TuiCommand::McpList,
        },
        CommandEntry {
            id: "memory:stats",
            label: "Show memory statistics",
            category: "memory",
            shortcut: None,
            command: TuiCommand::MemoryStats,
        },
        CommandEntry {
            id: "view:cost",
            label: "Show cost breakdown",
            category: "view",
            shortcut: None,
            command: TuiCommand::ViewCost,
        },
        CommandEntry {
            id: "view:tools",
            label: "List available tools",
            category: "view",
            shortcut: None,
            command: TuiCommand::ViewTools,
        },
        CommandEntry {
            id: "view:config",
            label: "Show active configuration",
            category: "view",
            shortcut: None,
            command: TuiCommand::ViewConfig,
        },
        CommandEntry {
            id: "view:autonomy",
            label: "Show autonomy/trust level",
            category: "view",
            shortcut: None,
            command: TuiCommand::ViewAutonomy,
        },
        CommandEntry {
            id: "view:latency",
            label: "Show classifier and turn-latency breakdown",
            category: "view",
            shortcut: None,
            command: TuiCommand::ViewLatency,
        },
        CommandEntry {
            id: "tasks",
            label: "Toggle task registry panel",
            category: "view",
            shortcut: None,
            command: TuiCommand::TaskPanel,
        },
        CommandEntry {
            id: "fleet",
            label: "Fleet: show agent sessions",
            category: "view",
            shortcut: Some("f"),
            command: TuiCommand::FleetPanel,
        },
        CommandEntry {
            id: "durable",
            label: "Durable: show durable executions",
            category: "view",
            shortcut: Some("D"),
            command: TuiCommand::DurablePanel,
        },
    ]
}

fn build_session_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "session:new",
            label: "Start new conversation",
            category: "session",
            shortcut: None,
            command: TuiCommand::NewSession,
        },
        CommandEntry {
            id: "session:history",
            label: "Browse session history",
            category: "session",
            shortcut: Some("H"),
            command: TuiCommand::SessionBrowser,
        },
        CommandEntry {
            id: "session:next",
            label: "Switch to next session (/session next)",
            category: "session",
            shortcut: None,
            command: TuiCommand::SessionSwitchNext,
        },
        CommandEntry {
            id: "session:prev",
            label: "Switch to previous session (/session prev)",
            category: "session",
            shortcut: None,
            command: TuiCommand::SessionSwitchPrev,
        },
        CommandEntry {
            id: "session:close",
            label: "Close current session (/session close)",
            category: "session",
            shortcut: None,
            command: TuiCommand::SessionClose,
        },
        CommandEntry {
            id: "session:undo",
            label: "Undo last shell checkpoint (/undo)",
            category: "session",
            shortcut: None,
            command: TuiCommand::Undo,
        },
        CommandEntry {
            id: "session:redo",
            label: "Re-apply last undone checkpoint (/redo)",
            category: "session",
            shortcut: None,
            command: TuiCommand::Redo,
        },
    ]
}

fn build_app_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "app:quit",
            label: "Quit application",
            category: "app",
            shortcut: Some("q"),
            command: TuiCommand::Quit,
        },
        CommandEntry {
            id: "app:help",
            label: "Show keybindings help",
            category: "app",
            shortcut: Some("?"),
            command: TuiCommand::Help,
        },
        CommandEntry {
            id: "app:theme",
            label: "Cycle theme (zephyr → zephyr-light → high-contrast)",
            category: "app",
            shortcut: None,
            command: TuiCommand::ToggleTheme,
        },
        CommandEntry {
            id: "app:theme-list",
            label: "List available themes (/theme)",
            category: "app",
            shortcut: None,
            command: TuiCommand::ListThemes,
        },
        CommandEntry {
            id: "app:mouse",
            label: "Toggle mouse mode (wheel scroll, click focus)",
            category: "app",
            shortcut: None,
            command: TuiCommand::ToggleMouse,
        },
        CommandEntry {
            id: "app:equalizer",
            label: "Toggle equalizer (compact VU-meter in busy separator)",
            category: "app",
            shortcut: None,
            command: TuiCommand::ToggleEqualizer,
        },
    ]
}

fn build_plugin_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "plugin:list",
            label: "List installed plugins (/plugins list)",
            category: "plugin",
            shortcut: None,
            command: TuiCommand::PluginList,
        },
        CommandEntry {
            id: "plugin:add",
            label: "Install a plugin (/plugins add <source>)",
            category: "plugin",
            shortcut: None,
            command: TuiCommand::PluginAdd,
        },
        CommandEntry {
            id: "plugin:remove",
            label: "Remove an installed plugin (/plugins remove <name>)",
            category: "plugin",
            shortcut: None,
            command: TuiCommand::PluginRemove,
        },
        CommandEntry {
            id: "plugin:overlay",
            label: "Plugin overlay status — source and skipped plugins (/plugins overlay)",
            category: "plugin",
            shortcut: None,
            command: TuiCommand::PluginListOverlay,
        },
    ]
}

fn build_core_commands() -> Vec<CommandEntry> {
    let mut cmds = build_view_commands();
    cmds.extend(build_session_commands());
    cmds.extend(build_app_commands());
    cmds.extend(build_plugin_commands());
    cmds
}

/// Returns the static registry of daemon / remote-connection commands.
///
/// These commands manage connectivity to a background Zeph daemon process.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::command::daemon_command_registry;
///
/// let registry = daemon_command_registry();
/// assert!(registry.iter().any(|e| e.id == "daemon:connect"));
/// ```
#[must_use]
pub fn daemon_command_registry() -> &'static [CommandEntry] {
    static DAEMON_COMMANDS: &[CommandEntry] = &[
        CommandEntry {
            id: "daemon:connect",
            label: "Connect to remote daemon",
            category: "daemon",
            shortcut: None,
            command: TuiCommand::DaemonConnect,
        },
        CommandEntry {
            id: "daemon:disconnect",
            label: "Disconnect from daemon",
            category: "daemon",
            shortcut: None,
            command: TuiCommand::DaemonDisconnect,
        },
        CommandEntry {
            id: "daemon:status",
            label: "Show connection status",
            category: "daemon",
            shortcut: None,
            command: TuiCommand::DaemonStatus,
        },
    ];
    DAEMON_COMMANDS
}

/// Returns the extended command registry (infrastructure, agent, plan, graph, experiment).
///
/// Lazily initialised on first call and then shared for the process lifetime.
/// Prefer [`filter_commands`] when you need a merged, fuzzy-filtered view.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::command::extra_command_registry;
///
/// let registry = extra_command_registry();
/// assert!(registry.iter().any(|e| e.id == "graph:stats"));
/// assert!(registry.iter().any(|e| e.id == "experiment:start"));
/// ```
#[must_use]
pub fn extra_command_registry() -> &'static [CommandEntry] {
    static EXTRA: std::sync::OnceLock<Vec<CommandEntry>> = std::sync::OnceLock::new();
    EXTRA.get_or_init(build_extra_commands)
}

#[allow(clippy::too_many_lines)]
fn build_infra_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "view:filters",
            label: "Show output filter statistics",
            category: "view",
            shortcut: None,
            command: TuiCommand::ViewFilters,
        },
        CommandEntry {
            id: "ingest",
            label: "Ingest document into memory (/ingest <path>)",
            category: "memory",
            shortcut: None,
            command: TuiCommand::Ingest,
        },
        CommandEntry {
            id: "gateway:status",
            label: "Show gateway server status",
            category: "gateway",
            shortcut: None,
            command: TuiCommand::GatewayStatus,
        },
        CommandEntry {
            id: "scheduler:list",
            label: "List scheduled tasks",
            category: "scheduler",
            shortcut: None,
            command: TuiCommand::SchedulerList,
        },
        CommandEntry {
            id: "router:stats",
            label: "Show Thompson router alpha/beta per provider",
            category: "router",
            shortcut: None,
            command: TuiCommand::RouterStats,
        },
        CommandEntry {
            id: "security:events",
            label: "Show security event history",
            category: "security",
            shortcut: None,
            command: TuiCommand::SecurityEvents,
        },
        CommandEntry {
            id: "sandbox:status",
            label: "Show sandbox status: backend, denied_domains, fail_if_unavailable",
            category: "security",
            shortcut: None,
            command: TuiCommand::SandboxStatus,
        },
        CommandEntry {
            id: "log:status",
            label: "Show log file path and recent entries (/log)",
            category: "log",
            shortcut: None,
            command: TuiCommand::ViewLog,
        },
        CommandEntry {
            id: "config:migrate",
            label: "Show config migration diff (missing parameters)",
            category: "config",
            shortcut: None,
            command: TuiCommand::MigrateConfig,
        },
        CommandEntry {
            id: "compaction:status",
            label: "Show server-side compaction status",
            category: "context",
            shortcut: None,
            command: TuiCommand::ServerCompactionStatus,
        },
        CommandEntry {
            id: "tafc:status",
            label: "Show Think-Augmented Function Calling (TAFC) status (/tafc)",
            category: "tools",
            shortcut: None,
            command: TuiCommand::TafcStatus,
        },
        CommandEntry {
            id: "memory:forgetting-sweep",
            label: "Run forgetting sweep once (/forgetting-sweep)",
            category: "memory",
            shortcut: None,
            command: TuiCommand::ForgettingSweep,
        },
        CommandEntry {
            id: "memory:trajectory",
            label: "Show trajectory memory statistics (/memory trajectory)",
            category: "memory",
            shortcut: None,
            command: TuiCommand::TrajectoryStats,
        },
        CommandEntry {
            id: "memory:tree",
            label: "Show memory tree statistics (/memory tree)",
            category: "memory",
            shortcut: None,
            command: TuiCommand::MemoryTreeStats,
        },
        CommandEntry {
            id: "worktree:list",
            label: "List active and stale git worktrees (/worktree list)",
            category: "worktree",
            shortcut: None,
            command: TuiCommand::WorktreeList,
        },
        CommandEntry {
            id: "worktree:clean",
            label: "Remove all stale git worktrees (/worktree clean)",
            category: "worktree",
            shortcut: None,
            command: TuiCommand::WorktreeClean,
        },
    ]
}

fn build_agent_plan_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "agent:list",
            label: "List sub-agents (/agent list)",
            category: "agent",
            shortcut: None,
            command: TuiCommand::AgentList,
        },
        CommandEntry {
            id: "agent:status",
            label: "Show sub-agent status (/agent status)",
            category: "agent",
            shortcut: None,
            command: TuiCommand::AgentStatus,
        },
        CommandEntry {
            id: "agent:cancel",
            label: "Cancel a sub-agent (/agent cancel <id>)",
            category: "agent",
            shortcut: None,
            command: TuiCommand::AgentCancelPrompt,
        },
        CommandEntry {
            id: "agent:spawn",
            label: "Spawn a sub-agent (/agent spawn <name>)",
            category: "agent",
            shortcut: None,
            command: TuiCommand::AgentSpawnPrompt,
        },
        CommandEntry {
            id: "agents:show",
            label: "Show sub-agent definition details (/agents show <name>)",
            category: "agents",
            shortcut: None,
            command: TuiCommand::AgentsShow,
        },
        CommandEntry {
            id: "agents:create",
            label: "Create a new sub-agent definition (/agents create <name>)",
            category: "agents",
            shortcut: None,
            command: TuiCommand::AgentsCreate,
        },
        CommandEntry {
            id: "agents:edit",
            label: "Edit a sub-agent definition (/agents edit <name>)",
            category: "agents",
            shortcut: None,
            command: TuiCommand::AgentsEdit,
        },
        CommandEntry {
            id: "agents:delete",
            label: "Delete a sub-agent definition (/agents delete <name>)",
            category: "agents",
            shortcut: None,
            command: TuiCommand::AgentsDelete,
        },
        CommandEntry {
            id: "plan:status",
            label: "Show orchestration plan status (/plan status)",
            category: "plan",
            shortcut: None,
            command: TuiCommand::PlanStatus,
        },
        CommandEntry {
            id: "plan:confirm",
            label: "Confirm and execute pending plan (/plan confirm)",
            category: "plan",
            shortcut: None,
            command: TuiCommand::PlanConfirm,
        },
        CommandEntry {
            id: "plan:cancel",
            label: "Cancel current plan (/plan cancel)",
            category: "plan",
            shortcut: None,
            command: TuiCommand::PlanCancel,
        },
        CommandEntry {
            id: "plan:list",
            label: "List recent plans (/plan list)",
            category: "plan",
            shortcut: None,
            command: TuiCommand::PlanList,
        },
        CommandEntry {
            id: "plan:toggle",
            label: "Toggle plan view / subagents panel (p)",
            category: "plan",
            shortcut: Some("p"),
            command: TuiCommand::PlanToggleView,
        },
    ]
}

fn build_graph_experiment_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "graph:stats",
            label: "Show graph memory statistics (/graph)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphStats,
        },
        CommandEntry {
            id: "graph:entities",
            label: "List graph entities (/graph entities)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphEntities,
        },
        CommandEntry {
            id: "graph:facts",
            label: "Show entity facts (/graph facts <name>)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphFactsPrompt,
        },
        CommandEntry {
            id: "graph:communities",
            label: "List graph communities (/graph communities)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphCommunities,
        },
        CommandEntry {
            id: "graph:backfill",
            label: "Backfill graph from existing messages (/graph backfill)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphBackfillPrompt,
        },
        CommandEntry {
            id: "experiment:start",
            label: "Start experiment session (/experiment start [N])",
            category: "experiment",
            shortcut: None,
            command: TuiCommand::ExperimentStart,
        },
        CommandEntry {
            id: "experiment:stop",
            label: "Stop running experiment (/experiment stop)",
            category: "experiment",
            shortcut: None,
            command: TuiCommand::ExperimentStop,
        },
        CommandEntry {
            id: "experiment:status",
            label: "Show experiment status (/experiment status)",
            category: "experiment",
            shortcut: None,
            command: TuiCommand::ExperimentStatus,
        },
        CommandEntry {
            id: "experiment:report",
            label: "Show experiment results (/experiment report)",
            category: "experiment",
            shortcut: None,
            command: TuiCommand::ExperimentReport,
        },
        CommandEntry {
            id: "experiment:best",
            label: "Show best experiment result (/experiment best)",
            category: "experiment",
            shortcut: None,
            command: TuiCommand::ExperimentBest,
        },
        CommandEntry {
            id: "guidelines:view",
            label: "Show compression guidelines (/guidelines)",
            category: "memory",
            shortcut: None,
            command: TuiCommand::ViewGuidelines,
        },
    ]
}

#[cfg(feature = "cocoon")]
fn build_cocoon_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "cocoon:status",
            label: "Show Cocoon sidecar status (/cocoon status)",
            category: "cocoon",
            shortcut: None,
            command: TuiCommand::CocoonStatus,
        },
        CommandEntry {
            id: "cocoon:models",
            label: "List Cocoon models (/cocoon models)",
            category: "cocoon",
            shortcut: None,
            command: TuiCommand::CocoonModels,
        },
    ]
}

fn build_clipboard_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "clipboard:copy",
            label: "Copy last assistant reply to clipboard (/copy)",
            category: "clipboard",
            shortcut: Some("Ctrl+O"),
            command: TuiCommand::CopyLastAssistant,
        },
        CommandEntry {
            id: "clipboard:copyblock",
            label: "Copy last code block from assistant reply to clipboard (/copyblock)",
            category: "clipboard",
            shortcut: Some("Ctrl+Y"),
            command: TuiCommand::CopyLastCodeBlock(0),
        },
    ]
}

fn build_knowledge_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            id: "knowledge:status",
            label: "Knowledge: show ingest ledger status (/knowledge status)",
            category: "knowledge",
            shortcut: None,
            command: TuiCommand::KnowledgeStatus,
        },
        CommandEntry {
            id: "knowledge:rollback",
            label: "Knowledge: roll back an import batch (/knowledge rollback <batch>)",
            category: "knowledge",
            shortcut: None,
            command: TuiCommand::KnowledgeRollbackPrompt,
        },
        CommandEntry {
            id: "knowledge:ingest",
            label: "Knowledge: ingest project artifacts (CLI command)",
            category: "knowledge",
            shortcut: None,
            command: TuiCommand::KnowledgeIngestPrompt,
        },
    ]
}

/// Top-level command names already covered — exactly, with no arguments — by an existing
/// hand-authored [`TuiCommand`] entry in [`command_registry`], [`daemon_command_registry`],
/// or [`extra_command_registry`].
///
/// Excluded from [`zeph_commands_entries`] so the merged autocomplete list never shows the
/// same bare invocation twice (#5875). Each comment names the existing entry that already
/// performs the identical bare command (either by sending the same text, or — for the
/// locally-rendered view commands — by displaying the same information without a round
/// trip through the agent).
///
/// Every name here must have a real matching `id` in [`command_registry`],
/// [`daemon_command_registry`], or [`extra_command_registry`] — see the
/// `zeph_commands_dedup_entries_have_a_real_hand_authored_replacement` test, which fails
/// loudly if a covering entry is ever renamed or removed without updating this list (#5875
/// F3). `/clear-queue` was deliberately **not** added here even though a `SendClearQueue`
/// `TuiCommand` variant exists — that variant is reachable only via the Ctrl+K keybinding
/// (`crates/zeph-tui/src/app/keys.rs`), not through any `CommandEntry` in the three
/// registries above, so there is nothing to actually deduplicate against.
const ZEPH_COMMANDS_DEDUP: &[&str] = &[
    "/skills",     // skill:list
    "/mcp",        // mcp:list
    "/memory",     // memory:stats
    "/guidelines", // guidelines:view
    "/log",        // log:status
    "/undo",       // session:undo
    "/redo",       // session:redo
    "/graph",      // graph:stats
    "/lsp",        // lsp:status
    "/scheduler",  // scheduler:list
    "/subagent",   // acp:subagent-spawn (already prefills "/subagent spawn " when empty)
];

/// Returns `false` only for entries whose `feature_gate` names a Cargo feature that is
/// unified, via the root binary crate, with this crate's own feature of the same name — and
/// that feature is disabled in this build.
///
/// Every `feature_gate` value in [`zeph_commands::COMMANDS`] is otherwise purely descriptive
/// (rendered as `[requires: X]` in `/help` text): the underlying `CommandHandler` is
/// unconditionally registered in `Agent::run` regardless of any Cargo feature with a
/// matching name (most such names — `"acp"`, `"guardrail"`, `"scheduler"`, `"session"`,
/// etc. — do not even exist as Cargo features on the relevant crates). `"cocoon"` is the one
/// exception: `CocoonCommand`'s registration in `crates/zeph-core/src/agent/slash_commands.rs`
/// really is `#[cfg(feature = "cocoon")]`-gated, and the root `Cargo.toml`'s `cocoon` feature
/// unifies `zeph-core/cocoon` with this crate's own `cocoon` feature (which already gates
/// `build_cocoon_commands`), so checking it here faithfully predicts whether `CocoonCommand`
/// exists in this exact build (#5875 F2) — without this check, a `cocoon`-feature-off build
/// would still show `/cocoon` in autocomplete and fail when submitted.
fn command_is_compiled_in_this_build(entry: &zeph_commands::CommandInfo) -> bool {
    entry.feature_gate != Some("cocoon") || cfg!(feature = "cocoon")
}

/// Returns the [`CommandEntry`] projection of every [`zeph_commands::COMMANDS`] entry that
/// has no dedicated hand-authored `TuiCommand` (see `ZEPH_COMMANDS_DEDUP`).
///
/// `zeph_commands::COMMANDS` is the canonical, always-up-to-date list of channel-agnostic
/// `AgentAccess` slash commands (`/model`, `/provider`, `/skill`, `/policy`, etc.) — the
/// same list `/help` renders from. Projecting it here means a new command registered there
/// automatically gets TUI autocomplete, instead of requiring a second, hand-authored
/// `TuiCommand` variant and registry entry that can silently drift out of sync (#5875).
///
/// Commands whose `args` hint signals a *required* argument (e.g. `/image <path>`,
/// `/feedback <skill> <message>`) dispatch [`TuiCommand::PrefillVerbatim`] instead — this
/// fills the input box with the bare command plus a trailing space for the user to complete,
/// the same behavior every existing hand-authored `*Prompt` `TuiCommand` variant uses (see
/// `execute_command` in `app/keys.rs`), rather than submitting an incomplete command that the
/// handler would just reject. Every other entry dispatches [`TuiCommand::SendVerbatim`] with
/// the command's bare name (no arguments) — verified against each handler's actual empty-args
/// behavior (not just the `args` hint text, which is documentation-only and not always
/// bracket-consistent — e.g. `/goal` and `/worktree` both default sensibly on empty args
/// despite their hint text not being `[`-wrapped).
///
/// Entries whose `feature_gate` corresponds to a real, compile-time-relevant Cargo feature
/// are excluded when that feature is off in this build (see `command_is_compiled_in_this_build`)
/// — otherwise a feature-gated command that was never actually registered would still appear
/// in autocomplete and fail when submitted (#5875 F2).
///
/// Lazily initialised and shared for the process lifetime, like [`command_registry`] and
/// [`extra_command_registry`].
///
/// # Examples
///
/// ```rust
/// use zeph_tui::command::zeph_commands_entries;
///
/// let entries = zeph_commands_entries();
/// assert!(entries.iter().any(|e| e.id == "/model"));
/// // Commands already covered by a hand-authored entry are not duplicated.
/// assert!(!entries.iter().any(|e| e.id == "/graph"));
/// ```
#[must_use]
pub fn zeph_commands_entries() -> &'static [CommandEntry] {
    static ENTRIES: std::sync::OnceLock<Vec<CommandEntry>> = std::sync::OnceLock::new();
    ENTRIES.get_or_init(|| {
        zeph_commands::COMMANDS
            .iter()
            .filter(|c| !ZEPH_COMMANDS_DEDUP.contains(&c.name))
            .filter(|c| command_is_compiled_in_this_build(c))
            .map(|c| CommandEntry {
                id: c.name,
                label: c.description,
                category: c.category.as_str(),
                shortcut: None,
                command: if c.args.starts_with('<') {
                    TuiCommand::PrefillVerbatim(format!("{} ", c.name))
                } else {
                    TuiCommand::SendVerbatim(c.name.to_owned())
                },
            })
            .collect()
    })
}

fn build_extra_commands() -> Vec<CommandEntry> {
    let mut cmds = build_infra_commands();
    cmds.extend(build_agent_plan_commands());
    cmds.extend(build_graph_experiment_commands());
    cmds.push(CommandEntry {
        id: "lsp:status",
        label: "Show LSP context injection status (/lsp)",
        category: "lsp",
        shortcut: None,
        command: TuiCommand::LspStatus,
    });
    cmds.push(CommandEntry {
        id: "acp:dirs",
        label: "ACP: list allowlisted directories (/acp dirs)",
        category: "acp",
        shortcut: None,
        command: TuiCommand::AcpDirsList,
    });
    cmds.push(CommandEntry {
        id: "acp:auth-methods",
        label: "ACP: list advertised auth methods (/acp auth-methods)",
        category: "acp",
        shortcut: None,
        command: TuiCommand::AcpAuthMethodsView,
    });
    cmds.push(CommandEntry {
        id: "acp:status",
        label: "ACP: show runtime status and feature flags (/acp status)",
        category: "acp",
        shortcut: None,
        command: TuiCommand::AcpStatus,
    });
    cmds.push(CommandEntry {
        id: "acp:subagent-spawn",
        label: "ACP: spawn a sub-agent (/subagent spawn <cmd>)",
        category: "acp",
        shortcut: None,
        command: TuiCommand::SubagentSpawn {
            command: String::new(),
        },
    });
    #[cfg(feature = "cocoon")]
    cmds.extend(build_cocoon_commands());
    cmds.extend(build_clipboard_commands());
    cmds.extend(build_knowledge_commands());
    cmds
}

/// Returns `true` if `a` and `b` should be treated as the same character for
/// fuzzy matching purposes.
///
/// Command ids use `:` or `-` as word separators (e.g. `session:new`,
/// `app:theme-list`) while users naturally type a space in their place (e.g.
/// "session new", "theme list"), so all three are treated as interchangeable
/// separators — otherwise the literal space in the query never matches
/// anything in the id and the whole match fails.
fn fuzzy_chars_equivalent(a: char, b: char) -> bool {
    a == b || (matches!(a, ' ' | ':' | '-') && matches!(b, ' ' | ':' | '-'))
}

/// Compute a fuzzy match score between `query` and `target`.
///
/// Matches characters of `query` in order within `target`, penalising gaps
/// between consecutive matches. Higher scores indicate better matches.
/// Space, `:`, and `-` are treated as equivalent separators (see
/// [`fuzzy_chars_equivalent`]).
///
/// Returns `None` if `target` does not contain all characters of `query`.
fn fuzzy_score(query: &str, target: &str) -> Option<isize> {
    if query.is_empty() {
        return Some(0);
    }
    let target_lower: Vec<char> = target.to_lowercase().chars().collect();
    let query_chars: Vec<char> = query.to_lowercase().chars().collect();

    let mut qi = 0usize;
    let mut last_match = 0usize;
    let mut gaps = 0isize;

    for (ti, &tc) in target_lower.iter().enumerate() {
        if qi < query_chars.len() && fuzzy_chars_equivalent(tc, query_chars[qi]) {
            if qi > 0 {
                gaps += ti.cast_signed() - last_match.cast_signed() - 1;
            }
            last_match = ti;
            qi += 1;
        }
    }

    if qi == query_chars.len() {
        // Higher is better: more matched chars, fewer gaps
        Some(query_chars.len().cast_signed() * 10 - gaps)
    } else {
        None
    }
}

/// Filter and rank all registered commands by fuzzy match against `query`.
///
/// Merges the core, daemon, and extra registries, scores each entry against
/// both its `id` and `label`, and returns the results sorted by descending
/// score. An empty query returns all commands in registration order.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::command::filter_commands;
///
/// // Exact prefix match
/// let results = filter_commands("skill");
/// assert!(!results.is_empty());
/// assert_eq!(results[0].id, "skill:list");
///
/// // Empty query returns everything
/// let all = filter_commands("");
/// assert!(all.len() > 10);
///
/// // No match returns empty
/// let none = filter_commands("xyzzy");
/// assert!(none.is_empty());
/// ```
#[must_use]
pub fn filter_commands(query: &str) -> Vec<&'static CommandEntry> {
    let mut all: Vec<&'static CommandEntry> = command_registry().iter().collect();
    all.extend(daemon_command_registry());
    all.extend(extra_command_registry());
    all.extend(zeph_commands_entries());

    if query.is_empty() {
        return all;
    }

    // Trim and collapse runs of whitespace so a stray leading/trailing/doubled
    // space (e.g. "session  new", "session new ") doesn't desync the query's
    // separator count from the target's and cause the match to fail outright.
    let normalized_query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    let query = normalized_query.as_str();

    let mut scored: Vec<(&'static CommandEntry, isize)> = all
        .into_iter()
        .filter_map(|e| {
            let id_score = fuzzy_score(query, e.id);
            let label_score = fuzzy_score(query, e.label);
            let best = match (id_score, label_score) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            best.map(|s| (e, s))
        })
        .collect();

    scored.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    scored.into_iter().map(|(e, _)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_correct_count() {
        // +1 view:latency (#6059)
        assert_eq!(command_registry().len(), 28);
    }

    #[test]
    fn extra_registry_has_correct_command_count() {
        // 24 base (14 + 5 plan + 5 graph) + 5 experiment + 1 log:status + 1 config:migrate
        // + 1 compaction:status + 1 guidelines:view + 1 tafc:status + 1 lsp:status
        // + 1 forgetting-sweep + 3 acp + 1 sandbox:status (#3294) = 43
        // + 2 cocoon (#3673) when feature = "cocoon"
        // + 2 clipboard (#3685, #5098)
        // + 2 worktree (#4679)
        // + 3 knowledge (#5019, #5020)
        let expected = 50 + if cfg!(feature = "cocoon") { 2 } else { 0 };
        assert_eq!(extra_command_registry().len(), expected);
    }

    #[cfg(feature = "cocoon")]
    #[test]
    fn filter_cocoon_returns_cocoon_entries() {
        let results = filter_commands("cocoon");
        assert!(results.iter().any(|e| e.id == "cocoon:status"));
        assert!(results.iter().any(|e| e.id == "cocoon:models"));
    }

    #[test]
    fn filter_commands_includes_extra() {
        let all = filter_commands("");
        assert!(all.iter().any(|e| e.id == "view:filters"));
        assert!(all.iter().any(|e| e.id == "ingest"));
        assert!(all.iter().any(|e| e.id == "gateway:status"));
        assert!(all.iter().any(|e| e.id == "scheduler:list"));
        assert!(all.iter().any(|e| e.id == "security:events"));
        assert!(all.iter().any(|e| e.id == "log:status"));
    }

    #[test]
    fn filter_empty_query_returns_all() {
        let results = filter_commands("");
        assert_eq!(
            results.len(),
            command_registry().len()
                + daemon_command_registry().len()
                + extra_command_registry().len()
                + zeph_commands_entries().len()
        );
    }

    #[test]
    fn filter_by_id_prefix() {
        let results = filter_commands("skill");
        assert!(!results.is_empty());
        // skill:list must be the top-ranked result
        assert_eq!(results[0].id, "skill:list");
    }

    #[test]
    fn filter_by_label_substring() {
        let results = filter_commands("memory");
        assert!(!results.is_empty());
        assert!(results.iter().any(|e| e.id == "memory:stats"));
    }

    #[test]
    fn filter_case_insensitive() {
        let results = filter_commands("view");
        assert!(results.len() >= 4);
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let results = filter_commands("xxxxxx");
        assert!(results.is_empty());
    }

    #[test]
    fn filter_partial_label_match() {
        let results = filter_commands("cost");
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "view:cost");
    }

    #[test]
    fn filter_mcp_matches_id_and_label() {
        let results = filter_commands("mcp");
        assert!(results.iter().any(|e| e.id == "mcp:list"));
    }

    #[test]
    fn fuzzy_ranks_skill_list_above_mcp_list_for_sl() {
        let results = filter_commands("sl");
        // skill:list should appear before mcp:list
        let skill_pos = results.iter().position(|e| e.id == "skill:list");
        let mcp_pos = results.iter().position(|e| e.id == "mcp:list");
        assert!(skill_pos.is_some());
        if let (Some(s), Some(m)) = (skill_pos, mcp_pos) {
            assert!(
                s <= m,
                "skill:list should rank at least as high as mcp:list for 'sl'"
            );
        }
    }

    #[test]
    fn new_commands_present() {
        let all = filter_commands("");
        assert!(all.iter().any(|e| e.id == "app:quit"));
        assert!(all.iter().any(|e| e.id == "app:help"));
        assert!(all.iter().any(|e| e.id == "session:new"));
        assert!(all.iter().any(|e| e.id == "session:history"));
        assert!(all.iter().any(|e| e.id == "session:next"));
        assert!(all.iter().any(|e| e.id == "session:prev"));
        assert!(all.iter().any(|e| e.id == "session:close"));
    }

    #[test]
    fn filter_space_query_matches_colon_separated_id() {
        // Typing the natural "session new" (space instead of colon) must
        // still match the "session:new" command id (#5790).
        let results = filter_commands("session new");
        assert!(
            results.iter().any(|e| e.id == "session:new"),
            "session:new must match query with a literal space"
        );

        let results = filter_commands("skill list");
        assert!(
            results.iter().any(|e| e.id == "skill:list"),
            "skill:list must match query with a literal space"
        );
    }

    #[test]
    fn filter_colon_query_still_matches_colon_id() {
        // A literal ':' in the query (typed verbatim, e.g. "session:new") must
        // keep matching its own id exactly as before the space/colon
        // equivalence fix (#5790).
        let results = filter_commands("session:new");
        assert_eq!(
            results.first().map(|e| e.id),
            Some("session:new"),
            "literal colon query must still rank its own id first"
        );
    }

    #[test]
    fn filter_multiword_label_query_matches_session_next_not_regressed() {
        // "session:next" has no literal colon-for-space substitution needed:
        // its label "Switch to next session (/session next)" already
        // contains "session next" as a literal substring. Confirm the
        // space/colon equivalence fix does not regress this pre-existing
        // label-based match.
        let results = filter_commands("session next");
        assert!(
            results.iter().any(|e| e.id == "session:next"),
            "session:next must still match its label substring 'session next'"
        );
    }

    #[test]
    fn filter_repeated_or_boundary_whitespace_normalized() {
        // `filter_commands` trims and collapses whitespace before scoring, so
        // a stray double space or leading/trailing space (trivially plausible
        // typos) no longer desyncs the query's separator count from the
        // target's and reproduces the #5790 symptom (autocomplete popup finds
        // nothing, raw text falls through as a chat message).
        assert!(
            filter_commands("session  new")
                .iter()
                .any(|e| e.id == "session:new"),
            "double space must still match session:new"
        );
        assert!(
            filter_commands("session new ")
                .iter()
                .any(|e| e.id == "session:new"),
            "trailing space must still match session:new"
        );
        assert!(
            filter_commands(" session new")
                .iter()
                .any(|e| e.id == "session:new"),
            "leading space must still match session:new"
        );
    }

    #[test]
    fn filter_hyphenated_id_matches_space_query() {
        // Hyphen-separated ids (e.g. app:theme-list) exhibit the same #5790
        // symptom as colon-separated ones: a query typed with a space in
        // place of the hyphen must still match.
        let results = filter_commands("theme list");
        assert!(
            results.iter().any(|e| e.id == "app:theme-list"),
            "app:theme-list must match query with a literal space in place of the hyphen"
        );
    }

    #[test]
    fn shortcut_on_quit_and_help() {
        let registry = command_registry();
        let quit = registry.iter().find(|e| e.id == "app:quit").unwrap();
        let help = registry.iter().find(|e| e.id == "app:help").unwrap();
        assert_eq!(quit.shortcut, Some("q"));
        assert_eq!(help.shortcut, Some("?"));
    }

    #[test]
    fn zeph_commands_entries_includes_previously_invisible_commands() {
        // #5875: these AgentAccess-routed commands were dispatchable when typed in full but
        // never appeared in TUI autocomplete because zeph-tui's registry never sourced from
        // zeph_commands::COMMANDS.
        let entries = zeph_commands_entries();
        for name in [
            "/model",
            "/provider",
            "/skill",
            "/policy",
            "/think-tokens",
            "/reasoning-effort",
            "/status",
            "/conv",
        ] {
            assert!(
                entries.iter().any(|e| e.id == name),
                "{name} must appear in zeph_commands_entries()"
            );
        }
    }

    #[test]
    fn zeph_commands_entries_excludes_dedup_list() {
        let entries = zeph_commands_entries();
        for name in ZEPH_COMMANDS_DEDUP {
            assert!(
                !entries.iter().any(|e| &e.id == name),
                "{name} is already covered by a hand-authored TuiCommand and must not be \
                 duplicated in zeph_commands_entries()"
            );
        }
    }

    #[test]
    fn zeph_commands_entries_includes_clear_queue_not_a_real_duplicate() {
        // #5875 F3 fix: /clear-queue was incorrectly in ZEPH_COMMANDS_DEDUP — the only
        // matching TuiCommand (SendClearQueue) is reachable exclusively via the Ctrl+K
        // keybinding, with no CommandEntry in any of the three hand-authored registries, so
        // there was nothing to actually deduplicate against. It must appear here.
        let entries = zeph_commands_entries();
        assert!(entries.iter().any(|e| e.id == "/clear-queue"));
    }

    #[test]
    fn zeph_commands_dedup_entries_have_a_real_hand_authored_replacement() {
        // #5875 F3: guards against a dedup'd name silently becoming an orphan (e.g. if the
        // hand-authored entry it claims to duplicate is later renamed or removed).
        let mut hand_authored: Vec<&'static CommandEntry> = command_registry().iter().collect();
        hand_authored.extend(daemon_command_registry());
        hand_authored.extend(extra_command_registry());

        let expected: &[(&str, &str)] = &[
            ("/skills", "skill:list"),
            ("/mcp", "mcp:list"),
            ("/memory", "memory:stats"),
            ("/guidelines", "guidelines:view"),
            ("/log", "log:status"),
            ("/undo", "session:undo"),
            ("/redo", "session:redo"),
            ("/graph", "graph:stats"),
            ("/lsp", "lsp:status"),
            ("/scheduler", "scheduler:list"),
            ("/subagent", "acp:subagent-spawn"),
        ];
        assert_eq!(
            expected.len(),
            ZEPH_COMMANDS_DEDUP.len(),
            "this test's `expected` table has drifted out of sync with ZEPH_COMMANDS_DEDUP — \
             update both together"
        );
        for (dedup_name, expected_hand_id) in expected {
            assert!(
                ZEPH_COMMANDS_DEDUP.contains(dedup_name),
                "test out of sync: {dedup_name} is not in ZEPH_COMMANDS_DEDUP"
            );
            assert!(
                hand_authored.iter().any(|e| &e.id == expected_hand_id),
                "{dedup_name} is in ZEPH_COMMANDS_DEDUP claiming to be covered by \
                 {expected_hand_id}, but no such hand-authored CommandEntry exists — either \
                 restore equivalent coverage or remove {dedup_name} from ZEPH_COMMANDS_DEDUP \
                 so it reappears in autocomplete"
            );
        }
    }

    #[test]
    fn zeph_commands_entries_prefills_mandatory_arg_commands() {
        // #5875 F1: commands whose bare form would just produce a usage error must prefill
        // the input for the user to complete, not submit immediately.
        let entries = zeph_commands_entries();
        for name in [
            "/image",
            "/feedback",
            "/skill",
            "/skill create",
            "/dump-format",
            "/loop",
        ] {
            let entry = entries
                .iter()
                .find(|e| e.id == name)
                .unwrap_or_else(|| panic!("{name} must appear in zeph_commands_entries()"));
            assert!(
                matches!(entry.command, TuiCommand::PrefillVerbatim(_)),
                "{name} requires an argument and must prefill rather than submit bare"
            );
        }
    }

    #[test]
    fn zeph_commands_entries_sends_safe_bare_commands_immediately() {
        // #5875 F1: commands whose bare (no-arg) form is a valid, useful default (verified
        // against each handler's real empty-args behavior, not just the `args` hint text —
        // `/goal` and `/worktree` both default sensibly despite non-bracket-wrapped hints)
        // must still submit immediately.
        let entries = zeph_commands_entries();
        for name in ["/model", "/status", "/goal", "/worktree", "/conv"] {
            let entry = entries
                .iter()
                .find(|e| e.id == name)
                .unwrap_or_else(|| panic!("{name} must appear in zeph_commands_entries()"));
            assert!(
                matches!(entry.command, TuiCommand::SendVerbatim(_)),
                "{name} has a safe bare default and should submit immediately"
            );
        }
    }

    #[test]
    #[cfg(feature = "cocoon")]
    fn zeph_commands_entries_includes_cocoon_when_feature_enabled() {
        // #5875 F2.
        let entries = zeph_commands_entries();
        assert!(entries.iter().any(|e| e.id == "/cocoon"));
    }

    #[test]
    #[cfg(not(feature = "cocoon"))]
    fn zeph_commands_entries_excludes_cocoon_when_feature_disabled() {
        // #5875 F2: without this, a cocoon-feature-off build would still show /cocoon in
        // autocomplete and fail when submitted, since CocoonCommand is never registered.
        let entries = zeph_commands_entries();
        assert!(!entries.iter().any(|e| e.id == "/cocoon"));
    }

    #[test]
    fn filter_commands_merges_zeph_commands_entries() {
        let results = filter_commands("model");
        assert!(results.iter().any(|e| e.id == "/model"));
    }

    #[test]
    fn no_duplicate_ids_across_merged_registries() {
        let all = filter_commands("");
        let mut seen = std::collections::HashSet::new();
        for entry in &all {
            assert!(
                seen.insert(entry.id),
                "duplicate command id in merged autocomplete list: {}",
                entry.id
            );
        }
    }

    #[test]
    fn filter_security_returns_security_events_entry() {
        let results = filter_commands("security");
        assert!(
            results.iter().any(|e| e.id == "security:events"),
            "security:events must appear when searching 'security'"
        );
    }

    #[test]
    fn filter_graph_returns_graph_entries() {
        let results = filter_commands("graph");
        assert!(results.iter().any(|e| e.id == "graph:stats"));
        assert!(results.iter().any(|e| e.id == "graph:entities"));
        assert!(results.iter().any(|e| e.id == "graph:facts"));
        assert!(results.iter().any(|e| e.id == "graph:communities"));
        assert!(results.iter().any(|e| e.id == "graph:backfill"));
    }

    #[test]
    fn filter_experiment_returns_experiment_entries() {
        let results = filter_commands("experiment");
        assert!(results.iter().any(|e| e.id == "experiment:start"));
        assert!(results.iter().any(|e| e.id == "experiment:stop"));
        assert!(results.iter().any(|e| e.id == "experiment:status"));
        assert!(results.iter().any(|e| e.id == "experiment:report"));
        assert!(results.iter().any(|e| e.id == "experiment:best"));
    }

    #[test]
    fn filter_clipboard_returns_copy_entry() {
        let results = filter_commands("copy");
        assert!(
            results.iter().any(|e| e.id == "clipboard:copy"),
            "clipboard:copy must appear when searching 'copy'"
        );
    }

    #[test]
    fn clipboard_copy_command_is_copy_last_assistant() {
        let all = filter_commands("");
        let entry = all.iter().find(|e| e.id == "clipboard:copy").unwrap();
        assert_eq!(entry.command, TuiCommand::CopyLastAssistant);
        assert_eq!(entry.shortcut, Some("Ctrl+O"));
    }
}
