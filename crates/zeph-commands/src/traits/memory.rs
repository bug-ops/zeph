// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`MemoryAccess`] — command handler access to the episodic/semantic memory tiers,
//! the cross-thread key-value store, and compression guidelines.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to memory-tier statistics, promotion, the cross-thread store, and compression
/// guidelines.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait MemoryAccess {
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

    // ----- /store -----

    /// Handle `/store {get,put,list,delete}` against the cross-thread key-value store
    /// (spec-080, #6363, FR-A-011).
    ///
    /// `args` is the raw text after `/store`, e.g. `"get orch/graph-1 finding"`. Returns a
    /// disabled/usage message (not an `Err`) when the store is disabled, no memory handle is
    /// configured, or the subcommand/arguments are malformed — only a real database failure
    /// is surfaced as `Err`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the underlying database query fails.
    fn store_command<'a>(
        &'a mut self,
        args: &'a str,
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
}
