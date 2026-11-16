// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared deny-on-timeout confirmation loop used by all channel adapters that
//! support interactive `yes`/`no` prompts (Telegram, Discord, Slack).
//!
//! The [`ConfirmLoop`] trait extracts the send-prompt / wait-for-reply-with-deadline
//! / compare-case-insensitively-against-"yes" logic that would otherwise be
//! duplicated across Discord, Slack, and Telegram's
//! [`Channel::confirm`](zeph_core::channel::Channel::confirm) implementations.

use std::collections::VecDeque;

use tokio::sync::mpsc;
use zeph_core::channel::ChannelError;

/// Outcome of [`ConfirmLoop::run_reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyOutcome {
    /// An in-scope reply, returned unchanged.
    Text(String),
    /// The incoming-message receiver closed before a reply arrived.
    Closed,
}

/// Upper bound on messages held back by [`Inbox::defer`].
const MAX_DEFERRED: usize = 256;

/// Incoming-message receiver with a front-of-line queue for messages that were
/// consumed while a confirmation was open but did not belong to it.
///
/// [`recv`](Self::recv) and [`try_recv`](Self::try_recv) drain the queue before
/// touching the underlying channel, so deferred messages are delivered first and
/// in their original order.
#[derive(Debug)]
pub struct Inbox<T> {
    rx: mpsc::Receiver<T>,
    deferred: VecDeque<T>,
}

impl<T> From<mpsc::Receiver<T>> for Inbox<T> {
    fn from(rx: mpsc::Receiver<T>) -> Self {
        Self {
            rx,
            deferred: VecDeque::new(),
        }
    }
}

impl<T> Inbox<T> {
    /// Await the next message, deferred ones first. Cancel-safe.
    pub async fn recv(&mut self) -> Option<T> {
        match self.deferred.pop_front() {
            Some(msg) => Some(msg),
            None => self.rx.recv().await,
        }
    }

    /// The underlying receiver, bypassing deferred messages.
    ///
    /// The confirmation loops read from it and park out-of-scope messages with
    /// [`defer`](Self::defer) as they arrive (cancel-safe), so they never re-read them.
    pub fn receiver_mut(&mut self) -> &mut mpsc::Receiver<T> {
        &mut self.rx
    }

    /// Return the next message without waiting, deferred ones first.
    pub fn try_recv(&mut self) -> Option<T> {
        self.deferred
            .pop_front()
            .or_else(|| self.rx.try_recv().ok())
    }

    /// Swap in a fresh underlying receiver, keeping already-deferred messages.
    pub fn replace_receiver(&mut self, rx: mpsc::Receiver<T>) {
        self.rx = rx;
    }

    /// Queue `messages` behind any already-deferred ones, ahead of the channel.
    ///
    /// The queue holds at most 256 messages; the oldest are dropped with a
    /// warning when the bound is exceeded.
    pub fn defer(&mut self, messages: impl IntoIterator<Item = T>) {
        self.deferred.extend(messages);
        let excess = self.deferred.len().saturating_sub(MAX_DEFERRED);
        if excess > 0 {
            self.deferred.drain(..excess);
            tracing::warn!(
                dropped = excess,
                "deferred inbox full — dropped oldest messages"
            );
        }
    }
}

/// Shared send-prompt-and-await-reply confirmation loop for channel adapters.
///
/// Implementors must provide:
/// - [`Incoming`](ConfirmLoop::Incoming) — the adapter's incoming-message type.
/// - [`confirm_label`](ConfirmLoop::confirm_label) — a short label used in log lines.
/// - [`confirm_receiver`](ConfirmLoop::confirm_receiver) — exclusive access to the
///   incoming-message receiver.
/// - [`confirm_accepts`](ConfirmLoop::confirm_accepts) — whether a message is in
///   scope to answer this confirmation.
/// - [`confirm_reply_text`](ConfirmLoop::confirm_reply_text) — the reply text to
///   compare against `"yes"`.
/// - [`confirm_send_prompt`](ConfirmLoop::confirm_send_prompt) — send the
///   formatted prompt out on the channel.
///
/// The default method [`run_confirm`](ConfirmLoop::run_confirm) encodes the shared
/// deadline loop: it sends the prompt with a timeout suffix, waits up to
/// [`CONFIRM_TIMEOUT`](crate::CONFIRM_TIMEOUT) for a matching reply, and denies
/// (`Ok(false)`) on timeout or channel close — it never returns `Err` for those
/// conditions.
#[allow(async_fn_in_trait)]
pub trait ConfirmLoop {
    /// Per-adapter incoming-message type carried on the receiver.
    type Incoming;

    /// Short label used in log lines, e.g. `"telegram"` / `"discord"` / `"slack"`.
    fn confirm_label(&self) -> &'static str;

    /// Exclusive access to the adapter's incoming-message receiver.
    fn confirm_receiver(&mut self) -> &mut mpsc::Receiver<Self::Incoming>;

