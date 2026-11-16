// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance test for #7090 (2): a failed turn caused by a Claude 404 emits exactly one
//! ERROR log line (#7066).
//!
//! Registered from `tests/mod.rs` with `mod failed_turn_error_log_tests;`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, registry::LookupSpan};

use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};
use zeph_llm::LlmError;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;

struct ErrorCounter(Arc<AtomicUsize>);

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for ErrorCounter {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() == Level::ERROR {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[tokio::test]
#[allow(clippy::large_futures)]
async fn process_user_message_claude_404_emits_exactly_one_error_event() {
    let errors = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(ErrorCounter(Arc::clone(&errors)));
    let _guard = tracing::subscriber::set_default(subscriber);

    let body = r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-x"}}"#;
    let provider = AnyProvider::Mock(
        MockProvider::default().with_errors(vec![LlmError::api_error("claude", 404, body)]),
    );
    let mut agent = crate::agent::Agent::new(
        provider,
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    );

    agent
        .process_user_message("hi".to_owned(), vec![])
        .await
        .unwrap();

    assert_eq!(
        errors.load(Ordering::SeqCst),
        1,
        "a failed Claude 404 turn must log exactly one ERROR event"
    );
}
