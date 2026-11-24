// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ask_user` support for `zeph serve` sessions (#7124, spec 090 FR-006).
//!
//! A [`ServeAskUserResponder`] publishes each question as a [`SessionOutput::AskUser`] event on
//! the session's SSE stream and waits for the client to answer through
//! `POST /sessions/{id}/answer`. The agent's `ask_user` choke point still owns the deadline, the
//! per-turn cap and secret scrubbing; this module only transports the question and validates the
//! shape of the reply.
//!
//! There is at most one pending question per session. A question asked while no client is
//! subscribed is declined at once, and a pending question is re-sent to every client that
//! subscribes later (see [`ServeAskUserResponder::pending_event`]).

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::{broadcast, oneshot};

use super::SessionOutput;
use crate::channel::{
    ASK_USER_HEADER, AskUserAnswer, AskUserKind, AskUserOutcome, AskUserRequest, AskUserResponder,
    ChannelError, DeclineReason, OtherAnswer,
};

/// Identifier of one question within a session; the client echoes it in its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct QuestionId(u64);

/// Shape of a question on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireQuestionKind {
    /// Open question answered with free text.
    FreeText,
    /// Exactly one of the options.
    SingleChoice,
    /// One or more of the options.
    MultiChoice,
}

/// Payload of the `ask_user` SSE event.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AskUserEvent {
    /// Id the answer must carry.
    pub question_id: QuestionId,
    /// Fixed header the client renders ahead of the question.
    pub header: &'static str,
    /// The model's question.
    pub question: String,
    /// Question shape.
    pub kind: WireQuestionKind,
    /// Option labels in order; empty for a free-text question.
    pub options: Vec<String>,
    /// Whether a free-text answer is accepted in addition to the options.
    pub allow_other: bool,
}

/// A client's reply to a pending question (`POST /sessions/{id}/answer` body field `reply`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AskUserReply {
    /// Free text, or an option given by number or label.
    Text {
        /// The reply text.
        text: String,
    },
    /// Zero-based indices of the chosen options.
    Selected {
        /// Chosen option indices.
        indices: Vec<usize>,
    },
    /// The user dismissed the question.
    Cancel,
}

/// Why [`ServeAskUserResponder::answer`] rejected a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    /// No question is pending, or `question_id` is not the pending one.
    #[error("question is not pending")]
    NotPending,
    /// The reply does not fit the pending question; the question stays pending.
    #[error("reply does not match the question")]
    Invalid,
}

struct Pending {
    id: QuestionId,
    request: AskUserRequest,
    event: AskUserEvent,
    reply: oneshot::Sender<AskUserOutcome>,
}

/// [`AskUserResponder`] for one serve session.
///
/// # Examples
///
/// ```
/// use tokio::sync::broadcast;
/// use zeph_core::serve::ServeAskUserResponder;
///
/// let (tx, _rx) = broadcast::channel(4);
/// let responder = ServeAskUserResponder::new(tx);
/// assert!(responder.pending_event().is_none());
/// ```
pub struct ServeAskUserResponder {
    tx_out: broadcast::Sender<SessionOutput>,
    pending: Mutex<Option<Pending>>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for ServeAskUserResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeAskUserResponder")
            .finish_non_exhaustive()
    }
}

/// Clears the pending slot when the question future is dropped (timeout, cancel, answer).
struct SlotGuard<'a> {
    slot: &'a Mutex<Option<Pending>>,
    id: QuestionId,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut slot = self.slot.lock();
        if slot.as_ref().is_some_and(|p| p.id == self.id) {
            *slot = None;
        }
    }
}

fn event_for(id: QuestionId, request: &AskUserRequest) -> AskUserEvent {
    let (kind, options, allow_other) = match request.kind() {
        AskUserKind::FreeText => (WireQuestionKind::FreeText, Vec::new(), false),
        AskUserKind::SingleChoice { options, other } => (
            WireQuestionKind::SingleChoice,
            options.labels().to_vec(),
            *other == OtherAnswer::Allowed,
        ),
        AskUserKind::MultiChoice { options, other } => (
            WireQuestionKind::MultiChoice,
            options.labels().to_vec(),
            *other == OtherAnswer::Allowed,
        ),
    };
    AskUserEvent {
        question_id: id,
        header: ASK_USER_HEADER,
        question: request.question().to_owned(),
        kind,
        options,
        allow_other,
    }
}

fn validate_selection(request: &AskUserRequest, indices: &[usize]) -> Option<AskUserAnswer> {
    let (count, single) = match request.kind() {
        AskUserKind::FreeText => return None,
        AskUserKind::SingleChoice { options, .. } => (options.labels().len(), true),
        AskUserKind::MultiChoice { options, .. } => (options.labels().len(), false),
    };
    let ascending = indices.windows(2).all(|w| w[0] < w[1]);
    let ok = !indices.is_empty()
        && ascending
        && indices.iter().all(|i| *i < count)
        && (!single || indices.len() == 1);
    ok.then(|| AskUserAnswer::Selected {
        indices: indices.to_vec(),
    })
}

