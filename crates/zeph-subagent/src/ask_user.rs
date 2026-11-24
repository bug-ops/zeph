// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Relay of the model-callable `ask_user` tool from a sub-agent to its parent (spec 090).
//!
//! A sub-agent never talks to the user. `AskUserToolExecutor` exposes the tool to the
//! sub-agent's model and forwards each call as an [`AskUserQuestion`] over a per-agent channel.
//! The parent drains that channel while it waits for the foreground sub-agent and answers every
//! question through its own `ask_user` choke point (cap, deadline, secret scrub, channel
//! sanitizing), then sends the finished [`AskUserReply`] back.
//!
//! The tool is installed only for a foreground spawn whose definition carries an explicit
//! [`AskUserGrant::Allow`](crate::def::AskUserGrant) and whose parent supplied a
//! [`SubagentAskUser`]; see `plan_install`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use zeph_common::ToolName;
use zeph_tools::executor::{
    CheckpointActionResult, CheckpointListResult, ClaimSource, ErasedToolExecutor, ToolCall,
    ToolError, ToolOutput, ToolStatus,
};
use zeph_tools::registry::ToolDef;

use crate::def::{AskUserGrant, SubAgentDef};

/// Extra time the relay waits beyond the parent's question deadline before giving up, so the
/// parent's own timeout reply (not the relay's) is the normal outcome.
const REPLY_GRACE: Duration = Duration::from_secs(5);

/// Questions that may be queued per sub-agent; tool calls of one agent are answered in turn.
const QUESTION_QUEUE: usize = 1;

/// Parent-supplied `ask_user` offer, set on [`SpawnContext`](crate::SpawnContext) only by the
/// foreground spawn and resume paths.
///
/// Carries the one tool definition the parent owns (so the schema has a single source) and the
/// deadline the parent applies to a question.
#[derive(Debug, Clone)]
pub struct SubagentAskUser {
    /// Definition of the `ask_user` tool as the parent offers it to its own model.
    pub tool: ToolDef,
    /// Deadline the parent applies to one question; the relay waits slightly longer.
    pub timeout: Duration,
}

/// A question forwarded from a sub-agent's tool call to its parent.
#[derive(Debug)]
pub struct AskUserQuestion {
    /// Raw tool input as the sub-agent's model produced it; the parent validates it.
    pub input: serde_json::Value,
    /// Where the parent sends the finished reply.
    pub reply: oneshot::Sender<AskUserReply>,
}

/// The parent's finished answer to a forwarded question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserReply {
    /// Tool result text, already scrubbed and sanitized by the parent's choke point.
    pub text: String,
    /// Typed outcome of the call.
    pub status: ToolStatus,
}

/// Installation arguments for [`AskUserToolExecutor`].
pub(crate) struct AskUserInstall {
    tool: ToolDef,
    timeout: Duration,
    tx: mpsc::Sender<AskUserQuestion>,
    cancel: CancellationToken,
}

/// Decide whether a spawn gets the `ask_user` relay and create its question channel.
///
/// Returns `Some` only when the definition grants [`AskUserGrant::Allow`], the definition is not
/// a background agent, and the parent supplied an offer. The receiver goes to the manager, which
/// hands it to the parent's wait loop.
pub(crate) fn plan_install(
    def: &SubAgentDef,
    offer: Option<&SubagentAskUser>,
    cancel: &CancellationToken,
) -> Option<(AskUserInstall, mpsc::Receiver<AskUserQuestion>)> {
    let offer = offer?;
    if def.permissions.ask_user != AskUserGrant::Allow || def.permissions.background {
        return None;
    }
    let (tx, rx) = mpsc::channel(QUESTION_QUEUE);
    let install = AskUserInstall {
        tool: offer.tool.clone(),
        timeout: offer.timeout,
        tx,
        cancel: cancel.clone(),
    };
    Some((install, rx))
}

