// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SkillState` impl block: pure data-manipulation helpers.
//!
//! Methods here only access `SkillState` fields — no cross-cutting agent dependencies.
//! Agent methods (`reload_skills`, `rebuild_skill_matcher`) stay on `Agent<C>` because
//! they need the embedding provider, channel, and memory state.

use std::collections::HashMap;

use zeph_skills::loader::Skill;
use zeph_skills::trust::SkillTrustLevel;

use super::SkillState;

impl SkillState {
    /// Rebuild the skills prompt text from the current registry + trust/health maps.
    ///
    /// Returns the formatted prompt string. Does NOT update `last_skills_prompt` —
    /// the caller is responsible for storing the result.
    pub(crate) fn rebuild_prompt(
        all_skills: &[Skill],
        trust_map: &HashMap<String, SkillTrustLevel>,
        health_map: &HashMap<String, (f64, u32)>,
    ) -> String {
        zeph_skills::prompt::format_skills_prompt(all_skills, trust_map, health_map)
    }

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
