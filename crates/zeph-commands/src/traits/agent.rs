// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`AgentAccess`] — a single dispatch trait that bridges `zeph-commands` handlers to
//! `zeph-core` subsystems that cannot be decomposed into smaller trait objects without
//! borrow-checker conflicts.
//!
//! ## Design rationale
//!
//! Commands like `/graph`, `/skill`, `/model`, `/policy`, and `/scheduler` access 10–20 internal
//! `Agent<C>` fields simultaneously. Decomposing each into a separate trait object field on
//! [`CommandContext`] would require splitting those fields from `&mut self.channel` (already
//! held by `ctx.sink`), which the borrow checker cannot express with safe Rust.
//!
//! The solution: one fat trait whose methods delegate to the existing `Agent<C>` methods.
//! The trait is object-safe because every method returns `Pin<Box<dyn Future + Send>>`.
//!
//! ## Implementors
//!
//! `zeph-core::agent::Agent<C>` implements `AgentAccess` in `command_context_impls.rs`.
//!
//! [`CommandContext`]: crate::context::CommandContext

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Broad access to agent subsystems for command handlers that cannot be served by
/// individual sub-traits.
///
/// Implemented by `zeph-core::Agent<C>`. Each method corresponds to one family of slash
/// commands that require access to multiple agent fields simultaneously.
///
/// All methods return `Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>`
/// for object safety — allowing `Box<dyn AgentAccess>` storage in [`CommandContext`].
///
/// [`CommandContext`]: crate::context::CommandContext
pub trait AgentAccess: Send {
    // ----- /memory -----

    /// Return formatted memory tier statistics.
    ///
    /// Used by `/memory` and `/memory tiers`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the database query fails.
    fn memory_tiers<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    /// Promote message IDs to the semantic tier.
    ///
    /// `ids_str` is a whitespace-separated list of integer IDs.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the database operation fails.
    fn memory_promote<'a>(
        &'a mut self,
        ids_str: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /graph -----

    /// Return graph memory statistics (entity/edge/community counts).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the graph store query fails.
    fn graph_stats<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    /// Return the list of all graph entities (up to 50).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the graph store query fails.
    fn graph_entities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    /// Return facts for the entity matching `name`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the graph store query fails.
    fn graph_facts<'a>(
        &'a mut self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    /// Return edge history for the entity matching `name`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the graph store query fails.
    fn graph_history<'a>(
        &'a mut self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    /// Return the list of detected graph communities.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the graph store query fails.
    fn graph_communities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    /// Run graph backfill, calling `progress_cb` for each progress update.
    ///
    /// Returns the final completion message.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the backfill operation fails.
    fn graph_backfill<'a>(
        &'a mut self,
        limit: Option<usize>,
        progress_cb: &'a mut (dyn FnMut(String) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /guidelines -----

    /// Return the current compression guidelines.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the database query fails.
    fn guidelines<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /model, /provider -----

    /// Handle `/model [arg]` and return a user-visible result.
    fn handle_model<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;

    /// Handle `/provider [arg]` and return a user-visible result.
    fn handle_provider<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;

    // ----- /skill -----

    /// Handle `/skill [subcommand]` and return a user-visible result.
    ///
    /// Subcommands: `stats`, `versions`, `activate`, `approve`, `reset`, `trust`,
    /// `block`, `unblock`, `install`, `remove`, `create`, `scan`, `reject`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a database or I/O operation fails.
    fn handle_skill<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /skills -----

    /// Handle `/skills [subcommand]` and return a user-visible result.
    ///
    /// Subcommands: (none) list all; `confusability` show pairs with high embedding similarity.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a database or embedding operation fails.
    fn handle_skills<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /feedback -----

    /// Handle `/feedback <skill_name> <message>` and return a user-visible result.
    ///
    /// Records skill outcome feedback and optionally triggers skill improvement.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the database operation fails.
    fn handle_feedback_command<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /policy -----

    /// Handle `/policy [status|check ...]` and return a user-visible result.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the policy is misconfigured or the subcommand is unknown.
    fn handle_policy<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /scheduler -----

