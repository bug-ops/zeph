// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Commands that can be sent from TUI to Agent loop.
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
    #[cfg(feature = "graph-memory")]
    GraphStats,
    #[cfg(feature = "graph-memory")]
    GraphEntities,
    #[cfg(feature = "graph-memory")]
    GraphFactsPrompt,
    #[cfg(feature = "graph-memory")]
    GraphCommunities,
    #[cfg(feature = "graph-memory")]
    GraphBackfillPrompt,
}

/// Metadata for command palette display and fuzzy matching.
pub struct CommandEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub category: &'static str,
    pub shortcut: Option<&'static str>,
    pub command: TuiCommand,
}

/// Static registry of all available commands.
#[must_use]
pub fn command_registry() -> &'static [CommandEntry] {
    static COMMANDS: &[CommandEntry] = &[
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
    ];
    COMMANDS
}

/// Daemon / remote-mode commands.
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

/// Extended command registry: filter/ingest/gateway entries.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn extra_command_registry() -> &'static [CommandEntry] {
    static EXTRA: &[CommandEntry] = &[
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
            id: "router:stats",
            label: "Show Thompson router alpha/beta per provider",
            category: "router",
            shortcut: None,
            command: TuiCommand::RouterStats,
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
            id: "security:events",
            label: "Show security event history",
            category: "security",
            shortcut: None,
            command: TuiCommand::SecurityEvents,
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
        #[cfg(feature = "graph-memory")]
        CommandEntry {
            id: "graph:stats",
            label: "Show graph memory statistics (/graph)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphStats,
        },
        #[cfg(feature = "graph-memory")]
        CommandEntry {
            id: "graph:entities",
            label: "List graph entities (/graph entities)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphEntities,
        },
        #[cfg(feature = "graph-memory")]
        CommandEntry {
            id: "graph:facts",
            label: "Show entity facts (/graph facts <name>)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphFactsPrompt,
        },
        #[cfg(feature = "graph-memory")]
        CommandEntry {
            id: "graph:communities",
            label: "List graph communities (/graph communities)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphCommunities,
        },
        #[cfg(feature = "graph-memory")]
        CommandEntry {
            id: "graph:backfill",
            label: "Backfill graph from existing messages (/graph backfill)",
            category: "graph",
            shortcut: None,
            command: TuiCommand::GraphBackfillPrompt,
        },
    ];
    EXTRA
}

/// Fuzzy score: count of matched characters in order, with penalty for gaps.
/// Returns `None` if not all query chars are found in target.
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

/// Filters and sorts commands by fuzzy score on id or label.
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

    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().map(|(e, _)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_twelve_commands() {
        assert_eq!(command_registry().len(), 12);
    }

    #[test]
    fn extra_registry_has_correct_command_count() {
        // 19 base (14 + 5 plan) + 5 graph-memory commands (when feature enabled)
        #[cfg(feature = "graph-memory")]
        assert_eq!(extra_command_registry().len(), 24);
        #[cfg(not(feature = "graph-memory"))]
        assert_eq!(extra_command_registry().len(), 19);
    }

    #[test]
    fn filter_commands_includes_extra() {
        let all = filter_commands("");
        assert!(all.iter().any(|e| e.id == "view:filters"));
        assert!(all.iter().any(|e| e.id == "ingest"));
        assert!(all.iter().any(|e| e.id == "gateway:status"));
        assert!(all.iter().any(|e| e.id == "scheduler:list"));
        assert!(all.iter().any(|e| e.id == "security:events"));
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
        assert!(results.len() >= 1);
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

    #[cfg(feature = "graph-memory")]
    #[test]
    fn filter_graph_returns_graph_entries() {
        let results = filter_commands("graph");
        assert!(results.iter().any(|e| e.id == "graph:stats"));
        assert!(results.iter().any(|e| e.id == "graph:entities"));
        assert!(results.iter().any(|e| e.id == "graph:facts"));
        assert!(results.iter().any(|e| e.id == "graph:communities"));
        assert!(results.iter().any(|e| e.id == "graph:backfill"));
    }
}
