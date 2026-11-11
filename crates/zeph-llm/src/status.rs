// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Typed, bounded, latest-wins status channel for background producers.
//!
//! A status indicator shows what the agent is doing behind the scenes ("Searching memory…").
//! This module replaces the unbounded `mpsc` plus empty-string convention with:
//!
//! - [`StatusText`]: a validated, non-empty message, so a producer can never clear the
//!   indicator by accident;
//! - [`StatusUpdate`]: an explicit `Set` / `Clear` protocol;
//! - [`status_channel`]: a [`tokio::sync::watch`]-backed channel holding at most one entry, so a
//!   burst of updates collapses to the latest and memory stays bounded;
//! - [`StatusSource`]: a closed identity per producer. A `clear` is honored only when its source
//!   has an outstanding `set` ("armed"): it then disarms that source and empties the slot
//!   whatever source wrote the current text, so a nested status (a provider retry inside a
//!   memory operation) is cleared by its owner. A clear from a source with no outstanding `set`
//!   is ignored.
//!
//! Status is replaceable state: updates collapse to the latest. A one-shot notice that must not
//! be lost (an OAuth URL, a skipped-skill warning) must also go through a non-collapsing path
//! such as a `tracing` warning or `Channel::send_notice`.
//!
//! Clear-if-armed applies to this background lane only. The agent-turn lane
//! (`Channel::send_status` / `clear_status`) drives the same visible indicator and clears it
//! unconditionally.
//!
//! # Examples
//!
//! ```
//! use zeph_llm::status::{StatusSource, StatusUpdate, status_channel};
//!
//! # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
//! let (tx, mut rx) = status_channel(StatusSource::Memory);
//! tx.set("Searching memory…").unwrap();
//! tx.set("Embedding query…").unwrap();
//!
//! // Updates collapse: the receiver sees only the latest one.
//! let update = rx.next().await.unwrap();
//! assert_eq!(update.to_string(), "Set(Embedding query…)");
//!
//! tx.clear().unwrap();
//! assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
//! # });
//! ```

use std::borrow::Cow;
use std::fmt;

use tokio::sync::watch;

/// A non-empty status message.
///
/// Construction fails for empty or whitespace-only text, so an empty status is unrepresentable.
///
/// # Examples
///
/// ```
/// use zeph_llm::status::StatusText;
///
/// assert!(StatusText::new("Connecting…").is_some());
/// assert!(StatusText::new("").is_none());
/// assert!(StatusText::new("   ").is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusText(Cow<'static, str>);

impl StatusText {
    /// Validate `text`, returning `None` when it is empty or whitespace-only.
    #[must_use]
    pub fn new(text: impl Into<Cow<'static, str>>) -> Option<Self> {
        let text = text.into();
        (!text.trim().is_empty()).then_some(Self(text))
    }

    /// The message text; never empty.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the status and return the owned text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0.into_owned()
    }
}

impl fmt::Display for StatusText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of a status producer, fixed at wiring time.
///
/// The set is closed: one variant per wiring point, so the channel can tell which producers
/// have an outstanding status (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusSource {
    /// LLM provider (retries, rate limits, model switches).
    Provider,
    /// MCP client (connection, tool decode).
    Mcp,
    /// Code indexer.
    Index,
    /// Memory subsystem (search, consolidation).
    Memory,
    /// Speech-to-text.
    Stt,
    /// Git worktree lifecycle.
    Worktree,
    /// The agent loop itself.
    Agent,
}

impl StatusSource {
    const fn bit(self) -> u8 {
        match self {
            Self::Provider => 1 << 0,
            Self::Mcp => 1 << 1,
            Self::Index => 1 << 2,
            Self::Memory => 1 << 3,
            Self::Stt => 1 << 4,
            Self::Worktree => 1 << 5,
            Self::Agent => 1 << 6,
        }
    }
}

/// One bit per [`StatusSource`]: the sources that have an outstanding `set`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ArmedSources(u8);

impl ArmedSources {
    fn arm(&mut self, source: StatusSource) {
        self.0 |= source.bit();
    }

    fn disarm(&mut self, source: StatusSource) {
        self.0 &= !source.bit();
    }

    fn is_armed(self, source: StatusSource) -> bool {
        self.0 & source.bit() != 0
    }
}

/// State held in the watch slot: the visible entry plus the armed sources.
#[derive(Debug, Clone, Default)]
struct Board {
    entry: Option<StatusEntry>,
    armed: ArmedSources,
}

