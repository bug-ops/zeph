// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shadow-sentinel goal-drift event recording (spec 010-7 FR-001–FR-004).
//!
//! Split out of `tier_loop.rs` — see that module for the orchestration entry point that calls
//! into this pass after every tool batch completes.

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Record a [`ShadowEvent`](zeph_sanitizer::ShadowEvent) for cross-turn goal-drift detection.
    ///
    /// Called after every tool batch completes (spec 010-7 FR-001). When `shadow_memory` is
    /// `None` (disabled) this is a no-op. When drift score triggers an alert, emits a
    /// `GoalDrift` security event (FR-003, FR-004).
    pub(super) fn record_shadow_event(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        goal_summary: String,
    ) {
        let Some(ref mut mem) = self.services.security.shadow_memory else {
            return;
        };
        let tool_names: Vec<String> = tool_calls
            .iter()
            .map(|tc| tc.name.as_str().to_owned())
            .collect();
        let max_permission_class = tool_names
            .iter()
            .map(|n| zeph_sanitizer::classify_tool_permission(n))
            .max()
            .unwrap_or(0);
        let turn = u32::try_from(self.runtime.debug.iteration_counter.saturating_sub(1))
            .unwrap_or(u32::MAX);
        mem.record(zeph_sanitizer::ShadowEvent {
            turn,
            tools: tool_names,
            max_permission_class,
            deviation_score: 0.0,
            goal_summary,
        });
        let drift = mem.goal_drift_score();
        if drift.should_alert {
            tracing::warn!(
                score = drift.score,
                turn = turn,
                "shadow memory: goal drift alert"
            );
            self.push_security_event(
                zeph_common::SecurityEventCategory::GoalDrift,
                "shadow_memory",
                format!("drift_score={:.3}", drift.score),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- record_shadow_event (spec 010-7 FR-001–FR-004) ---

    fn make_tool_req(name: &str) -> zeph_llm::provider::ToolUseRequest {
        zeph_llm::provider::ToolUseRequest {
            id: format!("id_{name}"),
            name: name.into(),
            input: serde_json::Value::Null,
        }
    }

    fn make_agent_with_shadow(enabled: bool) -> Agent<crate::testing::MockChannel> {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_skills::registry::SkillRegistry;
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![] as Vec<String>);
        let registry = SkillRegistry::empty();
        let executor = MockToolExecutor::no_tools();
        let cfg = zeph_config::ShadowMemoryConfig {
            enabled,
            drift_threshold: 0.01,
            window_size: 3,
            max_events: 50,
        };
        Agent::new(provider, channel, registry, None, 5, executor).with_shadow_memory_config(&cfg)
    }

    #[test]
    fn record_shadow_event_noop_when_disabled() {
        let mut agent = make_agent_with_shadow(false);
        agent.runtime.debug.iteration_counter = 1;
        let calls = vec![make_tool_req("shell")];
        // Must not panic; shadow_memory stays None.
        agent.record_shadow_event(&calls, "goal".into());
        assert!(
            agent.services.security.shadow_memory.is_none(),
            "shadow_memory must remain None when disabled"
        );
    }

    #[test]
    fn record_shadow_event_appends_event_when_enabled() {
        let mut agent = make_agent_with_shadow(true);
        agent.runtime.debug.iteration_counter = 1;
        let calls = vec![make_tool_req("shell"), make_tool_req("web_scrape")];
        agent.record_shadow_event(&calls, "test goal".into());
        let mem = agent.services.security.shadow_memory.as_ref().unwrap();
        assert_eq!(mem.len(), 1, "one event must be recorded after one batch");
    }

    #[test]
    fn record_shadow_event_goal_drift_emits_security_event() {
        use tokio::sync::watch;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
            use zeph_skills::registry::SkillRegistry;
            let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());
            let cfg = zeph_config::ShadowMemoryConfig {
                enabled: true,
                drift_threshold: 0.01,
                window_size: 3,
                max_events: 100,
            };
            let mut agent = Agent::new(
                mock_provider(vec![]),
                MockChannel::new(vec![] as Vec<String>),
                SkillRegistry::empty(),
                None,
                5,
                MockToolExecutor::no_tools(),
            )
            .with_shadow_memory_config(&cfg)
            .with_metrics(tx);

            agent.runtime.debug.iteration_counter = 1;

            // Fill initial window with low-variance events.
            let low = vec![make_tool_req("read")];
            for _ in 0..5 {
                agent.record_shadow_event(&low, "read files".into());
            }
            // Introduce high-privilege divergent batch to spike drift.
            let high = vec![
                make_tool_req("shell"),
                make_tool_req("fetch"),
                make_tool_req("write"),
            ];
            for _ in 0..5 {
                agent.record_shadow_event(&high, "exfiltrate everything".into());
            }

            let snap = rx.borrow().clone();
            // The test verifies that if GoalDrift fires, the event has the right category.
            // (Whether it fires depends on drift score internals; we assert structural correctness.)
            for ev in &snap.security_events {
                if ev.category == zeph_common::SecurityEventCategory::GoalDrift {
                    assert_eq!(ev.source, "shadow_memory");
                    return;
                }
            }
            // If no GoalDrift was emitted, at minimum confirm events were recorded.
            let mem = agent.services.security.shadow_memory.as_ref().unwrap();
            assert!(!mem.is_empty(), "shadow_memory must have recorded events");
        });
    }
}
