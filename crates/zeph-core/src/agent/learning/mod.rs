// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod arise;
mod background;
mod d2skill;
mod erl;
mod outcomes;
mod preferences;
mod rl;
mod skill_commands;
mod trust;

#[cfg(test)]
mod tests;

use super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    pub(crate) fn is_learning_enabled(&self) -> bool {
        self.learning_engine.is_enabled()
    }

    async fn is_skill_trusted_for_learning(&self, skill_name: &str) -> bool {
        let Some(memory) = &self.memory_state.memory else {
            return true;
        };
        let Ok(Some(row)) = memory.sqlite().load_skill_trust(skill_name).await else {
            return true; // no trust record = local skill = trusted
        };
        matches!(row.trust_level.as_str(), "trusted" | "verified")
    }

    pub(crate) async fn record_skill_outcomes(
        &mut self,
        outcome: &str,
        error_context: Option<&str>,
        outcome_detail: Option<&str>,
    ) {
        if self.skill_state.active_skill_names.is_empty() {
            return;
        }
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        if let Err(e) = memory
            .sqlite()
            .record_skill_outcomes_batch(
                &self.skill_state.active_skill_names,
                self.memory_state.conversation_id,
                outcome,
                error_context,
                outcome_detail,
            )
            .await
        {
            tracing::warn!("failed to record skill outcomes: {e:#}");
        }

        if outcome != "success" {
            for name in &self.skill_state.active_skill_names {
                self.check_rollback(name).await;
            }
        }

        let names: Vec<String> = self.skill_state.active_skill_names.clone();
        for name in &names {
            self.check_trust_transition(name).await;
        }
        self.update_skill_confidence_metrics().await;

        // SkillOrchestra RL routing head update (fire-and-forget).
        self.spawn_rl_head_update(outcome);

        // ARISE + STEM + ERL background tasks (fire-and-forget, never block response).
        self.spawn_stem_detection(outcome);
        if outcome == "success" {
            for name in &names {
                self.spawn_arise_trace_improvement(name);
                self.spawn_erl_reflection(name);
            }
        }
    }

    /// Returns true and spawns `fut` when the learning task cap has not been reached.
    ///
    /// When at capacity, logs a debug message and returns false (no abort of existing tasks).
    pub(super) fn try_spawn_learning_task(
        &mut self,
        fut: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        if self.learning_engine.learning_tasks.len()
            >= crate::agent::learning_engine::MAX_LEARNING_TASKS
        {
            tracing::debug!(
                "learning_tasks at capacity ({}), skipping spawn",
                crate::agent::learning_engine::MAX_LEARNING_TASKS
            );
            return false;
        }
        self.learning_engine.learning_tasks.spawn(fut);
        true
    }

    pub(crate) async fn update_skill_confidence_metrics(&self) {
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let Ok(stats) = memory.sqlite().load_skill_outcome_stats().await else {
            return;
        };
        let confidences: Vec<crate::metrics::SkillConfidence> = stats
            .iter()
            .map(|s| {
                let suc = u32::try_from(s.successes).unwrap_or(0);
                let fail = u32::try_from(s.failures).unwrap_or(0);
                crate::metrics::SkillConfidence {
                    name: s.skill_name.clone(),
                    posterior: zeph_skills::trust_score::posterior_mean(suc, fail),
                    total_uses: u32::try_from(s.total).unwrap_or(0),
                }
            })
            .collect();
        self.update_metrics(|m| m.skill_confidence = confidences);
    }
}
