// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Manager side of the sub-agent `ask_user` relay (spec 090): the parent's view of questions
//! that foreground sub-agents forward, and the cancel token that bounds each answer.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::SubAgentManager;
use crate::ask_user::AskUserQuestion;

impl SubAgentManager {
    /// Remember the question channel of a freshly spawned or resumed sub-agent.
    pub(super) fn register_ask_user_questions(
        &mut self,
        task_id: &str,
        rx: Option<mpsc::Receiver<AskUserQuestion>>,
    ) {
        if let Some(rx) = rx {
            self.ask_user_questions.insert(task_id.to_owned(), rx);
        }
    }

    /// Remove a sub-agent handle together with its question channel.
    ///
    /// The one removal path for `agents`, so a stale `ask_user` receiver cannot outlive its
    /// handle.
    pub(super) fn take_agent(&mut self, task_id: &str) -> Option<super::SubAgentHandle> {
        self.ask_user_questions.remove(task_id);
        self.agents.remove(task_id)
    }

    /// Take one pending `ask_user` question from `task_id` (non-blocking).
    ///
    /// Polls only that sub-agent's channel, so a sibling's question is never popped and lost.
    /// Returns `None` when the agent holds no `ask_user` relay, has no pending question, or is
    /// unknown.
    pub fn try_recv_ask_user_question_for(&mut self, task_id: &str) -> Option<AskUserQuestion> {
        self.ask_user_questions.get_mut(task_id)?.try_recv().ok()
    }

    /// A child of `task_id`'s cancel token: cancelled when the sub-agent is cancelled or
    /// times out, so a question the parent is still showing is cancelled with it.
    ///
    /// Returns `None` for an unknown task id.
    #[must_use]
    pub fn cancel_child_token(&self, task_id: &str) -> Option<CancellationToken> {
        self.agents.get(task_id).map(|h| h.cancel.child_token())
    }
}
