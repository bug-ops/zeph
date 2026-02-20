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
    // Daemon / remote connection commands
    DaemonConnect,
    DaemonDisconnect,
    DaemonStatus,
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
    fn registry_has_eleven_commands() {
        assert_eq!(command_registry().len(), 11);
    }

    #[test]
    fn filter_empty_query_returns_all() {
        let results = filter_commands("");
        assert_eq!(
            results.len(),
            command_registry().len() + daemon_command_registry().len()
        );
    }

    #[test]
    fn filter_by_id_prefix() {
        let results = filter_commands("skill");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "skill:list");
    }

    #[test]
    fn filter_by_label_substring() {
        let results = filter_commands("memory");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "memory:stats");
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
    }

    #[test]
    fn shortcut_on_quit_and_help() {
        let registry = command_registry();
        let quit = registry.iter().find(|e| e.id == "app:quit").unwrap();
        let help = registry.iter().find(|e| e.id == "app:help").unwrap();
        assert_eq!(quit.shortcut, Some("q"));
        assert_eq!(help.shortcut, Some("?"));
    }
}
