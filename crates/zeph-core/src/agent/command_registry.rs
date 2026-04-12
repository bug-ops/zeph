// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trait-based registry for slash command handlers.
//!
//! Replaces the hand-written `if`-chain dispatch in `handle_builtin_command` and
//! `dispatch_slash_command` with a unified `CommandRegistry` that supports `register`,
//! `dispatch`, and `list`. Handlers are registered once at agent initialization and
//! dispatched via longest-word-boundary matching.
//!
//! # Phase 1 scope
//!
//! The registry infrastructure and Batch 1 (trivial, self-contained) commands are implemented
//! here. Batch 2 and Batch 3 commands continue to dispatch through the existing `Agent<C>`
//! methods as a fallback until they are migrated in a follow-up PR.
//!
//! # Dispatch algorithm
//!
//! 1. Input must start with `/` to be considered a slash command.
//! 2. All registered handlers whose `name()` exactly matches the input, or whose `name()` is a
//!    word-boundary prefix of the input (i.e., `input.starts_with(name + " ")`), are collected.
//! 3. The handler with the longest matching name wins (subcommand resolution: `/plan confirm`
//!    beats `/plan`).
//! 4. `args` is extracted as `input[name.len()..].trim()`.
//! 5. Returns `None` when no handler matches, signalling the caller to fall through to the
//!    existing dispatch logic or LLM processing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_tools::executor::ErasedToolExecutor;

use crate::channel::Channel;

use super::context_manager;
use super::error::AgentError;
use super::focus;
use super::learning_engine;
use super::sidequest;
use super::state::{
    CompressionState, DebugState, ExperimentState, FeedbackState, IndexState, LifecycleState,
    McpState, MemoryState, MessageState, MetricsState, OrchestrationState, ProviderState,
    RuntimeConfig, SecurityState, SkillState, ToolState,
};
use super::tool_orchestrator;

// Re-export shared types from zeph-commands to avoid duplication.
pub(crate) use zeph_commands::{CommandInfo, CommandOutput, SlashCategory};

/// Typed access to agent subsystems needed by command handlers.
///
/// Constructed from `&mut Agent<C>` at dispatch time. Provides references to the subsystems
/// that commands need, without requiring direct access to the full `Agent<C>` struct.
///
/// # Phase 1 design note
///
/// `CommandContext` contains references to all major agent subsystems because handlers in
/// Phase 1 may have heterogeneous dependencies. Phase 2 will decompose this into narrower
/// per-handler context types. This is a transitional design that enables Batch 2/3 delegation.
///
/// # Lifetime
///
/// The lifetime `'a` ties all references to the dispatch scope. A `CommandContext` must not
/// outlive the `&mut Agent<C>` it was constructed from.
// Fields used by handler implementations in commands/; not all read at link-time.
#[allow(dead_code)]
pub(crate) struct CommandContext<'a, C: Channel> {
    /// I/O channel for sending responses to the user.
    pub channel: &'a mut C,
    /// Active LLM provider.
    pub provider: &'a mut AnyProvider,
    /// Dedicated embedding provider (never replaced by `/provider switch`).
    pub embedding_provider: &'a AnyProvider,
    /// Conversation message history and pending image parts.
    pub msg: &'a mut MessageState,
    /// Memory subsystems: persistence, compaction, extraction, subsystems.
    pub memory_state: &'a mut MemoryState,
    /// Skill registry, matcher, and self-learning state.
    pub skill_state: &'a mut SkillState,
    /// Context budgeting and summarization.
    pub context_manager: &'a mut context_manager::ContextManager,
    /// DAG-based tool execution with caching.
    pub tool_orchestrator: &'a mut tool_orchestrator::ToolOrchestrator,
    /// MCP server lifecycle and tool registry.
    pub mcp: &'a mut McpState,
    /// AST-based code index (read-only during command dispatch).
    pub index: &'a IndexState,
    /// Debug state: debug dumper, dump format, logging config, trace collector.
    pub debug_state: &'a mut DebugState,
    /// Runtime configuration: model name, adversarial policy info, etc.
    pub runtime: &'a mut RuntimeConfig,
    /// Session metrics snapshot (read-only during command dispatch).
    pub metrics: &'a MetricsState,
    /// Security state: guardrail, content sanitizer, URL tracking.
    pub security: &'a mut SecurityState,
    /// Orchestration state: sub-agent manager, orchestration metrics.
    pub orchestration: &'a mut OrchestrationState,
    /// Session lifecycle: cancel token, shutdown signal, start time.
    pub lifecycle: &'a mut LifecycleState,
    /// Focus Agent state (read-only during command dispatch).
    pub focus: &'a focus::FocusState,
    /// `SideQuest` state (read-only during command dispatch).
    pub sidequest: &'a sidequest::SidequestState,
    /// Provider registry for multi-model routing.
    pub providers: &'a mut ProviderState,
    /// Compression and subgoal registry state.
    pub compression: &'a mut CompressionState,
    /// Tool executor for shell, web, and custom tools.
    pub tool_executor: &'a Arc<dyn ErasedToolExecutor>,
    /// Experimental feature state.
    pub experiments: &'a mut ExperimentState,
    /// Feedback and correction tracking.
    pub feedback: &'a mut FeedbackState,
    /// Self-learning and skill evolution engine.
    pub learning_engine: &'a mut learning_engine::LearningEngine,
    /// Tool filtering, dependency tracking, and iteration bookkeeping.
    pub tool_state: &'a mut ToolState,
}

