// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub(super) mod assembler;
mod assembly;
mod summarization;

#[cfg(test)]
use super::Message;
use super::{Agent, Channel};

pub(super) use crate::text::truncate_to_chars as truncate_chars;
pub(super) use zeph_context::slot::{cap_summary, chunk_messages};

/// Return type from `compact_context()` that distinguishes between successful compaction,
/// probe rejection, and no-op.
///
/// Gives `maybe_compact()` enough information to handle probe rejection without triggering
/// the `Exhausted` state — which would only be correct if summarization itself is stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompactionOutcome {
    /// Messages were drained and replaced with a summary.
    Compacted,
    /// Messages were drained and replaced with a summary, but persisting the result failed.
    /// The in-memory state is correct; only persistence to storage failed.
    CompactedWithPersistError,
    /// Probe rejected the summary — original messages are preserved.
    /// Caller must NOT check `freed_tokens` or transition to `Exhausted`.
    ProbeRejected,
    /// No compaction was performed (too few messages, empty `to_compact`, etc.).
    NoChange,
}

pub(super) const PERSONA_PREFIX: &str = "[Persona context]\n";
pub(super) const TRAJECTORY_PREFIX: &str = "[Past experience]\n";
pub(super) const TREE_MEMORY_PREFIX: &str = "[Memory summary]\n";

impl<C: Channel> Agent<C> {
    pub(super) fn compaction_tier(&self) -> super::context_manager::CompactionTier {
        self.context_manager
            .compaction_tier(self.providers.cached_prompt_tokens)
    }
}

#[cfg(test)]
mod tests;
