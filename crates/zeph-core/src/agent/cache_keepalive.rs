// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-agent glue for the prompt-cache keepalive service (spec 091).
//!
//! The agent owns the snapshot rules: it captures the main-loop request after a successful
//! response, marks activity at every turn start, and tears the service down by dropping its
//! guard. The service loop itself lives in [`crate::cache_keepalive`].

use std::sync::Arc;

use zeph_common::ProviderName;
use zeph_common::task_supervisor::AbortOnDrop;
use zeph_config::CacheKeepaliveConfig;
use zeph_llm::provider::{LlmProvider as _, ToolDefinition};

use super::Agent;
use crate::cache_keepalive::{
    CaptureInput, KeepaliveState, ServiceDeps, Shared, StopReason, run_service,
};
use crate::channel::Channel;

/// Keepalive settings and service handle carried by one agent instance.
///
/// `Default` is the disabled configuration, so an agent built without
/// [`Agent::with_interaction_config`] never pings.
#[derive(Debug, Default)]
pub(crate) struct KeepaliveRuntime {
    pub(crate) config: CacheKeepaliveConfig,
    shared: Arc<Shared>,
    /// Aborts the keepalive task when dropped, so closing a session stops its pings.
    guard: Option<AbortOnDrop<()>>,
}

impl KeepaliveRuntime {
    pub(crate) fn new(config: CacheKeepaliveConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// The state shared with the service task.
    pub(super) fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Armed by the config, unless the user overrode it with `/keepalive`.
    fn is_armed(&self) -> bool {
        self.shared.user_override().unwrap_or(self.config.enabled)
    }
}

impl<C: Channel> Agent<C> {
    /// Hook called at the start of every turn; the service is spawned lazily from here.
    pub(super) fn keepalive_on_turn_start(&mut self) {
        if !self.runtime.cache_keepalive.is_armed() {
            return;
        }
        self.keepalive_ensure_service();
        self.keepalive_resolve_target();
        self.runtime.cache_keepalive.shared.mark_activity();
        self.keepalive_publish_state();
    }

    /// Hook called whenever the main provider is replaced: the captured snapshot is stale.
    pub(super) fn keepalive_on_provider_change(&mut self) {
        if self.runtime.cache_keepalive.guard.is_none() {
            return;
        }
        self.runtime.cache_keepalive.shared.provider_changed();
        self.keepalive_resolve_target();
        self.keepalive_publish_state();
    }

    /// A main-loop request is about to be sent: any ping in flight is now pointless, so cancel it
    /// and restart the timer.
    pub(super) fn keepalive_mark_activity(&self) {
        self.runtime.cache_keepalive.shared.mark_activity();
    }

    /// Capture the request the main loop just sent successfully, so the service can replay it.
    ///
    /// Called by the main-loop dispatch after every successful response, and only while the
    /// service is armed. Only the messages added since the previous capture are copied, and the
    /// tool definitions are shared with the previous snapshot when unchanged. Side-call provider clones and subagents run their
    /// own loops and never reach this.
    pub(super) fn keepalive_capture(&self, tool_defs: &[ToolDefinition]) {
        let keepalive = &self.runtime.cache_keepalive;
        if !keepalive.shared.is_capturing() {
            return;
        }
        let (target, _) = self.keepalive_target();
        let speed = self
            .provider
            .last_response_usage()
            .map_or_default(|usage| usage.speed);
        keepalive.shared.capture(
            CaptureInput {
                provider: self.provider.clone(),
                messages: &self.msg.messages,
                tools: keepalive
                    .shared
                    .reusable_tools(tool_defs)
                    .unwrap_or_else(|| Arc::from(tool_defs)),
                target,
            },
            speed,
        );
    }