    /// Hold back a message that is not in scope for the open question or confirmation, so it
    /// reaches the agent afterwards.
    ///
    /// The default discards it; adapters override this to park it in their [`Inbox`].
    fn confirm_defer(&mut self, incoming: Self::Incoming) {
        drop(incoming);
        tracing::debug!(
            channel = self.confirm_label(),
            "out-of-scope message dropped"
        );
    }

    /// Whether `incoming` is in scope to answer this confirmation (e.g. chat-id
    /// match for Telegram, allowlist authorization for Discord/Slack).
    fn confirm_accepts(&self, incoming: &Self::Incoming) -> bool;

    /// Extract the reply text to compare against `"yes"`.
    fn confirm_reply_text<'a>(&self, incoming: &'a Self::Incoming) -> &'a str;

    /// Send the already-formatted prompt out on the channel.
    async fn confirm_send_prompt(&mut self, text: &str) -> Result<(), ChannelError>;

    /// Send `prompt` once and wait for the next in-scope reply, with no deadline of its own.
    ///
    /// The caller owns the timeout; dropping the future abandons the wait. Out-of-scope
    /// messages are deferred in the [`Inbox`] as they arrive, so they reach the agent after
    /// the wait ends instead of being lost.
    ///
    /// # Errors
    ///
    /// Returns `Err` if sending the prompt message fails.
    async fn run_reply(&mut self, prompt: &str) -> Result<ReplyOutcome, ChannelError> {
        let label = self.confirm_label();
        self.confirm_send_prompt(prompt).await?;
        loop {
            let Some(incoming) = self.confirm_receiver().recv().await else {
                tracing::warn!("{label} reply channel closed");
                return Ok(ReplyOutcome::Closed);
            };
            if !self.confirm_accepts(&incoming) {
                tracing::debug!(channel = label, "reply: deferring out-of-scope message");
                self.confirm_defer(incoming);
                continue;
            }
            return Ok(ReplyOutcome::Text(
                self.confirm_reply_text(&incoming).to_owned(),
            ));
        }
    }

    /// Send `prompt` with a timeout suffix appended, then wait for a matching
    /// `"yes"` reply.
    ///
    /// Returns `Ok(false)` on timeout or channel close, never `Err` for those
    /// conditions. Messages rejected by
    /// [`confirm_accepts`](Self::confirm_accepts) do not reset the deadline and are
    /// deferred in the [`Inbox`], so they are delivered to the agent after the
    /// confirmation ends instead of being lost.
    ///
    /// # Errors
    ///
    /// Returns `Err` if sending the prompt message fails.
    async fn run_confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        let label = self.confirm_label();
        let prompt = format!(
            "{prompt}\nReply 'yes' to confirm (timeout: {}s).",
            crate::CONFIRM_TIMEOUT.as_secs()
        );
        match tokio::time::timeout(crate::CONFIRM_TIMEOUT, self.run_reply(&prompt)).await {
            Ok(Ok(ReplyOutcome::Text(text))) => Ok(text.trim().eq_ignore_ascii_case("yes")),
            Ok(Ok(ReplyOutcome::Closed)) => Ok(false),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                tracing::warn!(
                    "{label} confirm timed out after {}s — denied",
                    crate::CONFIRM_TIMEOUT.as_secs()
                );
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct MockIncoming {
        scope: &'static str,
        text: String,
    }

    struct MockConfirm {
        rx: Inbox<MockIncoming>,
        scope: &'static str,
        sent: Vec<String>,
    }

    impl MockConfirm {
        fn new(rx: mpsc::Receiver<MockIncoming>) -> Self {
            Self {
                rx: rx.into(),
                scope: "initiator",
                sent: Vec::new(),
            }
        }
    }

    impl ConfirmLoop for MockConfirm {
        type Incoming = MockIncoming;

        fn confirm_label(&self) -> &'static str {
            "mock"
        }

        fn confirm_receiver(&mut self) -> &mut mpsc::Receiver<MockIncoming> {
            self.rx.receiver_mut()
        }

        fn confirm_defer(&mut self, incoming: MockIncoming) {
            self.rx.defer([incoming]);
        }

        fn confirm_accepts(&self, incoming: &MockIncoming) -> bool {
            incoming.scope == self.scope
        }

        fn confirm_reply_text<'a>(&self, incoming: &'a MockIncoming) -> &'a str {
            &incoming.text
        }

        #[allow(clippy::unused_async_trait_impl)]
        async fn confirm_send_prompt(&mut self, text: &str) -> Result<(), ChannelError> {
            self.sent.push(text.to_string());
            Ok(())
        }
    }

    fn msg(text: &str) -> MockIncoming {
        MockIncoming {
            scope: "initiator",
            text: text.to_string(),
        }
    }

    #[tokio::test]
    async fn run_confirm_sends_formatted_prompt_and_accepts_yes() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(msg("yes")).await.unwrap();
        let mut mock = MockConfirm::new(rx);
        let result = mock.run_confirm("Proceed?").await.unwrap();
        assert!(result);
        assert_eq!(
            mock.sent,
            vec!["Proceed?\nReply 'yes' to confirm (timeout: 30s).".to_string()]
        );
    }

    #[tokio::test]
    async fn run_confirm_denies_on_non_yes_reply() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(msg("no")).await.unwrap();
        let mut mock = MockConfirm::new(rx);
        assert!(!mock.run_confirm("Proceed?").await.unwrap());
    }

    #[tokio::test]
    async fn run_confirm_is_case_insensitive() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(msg(" YES ")).await.unwrap();
        let mut mock = MockConfirm::new(rx);
        assert!(mock.run_confirm("Proceed?").await.unwrap());
    }

    #[tokio::test]
    async fn run_confirm_skips_out_of_scope_then_accepts_in_scope() {
        let (tx, rx) = mpsc::channel(2);
        tx.send(MockIncoming {
            scope: "other",
            text: "yes".to_string(),
        })
        .await
        .unwrap();
        tx.send(msg("yes")).await.unwrap();
        let mut mock = MockConfirm::new(rx);
        assert!(mock.run_confirm("Proceed?").await.unwrap());
    }

    #[tokio::test]
    async fn run_confirm_defers_out_of_scope_messages_in_order() {
        let (tx, rx) = mpsc::channel(4);
        for text in ["first", "second"] {
            tx.send(MockIncoming {
                scope: "other",
                text: text.to_string(),
            })
            .await
            .unwrap();
        }
        tx.send(msg("yes")).await.unwrap();
        let mut mock = MockConfirm::new(rx);
        assert!(mock.run_confirm("Proceed?").await.unwrap());

        let inbox = &mut mock.rx;
        assert_eq!(inbox.recv().await.unwrap().text, "first");
        assert_eq!(inbox.try_recv().unwrap().text, "second");
        assert!(inbox.try_recv().is_none());
    }

    #[tokio::test]
    async fn run_confirm_defers_out_of_scope_messages_on_timeout() {
        tokio::time::pause();
        let (tx, rx) = mpsc::channel(2);
        tx.send(MockIncoming {
            scope: "other",
            text: "lost?".to_string(),
        })
        .await
        .unwrap();
        let mut mock = MockConfirm::new(rx);
        assert!(!mock.run_confirm("Proceed?").await.unwrap());
        assert_eq!(mock.rx.recv().await.unwrap().text, "lost?");
    }

    #[test]
    fn defer_drops_oldest_beyond_the_bound() {
        let (_tx, rx) = mpsc::channel::<usize>(1);
        let mut inbox = Inbox::from(rx);
        inbox.defer(0..MAX_DEFERRED + 3);
        assert_eq!(inbox.try_recv(), Some(3));
        assert_eq!(inbox.deferred.len(), MAX_DEFERRED - 1);
    }

    #[tokio::test]
    async fn deferred_messages_survive_channel_close_and_receiver_replacement() {
        let (tx, rx) = mpsc::channel(2);
        let mut inbox = Inbox::from(rx);
        inbox.defer(["kept"]);
        drop(tx);
        let (_tx2, rx2) = mpsc::channel(1);
        inbox.replace_receiver(rx2);
        assert_eq!(inbox.recv().await, Some("kept"));

        let (tx3, rx3) = mpsc::channel::<&str>(1);
        let mut closed = Inbox::from(rx3);
        closed.defer(["last"]);
        drop(tx3);
        assert_eq!(closed.recv().await, Some("last"));
        assert_eq!(closed.recv().await, None);
    }

    #[tokio::test]
    async fn run_confirm_keeps_unread_deferred_queue_across_repeated_confirms() {
        let (tx, rx) = mpsc::channel(4);
        tx.send(MockIncoming {
            scope: "other",
            text: "pending".to_string(),
        })
        .await
        .unwrap();
        drop(tx);
        let mut mock = MockConfirm::new(rx);
        assert!(!mock.run_confirm("First?").await.unwrap());
        assert!(!mock.run_confirm("Second?").await.unwrap());
        let inbox = &mut mock.rx;
        assert_eq!(inbox.recv().await.unwrap().text, "pending");
        assert!(inbox.recv().await.is_none());
    }

    #[tokio::test]
    async fn run_confirm_denies_on_channel_close() {
        let (tx, rx) = mpsc::channel::<MockIncoming>(1);
        drop(tx);
        let mut mock = MockConfirm::new(rx);
        assert!(!mock.run_confirm("Proceed?").await.unwrap());
    }

    #[tokio::test]
    async fn run_confirm_denies_on_timeout() {
        tokio::time::pause();
        let (_tx, rx) = mpsc::channel::<MockIncoming>(1);
        let mut mock = MockConfirm::new(rx);
        let handle = tokio::spawn(async move { mock.run_confirm("Proceed?").await });
        tokio::time::advance(crate::CONFIRM_TIMEOUT + Duration::from_millis(1)).await;
        let result = handle.await.unwrap().unwrap();
        assert!(!result);
    }
}
