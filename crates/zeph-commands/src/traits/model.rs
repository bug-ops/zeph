// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ModelAccess`] — command handler access to the active LLM provider and model
//! selection, thinking-token budget, reasoning effort, and caveman mode.

use std::future::Future;
use std::pin::Pin;

/// Access to model/provider selection and per-provider generation parameters.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait ModelAccess {
    // ----- /caveman -----

    /// Handle `/caveman [on|off|status]` and return a user-visible result.
    ///
    /// - `""` — toggle current state.
    /// - `"on"` / `"enable"` — activate ultra-compressed output.
    /// - `"off"` / `"disable"` — deactivate ultra-compressed output.
    /// - `"status"` — report current state without changing it.
    ///
    /// Returns a one-line confirmation string (e.g. `"caveman: on"`).
    fn handle_caveman<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;

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

    // ----- /think-tokens, /reasoning-effort -----

    /// Handle `/think-tokens [N|Nk|NM|off]` and return a user-visible result.
    ///
    /// Empty `arg` displays the active provider's current thinking-token budget. A non-empty
    /// `arg` parses and applies a new budget (or disables thinking on `0`/`off`). Session-only:
    /// never persisted. Returns a "not supported by provider X" message for providers that
    /// do not support a thinking-token budget.
    fn handle_think_tokens<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;

    /// Handle `/reasoning-effort [low|medium|high]` and return a user-visible result.
    ///
    /// Empty `arg` displays the active provider's current reasoning-effort level. A non-empty
    /// `arg` parses and applies a new level. Session-only: never persisted. Returns a "not
    /// supported by provider X" message for providers that do not support a reasoning-effort
    /// level.
    fn handle_reasoning_effort<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;
}
