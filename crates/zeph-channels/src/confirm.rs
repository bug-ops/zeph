// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared deny-on-timeout confirmation loop used by all channel adapters that
//! support interactive `yes`/`no` prompts (Telegram, Discord, Slack).
//!
//! The [`ConfirmLoop`] trait extracts the send-prompt / wait-for-reply-with-deadline
//! / compare-case-insensitively-against-"yes" logic that would otherwise be
//! duplicated across Discord, Slack, and Telegram's
//! [`Channel::confirm`](zeph_core::channel::Channel::confirm) implementations.

use tokio::sync::mpsc;
use zeph_core::channel::ChannelError;

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

    /// Whether `incoming` is in scope to answer this confirmation (e.g. chat-id
    /// match for Telegram, allowlist authorization for Discord/Slack).
    fn confirm_accepts(&self, incoming: &Self::Incoming) -> bool;

    /// Extract the reply text to compare against `"yes"`.
    fn confirm_reply_text<'a>(&self, incoming: &'a Self::Incoming) -> &'a str;

    /// Send the already-formatted prompt out on the channel.
    async fn confirm_send_prompt(&mut self, text: &str) -> Result<(), ChannelError>;

    /// Send `prompt` with a timeout suffix appended, then wait for a matching
    /// `"yes"` reply.
    ///
    /// Returns `Ok(false)` on timeout or channel close, never `Err` for those
    /// conditions. Messages rejected by
    /// [`confirm_accepts`](Self::confirm_accepts) are skipped without resetting
    /// the deadline.
    ///
    /// # Errors
    ///
    /// Returns `Err` if sending the prompt message fails.
    async fn run_confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        let label = self.confirm_label();
        self.confirm_send_prompt(&format!(
            "{prompt}\nReply 'yes' to confirm (timeout: {}s).",
            crate::CONFIRM_TIMEOUT.as_secs()
        ))
        .await?;
        let deadline = tokio::time::Instant::now() + crate::CONFIRM_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!(
                    "{label} confirm timed out after {}s — denied",
                    crate::CONFIRM_TIMEOUT.as_secs()
                );
                return Ok(false);
            }
            match tokio::time::timeout(remaining, self.confirm_receiver().recv()).await {
                Ok(Some(incoming)) => {
                    if !self.confirm_accepts(&incoming) {
                        tracing::debug!(channel = label, "confirm: ignoring out-of-scope message");
                        continue;
                    }
                    return Ok(self
                        .confirm_reply_text(&incoming)
                        .trim()
                        .eq_ignore_ascii_case("yes"));
                }
                Ok(None) => {
                    tracing::warn!("{label} confirm channel closed — denying");
                    return Ok(false);
                }
                Err(_) => {
                    tracing::warn!(
                        "{label} confirm timed out after {}s — denied",
                        crate::CONFIRM_TIMEOUT.as_secs()
                    );
                    return Ok(false);
                }
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
        rx: mpsc::Receiver<MockIncoming>,
        scope: &'static str,
        sent: Vec<String>,
    }

    impl MockConfirm {
        fn new(rx: mpsc::Receiver<MockIncoming>) -> Self {
            Self {
                rx,
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
            &mut self.rx
        }

        fn confirm_accepts(&self, incoming: &MockIncoming) -> bool {
            incoming.scope == self.scope
        }

        fn confirm_reply_text<'a>(&self, incoming: &'a MockIncoming) -> &'a str {
            &incoming.text
        }

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
