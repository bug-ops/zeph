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
//! be lost (an OAuth URL, a skipped-skill warning) goes through the notice queue
//! ([`StatusSender::notice`]): a small bounded queue that never collapses, delivered to the
//! receiver as [`StatusEvent::Notice`] ahead of any pending status update.
//!
//! Clear-if-armed applies to this background lane only. The agent-turn lane
//! (`Channel::send_status` / `clear_status`) is a separate lane on the consumer side, so a turn
//! clear never hides a background status (see the TUI status board).
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

use tokio::sync::{mpsc, watch};

/// Capacity of the notice queue. A full queue drops the newest notice with a `warn` log.
const NOTICE_QUEUE_CAPACITY: usize = 16;

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

/// One item delivered by [`StatusReceiver::next_event`].
///
/// # Examples
///
/// ```
/// use zeph_llm::status::{StatusEvent, StatusSource, status_channel};
///
/// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
/// let (tx, mut rx) = status_channel(StatusSource::Mcp);
/// tx.set("Waiting for OAuth: a").unwrap();
/// tx.notice("Open https://auth.example/a").unwrap();
///
/// // Notices are delivered first and never collapse.
/// assert!(matches!(rx.next_event().await, Some(StatusEvent::Notice(_))));
/// assert!(matches!(rx.next_event().await, Some(StatusEvent::Update(_))));
/// # });
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEvent {
    /// The latest-wins status changed.
    Update(StatusUpdate),
    /// A one-shot message that must reach the user.
    Notice(StatusText),
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
    notices: mpsc::Sender<StatusText>,
}

impl StatusSender {
    /// A clone of this sender that labels its updates with `source`.
    #[must_use]
    pub fn with_source(&self, source: StatusSource) -> Self {
        Self {
            source,
            tx: self.tx.clone(),
            notices: self.notices.clone(),
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

    /// Show `text` for as long as the returned guard lives; dropping it clears the status.
    ///
    /// Preferred over a bare [`set`](Self::set): the clear on every exit path (early return,
    /// `?`, cancellation) is enforced by the guard. Text that reports a one-shot result or a
    /// warning belongs in [`notice`](Self::notice) instead. Empty text is a logged no-op set,
    /// and the guard still clears on drop.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::status::{StatusSource, StatusUpdate, status_channel};
    ///
    /// # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
    /// let (tx, mut rx) = status_channel(StatusSource::Memory);
    /// {
    ///     let _scope = tx.scope("Searching memory…");
    ///     assert_eq!(rx.next().await.unwrap().to_string(), "Set(Searching memory…)");
    /// }
    /// assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
    /// # });
    /// ```
    #[must_use = "dropping the scope clears the status"]
    pub fn scope(&self, text: impl Into<Cow<'static, str>>) -> StatusScope {
        let _ = self.set(text);
        StatusScope {
            sender: self.clone(),
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

    /// Queue a one-shot notice that must not collapse (an OAuth URL, a warning).
    ///
    /// Unlike [`set`](Self::set), notices are never replaced by later ones. The queue is
    /// bounded; when it is full the notice is dropped with a `warn` log and `Ok` is returned, so
    /// a slow consumer cannot stall the producer. Empty or whitespace-only text is a logged
    /// no-op.
    ///
    /// # Errors
    ///
    /// Returns [`StatusClosed`] when no receiver is left.
    pub fn notice(&self, text: impl Into<Cow<'static, str>>) -> Result<(), StatusClosed> {
        let Some(text) = StatusText::new(text) else {
            tracing::debug!(source = ?self.source, "ignoring empty status notice");
            return self.closed_result();
        };
        match self.notices.try_send(text) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(source = ?self.source, "status notice queue full, dropping notice");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(StatusClosed),
        }
    }

    fn closed_result(&self) -> Result<(), StatusClosed> {
        if self.tx.is_closed() {
            Err(StatusClosed)
        } else {
            Ok(())
        }
    }
}

/// RAII guard returned by [`StatusSender::scope`]: the status it set is cleared on drop.
///
/// Clearing is clear-if-armed, so a nested status from another source is removed by its owner
/// and a clear after the receiver is gone is silently ignored.
#[derive(Debug)]
#[must_use = "dropping the scope clears the status"]
pub struct StatusScope {
    sender: StatusSender,
}

impl StatusScope {
    /// Replace the scope's text while the work continues (latest wins).
    pub fn update(&self, text: impl Into<Cow<'static, str>>) {
        let _ = self.sender.set(text);
    }
}

impl Drop for StatusScope {
    fn drop(&mut self) {
        let _ = self.sender.clear();
    }
}

/// A status scope opened on the first message of an operation, for code paths that only
/// sometimes have something to report (a router fallback). The status is cleared when the
/// reporter is dropped, so it never outlives the operation.
#[derive(Debug)]
pub(crate) struct StatusReporter {
    sender: Option<StatusSender>,
    scope: Option<StatusScope>,
}

impl StatusReporter {
    pub(crate) fn new(sender: Option<StatusSender>) -> Self {
        Self {
            sender,
            scope: None,
        }
    }

    pub(crate) fn show(&mut self, text: impl Into<Cow<'static, str>>) {
        match (&self.scope, &self.sender) {
            (Some(scope), _) => scope.update(text),
            (None, Some(sender)) => self.scope = Some(sender.scope(text)),
            (None, None) => {}
        }
    }
}

/// Consumer handle of the status channel.
#[derive(Debug)]
pub struct StatusReceiver {
    rx: watch::Receiver<Board>,
    notices: mpsc::Receiver<StatusText>,
}

impl StatusReceiver {
    /// Wait for the next status change and return it as an update.
    ///
    /// Intermediate updates are collapsed: only the latest state is observed. Notices are
    /// skipped (use [`next_event`](Self::next_event) to receive them). Returns `None` once every
    /// sender is dropped and the last change was consumed.
    pub async fn next(&mut self) -> Option<StatusUpdate> {
        loop {
            match self.next_event().await? {
                StatusEvent::Update(update) => return Some(update),
                StatusEvent::Notice(_) => {}
            }
        }
    }

    /// Wait for the next status change or notice.
    ///
    /// Pending notices are delivered before status updates. Returns `None` once every sender is
    /// dropped, the notice queue is drained and the last status change was consumed.
    pub async fn next_event(&mut self) -> Option<StatusEvent> {
        tokio::select! {
            biased;
            notice = self.notices.recv() => match notice {
                Some(text) => Some(StatusEvent::Notice(text)),
                None => next_update(&mut self.rx).await.map(StatusEvent::Update),
            },
            update = next_update(&mut self.rx) => update.map(StatusEvent::Update),
        }
    }
}

async fn next_update(rx: &mut watch::Receiver<Board>) -> Option<StatusUpdate> {
    rx.changed().await.ok()?;
    let entry = rx.borrow_and_update().entry.clone();
    Some(match entry {
        Some(entry) => StatusUpdate::Set(entry.text),
        None => StatusUpdate::Clear,
    })
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
    let (notice_tx, notice_rx) = mpsc::channel(NOTICE_QUEUE_CAPACITY);
    (
        StatusSender {
            source,
            tx,
            notices: notice_tx,
        },
        StatusReceiver {
            rx,
            notices: notice_rx,
        },
    )
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
    async fn dropping_a_scope_delivers_clear() {
        let (tx, mut rx) = status_channel(StatusSource::Memory);
        {
            let scope = tx.scope("working");
            assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("working"))));
            scope.update("still working");
            assert_eq!(
                rx.next().await,
                Some(StatusUpdate::Set(text("still working")))
            );
        }
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
    }

