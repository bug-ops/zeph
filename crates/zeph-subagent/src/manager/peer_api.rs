// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Parent-side surface for live inter-sub-agent messaging (spec
//! `046-subagent-peer-messaging-parity`), mirroring the shape of
//! [`secrets`](super::secrets)'s approve/deny/`try_recv` API.

use crate::peer::{AgentId, DeliveryError, PeerGroupId, PeerMessage};

use super::SubAgentManager;

impl SubAgentManager {
    /// Send a message from the parent agent to the sub-agent addressable as `task_id`,
    /// backing the `/agent msg` slash command.
    ///
    /// The parent session holds a root registration for every spawn-tree group it has
    /// spawned into — its own [`PeerGroupId::Session`] root (always live) plus one
    /// [`PeerGroupId::Plan`] root per orchestration plan execution that has dispatched at
    /// least one sub-agent (registered lazily by `ensure_plan_peer_group`, crate-private).
    /// This method looks up
    /// `task_id`'s actual group and sends *as that group's own root*, since only a node's own
    /// root satisfies the authorization model's ancestor-walk clause for it — sending
    /// unconditionally as `Root(Session)` would make `/agent msg` unable to reach a
    /// `DagScheduler`-dispatched sub-agent at all (critic round-4 S2).
    ///
    /// # Errors
    ///
    /// See [`DeliveryError`]. An unresolvable `task_id` falls back to the `Session` group,
    /// which then fails as [`DeliveryError::TargetNotFound`] — the same outcome as an unknown
    /// id in any other group.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_subagent::SubAgentManager;
    /// use zeph_subagent::peer::DeliveryError;
    ///
    /// let manager = SubAgentManager::new(4);
    /// let err = manager
    ///     .send_to_subagent("nonexistent-task-id", "hello".to_owned())
    ///     .unwrap_err();
    /// assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    /// ```
    pub fn send_to_subagent(&self, task_id: &str, body: String) -> Result<(), DeliveryError> {
        let target_id = AgentId::Task(task_id.to_owned());
        let group = self
            .peer_router
            .group_of(&target_id)
            .unwrap_or(PeerGroupId::Session);
        let from = AgentId::Root(group);
        self.peer_router.send(&from, task_id, body)
    }

    /// Non-blocking drain of one message addressed to the parent agent itself — either its
    /// `AgentId::Root(PeerGroupId::Session)` identity, or any live
    /// `AgentId::Root(PeerGroupId::Plan(graph_id))` identity registered via
    /// `ensure_plan_peer_group` (crate-private, critic round-4 S2).
    ///
    /// `try_recv`-only — no `.await` anywhere on this path, exactly the shape of
    /// [`try_recv_secret_request`](Self::try_recv_secret_request). Call from the main loop
    /// (next to `notify_completed_subagents`) and the scheduler loop (next to
    /// `process_pending_secret_requests`) to surface incoming peer messages to the operator.
    ///
    /// # Known latency limitation
    ///
    /// A message addressed to the parent is not observed until the parent reaches its next
    /// drain point — at most one turn (or one scheduler tick) away. This is the same
    /// structural property as `plan_cancel_token` (issue #1603): the parent holds `&mut self`
    /// through a turn, so nothing can interrupt it mid-turn. It does **not** affect
    /// subagent-to-subagent traffic, which never routes through the parent.
    pub fn try_recv_peer_message(&mut self) -> Option<PeerMessage> {
        if let Ok(msg) = self.peer_root_rx.try_recv() {
            return Some(msg);
        }
        for (_registration, rx) in self.plan_peer_roots.values_mut() {
            if let Ok(msg) = rx.try_recv() {
                return Some(msg);
            }
        }
        None
    }