/// A slash command handler that can be registered with the [`CommandRegistry`].
///
/// Implementors must be `Send + Sync` because the registry is constructed at agent
/// initialization time and handlers may be invoked from async contexts.
///
/// # Object safety
///
/// The `handle` method uses `Pin<Box<dyn Future>>` instead of `async fn` to remain
/// object-safe, enabling the registry to store `Box<dyn CommandHandler<C>>`. Slash commands
/// are user-initiated, so the box allocation is negligible.
// Trait methods called through dynamic dispatch; dead_code lint cannot track dyn usage.
#[allow(dead_code)]
pub(crate) trait CommandHandler<C: Channel>: Send + Sync {
    /// Command name including the leading slash, e.g. `"/help"`.
    ///
    /// Must be unique per registry. Used as the dispatch key.
    fn name(&self) -> &'static str;

    /// One-line description shown in `/help` output.
    fn description(&self) -> &'static str;

    /// Argument hint shown after the command name in help, e.g. `"[path]"`.
    ///
    /// Return empty string if the command takes no arguments.
    fn args_hint(&self) -> &'static str {
        ""
    }

    /// Category for grouping in `/help`.
    fn category(&self) -> SlashCategory;

    /// Feature gate label, if this command is conditionally compiled.
    ///
    /// If `Some("feature_name")`, the command is only available when compiled with
    /// `--features feature_name`.
    fn feature_gate(&self) -> Option<&'static str> {
        None
    }

    /// Execute the command.
    ///
    /// # Arguments
    ///
    /// - `ctx`: Typed access to agent subsystems.
    /// - `args`: Trimmed text after the command name. For `/model gpt-4`, args is `"gpt-4"`.
    ///   For commands that take no arguments, args is an empty string.
    ///
    /// # Errors
    ///
    /// Returns `Err(AgentError)` when the command fails (I/O error, invalid args, etc.).
    /// The error is logged and reported to the user by the dispatch site.
    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_, C>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, AgentError>> + Send + 'a>>;
}

