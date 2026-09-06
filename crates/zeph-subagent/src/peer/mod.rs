// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live, addressable peer-to-peer messaging between sub-agents and their spawner
//! (spec `046-subagent-peer-messaging-parity`, issue #5871).
//!
//! # Architecture
//!
//! Routing state lives in an [`Arc<PeerRouter>`][router::PeerRouter] shared between
//! [`SubAgentManager`](crate::manager::SubAgentManager) and every spawned sub-agent's own
//! tool executor decorator ([`PeerToolExecutor`]) — never on
//! [`SubAgentHandle`](crate::manager::SubAgentHandle) itself, since a running sub-agent task
//! cannot reach back into the manager that owns its handle. A send is a direct,
//! non-blocking `try_send` from the sender's tool call into the target's mailbox; it never
//! touches the parent agent and adds no new `tokio::spawn` call site.
//!
//! Only messages addressed to the parent agent itself are drained by the parent, via
//! [`SubAgentManager::try_recv_peer_message`](crate::manager::SubAgentManager::try_recv_peer_message).

mod router;
mod tools;

pub use router::{PeerRegistration, PeerRouter};
pub use tools::PeerToolExecutor;

/// Addressable identity of a node in the peer graph.
///
/// [`Root`](Self::Root) is the parent [`Agent`](crate) for a given spawn scope;
/// [`Task`](Self::Task) is a spawned sub-agent, keyed by its `task_id`.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::peer::{AgentId, PeerGroupId};
///
/// let root = AgentId::Root(PeerGroupId::Session);
/// let task = AgentId::Task("abc-123".to_owned());
/// assert_ne!(root, task);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentId {
    /// The parent agent for one [`PeerGroupId`] spawn-tree root.
    Root(PeerGroupId),
    /// A spawned sub-agent, keyed by its `task_id`.
    Task(String),
}

/// Root identity of one spawn tree. Distinct groups never address each other
/// (spec `046` US-003).
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::peer::PeerGroupId;
///
/// let session = PeerGroupId::Session;
/// let plan = PeerGroupId::Plan("plan-run-42".to_owned());
/// assert_ne!(session, plan);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PeerGroupId {
    /// The interactive session's own root — `/agent run`, `/agent spawn`, `/agent resume`.
    Session,
    /// One orchestration plan execution, keyed by a driver-supplied plan run id.
    Plan(String),
}

/// A single message sent between two addressable agents (spawner, coordinator, or sibling
/// sub-agent); delivered in mailbox arrival order, best-effort (FR-011).
#[derive(Debug, Clone)]
pub struct PeerMessage {
    /// The sender's addressable identity.
    pub sender_id: AgentId,
    /// The sender's display name, for a human-readable notice.
    pub sender_name: String,
    /// The recipient's addressable identity.
    pub target_id: AgentId,
    /// The message body. Untrusted content — sanitized on read (NFR-007).
    pub body: String,
    /// Wall-clock time the message was sent.
    pub sent_at: chrono::DateTime<chrono::Utc>,
}

/// One entry in [`PeerRouter::peers_for`][router::PeerRouter::peers_for]'s result: an
/// addressable peer the querying agent is authorized to message, and its relation to it
/// (FR-008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDescriptor {
    /// The peer's addressable identity.
    pub id: AgentId,
    /// The peer's display name.
    pub name: String,
    /// The peer's relation to the querying agent.
    pub relation: PeerRelation,
}

/// Relation of a [`PeerDescriptor`] to the agent that queried
/// [`PeerRouter::peers_for`][router::PeerRouter::peers_for].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRelation {
    /// The querying agent's parent (or root).
    Parent,
    /// Another sub-agent sharing the same parent as the querying agent.
    Sibling,
    /// A descendant of the querying agent.
    Child,
}

/// Why a [`PeerRouter::send`][router::PeerRouter::send] was refused.
///
/// Kept distinct from a single boolean so the deny branch is assertable in tests and legible
/// in `tracing` events (NFR-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnauthorizedReason {
    /// Sender and target belong to different spawn-tree roots (US-003).
    DifferentGroup,
    /// Same tree, but the target is neither parent, sibling, nor descendant of the sender.
    NotInScope,
    /// The sender attempted to address itself.
    SelfAddress,
}

/// Failure to deliver a [`PeerMessage`], returned by
/// [`PeerRouter::send`][router::PeerRouter::send] and
/// [`SubAgentManager::send_to_subagent`](crate::manager::SubAgentManager::send_to_subagent).
#[derive(Debug, Clone, thiserror::Error)]
pub enum DeliveryError {
    /// No agent is addressable by the given `task_id` or display name.
    #[error("no agent is addressable as '{0}'")]
    TargetNotFound(String),
    /// The target agent's route was deregistered (collected, cancelled, or dropped) before
    /// delivery.
    #[error("target agent '{0}' has already terminated")]
    TargetTerminated(String),
    /// The sender is not authorized to message the target (US-003, FR-004).
    #[error("'{sender}' is not authorized to message '{target}': {reason:?}")]
    Unauthorized {
        /// The sender's display name.
        sender: String,
        /// The target's display name.
        target: String,
        /// Why the send was refused.
        reason: UnauthorizedReason,
    },
    /// The target's mailbox is at capacity (FR-005, NFR-002).
    #[error("target agent '{0}' mailbox is full")]
    MailboxFull(String),
    /// The message body exceeds `peer_messaging.max_body_bytes`.
    #[error("message body exceeds the configured limit ({actual} > {max} bytes)")]
    BodyTooLarge {
        /// The body's actual size in bytes.
        actual: usize,
        /// The configured maximum.
        max: usize,
    },
    /// The `target` identifier string exceeds the router's internal length cap.
    #[error("target identifier exceeds the maximum length ({actual} > {max} bytes)")]
    TargetTooLarge {
        /// The target string's actual size in bytes.
        actual: usize,
        /// The configured maximum.
        max: usize,
    },
    /// Peer messaging is disabled by configuration (`peer_messaging.enabled = false`).
    #[error("peer messaging is disabled by configuration")]
    Disabled,
}