/// Tool executor decorator exposing `ask_user` to a sub-agent and relaying calls to the parent.
///
/// `pub(crate)` and built only by [`build_filtered_executor`](crate::manager::spawn), so it
/// exists only in sub-agent executor chains. It is the one producer of
/// [`ClaimSource::UserAnswer`] besides the main agent's own handler; the sub-agent loop runs
/// every tool output through its sanitizer regardless of that claim.
pub(crate) struct AskUserToolExecutor {
    inner: Arc<dyn ErasedToolExecutor>,
    tool: ToolDef,
    timeout: Duration,
    tx: mpsc::Sender<AskUserQuestion>,
    cancel: CancellationToken,
}

impl AskUserToolExecutor {
    pub(crate) fn new(inner: Arc<dyn ErasedToolExecutor>, install: AskUserInstall) -> Self {
        Self {
            inner,
            tool: install.tool,
            timeout: install.timeout,
            tx: install.tx,
            cancel: install.cancel,
        }
    }

    fn is_ask_user(&self, tool_id: &str) -> bool {
        tool_id == self.tool.id
    }

    fn output(&self, reply: AskUserReply) -> ToolOutput {
        ToolOutput {
            tool_name: ToolName::new(self.tool.id.as_ref()),
            summary: reply.text,
            blocks_executed: 1,
            claim_source: Some(ClaimSource::UserAnswer),
            status: reply.status,
            ..Default::default()
        }
    }

    fn failed(&self, text: &str) -> ToolOutput {
        self.output(AskUserReply {
            text: text.to_owned(),
            status: ToolStatus::Failed,
        })
    }

    #[tracing::instrument(name = "subagent.ask_user.relay", skip_all, level = "debug")]
    async fn relay(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let question = AskUserQuestion {
            input: serde_json::Value::Object(call.params.clone()),
            reply: reply_tx,
        };
        let exchange = async {
            self.tx.send(question).await.ok()?;
            reply_rx.await.ok()
        };
        let reply = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = tokio::time::timeout(self.timeout + REPLY_GRACE, exchange) => r,
        };
        Ok(Some(match reply {
            Ok(Some(reply)) => self.output(reply),
            Ok(None) => self.failed("Error: the question could not be delivered to the user"),
            Err(_) => self.output(AskUserReply {
                text: "The user did not answer in time. Continue without an answer.".to_owned(),
                status: ToolStatus::Success,
            }),
        }))
    }
}

impl ErasedToolExecutor for AskUserToolExecutor {
    fn execute_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
    > {
        self.inner.execute_erased(response)
    }

    fn execute_confirmed_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
    > {
        self.inner.execute_confirmed_erased(response)
    }

    fn mcp_dispatch_ids_erased(&self) -> Arc<zeph_tools::McpToolIds> {
        self.inner.mcp_dispatch_ids_erased()
    }

    fn tool_definitions_erased(&self) -> Vec<ToolDef> {
        let mut defs = self.inner.tool_definitions_erased();
        defs.push(self.tool.clone());
        defs
    }

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
    > {
        if self.is_ask_user(call.tool_id.as_str()) {
            return Box::pin(self.relay(call));
        }
        self.inner.execute_tool_call_erased(call)
    }

    fn execute_tool_call_confirmed_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
    > {
        if self.is_ask_user(call.tool_id.as_str()) {
            return Box::pin(self.relay(call));
        }
        self.inner.execute_tool_call_confirmed_erased(call)
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: zeph_tools::SkillTrustLevel) {
        self.inner.set_effective_trust(level);
    }

    fn is_tool_retryable_erased(&self, tool_id: &str) -> bool {
        !self.is_ask_user(tool_id) && self.inner.is_tool_retryable_erased(tool_id)
    }

    fn requires_confirmation_erased(&self, call: &ToolCall) -> bool {
        !self.is_ask_user(call.tool_id.as_str()) && self.inner.requires_confirmation_erased(call)
    }

    fn checkpoint_undo_erased(&self, n: usize) -> CheckpointActionResult {
        self.inner.checkpoint_undo_erased(n)
    }

    fn checkpoint_redo_erased(&self) -> CheckpointActionResult {
        self.inner.checkpoint_redo_erased()
    }

    fn checkpoint_list_erased(&self) -> CheckpointListResult {
        self.inner.checkpoint_list_erased()
    }

    fn is_tool_speculatable_erased(&self, tool_id: &str) -> bool {
        !self.is_ask_user(tool_id) && self.inner.is_tool_speculatable_erased(tool_id)
    }
}
