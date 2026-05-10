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
    SubagentSpawn { command: String },
    // Sandbox egress status (#3294)
    SandboxStatus,
    // Cocoon sidecar inspection (#3673)
    CocoonStatus,
    CocoonModels,
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
            id: "tasks",
            label: "Toggle task registry panel",
            category: "view",
            shortcut: None,
            command: TuiCommand::TaskPanel,
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
            label: "Toggle theme (dark/light)",
            category: "app",
            shortcut: None,
            command: TuiCommand::ToggleTheme,
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
    cmds
}

/// Compute a fuzzy match score between `query` and `target`.
///
/// Matches characters of `query` in order within `target`, penalising gaps
/// between consecutive matches. Higher scores indicate better matches.
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
        if qi < query_chars.len() && tc == query_chars[qi] {
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

    if query.is_empty() {
        return all;
    }

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
        assert_eq!(command_registry().len(), 20);
    }

    #[test]
    fn extra_registry_has_correct_command_count() {
        // 24 base (14 + 5 plan + 5 graph) + 5 experiment + 1 log:status + 1 config:migrate
        // + 1 compaction:status + 1 guidelines:view + 1 tafc:status + 1 lsp:status
        // + 1 forgetting-sweep + 3 acp + 1 sandbox:status (#3294) = 43
        // + 2 cocoon (#3673) when feature = "cocoon"
        let expected = 43 + if cfg!(feature = "cocoon") { 2 } else { 0 };
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
    fn shortcut_on_quit_and_help() {
        let registry = command_registry();
        let quit = registry.iter().find(|e| e.id == "app:quit").unwrap();
        let help = registry.iter().find(|e| e.id == "app:help").unwrap();
        assert_eq!(quit.shortcut, Some("q"));
        assert_eq!(help.shortcut, Some("?"));
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
}
