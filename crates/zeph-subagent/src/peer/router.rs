// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`PeerRouter`]: the `Arc`-shared node table backing live inter-sub-agent messaging.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::mpsc;
use zeph_config::PeerMessagingConfig;
use zeph_sanitizer::secret_mask::SecretMaskRegistry;

use super::{
    AgentId, DeliveryError, PeerDescriptor, PeerGroupId, PeerMessage, PeerRelation,
    UnauthorizedReason,
};

/// Maximum length, in bytes, of a `send_peer_message`/`send_to_subagent` `target` argument.
/// Task IDs are UUIDs (36 bytes) and display names are short; this is a generous cap against
/// an LLM-supplied string padding allocations or log lines, not a realistic addressing limit.
const MAX_TARGET_BYTES: usize = 256;

/// One addressable node in the peer graph: a registered agent's routing metadata and the
/// sender half of its bounded mailbox.
struct PeerNode {
    id: AgentId,
    name: String,
    /// `None` only for a group root.
    parent: Option<AgentId>,
    group: PeerGroupId,
    mailbox_tx: mpsc::Sender<PeerMessage>,
}

/// `Arc`-shared node table for live inter-sub-agent messaging (spec
/// `046-subagent-peer-messaging-parity`).
///
/// Shared between [`SubAgentManager`](crate::manager::SubAgentManager) and every spawned
/// sub-agent's own [`PeerToolExecutor`](super::PeerToolExecutor) — never owned by value by
/// either side, since a running sub-agent task cannot reach back into the manager. A send is
/// one brief [`parking_lot::RwLock`] read guard (lookup, authorize, clone the target's
/// mailbox sender), dropped before the non-blocking `try_send` — there is no `.await` on the
/// send path at all.
pub struct PeerRouter {
    nodes: RwLock<HashMap<AgentId, PeerNode>>,
    config: PeerMessagingConfig,
    /// `RwLock`-wrapped (not a plain `Option`) so [`set_secret_registry`][Self::set_secret_registry]
    /// can be called at any point in bootstrap ordering relative to
    /// [`SubAgentManager::set_secret_registry`](crate::manager::SubAgentManager::set_secret_registry),
    /// which mutates the *same* live router rather than requiring one bootstrap call to
    /// strictly precede the other.
    secret_registry: RwLock<Option<Arc<SecretMaskRegistry>>>,
}

/// RAII route registration returned by `PeerRouter::register` (crate-private — only
/// [`SubAgentManager`](crate::manager::SubAgentManager) constructs routes).
///
/// Dropping it deregisters the node, so a [`SubAgentHandle`](crate::manager::SubAgentHandle)
/// dropped without an explicit `collect()`/`cancel()` cannot leave a dangling addressable
/// route — mirroring the existing `Drop for SubAgentHandle` cancel-and-revoke safety net.
pub struct PeerRegistration {
    id: AgentId,
    router: Arc<PeerRouter>,
}

impl PeerRegistration {
    /// The addressable identity this registration holds a route for.
    #[must_use]
    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// The shared router this registration is bound to.
    #[must_use]
    pub fn router(&self) -> &Arc<PeerRouter> {
        &self.router
    }
}

impl Drop for PeerRegistration {
    fn drop(&mut self) {
        self.router.deregister(&self.id);
    }
}

impl std::fmt::Debug for PeerRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerRouter")
            .field("nodes_count", &self.nodes.read().len())
            .field("config", &self.config)
            .field("secret_registry", &self.secret_registry.read().is_some())
            .finish()
    }
}