/// An explicit status command: show a message or clear the indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusUpdate {
    /// Show this message.
    Set(StatusText),
    /// Clear the indicator.
    Clear,
}

impl fmt::Display for StatusUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set(text) => write!(f, "Set({text})"),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

/// The status currently held by the channel: its message and the producer that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    source: StatusSource,
    text: StatusText,
}

impl StatusEntry {
    /// The producer that set this status.
    #[must_use]
    pub fn source(&self) -> StatusSource {
        self.source
    }

    /// The status message.
    #[must_use]
    pub fn text(&self) -> &StatusText {
        &self.text
    }
}

/// Every receiver of the status channel has been dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("status receiver closed")]
pub struct StatusClosed;

/// Producer handle of the status channel. Cheap to clone.
///
/// Every clone carries a [`StatusSource`]; use [`with_source`](Self::with_source) at wiring to
/// label the clone handed to each subsystem.
#[derive(Debug, Clone)]
pub struct StatusSender {
    source: StatusSource,
    tx: watch::Sender<Board>,
}

impl StatusSender {
    /// A clone of this sender that labels its updates with `source`.
    #[must_use]
    pub fn with_source(&self, source: StatusSource) -> Self {
        Self {
            source,
            tx: self.tx.clone(),
        }
    }

    /// Apply `update` to the channel, replacing any pending value (latest wins).
    ///
    /// `Set` replaces the slot and arms this sender's source. `Clear` is honored only when this
    /// source is armed: it disarms the source and empties the slot whatever source wrote the
    /// current text. A clear from an unarmed source is ignored and the receiver is not woken.
    ///
    /// # Errors
    ///
    /// Returns [`StatusClosed`] when no receiver is left; callers treat it as "stop producing".
    pub fn send(&self, update: StatusUpdate) -> Result<(), StatusClosed> {
        let source = self.source;
        // `send_if_modified` always stores and never reports closure, unlike `send`, which
        // refuses to store with no receiver. It is the only API that lets `Clear` be a no-op.
        self.tx.send_if_modified(|board| match update {
            StatusUpdate::Set(text) => {
                board.entry = Some(StatusEntry { source, text });
                board.armed.arm(source);
                true
            }
            StatusUpdate::Clear => {
                if !board.armed.is_armed(source) {
                    return false;
                }
                board.armed.disarm(source);
                board.entry.take().is_some()
            }
        });
        self.closed_result()
    }

    /// Show `text`. Empty or whitespace-only text is a logged no-op.
    ///
    /// # Errors
    ///
    /// Returns [`StatusClosed`] when no receiver is left, also for the empty-text no-op, so
    /// producers can keep using `is_err()` as their loop-exit condition.
    pub fn set(&self, text: impl Into<Cow<'static, str>>) -> Result<(), StatusClosed> {
        if let Some(text) = StatusText::new(text) {
            self.send(StatusUpdate::Set(text))
        } else {
            tracing::debug!(source = ?self.source, "ignoring empty status text");
            self.closed_result()
        }
    }

    /// Clear the status if this sender's source has an outstanding `set` (clear-if-armed).
    ///
    /// # Errors
    ///
    /// Returns [`StatusClosed`] when no receiver is left.
    pub fn clear(&self) -> Result<(), StatusClosed> {
        self.send(StatusUpdate::Clear)
    }

    fn closed_result(&self) -> Result<(), StatusClosed> {
        if self.tx.is_closed() {
            Err(StatusClosed)
        } else {
            Ok(())
        }
    }
}

/// Consumer handle of the status channel.
#[derive(Debug)]
pub struct StatusReceiver {
    rx: watch::Receiver<Board>,
}

impl StatusReceiver {
    /// Wait for the next change and return it as an update.
    ///
    /// Intermediate updates are collapsed: only the latest state is observed. Returns `None`
    /// once every sender is dropped and the last change was consumed.
    pub async fn next(&mut self) -> Option<StatusUpdate> {
        self.rx.changed().await.ok()?;
        let entry = self.rx.borrow_and_update().entry.clone();
        Some(match entry {
            Some(entry) => StatusUpdate::Set(entry.text),
            None => StatusUpdate::Clear,
        })
    }
}

