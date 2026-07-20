// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`IntegrationAccess`] — command handler access to external-integration subsystems:
//! the ACP server, the Cocoon sidecar, and the plugin manager.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to `/acp`, `/cocoon`, and `/plugins`.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait IntegrationAccess {
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

    // ----- /cocoon -----

    /// Handle `/cocoon [status|models]` and return a user-visible result.
    ///
    /// Queries the Cocoon sidecar HTTP endpoints and returns formatted status or model listing.
    /// No subcommand or empty args returns a short help text.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the sidecar is unreachable or an unknown subcommand is passed.
    fn handle_cocoon<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;
}