    /// Handle `/keepalive [on|off|status]` and return the user-visible result.
    pub(super) fn keepalive_command(&mut self, arg: &str) -> String {
        match arg.trim() {
            "on" => {
                let keepalive = &self.runtime.cache_keepalive;
                keepalive.shared.arm();
                self.keepalive_ensure_service();
                self.keepalive_resolve_target();
                let state = self.runtime.cache_keepalive.shared.user_on();
                self.keepalive_publish_state();
                if !self.provider.prefix_cache().is_prefix() {
                    return "keepalive: on, but the active provider has no prompt cache to keep \
                            warm"
                        .to_owned();
                }
                format!("keepalive: on ({})", describe(&state))
            }
            "off" => {
                self.runtime.cache_keepalive.shared.user_off();
                self.keepalive_publish_state();
                "keepalive: off".to_owned()
            }
            "" | "status" => {
                let shared = &self.runtime.cache_keepalive.shared;
                let (pings, cost_cents) = shared.stats();
                format!(
                    "keepalive: {} ({pings} pings, ${:.4})",
                    self.keepalive_status_text(),
                    cost_cents / 100.0
                )
            }
            other => format!("keepalive: unknown argument `{other}`; use on, off or status"),
        }
    }

    /// What `/keepalive status` says: the service state, corrected for what the state alone
    /// cannot tell (nothing captured yet, or a provider that has no prompt cache).
    fn keepalive_status_text(&self) -> &'static str {
        let keepalive = &self.runtime.cache_keepalive;
        let state = keepalive.shared.state();
        if state == KeepaliveState::Disabled && !keepalive.is_armed() {
            return describe(&state);
        }
        if !self.provider.prefix_cache().is_prefix() {
            return "unsupported provider: no prompt cache to keep warm";
        }
        if matches!(state, KeepaliveState::Disabled | KeepaliveState::Idle)
            && !keepalive.shared.has_snapshot()
        {
            return "idle, waiting for first response";
        }
        describe(&state)
    }

    fn keepalive_publish_state(&self) {
        let state = self.runtime.cache_keepalive.shared.state();
        self.update_metrics(|m| m.cache_keepalive = state);
    }

    /// Where pings go, and whether `keepalive_provider` contradicts the serving provider.
    ///
    /// A single provider can only ping itself, so a differing `keepalive_provider` is a
    /// mismatch. A router picks the backend, so the name is passed through as the target.
    fn keepalive_target(&self) -> (Option<ProviderName>, bool) {
        let configured = &self.runtime.cache_keepalive.config.keepalive_provider;
        let Some(wanted) = configured.as_non_empty() else {
            return (None, false);
        };
        if self.provider.is_router() {
            return (Some(configured.clone()), false);
        }
        let active = if self.runtime.config.active_provider_name.is_empty() {
            self.provider.name()
        } else {
            self.runtime.config.active_provider_name.as_str()
        };
        (None, wanted != active)
    }

    fn keepalive_resolve_target(&self) {
        let (_, mismatch) = self.keepalive_target();
        let shared = &self.runtime.cache_keepalive.shared;
        if mismatch && shared.state() != KeepaliveState::Stopped(StopReason::ProviderMismatch) {
            tracing::warn!(
                keepalive_provider = %self.runtime.cache_keepalive.config.keepalive_provider,
                "keepalive_provider does not name the serving provider; keepalive stopped"
            );
        }
        shared.set_mismatch(mismatch);
    }

    /// Spawn the per-agent keepalive task once, under a name unique to this agent so agents
    /// sharing a supervisor never replace each other's task.
    fn keepalive_ensure_service(&mut self) {
        if self.runtime.cache_keepalive.guard.is_some() {
            return;
        }
        let shared = Arc::clone(&self.runtime.cache_keepalive.shared);
        shared.arm();
        let deps = ServiceDeps {
            shared,
            metrics_tx: self.runtime.metrics.metrics_tx.clone(),
            cost: self.runtime.metrics.cost_tracker.clone(),
            max_pings: self
                .runtime
                .cache_keepalive
                .config
                .max_consecutive_pings
                .get(),
        };
        let name: Arc<str> = Arc::from(format!("llm.cache_keepalive.{}", uuid::Uuid::new_v4()));
        let handle = self
            .runtime
            .lifecycle
            .task_supervisor
            .spawn_oneshot(name, move || run_service(deps));
        self.runtime.cache_keepalive.guard = Some(AbortOnDrop::new(handle));
    }
}