impl ServeAskUserResponder {
    /// Create a responder publishing on `tx_out`.
    #[must_use]
    pub fn new(tx_out: broadcast::Sender<SessionOutput>) -> Self {
        Self {
            tx_out,
            pending: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// The pending question's event, for re-sending to a client that subscribed after it was
    /// asked. `None` when nothing is pending.
    #[must_use]
    pub fn pending_event(&self) -> Option<AskUserEvent> {
        self.pending.lock().as_ref().map(|p| p.event.clone())
    }

    /// Resolve the pending question `id` with `reply`.
    ///
    /// `sanitize` runs only on a free-text answer (the text of a free-text question or an
    /// "other" answer to a choice); a reply that selects options is never rewritten because it
    /// carries no client text.
    ///
    /// # Errors
    ///
    /// [`AnswerError::NotPending`] when `id` is not the pending question (stale, repeated or
    /// none pending); [`AnswerError::Invalid`] when the reply does not fit the question, which
    /// then stays pending.
    pub fn answer(
        &self,
        id: QuestionId,
        reply: AskUserReply,
        sanitize: impl FnOnce(&str) -> String,
    ) -> Result<(), AnswerError> {
        // The lock is held only to read and to take the slot; parsing and sanitizing run
        // outside it so an SSE subscribe (`pending_event`) never waits on them.
        let request = self
            .pending
            .lock()
            .as_ref()
            .filter(|p| p.id == id)
            .map(|p| p.request.clone())
            .ok_or(AnswerError::NotPending)?;
        let outcome = match reply {
            AskUserReply::Cancel => AskUserOutcome::Cancelled,
            AskUserReply::Selected { indices } => AskUserOutcome::Answered(
                validate_selection(&request, &indices).ok_or(AnswerError::Invalid)?,
            ),
            AskUserReply::Text { text } => {
                let answer = request
                    .parse_reply(&text)
                    .map_err(|_| AnswerError::Invalid)?;
                AskUserOutcome::Answered(match answer {
                    AskUserAnswer::Text(t) => AskUserAnswer::Text(sanitize(&t)),
                    selected @ AskUserAnswer::Selected { .. } => selected,
                })
            }
        };
        let taken = {
            let mut slot = self.pending.lock();
            if slot.as_ref().is_some_and(|p| p.id == id) {
                slot.take()
            } else {
                None
            }
        };
        // Another answer, a timeout or a cancel may have resolved the question meanwhile.
        let taken = taken.ok_or(AnswerError::NotPending)?;
        // A dropped receiver means the question already ended; the answer is moot.
        let _ = taken.reply.send(outcome);
        Ok(())
    }
}

impl AskUserResponder for ServeAskUserResponder {
    fn ask(
        &self,
        request: AskUserRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<AskUserOutcome, ChannelError>> + Send + '_>>
    {
        Box::pin(async move {
            let id = QuestionId(self.next_id.fetch_add(1, Ordering::Relaxed));
            let event = event_for(id, &request);
            let (reply, answered) = oneshot::channel();
            {
                let mut slot = self.pending.lock();
                if slot.is_some() || self.tx_out.receiver_count() == 0 {
                    return Ok(AskUserOutcome::Declined(DeclineReason::NonInteractive));
                }
                *slot = Some(Pending {
                    id,
                    request,
                    event: event.clone(),
                    reply,
                });
            }
            let _guard = SlotGuard {
                slot: &self.pending,
                id,
            };
            let _ = self.tx_out.send(SessionOutput::AskUser(event));
            Ok(answered
                .await
                .unwrap_or(AskUserOutcome::Declined(DeclineReason::NonInteractive)))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::channel::ChoiceOptions;

    fn single() -> AskUserRequest {
        let options = ChoiceOptions::new(vec!["red".into(), "green".into()]).unwrap();
        AskUserRequest::single_choice("Color?", options, OtherAnswer::Forbidden).unwrap()
    }

    fn responder() -> (
        Arc<ServeAskUserResponder>,
        broadcast::Receiver<SessionOutput>,
    ) {
        let (tx, rx) = broadcast::channel(8);
        (Arc::new(ServeAskUserResponder::new(tx)), rx)
    }

    async fn next_question(rx: &mut broadcast::Receiver<SessionOutput>) -> AskUserEvent {
        match rx.recv().await.unwrap() {
            SessionOutput::AskUser(ev) => ev,
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declines_without_subscriber() {
        let (tx, rx) = broadcast::channel(8);
        drop(rx);
        let r = ServeAskUserResponder::new(tx);
        let out = r.ask(single()).await.unwrap();
        assert_eq!(out, AskUserOutcome::Declined(DeclineReason::NonInteractive));
        assert!(r.pending_event().is_none());
    }

    #[tokio::test]
    async fn answer_resolves_question_once() {
        let (r, mut rx) = responder();
        let r2 = Arc::clone(&r);
        let task = tokio::spawn(async move { r2.ask(single()).await.unwrap() });
        let ev = next_question(&mut rx).await;
        assert_eq!(ev.kind, WireQuestionKind::SingleChoice);
        assert_eq!(ev.options, ["red", "green"]);
        assert_eq!(r.pending_event(), Some(ev.clone()));
        r.answer(
            ev.question_id,
            AskUserReply::Selected { indices: vec![1] },
            str::to_owned,
        )
        .unwrap();
        assert_eq!(
            task.await.unwrap(),
            AskUserOutcome::Answered(AskUserAnswer::Selected { indices: vec![1] })
        );
        assert_eq!(
            r.answer(ev.question_id, AskUserReply::Cancel, str::to_owned),
            Err(AnswerError::NotPending)
        );
    }

    #[tokio::test]
    async fn invalid_reply_keeps_question_pending() {
        let (r, mut rx) = responder();
        let r2 = Arc::clone(&r);
        let task = tokio::spawn(async move { r2.ask(single()).await.unwrap() });
        let ev = next_question(&mut rx).await;
        for bad in [vec![], vec![2], vec![0, 1]] {
            assert_eq!(
                r.answer(
                    ev.question_id,
                    AskUserReply::Selected { indices: bad },
                    str::to_owned
                ),
                Err(AnswerError::Invalid)
            );
        }
        assert_eq!(
            r.answer(
                ev.question_id,
                AskUserReply::Text {
                    text: "purple".into()
                },
                str::to_owned
            ),
            Err(AnswerError::Invalid)
        );
        r.answer(ev.question_id, AskUserReply::Cancel, str::to_owned)
            .unwrap();
        assert_eq!(task.await.unwrap(), AskUserOutcome::Cancelled);
    }

    #[tokio::test]
    async fn stale_question_id_is_rejected() {
        let (r, mut rx) = responder();
        let r2 = Arc::clone(&r);
        let task = tokio::spawn(async move { r2.ask(single()).await.unwrap() });
        let ev = next_question(&mut rx).await;
        let stale = QuestionId(ev.question_id.0 + 100);
        assert_eq!(
            r.answer(stale, AskUserReply::Cancel, str::to_owned),
            Err(AnswerError::NotPending)
        );
        r.answer(ev.question_id, AskUserReply::Cancel, str::to_owned)
            .unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn slot_is_not_locked_while_sanitizing() {
        let (r, mut rx) = responder();
        let r2 = Arc::clone(&r);
        let task = tokio::spawn(async move {
            r2.ask(AskUserRequest::free_text("Where?").unwrap())
                .await
                .unwrap()
        });
        let ev = next_question(&mut rx).await;
        r.answer(
            ev.question_id,
            AskUserReply::Text { text: "x".into() },
            |t| {
                assert!(r.pending_event().is_some(), "slot lock must be free");
                t.to_owned()
            },
        )
        .unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn dropped_question_clears_pending_slot() {
        let (r, mut rx) = responder();
        let timed_out =
            tokio::time::timeout(std::time::Duration::from_millis(50), r.ask(single())).await;
        assert!(timed_out.is_err());
        let _ = next_question(&mut rx).await;
        assert!(r.pending_event().is_none());
    }

    #[tokio::test]
    async fn free_text_is_sanitized_but_label_selection_is_not() {
        let (r, mut rx) = responder();
        let r2 = Arc::clone(&r);
        let task = tokio::spawn(async move {
            r2.ask(AskUserRequest::free_text("Where?").unwrap())
                .await
                .unwrap()
        });
        let ev = next_question(&mut rx).await;
        r.answer(
            ev.question_id,
            AskUserReply::Text { text: "x".into() },
            |t| format!("<w>{t}</w>"),
        )
        .unwrap();
        assert_eq!(
            task.await.unwrap(),
            AskUserOutcome::Answered(AskUserAnswer::Text("<w>x</w>".into()))
        );

        let r3 = Arc::clone(&r);
        let task = tokio::spawn(async move { r3.ask(single()).await.unwrap() });
        let ev = next_question(&mut rx).await;
        r.answer(
            ev.question_id,
            AskUserReply::Text {
                text: "green".into(),
            },
            |t| format!("<w>{t}</w>"),
        )
        .unwrap();
        assert_eq!(
            task.await.unwrap(),
            AskUserOutcome::Answered(AskUserAnswer::Selected { indices: vec![1] })
        );
    }
}
