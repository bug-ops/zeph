// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SkillState` impl block: pure data-manipulation helpers.
//!
//! Methods here only access `SkillState` fields — no cross-cutting agent dependencies.
//! Agent methods (`reload_skills`, `rebuild_skill_matcher`) stay on `Agent<C>` because
//! they need the embedding provider, channel, and memory state.

use super::SkillState;

impl SkillState {
    /// Rebuild the BM25 index from current registry metadata, if hybrid search is enabled.
    pub(crate) fn rebuild_bm25(&mut self, descs: &[&str]) {
        if self.hybrid_search {
            self.bm25_index = Some(zeph_skills::bm25::Bm25Index::build(descs));
        }
    }

    /// Return the current registry fingerprint.
    pub(crate) fn fingerprint(&self) -> u64 {
        self.registry.read().fingerprint()
    }
}
