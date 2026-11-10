// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-of-turn advisory goal check (`[goals] auto_check`, spec 089).
//!
//! After a user turn that used tools while a goal is active, one supervisor verification runs
//! as a supervised `Enrichment` task and its report is delivered between turns. Only one check
//! is ever in flight; its result travels over a oneshot channel, so abort, panic and
//! drop-on-overflow all surface as a closed channel and the status line is always cleared.

use tokio::sync::oneshot::{self, error::RecvError, error::TryRecvError};
use tracing::Instrument as _;

use super::Agent;
use super::agent_supervisor::TaskClass;
use super::goal_selfcheck::{
    ActiveGoalLookup, GoalCheckJob, GoalCheckOutcome, GoalCheckTrigger, STATUS_VERIFYING,
    recent_tool_actions,
};
use super::turn::TurnOrigin;
use crate::channel::Channel;

/// A resolved auto-check: its outcome plus whether it is still relevant.
pub(super) struct Resolution {
    outcome: Result<GoalCheckOutcome, RecvError>,
    /// `false` when a turn began after the check was spawned, so its report is stale.
    current: bool,
}

struct Inflight {
    rx: oneshot::Receiver<GoalCheckOutcome>,
    turn_gen: u64,
}

/// Single-slot tracker for the in-flight auto-check.
///
/// `turn_gen` counts turns and is never reset (unlike the debug iteration counter, which
/// `/clear` rewinds), so a report is delivered only when no turn began since it was spawned.
#[derive(Default)]
pub(crate) struct GoalAutoCheckState {
    inflight: Option<Inflight>,
    turn_gen: u64,
}

impl GoalAutoCheckState {
    pub(super) fn begin_turn(&mut self) {
        self.turn_gen = self.turn_gen.saturating_add(1);
    }

    fn is_inflight(&self) -> bool {
        self.inflight.is_some()
    }

    fn track(&mut self, rx: oneshot::Receiver<GoalCheckOutcome>) {
        self.inflight = Some(Inflight {
            rx,
            turn_gen: self.turn_gen,
        });
    }

    /// Drop a check that finished but was never consumed; returns `true` when one was dropped.
    ///
    /// A finished receiver must leave the slot immediately: polling it again would panic.
    fn reap_finished(&mut self) -> bool {
        let Some(inflight) = self.inflight.as_mut() else {
            return false;
        };
        match inflight.rx.try_recv() {
            Err(TryRecvError::Empty) => false,
            Ok(_) | Err(TryRecvError::Closed) => {
                self.inflight = None;
                true
            }
        }
    }

    /// Resolve when the in-flight check finishes; pending forever while the slot is empty.
    ///
    /// Cancel-safe: dropping the future before completion leaves the slot untouched.
    pub(super) async fn resolved(&mut self) -> Resolution {
        let Some(inflight) = self.inflight.as_mut() else {
            return std::future::pending().await;
        };
        let outcome = (&mut inflight.rx).await;
        let spawned_gen = inflight.turn_gen;
        self.inflight = None;
        Resolution {
            outcome,
            current: spawned_gen == self.turn_gen,
        }
    }
}

impl<C: Channel> Agent<C> {
    fn set_goal_check_status(&self, text: &str) {
        if let Some(tx) = &self.services.session.status_tx
            && tx.send(text.to_owned()).is_err()
        {
            tracing::debug!("goal auto-check: status receiver closed");
        }
    }

