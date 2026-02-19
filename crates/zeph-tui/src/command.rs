/// Commands that can be sent from TUI to Agent loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiCommand {
    SkillList,
    McpList,
    MemoryStats,
    ViewCost,
    ViewTools,
    ViewConfig,
    ViewAutonomy,
}

/// Metadata for command palette display and fuzzy matching.
pub struct CommandEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub category: &'static str,
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
            command: TuiCommand::SkillList,
        },
        CommandEntry {
            id: "mcp:list",
            label: "List MCP servers and tools",
            category: "mcp",
            command: TuiCommand::McpList,
        },
        CommandEntry {
            id: "memory:stats",
            label: "Show memory statistics",
            category: "memory",
            command: TuiCommand::MemoryStats,
        },
        CommandEntry {
            id: "view:cost",
            label: "Show cost breakdown",
            category: "view",
            command: TuiCommand::ViewCost,
        },
        CommandEntry {
            id: "view:tools",
            label: "List available tools",
            category: "view",
            command: TuiCommand::ViewTools,
        },
        CommandEntry {
            id: "view:config",
            label: "Show active configuration",
            category: "view",
            command: TuiCommand::ViewConfig,
        },
        CommandEntry {
            id: "view:autonomy",
            label: "Show autonomy/trust level",
            category: "view",
            command: TuiCommand::ViewAutonomy,
        },
    ];
    COMMANDS
}

/// Filters commands by case-insensitive substring match on id or label.
#[must_use]
pub fn filter_commands(query: &str) -> Vec<&'static CommandEntry> {
    if query.is_empty() {
        return command_registry().iter().collect();
    }
    let q = query.to_lowercase();
    command_registry()
        .iter()
        .filter(|e| e.id.to_lowercase().contains(&q) || e.label.to_lowercase().contains(&q))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_seven_commands() {
        assert_eq!(command_registry().len(), 7);
    }

    #[test]
    fn filter_empty_query_returns_all() {
        let results = filter_commands("");
        assert_eq!(results.len(), 7);
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
        let results = filter_commands("VIEW");
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let results = filter_commands("xxxxxx");
        assert!(results.is_empty());
    }

    #[test]
    fn filter_partial_label_match() {
        let results = filter_commands("cost");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "view:cost");
    }

    #[test]
    fn filter_mcp_matches_id_and_label() {
        let results = filter_commands("mcp");
        assert!(results.iter().any(|e| e.id == "mcp:list"));
    }
}
