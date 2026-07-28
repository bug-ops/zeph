// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod assembly;
mod summarization;

pub(super) use crate::text::truncate_to_chars as truncate_chars;
/// Re-exported so `agent::subagent_commands::parent_effective_trust_level`'s no-floor-wired
/// fallback path can apply the same D4 `skill_fallback_mode` guard as the parent's own gate
/// (#6701, S1), instead of duplicating the fold logic.
pub(super) use assembly::compute_effective_trust;
#[cfg(test)]
pub(super) use zeph_agent_context::state::CompactionOutcome;
#[cfg(test)]
pub(super) use zeph_context::slot::cap_summary;
#[cfg(test)]
pub(super) use zeph_context::slot::chunk_messages;

#[cfg(test)]
impl<C: super::Channel> super::Agent<C> {
    pub(super) fn compaction_tier(&self) -> super::context_manager::CompactionTier {
        self.context_manager
            .compaction_tier(self.runtime.providers.cached_prompt_tokens)
    }
}

#[cfg(test)]
mod tests;