    /// First unmet trigger condition as a log label, or the active goal text when all hold.
    fn goal_auto_check_target(
        &self,
        origin: TurnOrigin,
        turn_had_error: bool,
    ) -> Result<String, &'static str> {
        if origin != TurnOrigin::User {
            return Err("injected turn");
        }
        if self.services.session.is_guest_context {
            return Err("guest context");
        }
        if turn_had_error {
            return Err("turn failed");
        }
        if self.runtime.lifecycle.cancel_token.is_cancelled() {
            return Err("turn cancelled");
        }
        if self.runtime.lifecycle.turn_tool_calls == 0 {
            return Err("no tool calls");
        }
        if self.services.autonomous.session.is_some() {
            return Err("autonomous session running");
        }
        if !self.channel.delivers_between_turns() {
            return Err("channel cannot deliver between turns");
        }
        if self.services.goal_auto_check.is_inflight() {
            return Err("check already in flight");
        }
        self.services
            .goal_accounting
            .as_ref()
            .and_then(|a| a.snapshot())
            .map(|s| s.text)
            .ok_or("no active goal")
    }

    /// Spawn one background goal check when `[goals] auto_check` is on and the turn qualifies.
    ///
    /// Never awaits on the turn path: the supervisor is built and called inside the supervised
    /// task, and the report is delivered later through `LoopEvent::GoalAutoCheckDone`.
    pub(super) fn maybe_spawn_goal_auto_check(&mut self, origin: TurnOrigin, turn_had_error: bool) {
        let goals = &self.runtime.config.goals;
        if !(goals.enabled && goals.auto_check) {
            return;
        }
        if self.services.goal_auto_check.reap_finished() {
            self.set_goal_check_status("");
        }
        let goal_text = match self.goal_auto_check_target(origin, turn_had_error) {
            Ok(text) => text,
            Err(reason) => {
                tracing::debug!(reason, "goal auto-check skipped");
                return;
            }
        };

        let spec = self.supervisor_spec();
        let summary = self.build_conversation_summary();
        let recent_actions = recent_tool_actions(&self.msg.messages);
        let timeout_secs = self.runtime.config.goals.supervisor_timeout_secs;
        let (tx, rx) = oneshot::channel();
        let accepted = self.runtime.lifecycle.supervisor.spawn(
            TaskClass::Enrichment,
            "goal_auto_check",
            async move {
                let job = GoalCheckJob {
                    trigger: GoalCheckTrigger::TurnEnd,
                    explicit: None,
                    last_user: None,
                    summary,
                    recent_actions,
                    supervisor: spec.resolve(),
                    timeout_secs,
                    active_goal: ActiveGoalLookup::Known(goal_text),
                    status_tx: None,
                };
                if tx.send(job.run().await).is_err() {
                    tracing::debug!("goal auto-check result dropped: receiver gone");
                }
            }
            .instrument(tracing::info_span!("core.goal.auto_check")),
        );
        if accepted {
            self.services.goal_auto_check.track(rx);
            self.set_goal_check_status(STATUS_VERIFYING);
        }
    }

    /// Deliver a resolved auto-check: clear the status, then send the report if still current.
    pub(super) async fn handle_goal_auto_check_done(&mut self, resolution: Resolution) {
        self.set_goal_check_status("");
        match resolution.outcome {
            Ok(GoalCheckOutcome::Report(report)) if resolution.current => {
                if let Err(e) = self.channel.send(&report).await {
                    tracing::warn!("failed to send goal auto-check report: {e}");
                }
            }
            Ok(_) => tracing::debug!("goal auto-check result dropped (stale or no condition)"),
            Err(_) => tracing::debug!("goal auto-check ended without a result"),
        }
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use crate::agent::loop_event::LoopEvent;
    use crate::goal::GoalAccounting;

    const VERDICT: &str = r#"{"achieved":false,"reasoning":"half done","confidence":0.6,"suggestions":["run tests"]}"#;

    type TestAgent = Agent<MockChannel>;

    async fn armed_agent<C: Channel>(channel: C) -> (Agent<C>, GoalAccounting) {
        let store = crate::goal::store::in_memory_store().await;
        store.create("ship the parser", None, 2000).await.unwrap();
        let accounting = GoalAccounting::new(Arc::new(store));
        accounting.refresh().await.unwrap();
        let agent = Agent::new(
            mock_provider(vec![VERDICT.into(), VERDICT.into()]),
            channel,
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        (agent, accounting)
    }

    async fn qualifying_agent<C: Channel>(channel: C) -> Agent<C> {
        let (mut agent, accounting) = armed_agent(channel).await;
        agent.runtime.config.goals.enabled = true;
        agent.runtime.config.goals.auto_check = true;
        agent.services.goal_accounting = Some(Arc::new(accounting));
        agent.runtime.lifecycle.turn_tool_calls = 1;
        agent
    }

    fn enrichment_spawned(agent: &TestAgent) -> u64 {
        let metrics = agent.runtime.lifecycle.supervisor.metrics_snapshot();
        metrics.classes[TaskClass::Enrichment.index()].spawned
    }

    fn spawned(agent: &TestAgent) -> bool {
        agent.services.goal_auto_check.is_inflight()
    }

    async fn assert_skips(mutate: impl FnOnce(&mut TestAgent), origin: TurnOrigin, error: bool) {
        let mut agent = qualifying_agent(MockChannel::new(vec![])).await;
        mutate(&mut agent);
        agent.maybe_spawn_goal_auto_check(origin, error);
        assert!(!spawned(&agent));
        assert_eq!(agent.runtime.lifecycle.supervisor.inflight(), 0);
    }

    #[tokio::test]
    async fn delivers_labelled_report_and_clears_status() {
        let channel = MockChannel::new(vec![]);
        let sent = Arc::clone(&channel.sent);
        let mut agent = qualifying_agent(channel).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.services.session.status_tx = Some(tx);

        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        assert!(spawned(&agent));
        assert_eq!(rx.recv().await.as_deref(), Some(STATUS_VERIFYING));

        let resolution = agent.services.goal_auto_check.resolved().await;
        agent.handle_goal_auto_check_done(resolution).await;

        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(
            sent[0].contains("Goal check (auto, advisory)"),
            "{}",
            sent[0]
        );
        assert!(sent[0].contains("[active goal]"), "{}", sent[0]);
        assert_eq!(rx.recv().await.as_deref(), Some(""));
        assert!(!spawned(&agent));
    }

    #[tokio::test]
    async fn skips_when_any_condition_fails() {
        assert_skips(
            |a| a.runtime.config.goals.auto_check = false,
            TurnOrigin::User,
            false,
        )
        .await;
        assert_skips(
            |a| a.runtime.config.goals.enabled = false,
            TurnOrigin::User,
            false,
        )
        .await;
        assert_skips(
            |a| a.services.goal_accounting = None,
            TurnOrigin::User,
            false,
        )
        .await;
        assert_skips(|_| {}, TurnOrigin::Injected, false).await;
        assert_skips(|_| {}, TurnOrigin::User, true).await;
        assert_skips(
            |a| a.runtime.lifecycle.cancel_token.cancel(),
            TurnOrigin::User,
            false,
        )
        .await;
        assert_skips(
            |a| a.runtime.lifecycle.turn_tool_calls = 0,
            TurnOrigin::User,
            false,
        )
        .await;
        assert_skips(
            |a| drop(a.services.autonomous.start_session("g1", "goal", 5)),
            TurnOrigin::User,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn skips_on_channel_without_between_turn_delivery() {
        let mut agent =
            qualifying_agent(MockChannel::new(vec![]).without_between_turn_delivery()).await;
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        assert!(!spawned(&agent));
    }

    #[tokio::test]
    async fn skips_while_a_check_is_running() {
        let mut agent = qualifying_agent(MockChannel::new(vec![])).await;
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        assert_eq!(enrichment_spawned(&agent), 1);
    }

    #[tokio::test]
    async fn abort_resolves_without_result_and_clears_status() {
        let channel = MockChannel::new(vec![]);
        let sent = Arc::clone(&channel.sent);
        let mut agent = qualifying_agent(channel).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.services.session.status_tx = Some(tx);
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        agent
            .runtime
            .lifecycle
            .supervisor
            .abort_class(TaskClass::Enrichment);

        let resolution = agent.services.goal_auto_check.resolved().await;
        assert!(resolution.outcome.is_err());
        agent.handle_goal_auto_check_done(resolution).await;

        assert_eq!(sent.lock().unwrap().clone(), [] as [String; 0]);
        assert_eq!(rx.recv().await.as_deref(), Some(STATUS_VERIFYING));
        assert_eq!(rx.recv().await.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn report_is_dropped_when_a_turn_began_after_spawn() {
        let channel = MockChannel::new(vec![]);
        let sent = Arc::clone(&channel.sent);
        let mut agent = qualifying_agent(channel).await;
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        agent.services.goal_auto_check.begin_turn();

        let resolution = agent.services.goal_auto_check.resolved().await;
        assert!(!resolution.current);
        agent.handle_goal_auto_check_done(resolution).await;
        assert_eq!(sent.lock().unwrap().clone(), [] as [String; 0]);
    }

    #[tokio::test]
    async fn no_condition_outcome_is_silent() {
        let channel = MockChannel::new(vec![]);
        let sent = Arc::clone(&channel.sent);
        let mut agent = qualifying_agent(channel).await;
        agent
            .handle_goal_auto_check_done(Resolution {
                outcome: Ok(GoalCheckOutcome::NoCondition),
                current: true,
            })
            .await;
        assert_eq!(sent.lock().unwrap().clone(), [] as [String; 0]);
    }

    #[tokio::test]
    async fn finished_unconsumed_check_does_not_block_the_next_one() {
        let mut agent = qualifying_agent(MockChannel::new(vec![])).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.services.session.status_tx = Some(tx);
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        while agent.runtime.lifecycle.supervisor.inflight() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        agent.services.goal_auto_check.begin_turn();
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);

        assert!(spawned(&agent));
        assert_eq!(enrichment_spawned(&agent), 2);
        let statuses: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(statuses, [STATUS_VERIFYING, "", STATUS_VERIFYING]);
    }

    #[tokio::test]
    async fn resolved_is_pending_while_slot_is_empty() {
        let mut state = GoalAutoCheckState::default();
        let pending = tokio::time::timeout(Duration::from_millis(20), state.resolved()).await;
        assert!(pending.is_err());
    }

    #[tokio::test]
    async fn skips_on_loopback_channel_as_used_by_acp_sessions() {
        let (channel, _handle) = crate::channel::LoopbackChannel::pair(4);
        let mut agent = qualifying_agent(channel).await;
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        assert!(!agent.services.goal_auto_check.is_inflight());
    }

    #[tokio::test]
    async fn skips_guest_context_turns() {
        assert_skips(
            |a| a.services.session.is_guest_context = true,
            TurnOrigin::User,
            false,
        )
        .await;
    }

    fn tool_turn_provider() -> zeph_llm::any::AnyProvider {
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};
        let (mock, _) = MockProvider::with_responses(vec![VERDICT.into(), VERDICT.into()])
            .with_tool_use(vec![
                ChatResponse::tool_use(
                    None,
                    vec![ToolUseRequest {
                        id: "call-0".into(),
                        name: "tool_0".into(),
                        input: serde_json::json!({}),
                    }],
                ),
                ChatResponse::text("done"),
                ChatResponse::text("done again"),
            ]);
        zeph_llm::any::AnyProvider::Mock(mock)
    }

    /// Agent wired like production: goals, `auto_check` and an active goal, one scripted tool turn.
    async fn wired_agent(channel: MockChannel) -> TestAgent {
        use zeph_tools::executor::ToolOutput;
        let (mut agent, accounting) = armed_agent(channel).await;
        agent.provider = tool_turn_provider();
        agent.runtime.config.goals.enabled = true;
        agent.runtime.config.goals.auto_check = true;
        agent.services.goal_accounting = Some(Arc::new(accounting));
        let output = ToolOutput {
            tool_name: "tool_0".into(),
            summary: "result".to_owned(),
            blocks_executed: 1,
            ..Default::default()
        };
        agent.tool_executor = Arc::new(MockToolExecutor::new(vec![Ok(Some(output))]));
        agent
    }

    /// Run the loop until it idles on a blocked channel, then return what was sent.
    async fn run_until_idle(
        agent: &mut TestAgent,
        sent: &Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Vec<String> {
        let _ = tokio::time::timeout(Duration::from_secs(3), agent.run()).await;
        sent.lock().unwrap().clone()
    }

    fn has_auto_report(sent: &[String]) -> bool {
        sent.iter()
            .any(|m| m.contains("Goal check (auto, advisory)"))
    }

    #[tokio::test]
    async fn run_loop_delivers_auto_report_after_user_tool_turn() {
        let channel = MockChannel::new(vec!["run the tool".into()]).blocking_when_empty();
        let sent = Arc::clone(&channel.sent);
        let mut agent = wired_agent(channel).await;
        let sent = run_until_idle(&mut agent, &sent).await;
        assert!(has_auto_report(&sent), "{sent:?}");
    }

    #[tokio::test]
    async fn run_loop_does_not_check_bot_messages() {
        let channel = MockChannel::new(vec!["run the tool".into()])
            .blocking_when_empty()
            .with_bot_messages();
        let sent = Arc::clone(&channel.sent);
        let mut agent = wired_agent(channel).await;
        let sent = run_until_idle(&mut agent, &sent).await;
        assert!(!has_auto_report(&sent), "{sent:?}");
    }

    #[tokio::test]
    async fn run_loop_does_not_check_scheduled_turns() {
        let channel = MockChannel::new(vec![]).blocking_when_empty();
        let sent = Arc::clone(&channel.sent);
        let mut agent = wired_agent(channel).await;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        agent.runtime.lifecycle.custom_task_rx = Some(rx);
        tx.send("scheduled prompt".to_owned()).await.unwrap();
        let sent = run_until_idle(&mut agent, &sent).await;
        assert_eq!(
            agent.runtime.lifecycle.turn_tool_calls, 1,
            "turn must have run"
        );
        assert!(!has_auto_report(&sent), "{sent:?}");
    }

    #[tokio::test]
    async fn run_loop_does_not_check_loop_injected_turns() {
        let channel = MockChannel::new(vec![]).blocking_when_empty();
        let sent = Arc::clone(&channel.sent);
        let mut agent = wired_agent(channel).await;
        agent.runtime.lifecycle.user_loop = Some(crate::agent::state::LoopState {
            prompt: "loop prompt".to_owned(),
            iteration: 0,
            interval: tokio::time::interval(Duration::from_secs(3600)),
            cancel_tx: tokio_util::sync::CancellationToken::new(),
        });
        let sent = run_until_idle(&mut agent, &sent).await;
        assert_eq!(
            agent.runtime.lifecycle.turn_tool_calls, 1,
            "turn must have run"
        );
        assert!(!has_auto_report(&sent), "{sent:?}");
    }

    #[tokio::test]
    async fn next_event_yields_goal_auto_check_done() {
        let mut agent = qualifying_agent(MockChannel::new(vec![]).blocking_when_empty()).await;
        agent.maybe_spawn_goal_auto_check(TurnOrigin::User, false);
        let event = tokio::time::timeout(Duration::from_secs(3), agent.next_event())
            .await
            .expect("next_event must resolve")
            .unwrap();
        assert!(matches!(event, Some(LoopEvent::GoalAutoCheckDone(_))));
    }
}
