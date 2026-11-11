// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{Message, Role};

use crate::agent::Agent;
use crate::agent::tests::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};

/// #6998 regression: multi-byte content crossing the 120 and 200 byte preview cuts must not
/// abort the doom-loop check.
#[tokio::test]
#[tracing_test::traced_test]
async fn check_doom_loop_multibyte_content_at_preview_cuts() {
    for pad in 0..3 {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        let content = format!("{}{}", "a".repeat(pad), "\u{20ac}".repeat(120));
        for _ in 0..crate::agent::DOOM_LOOP_WINDOW {
            agent
                .msg
                .messages
                .push(Message::from_legacy(Role::User, &content));
            agent.check_doom_loop(1).await.unwrap();
        }
    }
}
