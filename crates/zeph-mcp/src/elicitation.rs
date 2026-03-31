// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tokio::sync::oneshot;

use rmcp::model::{CreateElicitationRequestParams, CreateElicitationResult};

/// Event sent from `ToolListChangedHandler::create_elicitation()` to the agent loop.
///
/// The handler awaits the `response_tx` oneshot. The agent loop must process this event
/// while concurrently awaiting a tool call result — it routes the request to the active
/// channel and sends the response back through `response_tx`.
pub struct ElicitationEvent {
    /// The MCP server that sent this elicitation request.
    pub server_id: String,
    /// Raw rmcp parameters. Converted to `zeph_core::ElicitationRequest` by the agent loop.
    pub request: CreateElicitationRequestParams,
    /// Send the user's response back to the handler waiting in `create_elicitation()`.
    pub response_tx: oneshot::Sender<CreateElicitationResult>,
}

impl std::fmt::Debug for ElicitationEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElicitationEvent")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CreateElicitationResult, ElicitationAction};

    #[test]
    fn elicitation_event_debug_does_not_expose_request_content() {
        let (response_tx, _rx) = oneshot::channel();
        let event = ElicitationEvent {
            server_id: "test-server".to_owned(),
            request: CreateElicitationRequestParams::FormElicitationParams {
                meta: None,
                message: "enter password".to_owned(),
                requested_schema: rmcp::model::ElicitationSchema::new(
                    std::collections::BTreeMap::new(),
                ),
            },
            response_tx,
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("test-server"));
        assert!(
            !debug.contains("password"),
            "request content must not be exposed in debug output"
        );
    }

    #[tokio::test]
    async fn elicitation_event_response_tx_delivers_result() {
        let (response_tx, response_rx) = oneshot::channel::<CreateElicitationResult>();
        let event = ElicitationEvent {
            server_id: "srv".to_owned(),
            request: CreateElicitationRequestParams::FormElicitationParams {
                meta: None,
                message: "test".to_owned(),
                requested_schema: rmcp::model::ElicitationSchema::new(
                    std::collections::BTreeMap::new(),
                ),
            },
            response_tx,
        };
        let result = CreateElicitationResult {
            action: ElicitationAction::Decline,
            content: None,
        };
        event.response_tx.send(result).unwrap();
        let received = response_rx.await.unwrap();
        assert_eq!(received.action, ElicitationAction::Decline);
    }
}
