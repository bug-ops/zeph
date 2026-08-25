// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression tests for #6418: `SessionState.owner_key` must never carry a stale value from
//! one `LoopEvent` into the next, even when a fast-path slash-command dispatch (or any
//! non-`Message` `LoopEvent`) exits the `Agent::run` loop iteration before ever reaching
//! `process_user_message`/`end_turn`.

#[allow(unused_imports)]
use super::*;

use std::collections::VecDeque;

/// Channel that yields a fixed queue of full [`ChannelMessage`]s (including `owner_key`) one
/// at a time via `recv()`. Deliberately does NOT override `try_recv()` — it keeps the
/// [`Channel`] trait's `None` default — so `Agent::drain_channel`'s opportunistic pre-drain
/// never intercepts these messages before `next_event()`'s `channel.recv()` branch sets
/// `session.owner_key` from `ChannelMessage::owner_key` (the `Some(LoopEvent::Message(msg))`
/// arm in `agent/mod.rs`). A channel that overrides `try_recv()` (like `MockChannel`) would
/// route these through the message-queue fast path instead, which never touches `owner_key`
/// at all, making the set/reset behavior this file tests unobservable.
struct OwnerKeyChannel {
    inbox: VecDeque<ChannelMessage>,
    sent: Arc<Mutex<Vec<String>>>,
}

impl OwnerKeyChannel {
    fn new(messages: Vec<ChannelMessage>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inbox: messages.into(),
                sent: Arc::clone(&sent),
            },
            sent,
        )
    }
}

impl Channel for OwnerKeyChannel {
    #[allow(clippy::unused_async_trait_impl)]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, crate::channel::ChannelError> {
        Ok(self.inbox.pop_front())
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn send(&mut self, text: &str) -> Result<(), crate::channel::ChannelError> {
        self.sent.lock().unwrap().push(text.to_owned());
        Ok(())
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), crate::channel::ChannelError> {
        self.sent.lock().unwrap().push(chunk.to_owned());
        Ok(())
    }

    fn flush_chunks(
        &mut self,
    ) -> impl std::future::Future<Output = Result<(), crate::channel::ChannelError>> + Send {
        std::future::ready(Ok(()))
    }
}

fn owner_key_message(text: &str, owner_key: Option<&str>) -> ChannelMessage {
    ChannelMessage {
        text: text.to_owned(),
        attachments: vec![],
        is_guest_context: false,
        is_from_bot: false,
        owner_key: owner_key.map(str::to_owned),
    }
}

/// A normal (non-slash) turn sets `session.owner_key` from the inbound
/// `ChannelMessage.owner_key`, and `Agent::end_turn` resets it back to `DEFAULT_OWNER_KEY`
/// once the turn completes — proven here by inspecting session state after `run()` exits with
/// no further messages pending.
#[tokio::test]
async fn owner_key_reset_to_default_after_normal_turn_completes() {
    let provider = mock_provider(vec!["ok".to_owned()]);
    let (channel, _sent) = OwnerKeyChannel::new(vec![owner_key_message(
        "hello agent",
        Some("gateway:alice"),
    )]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.run().await.expect("agent run failed");

    assert_eq!(
        agent.services.session.owner_key,
        crate::agent::state::persistence::DEFAULT_OWNER_KEY,
        "owner_key must be reset to the default once the normal turn (Message -> \
         process_user_message -> end_turn) completes and no further LoopEvent is pending"
    );
}

/// #6418 regression: a fast-path slash-command dispatch (session/debug registry
/// `DispatchFlow::Continue`) exits the loop iteration WITHOUT ever reaching
/// `process_user_message`/`end_turn`. Before the fix, `session.owner_key` — set from that
/// message's `ChannelMessage.owner_key` — would remain stale forever once the channel closed
/// (no further `Message` event to overwrite it). The unconditional per-iteration reset at the
/// top of `Agent::run`'s loop closes this gap.
///
/// `mock_provider(vec![])` (zero canned responses) doubles as a self-check: if `/help` ever
/// regressed into reaching `process_user_message`, the empty response queue would surface as a
/// hard failure here instead of silently passing.
#[tokio::test]
async fn owner_key_not_stale_after_fastpath_slash_command_bypasses_end_turn() {
    let provider = mock_provider(vec![]);
    let (channel, _sent) =
        OwnerKeyChannel::new(vec![owner_key_message("/help", Some("gateway:alice"))]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.run().await.expect("agent run failed");

    assert_eq!(
        agent.services.session.owner_key,
        crate::agent::state::persistence::DEFAULT_OWNER_KEY,
        "a fast-path slash-command dispatch that skips end_turn must not leave a stale \
         owner_key once the channel closes"
    );
}
