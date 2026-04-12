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

    // Note: /skill, /skills, /feedback are handled via handle_builtin_command in zeph-core
    // because their implementations hold non-Send references (&SemanticMemory, &AnyProvider)
    // across .await points. Adding them here would require those types to be Sync, which is a
    // broader change. They remain as TODO for a future migration phase.

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
}