    /// List scheduled tasks.
    ///
    /// Returns `None` when the scheduler is not enabled.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the tool executor call fails.
    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>>;

    // ----- /lsp -----

    /// Return formatted LSP status.
    ///
    /// # Errors
    ///
    /// Returns `Err` on failure (should not normally occur).
    fn lsp_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /recap -----

    /// Produce the session recap text.
    ///
    /// Returns the cached digest when available, otherwise generates a fresh summary of the
    /// current conversation. Non-fatal: on LLM timeout or error the implementor returns a
    /// user-visible message rather than `Err`.
    ///
    /// # Errors
    ///
    /// Returns `Err` only on unrecoverable internal agent errors.
    fn session_recap<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /compact -----

    /// Compact the context window and return a user-visible status string.
    ///
    /// Delegates to the agent's compaction subsystem. Returns a message describing
    /// whether compaction ran, was rejected by the probe, or there was nothing to compact.
    ///
    /// # Errors
    ///
    /// Returns `Err` when an internal agent error occurs.
    fn compact_context<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /new -----

    /// Start a new conversation and return a user-visible status string.
    ///
    /// `keep_plan` preserves the current plan. `no_digest` skips saving a digest of
    /// the previous conversation. Returns a formatted string with old and new session IDs.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the reset operation fails.
    fn reset_conversation<'a>(
        &'a mut self,
        keep_plan: bool,
        no_digest: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /cache-stats -----

    /// Return formatted tool orchestrator cache statistics.
    fn cache_stats(&self) -> String;

    // ----- /status -----

    /// Return a formatted session status string.
    ///
    /// # Errors
    ///
    /// Returns `Err` when an internal agent error occurs.
    fn session_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /guardrail -----

    /// Return formatted guardrail status.
    fn guardrail_status(&self) -> String;

    // ----- /focus -----

    /// Return formatted Focus Agent status.
    fn focus_status(&self) -> String;

    // ----- /sidequest -----

    /// Return formatted `SideQuest` eviction stats.
    fn sidequest_status(&self) -> String;

    // ----- /image -----

    /// Load an image from `path` and enqueue it for the next message.
    ///
    /// Returns a user-visible confirmation or error string.
    ///
    /// # Errors
    ///
    /// Returns `Err` when an internal agent error occurs.
    fn load_image<'a>(
        &'a mut self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /mcp -----

    /// Handle `/mcp [add|list|tools|remove]` and send output via the agent channel.
    ///
    /// Returns `Ok(())` on success. Intermediate messages are sent directly by the
    /// `Agent<C>` implementation via `self.channel`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or MCP operation fails.
    fn handle_mcp<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /plan -----

    /// Dispatch a `/plan` command and send output via the agent channel.
    ///
    /// `input` is the full trimmed command string (e.g. `"/plan status"`).
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or orchestration error occurs.
    fn handle_plan<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /experiment -----

    /// Dispatch a `/experiment` command and send output via the agent channel.
    ///
    /// `input` is the full trimmed command string (e.g. `"/experiment start"`).
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or experiment operation fails.
    fn handle_experiment<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /agent, @mention -----

    /// Dispatch a `/agent` or `@mention` command and return an optional response string.
    ///
    /// `input` is the full trimmed command string. Returns `Ok(None)` when no agent
    /// matched an `@mention` (caller should fall through to LLM processing).
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or subagent operation fails.
    fn handle_agent_dispatch<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>>;

    // ----- /plugins -----

    /// Handle `/plugins [subcommand] [args]` and return a user-visible result.
    ///
    /// Subcommands: `list`, `add <source>`, `remove <name>`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a plugin operation fails.
    fn handle_plugins<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /acp -----

    /// Handle `/acp [dirs|auth-methods|status]` and return a user-visible result.
    ///
    /// Subcommands: `dirs` (`additional_directories` allowlist), `auth-methods`, `status`.
    /// No subcommand or empty args returns a short help text.
    ///
    /// # Errors
    ///
    /// Returns `Err` when an unknown subcommand is passed.
    fn handle_acp<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /loop -----