/// Registry of slash command handlers.
///
/// Handlers are stored in a `Vec`, not a `HashMap`, because command count is small (< 40)
/// and registration happens once at agent initialization. Dispatch performs a linear scan
/// with longest-word-boundary match to support subcommands (e.g., `/plan confirm`, `/skill create`).
///
/// # Dispatch
///
/// See [`CommandRegistry::dispatch`] for the full dispatch algorithm.
///
/// # Borrow splitting
///
/// When stored as an `Agent<C>` field, the dispatch call site uses `std::mem::take` to
/// temporarily move the registry out of the agent, construct a `CommandContext`, dispatch,
/// and restore the registry. This avoids borrow-checker conflicts between `&self.command_registry`
/// and `&mut self.channel`, `&mut self.msg`, etc.
///
/// ```rust,ignore
/// let mut registry = std::mem::take(&mut self.command_registry);
/// let result = registry.dispatch(&mut ctx, input).await;
/// self.command_registry = registry;
/// ```
///
/// # Panic safety
///
/// If a handler panics, `std::mem::take` leaves an empty registry for the rest of the session.
/// Commands will fall through to the legacy dispatcher. This is a known Phase 1 limitation.
pub(crate) struct CommandRegistry<C: Channel> {
    handlers: Vec<Box<dyn CommandHandler<C>>>,
}

impl<C: Channel> CommandRegistry<C> {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// Register a command handler.
    ///
    /// # Panics
    ///
    /// Panics if a handler with the same name is already registered. Duplicate names indicate
    /// a programming error (two handlers claiming the same command slot).
    pub fn register(&mut self, handler: impl CommandHandler<C> + 'static) {
        let name = handler.name();
        assert!(
            !self.handlers.iter().any(|h| h.name() == name),
            "duplicate command name: {name}"
        );
        self.handlers.push(Box::new(handler));
    }

    /// Dispatch a command string to the matching handler.
    ///
    /// # Algorithm
    ///
    /// 1. Return `None` if `input` does not start with `/`.
    /// 2. Find all handlers where `input == name` or `input.starts_with(name + " ")`.
    /// 3. Pick the handler with the longest matching name (subcommand resolution).
    /// 4. Extract `args = input[name.len()..].trim()`.
    /// 5. Call `handler.handle(ctx, args)` and return the result.
    /// 6. Return `None` if no handler matches (caller should fall through to existing dispatch).
    ///
    /// # Word-boundary matching
    ///
    /// Uses exact word-boundary matching: a match occurs if and only if
    /// `input == name` OR `input.starts_with(name + " ")`. This prevents `/skillset` from
    /// matching `/skill`.
    ///
    /// # Errors
    ///
    /// Returns `Some(Err(_))` when the matched handler returns an error.
    pub async fn dispatch(
        &self,
        ctx: &mut CommandContext<'_, C>,
        input: &str,
    ) -> Option<Result<CommandOutput, AgentError>> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }

        // Collect all matching handlers with their match length.
        let mut best_len: usize = 0;
        let mut best_idx: Option<usize> = None;
        for (idx, handler) in self.handlers.iter().enumerate() {
            let name = handler.name();
            let matched = trimmed == name
                || trimmed
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with(' '));
            if matched && name.len() >= best_len {
                best_len = name.len();
                best_idx = Some(idx);
            }
        }

        let handler = &self.handlers[best_idx?];
        let name = handler.name();
        let args = trimmed[name.len()..].trim();
        Some(handler.handle(ctx, args).await)
    }

    /// Find the handler that would be selected for the given input, without dispatching.
    ///
    /// Returns `Some((idx, name))` with the index into the handler list and the matched command
    /// name, or `None` if no handler matches. Uses the same word-boundary algorithm as
    /// [`Self::dispatch`].
    ///
    /// Primarily used in tests to verify routing without constructing a [`CommandContext`].
    // Used in tests; production dispatch goes through Self::dispatch.
    #[allow(dead_code)]
    pub(crate) fn find_handler(&self, input: &str) -> Option<(usize, &'static str)> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let mut best_len: usize = 0;
        let mut best: Option<(usize, &'static str)> = None;
        for (idx, handler) in self.handlers.iter().enumerate() {
            let name = handler.name();
            let matched = trimmed == name
                || trimmed
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with(' '));
            if matched && name.len() >= best_len {
                best_len = name.len();
                best = Some((idx, name));
            }
        }
        best
    }

    /// List all registered commands for `/help` generation.
    ///
    /// Returns metadata in registration order.
    // Called by future /help handler; not yet wired in Phase 1.
    #[allow(dead_code)]
    pub fn list(&self) -> Vec<CommandInfo> {
        self.handlers
            .iter()
            .map(|h| CommandInfo {
                name: h.name(),
                args: h.args_hint(),
                description: h.description(),
                category: h.category(),
                feature_gate: h.feature_gate(),
            })
            .collect()
    }
}

