// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    /// Fire-and-forget RL routing head update for the selected skill.
    ///
    /// No-op when `rl_routing_enabled = false` or no RL head is loaded.
    /// Only updates for the first active skill name (the one that was selected).
    pub(crate) fn spawn_rl_head_update(&mut self, outcome: &str) {
        let Some(cfg) = self.learning_engine.rl_routing else {
            return;
        };
        if !cfg.enabled {
            return;
        }
        let Some(selected_skill) = self.skill_state.active_skill_names.first().cloned() else {
            return;
        };
        let Some(rl_head) = self.skill_state.rl_head.clone() else {
            return;
        };
        let reward = if outcome == "success" {
            1.0f32
        } else {
            -1.0f32
        };
        let lr = cfg.learning_rate;
        let persist_interval = cfg.persist_interval;
        let memory = self.memory_state.memory.clone();

        self.try_spawn_learning_task(async move {
            if !rl_head.update(reward, lr) {
                tracing::debug!(
                    skill = selected_skill,
                    "rl_head: no forward cache, skipping update"
                );
                return;
            }
            let update_count = rl_head.update_count();
            if (persist_interval == 0 || update_count % persist_interval == 0)
                && let Some(mem) = memory
            {
                let bytes = rl_head.to_bytes();
                let embed_dim = i64::try_from(rl_head.embed_dim()).unwrap_or(i64::MAX);
                let baseline = f64::from(rl_head.baseline());
                let count = i64::from(update_count);
                if let Err(e) = mem
                    .sqlite()
                    .save_routing_head_weights(embed_dim, &bytes, baseline, count)
                    .await
                {
                    tracing::debug!(
                        skill = selected_skill,
                        "rl_head: failed to persist weights: {e:#}"
                    );
                }
            }
        });
    }
}
