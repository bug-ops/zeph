// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! IDE elicitation bridge for ACP sessions.
//!
//! When the IDE advertises elicitation capability during `initialize()`, each new session
//! spawns a bridge task that accepts [`ElicitationRequest`]s from the agent loop, forwards
//! them to the IDE via `elicitation/create`, and returns the IDE's response via a oneshot
//! channel.
//!
//! The bridge task listens on a `cancel_signal` to terminate when the session is closed,
//! preventing task leaks on session reap or cancel. It applies no timeout of its own: the
//! caller owns the deadline and drops its response receiver to withdraw a request, which
//! frees the bridge for the next one.

use agent_client_protocol as acp;
use agent_client_protocol_schema::v1 as schema;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use zeph_core::channel::{
    ASK_USER_ANSWER_FIELD, ASK_USER_RETRY_HINT, AskUserAnswer, AskUserOutcome, AskUserRequest,
    AskUserResponder, ChannelError, DeclineReason,
};

use crate::error::AcpError;

/// A pending elicitation request from the agent loop to the IDE.
pub(crate) struct ElicitationRequest {
    /// The elicitation form request to forward to the IDE.
    pub create_req: schema::CreateElicitationRequest,
    /// Channel to send the IDE's response back to the agent loop.
    pub response_tx: oneshot::Sender<Result<schema::CreateElicitationResponse, AcpError>>,
}

/// Sender half for delivering elicitation requests to the bridge task.
pub(crate) type ElicitationSender = mpsc::Sender<ElicitationRequest>;
/// Receiver half consumed by the bridge task.
pub(crate) type ElicitationReceiver = mpsc::Receiver<ElicitationRequest>;

/// Create a bounded elicitation channel pair.
///
/// Capacity 4 is sufficient since elicitations are sequential (the agent loop awaits
/// each response before sending the next request).
pub(crate) fn elicitation_channel() -> (ElicitationSender, ElicitationReceiver) {
    mpsc::channel(4)
}

/// Spawn the per-session elicitation bridge task.
///
/// The task forwards each [`ElicitationRequest`] to the IDE and returns the response
/// via the embedded oneshot channel. It terminates cleanly when:
/// - `elicitation_rx` is dropped (session entry dropped, channel closed).
/// - `cancel_signal` is notified (session cancelled or reaped).
///
/// # Arguments
///
/// - `cx`: ACP connection to the IDE.
/// - `elicitation_rx`: Receives elicitation requests from the agent loop.
/// - `cancel_signal`: Session cancel notify — bridge exits on notification.
pub(crate) fn spawn_elicitation_bridge(
    cx: acp::v1::ConnectionTo<acp::Client>,
    mut elicitation_rx: ElicitationReceiver,
    cancel_signal: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    // EXEMPT(#5144): returns JoinHandle by contract; per-session lifetime via cancel_signal.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cancel_signal.notified() => {
                    tracing::debug!("elicitation bridge: session cancelled, exiting");
                    break;
                }
                req = elicitation_rx.recv() => {
                    let Some(ElicitationRequest { create_req, mut response_tx }) = req else {
                        // Sender dropped — session entry is gone.
                        tracing::debug!("elicitation bridge: channel closed, exiting");
                        break;
                    };
                    // A closed receiver means the caller gave up (its deadline fired); stop
                    // waiting so the next request is not queued behind a dead one.
                    let result = tokio::select! {
                        () = response_tx.closed() => continue,
                        result = send_elicitation(&cx, create_req) => result,
                    };
                    let _ = response_tx.send(result);
                }
            }
        }
    })
}

/// Send an elicitation request to the IDE and await its response.
///
/// `CreateElicitationRequest` is not yet registered in the SDK's typed dispatch table, so the
/// request is sent as an `UntypedMessage` and the response is deserialized from `serde_json::Value`.
async fn send_elicitation(
    cx: &acp::v1::ConnectionTo<acp::Client>,
    req: schema::CreateElicitationRequest,
) -> Result<schema::CreateElicitationResponse, AcpError> {
    let params = serde_json::to_value(&req).map_err(|e| AcpError::ClientError(e.to_string()))?;
    let msg = acp::UntypedMessage::new("elicitation/create", params)
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    let raw: serde_json::Value = cx
        .send_request(msg)
        .block_task()
        .await
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    serde_json::from_value(raw).map_err(|e| AcpError::ClientError(e.to_string()))
}

/// Extension methods for the elicitation bridge in [`crate::AcpContext`].
///
/// Available only when the `unstable-elicitation` feature is enabled and the IDE
/// advertised elicitation capability during `initialize()`.
#[derive(Clone)]
pub(crate) struct ElicitationBridge {
    /// Sender to the bridge task for this session.
    pub tx: ElicitationSender,
}