impl<C: Channel> Default for CommandRegistry<C> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{Channel, ChannelError, ChannelMessage};

    // --- Minimal stub channel for testing ---

    struct StubChannel;

    impl Channel for StubChannel {
        async fn send(&mut self, _message: &str) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
            Ok(None)
        }

        fn supports_exit(&self) -> bool {
            true
        }

        async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    // --- Test handlers (name-only, no ctx access) ---

    struct NamedHandler {
        name: &'static str,
    }

    impl CommandHandler<StubChannel> for NamedHandler {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "test handler"
        }
        fn category(&self) -> SlashCategory {
            SlashCategory::Debugging
        }
        fn handle<'a>(
            &'a self,
            _ctx: &'a mut CommandContext<'_, StubChannel>,
            _args: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, AgentError>> + Send + 'a>> {
            let name = self.name;
            Box::pin(async move { Ok(CommandOutput::Message(name.to_owned())) })
        }
    }

    // --- list() tests ---

    #[test]
    fn list_returns_registered_entries() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/ping" });
        reg.register(NamedHandler { name: "/exit" });

        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "/ping");
        assert_eq!(list[1].name, "/exit");
    }

    #[test]
    fn list_empty_when_no_handlers() {
        let reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        assert!(reg.list().is_empty());
    }

    // --- dispatch word-boundary tests (via find_handler) ---
    // These tests verify the matching algorithm without needing a CommandContext.

    #[test]
    fn find_handler_exact_match() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/ping" });
        assert!(reg.find_handler("/ping").is_some());
    }

    #[test]
    fn find_handler_no_spurious_prefix_match() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/ping" });
        // /pingpong must NOT match /ping
        assert!(reg.find_handler("/pingpong").is_none());
    }

    #[test]
    fn find_handler_word_boundary_with_args() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/ping" });
        // "/ping foo" should match /ping
        assert!(reg.find_handler("/ping foo").is_some());
    }

    #[test]
    fn find_handler_subcommand_picks_longest_match() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/ping" });
        reg.register(NamedHandler {
            name: "/ping verbose",
        });
        let (_, name) = reg.find_handler("/ping verbose").unwrap();
        assert_eq!(name, "/ping verbose", "longer match must win");
    }

    #[test]
    fn find_handler_none_for_non_slash() {
        let reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        assert!(reg.find_handler("hello").is_none());
    }

    #[test]
    fn find_handler_none_for_unknown_slash_command() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/ping" });
        assert!(reg.find_handler("/unknown").is_none());
    }

    // --- /skill does NOT match /skillset ---

    #[test]
    fn find_handler_skill_does_not_match_skillset() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/skill" });
        assert!(
            reg.find_handler("/skillset").is_none(),
            "/skillset must not match /skill"
        );
    }

    // --- register duplicate panic test ---

    #[test]
    #[should_panic(expected = "duplicate command name: /ping")]
    fn register_duplicate_panics() {
        let mut reg: CommandRegistry<StubChannel> = CommandRegistry::new();
        reg.register(NamedHandler { name: "/ping" });
        reg.register(NamedHandler { name: "/ping" }); // second registration must panic
    }
}
