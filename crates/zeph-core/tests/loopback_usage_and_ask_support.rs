// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Fixture construction failures should fail the test immediately.; Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7137 (`LoopbackEvent::Usage` carries a typed
//! `TokenUsage`) and the Loopback part of #7122 (`ask_user_support` follows the responder).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zeph_common::money::Cents;
use zeph_core::channel::{
    AskUserOutcome, AskUserRequest, AskUserResponder, AskUserSupport, Channel, ChannelError,
    DeclineReason, LoopbackChannel, LoopbackEvent, TokenUsage,
};

struct Decline;

impl AskUserResponder for Decline {
    fn ask(
        &self,
        _request: AskUserRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AskUserOutcome, ChannelError>> + Send + '_>> {
        Box::pin(async { Ok(AskUserOutcome::Declined(DeclineReason::Unsupported)) })
    }
}

#[tokio::test]
async fn send_usage_emits_typed_usage_event() {
    let (mut channel, mut handle) = LoopbackChannel::pair(4);
    let usage = TokenUsage {
        input: 10,
        output: 20,
        cache_write: 3,
        cache_read: 4,
    };
    channel
        .send_usage(usage, 200_000, Cents::new(1.5))
        .await
        .unwrap();
    let event = handle.output_rx.recv().await.expect("event");
    let LoopbackEvent::Usage {
        usage: got,
        context_window,
        cost_cents,
        ..
    } = event
    else {
        panic!("expected Usage event");
    };
    assert_eq!(got, usage);
    assert_eq!(context_window, 200_000);
    assert_eq!(cost_cents, Cents::new(1.5));
}

#[test]
fn loopback_ask_user_support_is_unavailable_without_responder() {
    let (channel, _handle) = LoopbackChannel::pair(4);
    assert_eq!(channel.ask_user_support(), AskUserSupport::Unavailable);
}

#[test]
fn loopback_ask_user_support_is_interactive_with_responder() {
    let (channel, _handle) = LoopbackChannel::pair(4);
    let channel = channel.with_ask_user_responder(Arc::new(Decline));
    assert_eq!(channel.ask_user_support(), AskUserSupport::Interactive);
}
