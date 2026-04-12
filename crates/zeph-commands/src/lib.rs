// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Slash command registry, handler trait, and channel sink abstraction for Zeph.
//!
//! This crate provides the non-generic infrastructure for slash command dispatch:
//! - [`ChannelSink`] — minimal async I/O trait replacing the `C: Channel` generic in handlers
//! - [`CommandOutput`] — exhaustive result type for command execution
//! - [`SlashCategory`] — grouping enum for `/help` output
//! - [`CommandInfo`] — static metadata for a registered command
//! - [`CommandHandler`] — object-safe handler trait (no `C` generic)
//! - [`CommandRegistry`] — registry with longest-word-boundary dispatch
//! - [`CommandContext`] — non-generic dispatch context with trait-object fields
//! - [`traits`] — sub-trait definitions for subsystem access
//! - [`handlers`] — concrete handler implementations (session, debug)
//!
//! # Design
//!
//! `CommandRegistry` and `CommandHandler` are non-generic: they operate on [`CommandContext`],
//! a concrete struct whose fields are trait objects (`&mut dyn DebugAccess`, etc.). `zeph-core`
//! implements these traits on its internal state types and constructs `CommandContext` at dispatch
//! time from `Agent<C>` fields.
//!
//! This crate does NOT depend on `zeph-core`. A change in `zeph-core`'s agent loop does
//! not recompile `zeph-commands`.

pub mod context;
pub mod handlers;
pub mod sink;
pub mod traits;

pub use context::CommandContext;
pub use sink::{ChannelSink, NullSink};
pub use traits::agent::{AgentAccess, NullAgent};

use std::future::Future;
use std::pin::Pin;

/// Result of executing a slash command.
///
/// Replaces the heterogeneous return types of earlier command dispatch with a unified,
/// exhaustive enum.
#[derive(Debug)]
pub enum CommandOutput {
    /// Send a message to the user via the channel.
    Message(String),
    /// Command handled silently; no output (e.g., `/clear`).
    Silent,
    /// Exit the agent loop immediately.
    Exit,
    /// Continue to the next loop iteration.
    Continue,
}

/// Category for grouping commands in `/help` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCategory {
    /// Session management: `/clear`, `/reset`, `/exit`, etc.
    Session,
    /// Model and provider configuration: `/model`, `/provider`, `/guardrail`, etc.
    Configuration,
    /// Memory and knowledge: `/memory`, `/graph`, `/compact`, etc.
    Memory,
    /// Skill management: `/skill`, `/skills`, etc.
    Skills,
    /// Planning and focus: `/plan`, `/focus`, `/sidequest`, etc.
    Planning,
    /// Debugging and diagnostics: `/debug-dump`, `/log`, `/lsp`, etc.
    Debugging,
    /// External integrations: `/mcp`, `/image`, `/agent`, etc.
    Integration,
    /// Advanced and experimental: `/experiment`, `/policy`, `/scheduler`, etc.
    Advanced,
}

impl SlashCategory {
    /// Return the display label for this category in `/help` output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Configuration => "Configuration",
            Self::Memory => "Memory",
            Self::Skills => "Skills",
            Self::Planning => "Planning",
            Self::Debugging => "Debugging",
            Self::Integration => "Integration",
            Self::Advanced => "Advanced",
        }
    }
}

/// Static metadata about a registered command, used for `/help` output generation.
pub struct CommandInfo {
    /// Command name including the leading slash, e.g. `"/help"`.
    pub name: &'static str,
    /// Argument hint shown after the command name in help, e.g. `"[path]"`.
    pub args: &'static str,
    /// One-line description shown in `/help` output.
    pub description: &'static str,
    /// Category for grouping in `/help`.
    pub category: SlashCategory,
    /// Feature gate label, if this command is conditionally compiled.
    pub feature_gate: Option<&'static str>,
}

/// Error type returned by command handlers.
///
/// Wraps agent-level errors as a string to avoid depending on `zeph-core`'s `AgentError`.
/// `zeph-core` converts between `AgentError` and `CommandError` at the dispatch boundary.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CommandError(pub String);

impl CommandError {
    /// Create a `CommandError` from any displayable value.
    pub fn new(msg: impl std::fmt::Display) -> Self {
        Self(msg.to_string())
    }
}

/// A slash command handler that can be registered with [`CommandRegistry`].
///
/// Implementors must be `Send + Sync` because the registry is constructed at agent
/// initialization time and handlers may be invoked from async contexts.
///
/// # Object safety
///
/// The `handle` method uses `Pin<Box<dyn Future>>` instead of `async fn` to remain
/// object-safe, enabling the registry to store `Box<dyn CommandHandler<Ctx>>`. Slash
/// commands are user-initiated so the box allocation is negligible.
pub trait CommandHandler<Ctx: ?Sized>: Send + Sync {
    /// Command name including the leading slash, e.g. `"/help"`.
    ///
    /// Must be unique per registry. Used as the dispatch key.
    fn name(&self) -> &'static str;

    /// One-line description shown in `/help` output.
    fn description(&self) -> &'static str;

    /// Argument hint shown after the command name in help, e.g. `"[path]"`.
    ///
    /// Return an empty string if the command takes no arguments.
    fn args_hint(&self) -> &'static str {
        ""
    }

    /// Category for grouping in `/help`.
    fn category(&self) -> SlashCategory;

    /// Feature gate label, if this command is conditionally compiled.
    fn feature_gate(&self) -> Option<&'static str> {
        None
    }

    /// Execute the command.
    ///
    /// # Arguments
    ///
    /// - `ctx`: Typed access to agent subsystems.
    /// - `args`: Trimmed text after the command name. Empty string when no args given.
    ///
    /// # Errors
    ///
    /// Returns `Err(CommandError)` when the command fails. The dispatch site logs and
    /// reports the error to the user.
    fn handle<'a>(
        &'a self,
        ctx: &'a mut Ctx,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>>;
}

