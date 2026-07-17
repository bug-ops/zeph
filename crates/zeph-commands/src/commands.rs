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
    // --- Goals ---
    CommandInfo {
        name: "/goal",
        args: "create <text> [--budget N] | pause | resume | complete | clear | status | list",
        description: "Manage long-horizon goals that persist across conversation turns",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    // --- Undo / Redo ---
    CommandInfo {
        name: "/undo",
        args: "[N | list]",
        description: "Undo the last N file-mutating shell commands (session-scoped)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    CommandInfo {
        name: "/redo",
        args: "",
        description: "Re-apply the last undone shell command",
        category: SlashCategory::Session,
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
        name: "/quit",
        args: "",
        description: "Exit the agent (alias for /exit)",
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
    CommandInfo {
        name: "/conv",
        args: "[list | show <id> | resume <id> | fork <id>]",
        description: "List, inspect, resume, or fork durable conversation-sessions",
        category: SlashCategory::Session,
        feature_gate: Some("session"),
    },
    CommandInfo {
        name: "/cd",
        args: "[path]",
        description: "Change the session's working directory (no arg: show current)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    // --- Configuration (model/provider) ---
    CommandInfo {
        name: "/model",
        args: "[id|refresh]",
        description: "Show or switch the active model. Examples: `/model claude-sonnet-5` to switch, `/model refresh` to re-query the provider model list",
        category: SlashCategory::Configuration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/provider",
        args: "[name|status]",
        description: "List configured providers or switch to one by name. Examples: `/provider quality` to switch, `/provider status` to show health of all configured providers",
        category: SlashCategory::Configuration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/think-tokens",
        args: "[N|Nk|NM|off]",
        description: "Show or set the active provider's runtime thinking-token budget (session-only, not persisted). Examples: `/think-tokens 8k`, `/think-tokens off`",
        category: SlashCategory::Configuration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/reasoning-effort",
        args: "[low|medium|high]",
        description: "Show or set the active provider's runtime reasoning-effort level (session-only, not persisted). Example: `/reasoning-effort high`",
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
        name: "/knowledge",
        args: "[status | rollback <batch_id>]",
        description: "Query the knowledge ingest ledger or roll back a batch",
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
        name: "/store",
        args: "get <ns> <key> | put <ns> <key> <value...> | list <ns_prefix> [limit] | delete <ns> <key>",
        description: "Read/write the cross-thread key-value store (opt-in, [memory.store])",
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
        name: "/plugins",
        args: "[list | install <name> | remove <name> | update [name]]",
        description: "Manage installed plugins",
        category: SlashCategory::Integration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/acp",
        args: "[dirs | auth-methods | status]",
        description: "Inspect ACP server configuration",
        category: SlashCategory::Integration,
        feature_gate: Some("acp"),
    },
    CommandInfo {
        name: "/mcp",
        args: "[add|list|tools|remove]",
        description: "Manage MCP servers",
        category: SlashCategory::Integration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/cocoon",
        args: "[status | models]",
        description: "Inspect Cocoon sidecar (status, models)",
        category: SlashCategory::Integration,
        feature_gate: Some("cocoon"),
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
    CommandInfo {
        name: "/agents",
        args: "[list|show|create|edit|delete <name>]",
        description: "Fleet view: active autonomous goals and sub-agent definitions",
        category: SlashCategory::Integration,
        feature_gate: None,
    },
    CommandInfo {
        name: "/subagent",
        args: "spawn <command>",
        description: "Spawn an external ACP sub-agent process",
        category: SlashCategory::Integration,
        feature_gate: Some("acp"),
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
        name: "/trajectory",
        args: "[status|reset]",
        description: "Show trajectory risk sentinel status or reset it",
        category: SlashCategory::Advanced,
        feature_gate: None,
    },
    CommandInfo {
        name: "/scope",
        args: "[list [task_type]]",
        description: "List configured capability scopes (spec 050)",
        category: SlashCategory::Advanced,
        feature_gate: None,
    },
    CommandInfo {
        name: "/worktree",
        args: "list | clean [--force]",
        description: "List or clean the live session's git worktrees",
        category: SlashCategory::Advanced,
        feature_gate: None,
    },
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
    CommandInfo {
        name: "/loop",
        args: "<prompt> every <N> <unit> | stop",
        description: "Repeat a prompt on a fixed interval (min 5s), or stop the active loop",
        category: SlashCategory::Advanced,
        feature_gate: None,
    },
    CommandInfo {
        name: "/notify-test",
        args: "",
        description: "Send a test notification via all enabled channels (macOS, webhook)",
        category: SlashCategory::Debugging,
        feature_gate: None,
    },
    CommandInfo {
        name: "/caveman",
        args: "[on|off|status]",
        description: "Toggle ultra-compressed telegraphic output mode",
        category: SlashCategory::Configuration,
        feature_gate: None,
    },
];

/// Commands dispatched by `zeph-core`'s `dispatch_slash_command`
/// (`crates/zeph-core/src/agent/slash_commands.rs`) rather than through
/// [`crate::CommandRegistry::dispatch`], and therefore never subject to the registry's
/// `trusted`/`requires_auth` gate at all — `dispatch_slash_command` runs them unconditionally,
/// regardless of channel trust. [`is_recognized_command`] must never recognize these: HTTP
/// entry points forward recognized text raw specifically so the registry's trust gate can
/// decide whether to run it, and a command with no gate to defer to would run unconditionally
/// on any caller, trusted or not. Currently: `/subagent` (`spawn <cmd>` spawns an external ACP
/// process — remote code execution if reachable from an untrusted caller, #5904 CRITICAL-2).
const UNGATED_DISPATCH_COMMANDS: &[&str] = &["/subagent"];

fn matches_command_name(trimmed: &str, name: &str) -> bool {
    trimmed == name
        || trimmed
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(' '))
}

/// Returns `true` when `trimmed` is an exact match for a command name in [`COMMANDS`],
/// or a command name followed by a space-separated argument — excluding
/// `UNGATED_DISPATCH_COMMANDS` (commands dispatched outside the trust-gated registry).
///
/// Mirrors the matching rule used by [`crate::CommandRegistry::find_handler`] (exact
/// name, or name + `' '` + args — never a fuzzy/substring match), but does not require
/// constructing a live registry or context. Intended for entry points that must decide,
/// on raw untrusted text and *before* any content sanitization, whether a message is a
/// recognized command — e.g. HTTP prompt/webhook endpoints that would otherwise wrap the
/// text in a sanitizer delimiter and hide the leading `/` from the agent's dispatch
/// registries. The actual authorization decision (whether the command may run for an
/// untrusted caller) still happens downstream in [`crate::CommandRegistry::dispatch`] via
/// [`crate::CommandHandler::requires_auth`]; this function only answers "is this text
/// shaped like a known command that defers to that gate", not "is it allowed to run".
///
/// # Examples
///
/// ```
/// use zeph_commands::is_recognized_command;
///
/// assert!(is_recognized_command("/status"));
/// assert!(is_recognized_command("/skill create a new skill"));
/// assert!(!is_recognized_command("/not-a-real-command"));
/// assert!(!is_recognized_command("hello, how are you?"));
/// // Dispatched outside the trust-gated registry — never treated as recognized.
/// assert!(!is_recognized_command("/subagent spawn zeph --acp"));
/// ```
#[must_use]
pub fn is_recognized_command(trimmed: &str) -> bool {
    if UNGATED_DISPATCH_COMMANDS
        .iter()
        .any(|name| matches_command_name(trimmed, name))
    {
        return false;
    }
    COMMANDS
        .iter()
        .any(|c| matches_command_name(trimmed, c.name))
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, UNGATED_DISPATCH_COMMANDS, is_recognized_command};

    #[test]
    fn recognizes_exact_and_arged_commands() {
        assert!(is_recognized_command("/status"));
        assert!(is_recognized_command("/skill create a widget"));
        assert!(is_recognized_command("/plan confirm"));
    }

    #[test]
    fn rejects_unknown_and_chat_text() {
        assert!(!is_recognized_command("/not-a-real-command"));
        assert!(!is_recognized_command("hello, how are you?"));
        assert!(!is_recognized_command("/statusfoo"));
    }

    #[test]
    fn does_not_fuzzy_match_a_prefix_without_boundary() {
        // "/status" is a real command; "/statuses" must not match it.
        assert!(!is_recognized_command("/statuses"));
    }

    /// #5904 CRITICAL-2 regression: `/subagent` is dispatched by `dispatch_slash_command`
    /// with no `trusted`/`requires_auth` check at all, so it must never be recognized here —
    /// otherwise an HTTP entry point would forward it raw and it would spawn an external
    /// process unconditionally, regardless of caller trust.
    #[test]
    fn excludes_ungated_dispatch_commands() {
        assert!(!is_recognized_command("/subagent"));
        assert!(!is_recognized_command("/subagent spawn zeph --acp"));
        assert!(!is_recognized_command("/subagent spawn evil-command"));
    }

    /// Every name in [`UNGATED_DISPATCH_COMMANDS`] must itself be a real, listed command —
    /// otherwise the exclusion is dead code silently guarding nothing (or, worse, a typo'd
    /// entry gives false confidence while the real ungated command slips through unexcluded).
    #[test]
    fn ungated_dispatch_commands_are_real_listed_commands() {
        for name in UNGATED_DISPATCH_COMMANDS {
            assert!(
                COMMANDS.iter().any(|c| c.name == *name),
                "{name} is in UNGATED_DISPATCH_COMMANDS but not in COMMANDS — stale entry?"
            );
        }
    }

    /// #5904 SIGNIFICANT-1: every `COMMANDS` entry not deliberately excluded via
    /// [`UNGATED_DISPATCH_COMMANDS`] must be recognized — this is a completeness guard so a
    /// future accidental over-broad addition to `UNGATED_DISPATCH_COMMANDS` (or a typo that
    /// silently fails to match any real command name) is caught immediately, rather than
    /// quietly regressing the #5898/#5904 fix for that command back to always-sanitized.
    #[test]
    fn every_non_excluded_command_is_recognized() {
        for c in COMMANDS {
            if UNGATED_DISPATCH_COMMANDS.contains(&c.name) {
                continue;
            }
            assert!(
                is_recognized_command(c.name),
                "{} is in COMMANDS but not recognized by is_recognized_command",
                c.name
            );
        }
    }
}