    /// Current queued (undelivered) peer-message count for `task_id`'s mailbox, backing the
    /// TUI unread badge (FR-010).
    ///
    /// Returns `0` for an unknown or already-terminated `task_id`, mirroring
    /// [`forwarded_tail`](Self::forwarded_tail)'s empty-on-unknown convention.
    #[must_use]
    pub fn mailbox_depth(&self, task_id: &str) -> usize {
        self.peer_router
            .mailbox_depth(&AgentId::Task(task_id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::SubAgentDef;
    use crate::manager::SubAgentHandle;

    #[test]
    fn send_to_unknown_subagent_fails() {
        let mgr = SubAgentManager::new(4);
        let err = mgr
            .send_to_subagent("nonexistent", "hi".to_owned())
            .unwrap_err();
        assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    }

    #[test]
    fn try_recv_peer_message_drains_once_then_none() {
        let mut mgr = SubAgentManager::new(4);
        let (_reg, _rx) = mgr.peer_router.register(
            AgentId::Task("child".to_owned()),
            "child".to_owned(),
            Some(AgentId::Root(PeerGroupId::Session)),
            PeerGroupId::Session,
        );
        mgr.send_to_subagent("child", "clarify?".to_owned())
            .expect("send to registered child");

        let received = mgr.try_recv_peer_message();
        assert!(
            received.is_none(),
            "message was sent to the child, not the parent"
        );

        // Send a message addressed back to the parent root and confirm the drain sees it.
        mgr.peer_router
            .send(
                &AgentId::Task("child".to_owned()),
                "spawner",
                "clarifying question".to_owned(),
            )
            .expect("child to parent send");
        let received = mgr
            .try_recv_peer_message()
            .expect("parent-addressed message must be observable");
        assert_eq!(received.body, "clarifying question");
        assert!(
            mgr.try_recv_peer_message().is_none(),
            "second drain must be empty"
        );
    }

    #[test]
    fn mailbox_depth_reflects_router_state() {
        let mgr = SubAgentManager::new(4);
        let (_reg, _rx) = mgr.peer_router.register(
            AgentId::Task("child".to_owned()),
            "child".to_owned(),
            Some(AgentId::Root(PeerGroupId::Session)),
            PeerGroupId::Session,
        );
        assert_eq!(mgr.mailbox_depth("child"), 0);
        mgr.send_to_subagent("child", "hi".to_owned())
            .expect("send");
        assert_eq!(mgr.mailbox_depth("child"), 1);
        assert_eq!(mgr.mailbox_depth("unknown"), 0);
    }

    #[test]
    fn set_peer_messaging_config_preserves_secret_registry_set_before() {
        let mut mgr = SubAgentManager::new(4);
        let registry = std::sync::Arc::new(zeph_sanitizer::secret_mask::SecretMaskRegistry::new());
        registry.register(
            "K",
            "super-secret-value",
            zeph_sanitizer::secret_mask::SecretCategory::Generic,
        );
        mgr.set_secret_registry(std::sync::Arc::clone(&registry));
        mgr.set_peer_messaging_config(zeph_config::PeerMessagingConfig::default());

        let (_reg, mut rx) = mgr.peer_router.register(
            AgentId::Task("child".to_owned()),
            "child".to_owned(),
            Some(AgentId::Root(PeerGroupId::Session)),
            PeerGroupId::Session,
        );
        mgr.send_to_subagent("child", "value is super-secret-value".to_owned())
            .expect("send");
        let msg = rx.try_recv().expect("delivered");
        assert!(!msg.body.contains("super-secret-value"));
    }

    #[test]
    fn set_peer_messaging_config_then_set_secret_registry_still_masks() {
        let mut mgr = SubAgentManager::new(4);
        mgr.set_peer_messaging_config(zeph_config::PeerMessagingConfig::default());
        let registry = std::sync::Arc::new(zeph_sanitizer::secret_mask::SecretMaskRegistry::new());
        registry.register(
            "K",
            "super-secret-value",
            zeph_sanitizer::secret_mask::SecretCategory::Generic,
        );
        mgr.set_secret_registry(std::sync::Arc::clone(&registry));

        let (_reg, mut rx) = mgr.peer_router.register(
            AgentId::Task("child".to_owned()),
            "child".to_owned(),
            Some(AgentId::Root(PeerGroupId::Session)),
            PeerGroupId::Session,
        );
        mgr.send_to_subagent("child", "value is super-secret-value".to_owned())
            .expect("send");
        let msg = rx.try_recv().expect("delivered");
        assert!(!msg.body.contains("super-secret-value"));
    }

    #[test]
    fn send_to_subagent_reaches_a_plan_group_task_via_its_own_root() {
        // Critic round-4 S2: /agent msg must be able to reach a DagScheduler-dispatched
        // sub-agent, not just Session-group ones.
        let mut mgr = SubAgentManager::new(4);
        mgr.ensure_plan_peer_group("plan-1");
        let plan_root_id = AgentId::Root(PeerGroupId::Plan("plan-1".to_owned()));
        let (_reg, _rx) = mgr.peer_router.register(
            AgentId::Task("plan-task".to_owned()),
            "plan-task".to_owned(),
            Some(plan_root_id),
            PeerGroupId::Plan("plan-1".to_owned()),
        );

        mgr.send_to_subagent("plan-task", "go".to_owned())
            .expect("send must reach the plan-group task via its own Plan root");
    }

    #[test]
    fn try_recv_peer_message_observes_a_plan_group_root_mailbox() {
        let mut mgr = SubAgentManager::new(4);
        mgr.ensure_plan_peer_group("plan-1");
        let plan_root_id = AgentId::Root(PeerGroupId::Plan("plan-1".to_owned()));
        let (task_reg, _rx) = mgr.peer_router.register(
            AgentId::Task("plan-task".to_owned()),
            "plan-task".to_owned(),
            Some(plan_root_id.clone()),
            PeerGroupId::Plan("plan-1".to_owned()),
        );

        mgr.peer_router
            .send(task_reg.id(), "plan", "clarifying question".to_owned())
            .expect("plan task to its own plan root must succeed");

        let received = mgr
            .try_recv_peer_message()
            .expect("a Plan-root mailbox message must be observable, not just Session's");
        assert_eq!(received.body, "clarifying question");
    }

    #[test]
    fn release_plan_peer_group_drops_the_route() {
        let mut mgr = SubAgentManager::new(4);
        mgr.ensure_plan_peer_group("plan-1");
        mgr.release_plan_peer_group("plan-1");

        let err = mgr
            .send_to_subagent("nonexistent-in-released-plan", "hi".to_owned())
            .unwrap_err();
        assert!(matches!(err, DeliveryError::TargetNotFound(_)));
        assert!(
            mgr.try_recv_peer_message().is_none(),
            "no dangling Plan-root mailbox after release"
        );
    }

    #[test]
    fn insert_handle_for_test_default_peer_is_none() {
        let mut mgr = SubAgentManager::new(4);
        let handle = SubAgentHandle::for_test("t1", SubAgentDef::for_test("helper"));
        assert!(handle.peer.is_none());
        mgr.insert_handle_for_test("t1".to_owned(), handle);
    }
}
