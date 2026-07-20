// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`GraphAccess`] — command handler access to the graph memory store and the
//! knowledge-ingest ledger.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to graph memory (entities, edges, communities, backfill) and the knowledge-ingest
/// ledger.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait GraphAccess {
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

    // ----- /knowledge -----

    /// Return a formatted summary of the ingest ledger (batches, counts).
    ///
    /// # Errors
    ///
    /// Returns `Err` when the database query fails.
    fn knowledge_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    /// Roll back a graph import batch by `batch_id`.
    ///
    /// Deletes edges, orphaned entities, and ledger rows for the batch.
    /// Returns a summary line on success, or an error message if the batch is unknown.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the database query fails or the batch does not exist.
    fn knowledge_rollback<'a>(
        &'a mut self,
        batch_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;
}