impl ElicitationBridge {
    /// Send a form elicitation to the IDE and await the user's response.
    ///
    /// Returns the [`schema::ElicitationAction`] chosen by the user (accept, decline,
    /// or cancel). There is no timeout here: dropping the returned future withdraws the
    /// request, so the caller enforces its own deadline.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ChannelClosed`] when the bridge task has exited (session
    /// cancelled or sender dropped), or [`AcpError::ClientError`] when the IDE returns
    /// an error response.
    pub(crate) async fn elicit(
        &self,
        session_id: Arc<schema::SessionId>,
        message: impl Into<String>,
        requested_schema: schema::ElicitationSchema,
    ) -> Result<schema::ElicitationAction, AcpError> {
        let scope = schema::ElicitationScope::Session(schema::ElicitationSessionScope::new(
            session_id.to_string(),
        ));
        let mode = schema::ElicitationFormMode::new(scope, requested_schema);
        let create_req = schema::CreateElicitationRequest::new(mode, message);
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ElicitationRequest {
                create_req,
                response_tx,
            })
            .await
            .map_err(|_| AcpError::ChannelClosed)?;

        let resp = response_rx.await.map_err(|_| AcpError::ChannelClosed)??;

        Ok(resp.action)
    }
}

/// What an elicitation answer means for an open question.
#[derive(Debug, PartialEq, Eq)]
enum Interpretation {
    /// The user answered.
    Answer(AskUserAnswer),
    /// The user declined or cancelled the form.
    Cancel,
    /// The value does not fit the question; ask again.
    Retry,
}

/// Map the IDE's elicitation action onto the question it answers.
fn interpret(request: &AskUserRequest, action: &schema::ElicitationAction) -> Interpretation {
    let schema::ElicitationAction::Accept(accepted) = action else {
        return Interpretation::Cancel;
    };
    let Some(schema::ElicitationContentValue::String(raw)) = accepted
        .content
        .as_ref()
        .and_then(|content| content.get(ASK_USER_ANSWER_FIELD))
    else {
        return Interpretation::Cancel;
    };
    match request.parse_dialog_answer(raw) {
        Ok(answer) => Interpretation::Answer(answer),
        Err(error) => {
            tracing::debug!(%error, "ask_user: IDE answer did not match the question");
            Interpretation::Retry
        }
    }
}

/// The form an `ask_user` question is shown as: one string field, a selection list for a
/// closed single choice.
fn answer_schema(request: &AskUserRequest) -> schema::ElicitationSchema {
    let mut field = schema::StringPropertySchema::new();
    match request.closed_choice_values() {
        Some(values) => field = field.enum_values(values),
        None => field = field.description(request.dialog_answer_hint()),
    }
    schema::ElicitationSchema::new().property(ASK_USER_ANSWER_FIELD, field, true)
}

/// Answers `ask_user` questions through the IDE's elicitation form.
///
/// Built per session from its [`ElicitationBridge`]; the agent owns the deadline and drops the
/// future when it expires, so nothing here waits on a clock of its own.
pub(crate) struct AcpAskUserResponder {
    bridge: ElicitationBridge,
    session_id: Arc<schema::SessionId>,
}

impl AcpAskUserResponder {
    pub(crate) fn new(bridge: ElicitationBridge, session_id: schema::SessionId) -> Self {
        Self {
            bridge,
            session_id: Arc::new(session_id),
        }
    }
}