    #[tokio::test]
    async fn scope_owner_clears_a_nested_status_from_another_source() {
        let (agent, mut rx) = status_channel(StatusSource::Agent);
        let provider = agent.with_source(StatusSource::Provider);
        let scope = agent.scope("sidequest");
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("sidequest"))));
        provider.set("retrying").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("retrying"))));

        drop(scope);
        assert_eq!(rx.next().await, Some(StatusUpdate::Clear));
    }

    #[test]
    fn scope_drop_after_receiver_closed_does_not_panic() {
        let (tx, rx) = status_channel(StatusSource::Agent);
        let scope = tx.scope("busy");
        drop(rx);
        drop(scope);
    }

    #[tokio::test]
    async fn notices_never_collapse_and_precede_status_updates() {
        let (tx, mut rx) = status_channel(StatusSource::Mcp);
        tx.set("Waiting for OAuth: a").unwrap();
        tx.notice("url-a").unwrap();
        tx.notice("url-b").unwrap();

        assert_eq!(
            rx.next_event().await,
            Some(StatusEvent::Notice(text("url-a")))
        );
        assert_eq!(
            rx.next_event().await,
            Some(StatusEvent::Notice(text("url-b")))
        );
        assert_eq!(
            rx.next_event().await,
            Some(StatusEvent::Update(StatusUpdate::Set(text(
                "Waiting for OAuth: a"
            ))))
        );
    }

    #[tokio::test]
    async fn next_skips_notices_and_still_yields_updates() {
        let (tx, mut rx) = status_channel(StatusSource::Mcp);
        tx.notice("url").unwrap();
        tx.set("busy").unwrap();
        assert_eq!(rx.next().await, Some(StatusUpdate::Set(text("busy"))));
    }

    #[tokio::test]
    async fn full_notice_queue_drops_newest_without_error() {
        let (tx, mut rx) = status_channel(StatusSource::Mcp);
        for _ in 0..NOTICE_QUEUE_CAPACITY {
            tx.notice("queued").unwrap();
        }
        assert!(tx.notice("overflow").is_ok());
        for _ in 0..NOTICE_QUEUE_CAPACITY {
            assert_eq!(
                rx.next_event().await,
                Some(StatusEvent::Notice(text("queued")))
            );
        }
        assert_silent_events(&mut rx).await;
    }

    async fn assert_silent_events(rx: &mut StatusReceiver) {
        let woken = tokio::time::timeout(Duration::from_millis(50), rx.next_event()).await;
        assert!(woken.is_err(), "no further event expected");
    }

    #[test]
    fn notice_reports_closed_and_ignores_empty_text() {
        let (tx, rx) = status_channel(StatusSource::Mcp);
        assert!(tx.notice("  ").is_ok());
        drop(rx);
        assert_eq!(tx.notice("url"), Err(StatusClosed));
    }

    #[tokio::test]
    async fn pending_notice_is_delivered_after_senders_drop() {
        let (tx, mut rx) = status_channel(StatusSource::Mcp);
        tx.notice("last words").unwrap();
        drop(tx);
        assert_eq!(
            rx.next_event().await,
            Some(StatusEvent::Notice(text("last words")))
        );
        assert_eq!(rx.next_event().await, None);
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
