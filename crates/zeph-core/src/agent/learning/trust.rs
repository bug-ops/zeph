// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    pub(crate) async fn check_trust_transition(&self, skill_name: &str) {
        if let Err(_elapsed) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.check_trust_transition_inner(skill_name),
        )
        .await
        {
            tracing::warn!(
                skill = skill_name,
                "check_trust_transition timed out after 2s"
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn check_trust_transition_inner(&self, skill_name: &str) {
        let Some(memory) = &self.memory_state.persistence.memory else {
            return;
        };
        let Some(config) = &self.learning_engine.config else {
            return;
        };
        let Ok(Some(metrics)) = memory.sqlite().skill_metrics(skill_name).await else {
            return;
        };
        let successes = u32::try_from(metrics.successes).unwrap_or(0);
        let failures = u32::try_from(metrics.failures).unwrap_or(0);
        let total = u32::try_from(metrics.total).unwrap_or(0);
        let posterior = zeph_skills::trust_score::posterior_mean(successes, failures);

        if total >= config.auto_promote_min_uses && posterior > config.auto_promote_threshold {
            if config.cross_session_rollout {
                match memory.sqlite().distinct_session_count(skill_name).await {
                    Ok(sessions) if sessions < i64::from(config.min_sessions_before_promote) => {
                        tracing::debug!(
                            skill = skill_name,
                            sessions,
                            required = config.min_sessions_before_promote,
                            "cross-session rollout: insufficient sessions for promotion"
                        );
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("cross-session count query failed for {skill_name}: {e:#}");
                    }
                }
            }

            let trust_level = memory
                .sqlite()
                .load_skill_trust(skill_name)
                .await
                .ok()
                .flatten()
                .map(|r| r.trust_level);
            // Skip promotion only if explicitly blocked; promote even if no record exists.
            if trust_level.as_deref() != Some("trusted")
                && trust_level.as_deref() != Some("blocked")
            {
                tracing::info!(
                    skill = skill_name,
                    posterior = format!("{posterior:.3}"),
                    total,
                    "auto-promoting skill to trusted"
                );
                if trust_level.is_none() {
                    // No existing record — create one via upsert.
                    let _ = memory
                        .sqlite()
                        .upsert_skill_trust(
                            skill_name,
                            "trusted",
                            zeph_memory::store::SourceKind::Local,
                            None,
                            None,
                            "",
                        )
                        .await;
                } else {
                    let _ = memory
                        .sqlite()
                        .set_skill_trust_level(skill_name, "trusted")
                        .await;
                }
            }
        }

        if total >= config.auto_demote_min_uses && posterior < config.auto_demote_threshold {
            if config.cross_session_rollout {
                match memory.sqlite().distinct_session_count(skill_name).await {
                    Ok(sessions) if sessions < i64::from(config.min_sessions_before_demote) => {
                        tracing::debug!(
                            skill = skill_name,
                            sessions,
                            required = config.min_sessions_before_demote,
                            "cross-session rollout: insufficient sessions for demotion"
                        );
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("cross-session count query failed for {skill_name}: {e:#}");
                    }
                }
            }

            let Ok(Some(trust_row)) = memory.sqlite().load_skill_trust(skill_name).await else {
                return;
            };
            if trust_row.trust_level == "trusted" || trust_row.trust_level == "verified" {
                tracing::warn!(
                    skill = skill_name,
                    posterior = format!("{posterior:.3}"),
                    total,
                    "auto-demoting skill to quarantined"
                );
                let _ = memory
                    .sqlite()
                    .set_skill_trust_level(skill_name, "quarantined")
                    .await;
            }
        }
    }
}
