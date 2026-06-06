// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Persistence subsystem for [`crate::agent::Agent`].
//!
//! The heavy lifting lives in the `zeph-agent-persistence` crate; the methods here are thin
//! shims that bridge that crate's borrow-lens views to agent-internal singletons (session
//! counts, token recompute, metrics broadcast) which fall outside the lens.
//!
//! The implementation is split by concern across focused submodules, each contributing methods
//! to the same `impl<C: Channel> Agent<C>` block:
//!
//! - [`history`] — load conversation history with post-load agent mutations.
//! - [`store`] — persist a message and schedule summarization / `MemCoT` distillation.
//! - [`extract`] — enqueue background graph, persona, trajectory, and reasoning extraction.

mod extract;
mod history;
mod store;

#[cfg(test)]
mod tests;