    /// Handle `/loop <prompt> every <N> <unit>` or `/loop stop`.
    ///
    /// Starts a repeating loop that injects `prompt` as a new agent turn on each tick,
    /// or stops the currently active loop. Returns a user-visible ACK string.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the arguments are malformed or the interval is below the minimum.
    fn handle_loop<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /notify-test -----

    /// Fire a test notification via all enabled notification channels.
    ///
    /// Returns a status message for the user. If all channels are disabled or the
    /// notifier is not configured, returns a user-visible explanation.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the notification send fails.
    fn notify_test<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /trajectory -----

    /// Handle `/trajectory [status|reset]` and return a user-visible result.
    fn handle_trajectory(&mut self, args: &str) -> String;

    // ----- /scope -----

    /// Handle `/scope [list [task_type]]` and return a user-visible result.
    fn handle_scope(&self, args: &str) -> String;

    // ----- /goal -----

    /// Execute a `/goal` subcommand (create, pause, resume, clear, complete, status, list).
    ///
    /// `args` contains everything after `/goal` (e.g., `"create buy groceries"`).
    ///
    /// Returns a formatted response string on success, or an error message string.
    /// The default implementation returns an error indicating that goals are not supported,
    /// which is the correct behaviour for contexts where goal tracking is not wired in.
    fn handle_goal<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = args;
        Box::pin(async move { Err(CommandError::new("/goal is not supported in this context")) })
    }

    /// Return a lightweight snapshot of the currently active goal, if any.
    ///
    /// Used by the TUI status bar and metrics bridge. The default returns `None`.
    fn active_goal_snapshot(&self) -> Option<crate::GoalSnapshot> {
        None
    }
}

/// A no-op [`AgentAccess`] implementation.
///
/// Used when constructing a [`crate::CommandContext`] for a dispatch block that does not invoke
/// any agent-access commands (e.g., the session/debug-only registry block in `Agent::run`).
/// Allows the borrow checker to accept a split borrow: `sink` holds `&mut channel` while
/// `agent` holds this zero-size sentinel instead of `&mut self`.
pub struct NullAgent;

impl AgentAccess for NullAgent {
    fn memory_tiers<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn memory_promote<'a>(
        &'a mut self,
        _ids_str: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_stats<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_entities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_facts<'a>(
        &'a mut self,
        _name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_history<'a>(
        &'a mut self,
        _name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_communities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn graph_backfill<'a>(
        &'a mut self,
        _limit: Option<usize>,
        _progress_cb: &'a mut (dyn FnMut(String) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn guidelines<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_model<'a>(
        &'a mut self,
        _arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn handle_provider<'a>(
        &'a mut self,
        _arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn handle_skill<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_skills<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_feedback_command<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_policy<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(None) })
    }

    fn lsp_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn session_recap<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn compact_context<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn reset_conversation<'a>(
        &'a mut self,
        _keep_plan: bool,
        _no_digest: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn cache_stats(&self) -> String {
        String::new()
    }

    fn session_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn guardrail_status(&self) -> String {
        String::new()
    }

    fn focus_status(&self) -> String {
        String::new()
    }

    fn sidequest_status(&self) -> String {
        String::new()
    }

    fn load_image<'a>(
        &'a mut self,
        _path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_mcp<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_plan<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_experiment<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_agent_dispatch<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(None) })
    }

    fn handle_plugins<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_acp<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn handle_loop<'a>(
        &'a mut self,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(String::new()) })
    }

    /// Fire a test notification via all enabled notification channels.
    ///
    /// Returns a status message for the user. If all channels are disabled or the
    /// notifier is not configured, returns a user-visible explanation.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the notification send fails.
    fn notify_test<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok("Notifications not configured.".to_owned()) })
    }

    fn handle_trajectory(&mut self, _args: &str) -> String {
        String::new()
    }

    fn handle_scope(&self, _args: &str) -> String {
        String::new()
    }
}
