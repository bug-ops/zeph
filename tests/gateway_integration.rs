// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the HTTP gateway webhook ingestion path (#1026).

/// Start gateway server, POST a webhook payload, and verify the agent receives the event.
///
/// Requires a running agent instance with gateway enabled and Qdrant available.
#[ignore = "requires running gateway and agent"]
#[tokio::test]
async fn gateway_receives_webhook_and_forwards_to_agent() {
    // Placeholder: full implementation tracked in the live-testing playbook.
    // The test body requires a running GatewayServer and agent instance (see #1026).
}