impl AskUserResponder for AcpAskUserResponder {
    fn ask(
        &self,
        request: AskUserRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AskUserOutcome, ChannelError>> + Send + '_>,
    > {
        Box::pin(async move {
            let mut hint: Option<&str> = None;
            loop {
                let mut message = String::new();
                if let Some(hint) = hint {
                    message.push_str(hint);
                    message.push_str("\n\n");
                }
                message.push_str(&request.dialog_message());
                let action = match self
                    .bridge
                    .elicit(
                        Arc::clone(&self.session_id),
                        message,
                        answer_schema(&request),
                    )
                    .await
                {
                    Ok(action) => action,
                    Err(AcpError::ChannelClosed) => return Err(ChannelError::ChannelClosed),
                    Err(error) => {
                        tracing::warn!(%error, "ask_user: the IDE could not show the question");
                        return Ok(AskUserOutcome::Declined(DeclineReason::Unsupported));
                    }
                };
                match interpret(&request, &action) {
                    Interpretation::Answer(answer) => return Ok(AskUserOutcome::Answered(answer)),
                    Interpretation::Cancel => return Ok(AskUserOutcome::Cancelled),
                    Interpretation::Retry => {
                        hint = Some(ASK_USER_RETRY_HINT);
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod ask_user_tests {
    use std::collections::BTreeMap;

    use zeph_core::channel::{AskUserKind, ChoiceOptions, OtherAnswer};

    use super::*;

    fn closed_choice() -> AskUserRequest {
        let options =
            ChoiceOptions::new(vec!["2 GB".into(), "4 GB".into(), "8 GB".into()]).unwrap();
        AskUserRequest::single_choice("Size?", options, OtherAnswer::Forbidden).unwrap()
    }

    fn accept(value: &str) -> schema::ElicitationAction {
        let mut content = BTreeMap::new();
        content.insert(
            ASK_USER_ANSWER_FIELD.to_owned(),
            schema::ElicitationContentValue::String(value.to_owned()),
        );
        schema::ElicitationAction::Accept(schema::ElicitationAcceptAction::new().content(content))
    }

    #[test]
    fn closed_choice_is_a_selection_list_read_by_position() {
        let request = closed_choice();
        assert_eq!(
            interpret(&request, &accept("1. 2 GB")),
            Interpretation::Answer(AskUserAnswer::Selected { indices: vec![0] })
        );
        let schema = answer_schema(&request);
        assert!(schema.properties.contains_key(ASK_USER_ANSWER_FIELD));
    }

    #[test]
    fn free_text_answer_is_passed_through() {
        let request = AskUserRequest::free_text("Where?").unwrap();
        assert_eq!(
            interpret(&request, &accept("/home/me")),
            Interpretation::Answer(AskUserAnswer::Text("/home/me".into()))
        );
    }

    #[test]
    fn decline_cancel_and_empty_accept_cancel_the_question() {
        let request = closed_choice();
        assert_eq!(
            interpret(&request, &schema::ElicitationAction::Decline),
            Interpretation::Cancel
        );
        assert_eq!(
            interpret(&request, &schema::ElicitationAction::Cancel),
            Interpretation::Cancel
        );
        let empty = schema::ElicitationAction::Accept(schema::ElicitationAcceptAction::new());
        assert_eq!(interpret(&request, &empty), Interpretation::Cancel);
    }

    #[test]
    fn a_value_outside_the_options_asks_again() {
        assert_eq!(
            interpret(&closed_choice(), &accept("9. nope")),
            Interpretation::Retry
        );
    }

    #[test]
    fn the_form_message_carries_the_fixed_header() {
        let request = closed_choice();
        assert!(matches!(request.kind(), AskUserKind::SingleChoice { .. }));
        assert!(
            request
                .dialog_message()
                .starts_with(zeph_core::channel::ASK_USER_HEADER)
        );
    }

    #[tokio::test]
    async fn the_responder_returns_what_the_ide_selected() {
        let (tx, mut rx) = elicitation_channel();
        let bridge = ElicitationBridge { tx };
        let responder = AcpAskUserResponder::new(bridge, schema::SessionId::new("s1"));
        let ide = async move {
            let req = rx.recv().await.expect("the form request");
            let response = schema::CreateElicitationResponse::new(accept("3. 8 GB"));
            let _ = req.response_tx.send(Ok(response));
        };
        let (outcome, ()) = tokio::join!(responder.ask(closed_choice()), ide);
        assert_eq!(
            outcome.unwrap(),
            AskUserOutcome::Answered(AskUserAnswer::Selected { indices: vec![2] })
        );
    }

    #[tokio::test]
    async fn a_dropped_bridge_cancels_the_question() {
        let (tx, rx) = elicitation_channel();
        drop(rx);
        let responder =
            AcpAskUserResponder::new(ElicitationBridge { tx }, schema::SessionId::new("s1"));
        let err = responder.ask(closed_choice()).await.unwrap_err();
        assert!(matches!(err, ChannelError::ChannelClosed));
    }

    #[tokio::test]
    async fn an_unanswered_question_is_bounded_by_the_caller_and_withdrawn_on_drop() {
        let (tx, mut rx) = elicitation_channel();
        let responder =
            AcpAskUserResponder::new(ElicitationBridge { tx }, schema::SessionId::new("s1"));
        let ask = responder.ask(closed_choice());
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(50), ask).await;
        assert!(outcome.is_err(), "no bridge-side deadline ends the wait");
        let mut req = rx.recv().await.expect("the form request");
        tokio::time::timeout(std::time::Duration::from_secs(1), req.response_tx.closed())
            .await
            .expect("dropping the question closes the response channel");
    }
}
