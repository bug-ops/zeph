// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Static registry of all slash commands available in the agent loop.
//!
//! Used by `/help` to enumerate and display commands grouped by category.

/// Broad grouping for displaying commands in `/help` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCategory {
    Session,
    Model,
    Info,
    Memory,
    Tools,
    Debug,
    Planning,
    Advanced,
}

impl SlashCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Model => "Model",
            Self::Info => "Info",
            Self::Memory => "Memory",
            Self::Tools => "Tools",
            Self::Debug => "Debug",
            Self::Planning => "Planning",
            Self::Advanced => "Advanced",
        }
    }
}

/// Metadata for a single slash command displayed by `/help`.
pub struct SlashCommandInfo {
    pub name: &'static str,
    /// Argument hint shown after the command name, e.g. `[path]` or `<name>`.
    pub args: &'static str,
    pub description: &'static str,
    pub category: SlashCategory,
    /// When `Some`, this entry was compiled in only for that feature.
    /// Shown in the help output as `[requires: <feature>]`.
    pub feature_gate: Option<&'static str>,
}

/// All slash commands recognised by the agent loop, in display order.
///
/// Feature-gated entries are wrapped in `#[cfg(feature = "...")]` so that
/// only commands compiled into the binary appear in `/help`.
pub const COMMANDS: &[SlashCommandInfo] = &[
    // --- Info ---
    SlashCommandInfo {
        name: "/help",
        args: "",
        description: "Show this help message",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/status",
        args: "",
        description: "Show current session status (provider, model, tokens, uptime)",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/skills",
        args: "",
        description: "List loaded skills",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    #[cfg(feature = "guardrail")]
    SlashCommandInfo {
        name: "/guardrail",
        args: "",
        description: "Show guardrail status (provider, model, action, timeout, stats)",
        category: SlashCategory::Info,
        feature_gate: Some("guardrail"),
    },
    SlashCommandInfo {
        name: "/log",
        args: "",
        description: "Toggle verbose log output",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    // --- Session ---
    SlashCommandInfo {
        name: "/exit",
        args: "",
        description: "Exit the agent (also: /quit)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/clear",
        args: "",
        description: "Clear conversation history",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/clear-queue",
        args: "",
        description: "Discard queued messages",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/compact",
        args: "",
        description: "Compact the context window",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    // --- Model ---
    SlashCommandInfo {
        name: "/model",
        args: "[id|refresh]",
        description: "Show or switch the active model",
        category: SlashCategory::Model,
        feature_gate: None,
    },
    // --- Memory ---
    SlashCommandInfo {
        name: "/feedback",
        args: "<skill> <message>",
        description: "Submit feedback for a skill",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/graph",
        args: "[subcommand]",
        description: "Query or manage the knowledge graph",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    #[cfg(feature = "compression-guidelines")]
    SlashCommandInfo {
        name: "/guidelines",
        args: "",
        description: "Show current compression guidelines",
        category: SlashCategory::Memory,
        feature_gate: Some("compression-guidelines"),
    },
    // --- Tools ---
    SlashCommandInfo {
        name: "/skill",
        args: "<name>",
        description: "Load and display a skill body",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/mcp",
        args: "[add|list|tools|remove]",
        description: "Manage MCP servers",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/image",
        args: "<path>",
        description: "Attach an image to the next message",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/agent",
        args: "[subcommand]",
        description: "Manage sub-agents",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    // --- Planning ---
    SlashCommandInfo {
        name: "/plan",
        args: "[goal|confirm|cancel|status|list|resume|retry]",
        description: "Create or manage execution plans",
        category: SlashCategory::Planning,
        feature_gate: None,
    },
    // --- Debug ---
    SlashCommandInfo {
        name: "/debug-dump",
        args: "[path]",
        description: "Enable or toggle debug dump output",
        category: SlashCategory::Debug,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/dump-format",
        args: "<json|raw|trace>",
        description: "Switch debug dump format at runtime",
        category: SlashCategory::Debug,
        feature_gate: None,
    },
    // --- Advanced (feature-gated) ---
    #[cfg(feature = "scheduler")]
    SlashCommandInfo {
        name: "/scheduler",
        args: "[list]",
        description: "List scheduled tasks",
        category: SlashCategory::Tools,
        feature_gate: Some("scheduler"),
    },
    #[cfg(feature = "experiments")]
    SlashCommandInfo {
        name: "/experiment",
        args: "[subcommand]",
        description: "Experimental features",
        category: SlashCategory::Advanced,
        feature_gate: Some("experiments"),
    },
    #[cfg(feature = "lsp-context")]
    SlashCommandInfo {
        name: "/lsp",
        args: "",
        description: "Show LSP context status",
        category: SlashCategory::Advanced,
        feature_gate: Some("lsp-context"),
    },
    #[cfg(feature = "policy-enforcer")]
    SlashCommandInfo {
        name: "/policy",
        args: "[status|check <tool> [args_json]]",
        description: "Inspect policy status or dry-run evaluation",
        category: SlashCategory::Tools,
        feature_gate: Some("policy-enforcer"),
    },
    #[cfg(feature = "context-compression")]
    SlashCommandInfo {
        name: "/focus",
        args: "",
        description: "Show Focus Agent status (active session, knowledge block size)",
        category: SlashCategory::Advanced,
        feature_gate: Some("context-compression"),
    },
    #[cfg(feature = "context-compression")]
    SlashCommandInfo {
        name: "/sidequest",
        args: "",
        description: "Show SideQuest eviction stats (passes run, tokens freed)",
        category: SlashCategory::Advanced,
        feature_gate: Some("context-compression"),
    },
];
