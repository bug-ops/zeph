// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`LspAccess`] — command handler access to LSP context-injection status.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to the `/lsp` command (LSP context-injection status).
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait LspAccess {
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