/// Create a status channel whose sender is labelled with `source`.
///
/// # Examples
///
/// ```
/// use zeph_llm::status::{StatusSource, status_channel};
///
/// let (tx, rx) = status_channel(StatusSource::Mcp);
/// drop(rx);
/// assert!(tx.set("Connecting to MCP server…").is_err());
/// ```
#[must_use]
pub fn status_channel(source: StatusSource) -> (StatusSender, StatusReceiver) {
    let (tx, rx) = watch::channel(Board::default());
    (StatusSender { source, tx }, StatusReceiver { rx })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn text(s: &'static str) -> StatusText {
        StatusText::new(s).unwrap()
    }

    #[test]
    fn status_text_rejects_empty_and_whitespace() {
        assert!(StatusText::new("").is_none());
        assert!(StatusText::new(" \t\n").is_none());
        assert_eq!(StatusText::new("ok").unwrap().as_str(), "ok");
        assert_eq!(
            StatusText::new(String::from("ok")).unwrap().into_string(),
            "ok"
        );
    }

    #[tokio::test]
    async fn updates_collapse_to_latest() {
        let (tx, mut rx) = status_channel(StatusSource::Memory);
        tx.set("one").unwrap();
        tx.set("two").unwrap();
        tx.set("three").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("three"))));
    }

    #[tokio::test]
    async fn repeated_identical_set_still_notifies() {
        let (tx, mut rx) = status_channel(StatusSource::Memory);
        tx.set("busy").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("busy"))));
        tx.set("busy").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("busy"))));
    }

    #[tokio::test]
    async fn clear_by_same_source_is_delivered() {
        let (tx, mut rx) = status_channel(StatusSource::Memory);
        tx.set("working").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("working"))));
        tx.clear().unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
    }

    async fn assert_silent(rx: &mut StatusReceiver) {
        let woken = tokio::time::timeout(Duration::from_millis(50), rx.next()).await;
        assert!(woken.is_err(), "the receiver must not be woken");
    }

    #[tokio::test]
    async fn owner_clear_removes_a_nested_status_from_another_source() {
        let (agent, mut rx) = status_channel(StatusSource::Agent);
        let provider = agent.with_source(StatusSource::Provider);
        agent.set("sidequest").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("sidequest"))));
        provider.set("retrying").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("retrying"))));

        agent.clear().unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
    }

    #[tokio::test]
    async fn clear_from_unarmed_source_is_ignored_and_does_not_notify() {
        let (agent, mut rx) = status_channel(StatusSource::Agent);
        let provider = agent.with_source(StatusSource::Provider);
        agent.set("busy").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("busy"))));

        provider.clear().unwrap();
        assert_silent(&mut rx).await;

        agent.clear().unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
    }

    #[tokio::test]
    async fn repeated_clear_is_a_no_op() {
        let (agent, mut rx) = status_channel(StatusSource::Agent);
        agent.set("busy").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("busy"))));
        agent.clear().unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));

        agent.clear().unwrap();
        assert_silent(&mut rx).await;
    }

    #[tokio::test]
    async fn source_can_rearm_after_clear() {
        let (agent, mut rx) = status_channel(StatusSource::Agent);
        agent.set("first").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("first"))));
        agent.clear().unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));

        agent.set("second").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("second"))));
        agent.clear().unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
    }

    #[test]
    fn send_reports_closed_when_receiver_dropped() {
        let (tx, rx) = status_channel(StatusSource::Agent);
        assert!(tx.set("alive").is_ok());
        drop(rx);
        assert_eq!(tx.set("dead"), Err(StatusClosed));
        assert_eq!(tx.clear(), Err(StatusClosed));
    }

    #[test]
    fn empty_set_is_a_no_op_that_still_reports_closure() {
        let (tx, rx) = status_channel(StatusSource::Agent);
        assert!(tx.set("").is_ok());
        assert!(tx.set("  ").is_ok());
        drop(rx);
        assert_eq!(tx.set(""), Err(StatusClosed));
    }

    #[tokio::test]
    async fn empty_set_does_not_wake_the_receiver() {
        let (tx, mut rx) = status_channel(StatusSource::Agent);
        tx.set("").unwrap();
        let woken = tokio::time::timeout(Duration::from_millis(50), rx.next()).await;
        assert!(woken.is_err());
    }

    #[tokio::test]
    async fn receiver_ends_when_all_senders_drop() {
        let (tx, mut rx) = status_channel(StatusSource::Agent);
        let clone = tx.with_source(StatusSource::Memory);
        drop(tx);
        drop(clone);
        assert_eq!(rx.next().await, None);
    }

    #[tokio::test]
    async fn final_clear_before_drop_is_still_delivered() {
        let (tx, mut rx) = status_channel(StatusSource::Agent);
        tx.set("busy").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("busy"))));
        tx.clear().unwrap();
        drop(tx);
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
        assert_eq!(rx.next().await, None);
    }
}
