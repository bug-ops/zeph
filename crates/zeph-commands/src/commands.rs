// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Static registry of all slash commands, used for `/help` output generation.
//!
//! This module holds the `COMMANDS` constant that was previously in `zeph-core`.
//! Moving it here allows the `/help` handler to reference it without depending
//! on `zeph-core`.

use crate::{CommandInfo, SlashCategory};

/// All slash commands recognised by the agent loop, in display order.
///
/// Feature-gated entries use `feature_gate: Some("feature-name")` for display
/// purposes (showing `[requires: feature]` in `/help` output). All entries are
/// always compiled in; gating is runtime-only via the `feature_gate` field.
pub const COMMANDS: &[CommandInfo] = &[
    // --- Debugging (info/status commands) ---
    CommandInfo {
        name: "/help",
        args: "",
        description: "Show this help message",
        category: SlashCategory::Debugging,
        feature_gate: None,
    },
    CommandInfo {
        name: "/status",
        args: "",
        description: "Show current session status (provider, model, tokens, uptime)",
        category: SlashCategory::Debugging,
        feature_gate: None,
    },
    CommandInfo {
        name: "/skills",
        args: "",
        description: "List loaded skills (grouped by category when available)",
        category: SlashCategory::Skills,
        feature_gate: None,
    },
    CommandInfo {
        name: "/skills confusability",
        args: "",
        description: "Show skill pairs with high embedding similarity (potential disambiguation failures)",
        category: SlashCategory::Skills,
        feature_gate: None,
    },
    CommandInfo {
        name: "/guardrail",
        args: "",
        description: "Show guardrail status (provider, model, action, timeout, stats)",
        category: SlashCategory::Debugging,
        feature_gate: Some("guardrail"),
    },
    CommandInfo {
        name: "/log",
        args: "",
        description: "Toggle verbose log output",
        category: SlashCategory::Debugging,
        feature_gate: None,
    },
    // --- Session ---
    CommandInfo {
        name: "/exit",
        args: "",
        description: "Exit the agent (also: /quit)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    CommandInfo {
        name: "/new",
        args: "[--no-digest] [--keep-plan]",
        description: "Start a new conversation (reset context, preserve memory and MCP)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    CommandInfo {
        name: "/clear",
        args: "",
        description: "Clear conversation history",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    CommandInfo {
        name: "/reset",
        args: "",
        description: "Reset conversation history (alias for /clear, replies with confirmation)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    CommandInfo {
        name: "/clear-queue",
        args: "",
        description: "Discard queued messages",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    CommandInfo {
        name: "/compact",
        args: "",
        description: "Compact the context window",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    CommandInfo {
        name: "/recap",
        args: "",
        description: "Show a recap of the current or previous session",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    // --- Configuration (model/provider) ---
    CommandInfo {
        name: "/model",
        args: "[id|refresh]",
        description: "Show or switch the active model",
        category: SlashCategory::Configuration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/provider",
        args: "[name|status]",
        description: "List configured providers or switch to one by name",
        category: SlashCategory::Configuration,
        feature_gate: None,
    },
    // --- Memory ---
    CommandInfo {
        name: "/feedback",
        args: "<skill> <message>",
        description: "Submit feedback for a skill",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    CommandInfo {
        name: "/graph",
        args: "[subcommand]",
        description: "Query or manage the knowledge graph",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    CommandInfo {
        name: "/memory",
        args: "[tiers|promote <id>...]",
        description: "Show memory tier stats or manually promote messages to semantic tier",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    CommandInfo {
        name: "/guidelines",
        args: "",
        description: "Show current compression guidelines",
        category: SlashCategory::Memory,
        feature_gate: Some("compression-guidelines"),
    },
    // --- Skills ---
    CommandInfo {
        name: "/skill",
        args: "<name>",
        description: "Load and display a skill body",
        category: SlashCategory::Skills,
        feature_gate: None,
    },
    CommandInfo {
        name: "/skill create",
        args: "<description>",
        description: "Generate a SKILL.md from natural language via LLM",
        category: SlashCategory::Skills,
        feature_gate: None,
    },
    // --- Integration (external tools) ---
    CommandInfo {
        name: "/mcp",
        args: "[add|list|tools|remove]",
        description: "Manage MCP servers",
        category: SlashCategory::Integration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/image",
        args: "<path>",
        description: "Attach an image to the next message",
        category: SlashCategory::Integration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/agent",
        args: "[subcommand]",
        description: "Manage sub-agents",
        category: SlashCategory::Integration,
        feature_gate: None,
    },
    // --- Planning ---
    CommandInfo {
        name: "/plan",
        args: "[goal|confirm|cancel|status|list|resume|retry]",
        description: "Create or manage execution plans",
        category: SlashCategory::Planning,
        feature_gate: None,
    },
    // --- Debugging ---
    CommandInfo {
        name: "/debug-dump",
        args: "[path]",
        description: "Enable or toggle debug dump output",
        category: SlashCategory::Debugging,
        feature_gate: None,
    },
    CommandInfo {
        name: "/dump-format",
        args: "<json|raw|trace>",
        description: "Switch debug dump format at runtime",
        category: SlashCategory::Debugging,
        feature_gate: None,
    },
    // --- Advanced (feature-gated) ---
    CommandInfo {
        name: "/scheduler",
        args: "[list]",
        description: "List scheduled tasks",
        category: SlashCategory::Integration,
        feature_gate: Some("scheduler"),
    },
    CommandInfo {
        name: "/experiment",
        args: "[subcommand]",
        description: "Experimental features",
        category: SlashCategory::Advanced,
        feature_gate: Some("experiments"),
    },
    CommandInfo {
        name: "/lsp",
        args: "",
        description: "Show LSP context status",
        category: SlashCategory::Debugging,
        feature_gate: Some("lsp-context"),
    },
    CommandInfo {
        name: "/policy",
        args: "[status|check <tool> [args_json]]",
        description: "Inspect policy status or dry-run evaluation",
        category: SlashCategory::Advanced,
        feature_gate: Some("policy-enforcer"),
    },
    CommandInfo {
        name: "/focus",
        args: "",
        description: "Show Focus Agent status (active session, knowledge block size)",
        category: SlashCategory::Advanced,
        feature_gate: Some("context-compression"),
    },
    CommandInfo {
        name: "/sidequest",
        args: "",
        description: "Show SideQuest eviction stats (passes run, tokens freed)",
        category: SlashCategory::Advanced,
        feature_gate: Some("context-compression"),
    },
    CommandInfo {
        name: "/cache-stats",
        args: "",
        description: "Show tool orchestrator cache statistics",
        category: SlashCategory::Debugging,
        feature_gate: None,
    },
];