/// Registry of slash command handlers.
///
/// Handlers are stored in a `Vec`, not a `HashMap`, because command count is small (< 40)
/// and registration happens once at agent initialization. Dispatch performs a linear scan
/// with longest-word-boundary match to support subcommands.
///
/// # Dispatch
///
/// See [`CommandRegistry::dispatch`] for the full dispatch algorithm.
///
/// # Borrow splitting
///
/// When stored as an `Agent<C>` field, the dispatch call site uses `std::mem::take` to
/// temporarily move the registry out of the agent, construct a context, dispatch, and
/// restore the registry. This avoids borrow-checker conflicts.
pub struct CommandRegistry<Ctx: ?Sized> {
    handlers: Vec<Box<dyn CommandHandler<Ctx>>>,
}

impl<Ctx: ?Sized> CommandRegistry<Ctx> {
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
    /// Panics if a handler with the same name is already registered.
    pub fn register(&mut self, handler: impl CommandHandler<Ctx> + 'static) {
        let name = handler.name();
        assert!(
            !self.handlers.iter().any(|h| h.name() == name),
            "duplicate command name: {name}"
        );
        self.handlers.push(Box::new(handler));
    }

    /// Dispatch a command string to the matching handler.
    ///
    /// Returns `None` if the input does not start with `/` or no handler matches.
    ///
    /// # Algorithm
    ///
    /// 1. Return `None` if `input` does not start with `/`.
    /// 2. Find all handlers where `input == name` or `input.starts_with(name + " ")`.
    /// 3. Pick the handler with the longest matching name (subcommand resolution).
    /// 4. Extract `args = input[name.len()..].trim()`.
    /// 5. Call `handler.handle(ctx, args)` and return the result.
    ///
    /// # Errors
    ///
    /// Returns `Some(Err(_))` when the matched handler returns an error.
    pub async fn dispatch(
        &self,
        ctx: &mut Ctx,
        input: &str,
    ) -> Option<Result<CommandOutput, CommandError>> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }

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
    /// Returns `Some((idx, name))` or `None` if no handler matches.
    /// Primarily used in tests to verify routing.
    #[must_use]
    pub fn find_handler(&self, input: &str) -> Option<(usize, &'static str)> {
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
    #[must_use]
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

impl<Ctx: ?Sized> Default for CommandRegistry<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    struct MockCtx;

    struct FixedHandler {
        name: &'static str,
        category: SlashCategory,
    }

    impl CommandHandler<MockCtx> for FixedHandler {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            "test handler"
        }

        fn category(&self) -> SlashCategory {
            self.category
        }

        fn handle<'a>(
            &'a self,
            _ctx: &'a mut MockCtx,
            args: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>>
        {
            let name = self.name;
            Box::pin(async move { Ok(CommandOutput::Message(format!("{name}:{args}"))) })
        }
    }

    fn make_handler(name: &'static str) -> FixedHandler {
        FixedHandler {
            name,
            category: SlashCategory::Session,
        }
    }

    #[tokio::test]
    async fn dispatch_routes_longest_match() {
        let mut reg: CommandRegistry<MockCtx> = CommandRegistry::new();
        reg.register(make_handler("/plan"));
        reg.register(make_handler("/plan confirm"));

        let mut ctx = MockCtx;
        let out = reg
            .dispatch(&mut ctx, "/plan confirm foo")
            .await
            .unwrap()
            .unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message");
        };
        assert_eq!(msg, "/plan confirm:foo");
    }

    #[tokio::test]
    async fn dispatch_returns_none_for_non_slash() {
        let mut reg: CommandRegistry<MockCtx> = CommandRegistry::new();
        reg.register(make_handler("/help"));
        let mut ctx = MockCtx;
        assert!(reg.dispatch(&mut ctx, "hello").await.is_none());
    }

    #[tokio::test]
    async fn dispatch_returns_none_for_unregistered() {
        let mut reg: CommandRegistry<MockCtx> = CommandRegistry::new();
        reg.register(make_handler("/help"));
        let mut ctx = MockCtx;
        assert!(reg.dispatch(&mut ctx, "/unknown").await.is_none());
    }

    #[test]
    #[should_panic(expected = "duplicate command name")]
    fn register_panics_on_duplicate() {
        let mut reg: CommandRegistry<MockCtx> = CommandRegistry::new();
        reg.register(make_handler("/plan"));
        reg.register(make_handler("/plan"));
    }

    #[test]
    fn list_returns_metadata_in_order() {
        let mut reg: CommandRegistry<MockCtx> = CommandRegistry::new();
        reg.register(make_handler("/alpha"));
        reg.register(make_handler("/beta"));
        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "/alpha");
        assert_eq!(list[1].name, "/beta");
    }

    #[test]
    fn slash_category_as_str_all_variants() {
        let variants = [
            (SlashCategory::Session, "Session"),
            (SlashCategory::Configuration, "Configuration"),
            (SlashCategory::Memory, "Memory"),
            (SlashCategory::Skills, "Skills"),
            (SlashCategory::Planning, "Planning"),
            (SlashCategory::Debugging, "Debugging"),
            (SlashCategory::Integration, "Integration"),
            (SlashCategory::Advanced, "Advanced"),
        ];
        for (variant, expected) in variants {
            assert_eq!(variant.as_str(), expected);
        }
    }
}