/// One-line description of the service state for `/keepalive`.
fn describe(state: &KeepaliveState) -> &'static str {
    match state {
        KeepaliveState::Disabled => "disabled",
        KeepaliveState::Idle => "armed",
        KeepaliveState::InFlight => "pinging",
        KeepaliveState::Stopped(StopReason::CapReached) => {
            "paused at the ping cap until the next activity"
        }
        KeepaliveState::Stopped(StopReason::NoCacheHit) => "stopped: pings did not hit the cache",
        KeepaliveState::Stopped(StopReason::PrefixRewritten) => {
            "stopped: a ping rewrote the cached prefix"
        }
        KeepaliveState::Stopped(StopReason::BudgetExhausted) => {
            "paused: pinging would cost too much, resumes on the next activity"
        }
        KeepaliveState::Stopped(StopReason::ProviderMismatch) => {
            "stopped: keepalive_provider does not match the serving provider"
        }
        KeepaliveState::Stopped(StopReason::UserOff) => "off",
        KeepaliveState::Stopped(StopReason::Unpriced) => {
            "stopped: pings cannot be priced while thinking is on"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    fn agent() -> Agent<MockChannel> {
        Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
    }

    #[tokio::test]
    async fn status_reports_disabled_by_default_and_never_spawns_a_task() {
        let mut agent = agent();
        assert_eq!(
            agent.keepalive_command("status"),
            "keepalive: disabled (0 pings, $0.0000)"
        );
        agent.keepalive_on_turn_start();
        assert!(agent.runtime.cache_keepalive.guard.is_none());
    }

    #[tokio::test]
    async fn on_arms_the_service_even_when_the_config_is_off_and_off_stops_it() {
        let mut agent = agent();
        let reply = agent.keepalive_command("on");
        assert!(reply.starts_with("keepalive: on"), "{reply}");
        assert!(agent.runtime.cache_keepalive.guard.is_some());
        assert_eq!(
            agent.runtime.cache_keepalive.shared.state(),
            KeepaliveState::Idle
        );
        assert_eq!(agent.keepalive_command("off"), "keepalive: off");
        assert_eq!(
            agent.runtime.cache_keepalive.shared.state(),
            KeepaliveState::Stopped(StopReason::UserOff)
        );
    }

    #[tokio::test]
    async fn on_warns_when_the_provider_has_no_prompt_cache() {
        let mut agent = agent();
        let reply = agent.keepalive_command("on");
        assert!(reply.contains("no prompt cache"), "{reply}");
    }

    #[tokio::test]
    async fn unknown_argument_is_reported() {
        let mut agent = agent();
        assert!(
            agent
                .keepalive_command("maybe")
                .contains("unknown argument")
        );
    }

    fn armed_agent(cached: bool) -> Agent<MockChannel> {
        let mut config = zeph_config::Config::default();
        config.llm.cache_keepalive.enabled = true;
        let mut mock = zeph_llm::mock::MockProvider::default();
        if cached {
            mock = mock.with_prefix_cache(zeph_llm::provider::PrefixCache::Prefix {
                ttl: Some(std::time::Duration::from_secs(300)),
            });
        }
        Agent::new(
            zeph_llm::any::AnyProvider::Mock(mock),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_interaction_config(crate::InteractionConfig::from_config(&config))
    }

    #[tokio::test]
    async fn status_before_the_first_response_says_it_is_waiting_not_disabled() {
        let mut agent = armed_agent(true);
        assert!(
            agent
                .keepalive_command("status")
                .starts_with("keepalive: idle, waiting for first response"),
            "{}",
            agent.keepalive_command("status")
        );
    }

    #[tokio::test]
    async fn status_on_a_provider_without_a_prompt_cache_says_unsupported() {
        let mut agent = armed_agent(false);
        assert!(
            agent
                .keepalive_command("status")
                .contains("unsupported provider"),
            "{}",
            agent.keepalive_command("status")
        );
    }
}