impl PeerRouter {
    /// Construct a new, empty router.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_config::PeerMessagingConfig;
    /// use zeph_subagent::peer::PeerRouter;
    ///
    /// let router = PeerRouter::new(PeerMessagingConfig::default(), None);
    /// assert_eq!(std::sync::Arc::strong_count(&router), 1);
    /// ```
    #[must_use]
    pub fn new(
        config: PeerMessagingConfig,
        secret_registry: Option<Arc<SecretMaskRegistry>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            nodes: RwLock::new(HashMap::new()),
            config,
            secret_registry: RwLock::new(secret_registry),
        })
    }

    /// The peer-messaging configuration this router was constructed with.
    #[must_use]
    pub fn config(&self) -> &PeerMessagingConfig {
        &self.config
    }

    /// Replace the secret-mask registry applied to message bodies at send time.
    ///
    /// Callable at any point relative to construction — see the field doc comment on
    /// [`secret_registry`][Self].
    pub(crate) fn set_secret_registry(&self, registry: Option<Arc<SecretMaskRegistry>>) {
        *self.secret_registry.write() = registry;
    }

    /// Register a node and return its RAII registration plus the receiver half of its
    /// mailbox (moved into that agent's own [`PeerToolExecutor`](super::PeerToolExecutor)).
    ///
    /// Known limitation (review M3): registering an already-live `id` silently overwrites the
    /// existing node (`HashMap::insert`'s normal replace semantics) rather than erroring. Not
    /// reachable today — `manager::spawn`'s task ids are freshly minted `Uuid::new_v4`s and
    /// there is exactly one root per `PeerGroupId` (`Session`, and one `Plan(graph_id)` per
    /// live plan execution) — but a future caller must not assume `register` is idempotent.
    #[tracing::instrument(name = "subagent.mailbox.register", skip(self, name), fields(id = ?id))]
    pub(crate) fn register(
        self: &Arc<Self>,
        id: AgentId,
        name: String,
        parent: Option<AgentId>,
        group: PeerGroupId,
    ) -> (PeerRegistration, mpsc::Receiver<PeerMessage>) {
        let capacity = self.config.mailbox_capacity.max(1);
        let (mailbox_tx, mailbox_rx) = mpsc::channel(capacity);
        let node = PeerNode {
            id: id.clone(),
            name,
            parent,
            group,
            mailbox_tx,
        };
        self.nodes.write().insert(id.clone(), node);
        (
            PeerRegistration {
                id,
                router: Arc::clone(self),
            },
            mailbox_rx,
        )
    }

    /// Remove a node from the table. Called by [`PeerRegistration::drop`].
    fn deregister(&self, id: &AgentId) {
        self.nodes.write().remove(id);
    }

    /// Resolve `target` to a node within `group`: exact `task_id` first, then unique display
    /// name.
    ///
    /// Candidates are filtered to `group` **before** the id/name match (critic round-4 S3) —
    /// a node in a different spawn-tree root is excluded from the lookup entirely, so it is
    /// indistinguishable from a nonexistent node (`TargetNotFound`) rather than surfacing as
    /// an `Unauthorized` rejection, which would let a sender learn a target *exists* in
    /// another tree (NFR-003, spec §8 "Never... discover"). An ambiguous name (multiple nodes
    /// in `group` share it) resolves to `None` rather than an arbitrary node.
    fn resolve_target<'a>(
        nodes: &'a HashMap<AgentId, PeerNode>,
        group: &PeerGroupId,
        target: &str,
    ) -> Option<&'a PeerNode> {
        if let Some(node) = nodes
            .get(&AgentId::Task(target.to_owned()))
            .filter(|n| &n.group == group)
        {
            return Some(node);
        }
        let mut matches = nodes
            .values()
            .filter(|n| &n.group == group && n.name == target);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first)
    }

    /// Implements the plan's authorization truth table:
    /// `root(S) == root(T) AND S != T AND (T == parent(S) | parent(T) == parent(S) | S is an
    /// ancestor of T)`.
    ///
    /// TODO(spec `046-subagent-peer-messaging-parity`, review M6): a grandchild cannot reply
    /// to an ancestor beyond its direct parent (only `T == parent(S)`, not "T is an ancestor
    /// of S") — latent until a sub-agent-side spawn tool exists, since every spawn's `parent`
    /// is a group root today (`manager/spawn.rs`'s own comment at the `spawn()`/`resume()`
    /// peer-install sites), making every live node's depth exactly 1 and this asymmetry
    /// unreachable in production. Do not "fix" the truth table without a nested-spawn path to
    /// verify it against end to end.
    fn authorize(
        nodes: &HashMap<AgentId, PeerNode>,
        from_node: &PeerNode,
        to_node: &PeerNode,
    ) -> Result<(), UnauthorizedReason> {
        if from_node.id == to_node.id {
            return Err(UnauthorizedReason::SelfAddress);
        }
        if from_node.group != to_node.group {
            return Err(UnauthorizedReason::DifferentGroup);
        }
        if from_node.parent.as_ref() == Some(&to_node.id) {
            return Ok(());
        }
        if to_node.parent.is_some() && to_node.parent == from_node.parent {
            return Ok(());
        }
        // S is an ancestor of T: walk T's parent chain looking for S. No visited-set or depth
        // cap (review LOW finding): a parent cycle would spin forever while holding the
        // router's read lock, wedging every concurrent send. Unreachable today — every node's
        // `parent` is either `None` (a root) or a root's own `AgentId` (register()'s only
        // callers, `manager::spawn`, never nest deeper) — but a future nested-spawn feature
        // must add a cycle guard here before it can register a node whose ancestry isn't
        // trivially a root.
        let mut cursor = to_node.parent.clone();
        while let Some(ancestor_id) = cursor {
            if ancestor_id == from_node.id {
                return Ok(());
            }
            cursor = nodes.get(&ancestor_id).and_then(|n| n.parent.clone());
        }
        Err(UnauthorizedReason::NotInScope)
    }

    /// Send a message from `from` to the agent addressable as `target` (FR-001).
    ///
    /// Never blocks: one brief read-lock for lookup, authorization, and mailbox-sender
    /// clone, dropped before the non-blocking `try_send` — there is no `.await` anywhere on
    /// this path (US-004, NFR-001).
    ///
    /// # Errors
    ///
    /// See [`DeliveryError`] for every rejection case: peer messaging disabled, the body
    /// exceeds the configured limit, `from` is not itself a registered/live node (rejected
    /// the same as an unknown target — never implicitly trusted), `target` does not resolve
    /// (unknown or ambiguous name), the send is unauthorized (different spawn-tree root, out
    /// of scope, or self-addressing), the target has already terminated, or the target's
    /// mailbox is full.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_config::PeerMessagingConfig;
    /// use zeph_subagent::peer::{AgentId, DeliveryError, PeerGroupId, PeerRouter};
    ///
    /// let router = PeerRouter::new(PeerMessagingConfig::default(), None);
    /// let from = AgentId::Root(PeerGroupId::Session);
    ///
    /// // No node is registered for either side, so the send fails fast rather than
    /// // blocking or silently dropping the message (US-004, FR-009).
    /// let err = router.send(&from, "nonexistent", "hello".to_owned()).unwrap_err();
    /// assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    /// ```
    #[tracing::instrument(name = "subagent.mailbox.send", skip(self, body), fields(from = ?from, target))]
    pub fn send(&self, from: &AgentId, target: &str, body: String) -> Result<(), DeliveryError> {
        if !self.config.enabled {
            tracing::warn!("peer message send rejected: peer messaging disabled");
            return Err(DeliveryError::Disabled);
        }
        if body.len() > self.config.max_body_bytes {
            tracing::warn!(
                actual = body.len(),
                max = self.config.max_body_bytes,
                "peer message send rejected: body too large"
            );
            return Err(DeliveryError::BodyTooLarge {
                actual: body.len(),
                max: self.config.max_body_bytes,
            });
        }
        // `target` is LLM-supplied (via `send_peer_message`'s tool argument) just like `body`;
        // cap it too so an unbounded string can't be used to pad allocations or log lines.
        if target.len() > MAX_TARGET_BYTES {
            tracing::warn!(
                actual = target.len(),
                max = MAX_TARGET_BYTES,
                "peer message send rejected: target identifier too large"
            );
            return Err(DeliveryError::TargetTooLarge {
                actual: target.len(),
                max: MAX_TARGET_BYTES,
            });
        }

        let (sender_name, target_display, mailbox_tx) = {
            let nodes = self.nodes.read();
            // Critic M3: a sender whose own registration was already dropped (e.g.
            // mid-cancellation) is rejected the same as an unknown target — never
            // implicitly trusted.
            let Some(from_node) = nodes.get(from) else {
                tracing::warn!("peer message send rejected: sender is not a live node");
                return Err(DeliveryError::TargetNotFound(target.to_owned()));
            };
            let Some(to_node) = Self::resolve_target(&nodes, &from_node.group, target) else {
                tracing::warn!(target, "peer message send rejected: target not found");
                return Err(DeliveryError::TargetNotFound(target.to_owned()));
            };

            let authorize_span = tracing::debug_span!("subagent.mailbox.authorize");
            let auth_result =
                authorize_span.in_scope(|| Self::authorize(&nodes, from_node, to_node));
            if let Err(reason) = auth_result {
                tracing::warn!(
                    ?reason,
                    sender = %from_node.name,
                    target = %to_node.name,
                    "peer message send rejected: unauthorized"
                );
                return Err(DeliveryError::Unauthorized {
                    sender: from_node.name.clone(),
                    target: to_node.name.clone(),
                    reason,
                });
            }

            (
                from_node.name.clone(),
                to_node.name.clone(),
                to_node.mailbox_tx.clone(),
            )
        };

        let masked_body = match self.secret_registry.read().as_ref() {
            Some(registry) => registry.mask(&body),
            None => body,
        };

        let message = PeerMessage {
            sender_id: from.clone(),
            sender_name,
            target_id: AgentId::Task(target_display),
            body: masked_body,
            sent_at: chrono::Utc::now(),
        };

        match mailbox_tx.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(target, "peer message send rejected: mailbox full");
                Err(DeliveryError::MailboxFull(target.to_owned()))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(target, "peer message send rejected: target terminated");
                Err(DeliveryError::TargetTerminated(target.to_owned()))
            }
        }
    }

    /// Return every node `from` is authorized to address, with its relation to `from`
    /// (FR-008). Never leaks a node from another spawn-tree root (NFR-003).
    #[must_use]
    pub fn peers_for(&self, from: &AgentId) -> Vec<PeerDescriptor> {
        let nodes = self.nodes.read();
        let Some(from_node) = nodes.get(from) else {
            return Vec::new();
        };
        nodes
            .values()
            .filter(|n| n.id != from_node.id)
            .filter_map(|n| {
                Self::authorize(&nodes, from_node, n).ok()?;
                let relation = if from_node.parent.as_ref() == Some(&n.id) {
                    PeerRelation::Parent
                } else if n.parent == from_node.parent {
                    PeerRelation::Sibling
                } else {
                    PeerRelation::Child
                };
                Some(PeerDescriptor {
                    id: n.id.clone(),
                    name: n.name.clone(),
                    relation,
                })
            })
            .collect()
    }

    /// Current queued (undelivered) message count for `id`'s mailbox, used for the TUI
    /// unread badge (FR-010). `max_capacity() - capacity()` is O(1) and needs no second
    /// counter to keep in sync. Returns `0` for an unknown or deregistered id.
    #[must_use]
    pub fn mailbox_depth(&self, id: &AgentId) -> usize {
        self.nodes
            .read()
            .get(id)
            .map_or(0, |n| n.mailbox_tx.max_capacity() - n.mailbox_tx.capacity())
    }

    /// The spawn-tree root group `id` is registered under, if it is currently a live node.
    ///
    /// Used by [`SubAgentManager::send_to_subagent`](crate::manager::SubAgentManager::send_to_subagent)
    /// to determine which root (`Session` or a specific `Plan`) it must send *as* in order to
    /// reach `id` — the parent session holds routes for every root it has spawned into, but
    /// only messaging *as the target's own root* satisfies the ancestor-walk authorization
    /// clause (critic round-4 S2).
    #[must_use]
    pub(crate) fn group_of(&self, id: &AgentId) -> Option<PeerGroupId> {
        self.nodes.read().get(id).map(|n| n.group.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(config: PeerMessagingConfig) -> Arc<PeerRouter> {
        PeerRouter::new(config, None)
    }

    fn register_root(router: &Arc<PeerRouter>, group: PeerGroupId, name: &str) -> PeerRegistration {
        let (reg, rx) = router.register(AgentId::Root(group.clone()), name.to_owned(), None, group);
        std::mem::forget(rx);
        reg
    }

    fn register_task(
        router: &Arc<PeerRouter>,
        task_id: &str,
        name: &str,
        parent: AgentId,
        group: PeerGroupId,
    ) -> (PeerRegistration, mpsc::Receiver<PeerMessage>) {
        router.register(
            AgentId::Task(task_id.to_owned()),
            name.to_owned(),
            Some(parent),
            group,
        )
    }

    #[test]
    fn send_to_unknown_target_fails() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let err = r.send(root.id(), "nope", "hi".to_owned()).unwrap_err();
        assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    }

    #[test]
    fn send_from_unregistered_sender_is_rejected_like_unknown_target() {
        // Critic M3: an unknown/dropped sender must never be implicitly trusted.
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (_child_reg, _rx) = register_task(
            &r,
            "child",
            "child",
            root.id().clone(),
            PeerGroupId::Session,
        );
        let ghost = AgentId::Task("never-registered".to_owned());
        let err = r.send(&ghost, "child", "hi".to_owned()).unwrap_err();
        assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    }

    #[test]
    fn sibling_to_sibling_send_succeeds() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (a, _a_rx) = register_task(
            &r,
            "a",
            "researcher",
            root.id().clone(),
            PeerGroupId::Session,
        );
        let (_b, mut b_rx) = register_task(
            &r,
            "b",
            "implementer",
            root.id().clone(),
            PeerGroupId::Session,
        );

        r.send(a.id(), "b", "redirect".to_owned())
            .expect("sibling send");
        let msg = b_rx.try_recv().expect("message delivered");
        assert_eq!(msg.body, "redirect");
        assert_eq!(msg.sender_name, "researcher");
    }

    #[test]
    fn child_to_parent_send_succeeds() {
        let r = router(PeerMessagingConfig::default());
        let mut root_rx = {
            let (reg, rx) = r.register(
                AgentId::Root(PeerGroupId::Session),
                "spawner".to_owned(),
                None,
                PeerGroupId::Session,
            );
            std::mem::forget(reg);
            rx
        };
        let (child, _rx) = register_task(
            &r,
            "child",
            "child",
            AgentId::Root(PeerGroupId::Session),
            PeerGroupId::Session,
        );
        r.send(child.id(), "spawner", "question?".to_owned())
            .expect("child to parent send");
        let msg = root_rx.try_recv().expect("delivered to root");
        assert_eq!(msg.body, "question?");
    }

    #[test]
    fn parent_to_descendant_send_succeeds() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (child, _child_rx) = register_task(
            &r,
            "child",
            "child",
            root.id().clone(),
            PeerGroupId::Session,
        );
        let (_grandchild, mut gc_rx) = register_task(
            &r,
            "grandchild",
            "grandchild",
            child.id().clone(),
            PeerGroupId::Session,
        );

        r.send(root.id(), "grandchild", "go".to_owned())
            .expect("root reaches a grandchild by ancestor walk");
        let msg = gc_rx.try_recv().expect("delivered");
        assert_eq!(msg.body, "go");
    }

    #[test]
    fn cousin_send_is_denied_not_in_scope() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (parent_a, _a_rx) = register_task(
            &r,
            "parent-a",
            "parent-a",
            root.id().clone(),
            PeerGroupId::Session,
        );
        let (parent_b, _b_rx) = register_task(
            &r,
            "parent-b",
            "parent-b",
            root.id().clone(),
            PeerGroupId::Session,
        );
        let (cousin_1, _c1_rx) = register_task(
            &r,
            "cousin-1",
            "cousin-1",
            parent_a.id().clone(),
            PeerGroupId::Session,
        );
        let (_cousin_2, _c2_rx) = register_task(
            &r,
            "cousin-2",
            "cousin-2",
            parent_b.id().clone(),
            PeerGroupId::Session,
        );

        let err = r
            .send(cousin_1.id(), "cousin-2", "hi".to_owned())
            .unwrap_err();
        assert!(matches!(
            err,
            DeliveryError::Unauthorized {
                reason: UnauthorizedReason::NotInScope,
                ..
            }
        ));
    }

    #[test]
    fn cross_group_send_is_denied_as_target_not_found_not_unauthorized() {
        // Critic round-4 S3: cross-group targets must be indistinguishable from nonexistent
        // ones (NFR-003) — `resolve_target` filters to the sender's own group before the
        // id/name match, so this never reaches `authorize`'s `DifferentGroup` branch at all.
        let r = router(PeerMessagingConfig::default());
        let session_root = register_root(&r, PeerGroupId::Session, "session-root");
        let plan_root = register_root(&r, PeerGroupId::Plan("plan-1".to_owned()), "plan-root");
        let (session_task, _rx1) = register_task(
            &r,
            "session-task",
            "session-task",
            session_root.id().clone(),
            PeerGroupId::Session,
        );
        let (_plan_task, _rx2) = register_task(
            &r,
            "plan-task",
            "plan-task",
            plan_root.id().clone(),
            PeerGroupId::Plan("plan-1".to_owned()),
        );

        let err = r
            .send(session_task.id(), "plan-task", "leak?".to_owned())
            .unwrap_err();
        assert!(
            matches!(err, DeliveryError::TargetNotFound(_)),
            "expected TargetNotFound (no existence oracle across groups), got: {err:?}"
        );
        assert_eq!(r.mailbox_depth(&AgentId::Task("plan-task".to_owned())), 0);
    }

    #[test]
    fn sequential_plans_produce_distinct_roots() {
        let r = router(PeerMessagingConfig::default());
        let (plan1_task, _rx1) = register_task(
            &r,
            "p1-task",
            "p1-task",
            AgentId::Root(PeerGroupId::Plan("plan-1".to_owned())),
            PeerGroupId::Plan("plan-1".to_owned()),
        );
        let (_plan2_task, _rx2) = register_task(
            &r,
            "p2-task",
            "p2-task",
            AgentId::Root(PeerGroupId::Plan("plan-2".to_owned())),
            PeerGroupId::Plan("plan-2".to_owned()),
        );

        let err = r
            .send(plan1_task.id(), "p2-task", "straggler".to_owned())
            .unwrap_err();
        assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    }

    #[test]
    fn self_addressing_is_denied() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let err = r.send(root.id(), "spawner", "echo".to_owned()).unwrap_err();
        assert!(matches!(
            err,
            DeliveryError::Unauthorized {
                reason: UnauthorizedReason::SelfAddress,
                ..
            }
        ));
    }

    #[test]
    fn ambiguous_name_resolves_to_target_not_found() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (_a, _a_rx) = register_task(&r, "a", "dup", root.id().clone(), PeerGroupId::Session);
        let (_b, _b_rx) = register_task(&r, "b", "dup", root.id().clone(), PeerGroupId::Session);

        let err = r.send(root.id(), "dup", "hi".to_owned()).unwrap_err();
        assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    }

    #[test]
    fn task_id_resolution_takes_precedence_over_name() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        // A node whose display name collides with another node's task_id.
        let (_a, mut a_rx) = register_task(&r, "a", "b", root.id().clone(), PeerGroupId::Session);
        let (_b, mut b_rx) =
            register_task(&r, "b", "b-name", root.id().clone(), PeerGroupId::Session);

        r.send(root.id(), "b", "by-id".to_owned())
            .expect("send by task_id");
        assert!(
            b_rx.try_recv().is_ok(),
            "task_id match must win over name match"
        );
        assert!(a_rx.try_recv().is_err());
    }

    #[test]
    fn body_too_large_is_rejected_before_lock() {
        let cfg = PeerMessagingConfig {
            max_body_bytes: 4,
            ..PeerMessagingConfig::default()
        };
        let r = router(cfg);
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let err = r
            .send(root.id(), "anyone", "way too long".to_owned())
            .unwrap_err();
        assert!(matches!(err, DeliveryError::BodyTooLarge { .. }));
    }

    #[test]
    fn disabled_config_rejects_every_send() {
        let cfg = PeerMessagingConfig {
            enabled: false,
            ..PeerMessagingConfig::default()
        };
        let r = router(cfg);
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let err = r.send(root.id(), "anyone", "hi".to_owned()).unwrap_err();
        assert!(matches!(err, DeliveryError::Disabled));
    }

    #[test]
    fn mailbox_full_fails_fast_without_awaiting() {
        let cfg = PeerMessagingConfig {
            mailbox_capacity: 1,
            ..PeerMessagingConfig::default()
        };
        let r = router(cfg);
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (_child, _rx) = register_task(
            &r,
            "child",
            "child",
            root.id().clone(),
            PeerGroupId::Session,
        );

        r.send(root.id(), "child", "first".to_owned())
            .expect("first fits");
        let err = r.send(root.id(), "child", "second".to_owned()).unwrap_err();
        assert!(matches!(err, DeliveryError::MailboxFull(_)));
    }

    #[test]
    fn dropped_receiver_yields_target_terminated() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (_child, rx) = register_task(
            &r,
            "child",
            "child",
            root.id().clone(),
            PeerGroupId::Session,
        );
        drop(rx);

        let err = r
            .send(root.id(), "child", "anyone home?".to_owned())
            .unwrap_err();
        assert!(matches!(err, DeliveryError::TargetTerminated(_)));
    }

    #[test]
    fn registration_drop_deregisters_and_send_yields_target_not_found() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (child, rx) = register_task(
            &r,
            "child",
            "child",
            root.id().clone(),
            PeerGroupId::Session,
        );
        drop(rx);
        drop(child);

        let err = r.send(root.id(), "child", "hi".to_owned()).unwrap_err();
        assert!(matches!(err, DeliveryError::TargetNotFound(_)));
    }

    #[test]
    fn mailbox_depth_tracks_queued_and_drained() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (child, mut rx) = register_task(
            &r,
            "child",
            "child",
            root.id().clone(),
            PeerGroupId::Session,
        );

        assert_eq!(r.mailbox_depth(child.id()), 0);
        r.send(root.id(), "child", "one".to_owned()).expect("send");
        r.send(root.id(), "child", "two".to_owned()).expect("send");
        assert_eq!(r.mailbox_depth(child.id()), 2);

        rx.try_recv().expect("drain one");
        assert_eq!(r.mailbox_depth(child.id()), 1);
    }

    #[test]
    fn peers_for_never_leaks_another_group() {
        let r = router(PeerMessagingConfig::default());
        let session_root = register_root(&r, PeerGroupId::Session, "session-root");
        let plan_root = register_root(&r, PeerGroupId::Plan("p1".to_owned()), "plan-root");
        let (session_task, _rx1) = register_task(
            &r,
            "s-task",
            "s-task",
            session_root.id().clone(),
            PeerGroupId::Session,
        );
        let (_plan_task, _rx2) = register_task(
            &r,
            "p-task",
            "p-task",
            plan_root.id().clone(),
            PeerGroupId::Plan("p1".to_owned()),
        );

        let peers = r.peers_for(session_task.id());
        assert!(
            peers
                .iter()
                .all(|p| p.name != "p-task" && p.name != "plan-root")
        );
        assert!(peers.iter().any(|p| p.name == "session-root"));
    }

    #[test]
    fn peers_for_labels_parent_sibling_child() {
        let r = router(PeerMessagingConfig::default());
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (a, _a_rx) = register_task(&r, "a", "a", root.id().clone(), PeerGroupId::Session);
        let (_b, _b_rx) = register_task(&r, "b", "b", root.id().clone(), PeerGroupId::Session);
        let (_c, _c_rx) = register_task(&r, "c", "c", a.id().clone(), PeerGroupId::Session);

        let peers = r.peers_for(a.id());
        let parent = peers
            .iter()
            .find(|p| p.name == "spawner")
            .expect("parent listed");
        assert_eq!(parent.relation, PeerRelation::Parent);
        let sibling = peers
            .iter()
            .find(|p| p.name == "b")
            .expect("sibling listed");
        assert_eq!(sibling.relation, PeerRelation::Sibling);
        let child = peers.iter().find(|p| p.name == "c").expect("child listed");
        assert_eq!(child.relation, PeerRelation::Child);
    }

    #[test]
    fn secret_registered_in_mask_registry_is_masked_in_relayed_body() {
        let registry = Arc::new(SecretMaskRegistry::new());
        registry.register(
            "SOME_KEY",
            "super-secret-value",
            zeph_sanitizer::secret_mask::SecretCategory::Generic,
        );
        let r = PeerRouter::new(PeerMessagingConfig::default(), Some(Arc::clone(&registry)));
        let root = register_root(&r, PeerGroupId::Session, "spawner");
        let (_child, mut rx) = register_task(
            &r,
            "child",
            "child",
            root.id().clone(),
            PeerGroupId::Session,
        );

        r.send(root.id(), "child", "value is super-secret-value".to_owned())
            .expect("send");
        let msg = rx.try_recv().expect("delivered");
        assert!(!msg.body.contains("super-secret-value"));
    }
}
