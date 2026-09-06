// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`PeerToolExecutor`]: the per-spawn tool executor decorator exposing peer messaging to a
//! sub-agent's own LLM (FR-012).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;
use zeph_common::ToolName;
use zeph_sanitizer::exfiltration::ExfiltrationGuard;
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};
use zeph_tools::executor::{
    CheckpointActionResult, CheckpointListResult, ErasedToolExecutor, ToolCall, ToolError,
    ToolOutput, deserialize_params,
};
use zeph_tools::registry::{InvocationHint, ToolDef};

use super::{AgentId, PeerMessage, PeerRouter};

const SEND_PEER_MESSAGE: &str = "send_peer_message";
const CHECK_MESSAGES: &str = "check_messages";
const LIST_PEERS: &str = "list_peers";

/// How often `check_messages(wait_ms)`'s wait records a progress heartbeat, well under any
/// realistic orchestration idle-timeout threshold (critic round-2 S2).
const PROGRESS_TICK: Duration = Duration::from_secs(5);

#[derive(Deserialize, JsonSchema)]
struct SendPeerMessageParams {
    /// `task_id` or unique display name of the recipient.
    target: String,
    /// The message body.
    body: String,
}

#[derive(Deserialize, JsonSchema)]
struct CheckMessagesParams {
    /// Milliseconds to wait for a new message if the mailbox is currently empty. Absent or
    /// `0` drains and returns immediately. Clamped to the configured `max_wait_ms` ceiling.
    #[serde(default)]
    wait_ms: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct ListPeersParams {}

#[derive(Serialize)]
struct MessageView {
    sender: String,
    body: String,
    sent_at: String,
}

#[derive(Serialize)]
struct PeerView {
    id: String,
    name: String,
    relation: &'static str,
}

fn is_peer_tool(tool_id: &str) -> bool {
    matches!(tool_id, SEND_PEER_MESSAGE | CHECK_MESSAGES | LIST_PEERS)
}

fn tool_output(tool_id: &str, summary: String) -> ToolOutput {
    ToolOutput {
        tool_name: ToolName::new(tool_id),
        summary,
        blocks_executed: 1,
        ..Default::default()
    }
}

/// Per-spawn [`ErasedToolExecutor`] decorator exposing `send_peer_message`, `check_messages`,
/// and `list_peers` to a sub-agent's own LLM (FR-012), modelled structurally on
/// [`NetworkDenyToolExecutor`](crate::filter::NetworkDenyToolExecutor).
///
/// Constructed once per spawn with its own [`AgentId`] baked in — never taken from a tool
/// argument or `ToolCall.caller_id` — so an LLM cannot spoof its sender identity (critic S2).
///
/// # Single-consumer mailbox receiver
///
/// [`mailbox_rx`][Self] is held as a [`tokio::sync::Mutex`] (await-aware, not a `std`/
/// `parking_lot` one) so `execute_tool_call_erased`'s `&self` receiver can still reach it
/// mutably. This is the **one deliberate hold-across-`.await`** in the whole design (plan.md
/// §8): the mutex is per-spawn, has exactly one logical consumer — this sub-agent's own
/// sequential tool calls — and is never acquired by the parent, the manager, or any sibling.
/// Treat a second consumer of this mutex as a real Await Discipline defect, not a variation
/// on this documented exception.
pub struct PeerToolExecutor {
    inner: Arc<dyn ErasedToolExecutor>,
    id: AgentId,
    router: Arc<PeerRouter>,
    mailbox_rx: AsyncMutex<mpsc::Receiver<PeerMessage>>,
    cancel: CancellationToken,
    progress_at: Option<Arc<AtomicU64>>,
    sanitizer: ContentSanitizer,
    exfil_guard: ExfiltrationGuard,
    max_wait_ms: u64,
}

impl PeerToolExecutor {
    /// Wrap `inner`, adding the three peer-messaging tools for the agent addressable as `id`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        inner: Arc<dyn ErasedToolExecutor>,
        id: AgentId,
        router: Arc<PeerRouter>,
        mailbox_rx: mpsc::Receiver<PeerMessage>,
        cancel: CancellationToken,
        progress_at: Option<Arc<AtomicU64>>,
        sanitizer: ContentSanitizer,
        exfil_guard: ExfiltrationGuard,
        max_wait_ms: u64,
    ) -> Self {
        Self {
            inner,
            id,
            router,
            mailbox_rx: AsyncMutex::new(mailbox_rx),
            cancel,
            progress_at,
            sanitizer,
            exfil_guard,
            max_wait_ms,
        }
    }

    /// Sanitize and exfiltration-scan one message body before it reaches the LLM (NFR-007).
    fn sanitize_body(&self, sender_name: &str, body: &str) -> String {
        let source =
            ContentSource::new(ContentSourceKind::SubagentPeerMessage).with_identifier(sender_name);
        let sanitized = self.sanitizer.sanitize(body, source);
        if !sanitized.injection_flags.is_empty() {
            tracing::warn!(
                sender = sender_name,
                flags = sanitized.injection_flags.len(),
                "injection patterns detected in peer message body"
            );
        }
        let (cleaned, events) = self.exfil_guard.scan_output(&sanitized.body);
        if !events.is_empty() {
            tracing::warn!(
                sender = sender_name,
                blocked = events.len(),
                "exfiltration guard blocked content in peer message body"
            );
        }
        cleaned
    }

    fn drain_ready(rx: &mut mpsc::Receiver<PeerMessage>, out: &mut Vec<PeerMessage>) {
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
    }

    #[tracing::instrument(name = "subagent.mailbox.send_tool", skip(self, call), fields(sender = ?self.id))]
    async fn handle_send_peer_message(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let params: SendPeerMessageParams = deserialize_params(&call.params)?;
        // Critic round-4 item 5: `target` (and `DeliveryError`'s `Display`, which echoes it
        // verbatim) is LLM-supplied — hand-rolling this as a format string let a `target`
        // containing `"` produce malformed JSON or inject a spoofed `"delivered":true` key.
        // `check_messages`/`list_peers` already build their JSON via `serde_json`; this was
        // the one inconsistent handler.
        let summary = match self.router.send(&self.id, &params.target, params.body) {
            Ok(()) => serde_json::json!({"delivered": true}).to_string(),
            Err(e) => serde_json::json!({"delivered": false, "error": e.to_string()}).to_string(),
        };
        Ok(Some(tool_output(SEND_PEER_MESSAGE, summary)))
    }

    /// `wait_ms` clamped by config and raced against `cancel.cancelled()`. A periodic
    /// progress tick during the wait keeps orchestration idle-detection from reaping a
    /// legitimately-waiting sub-agent (critic round-2 S2).
    #[tracing::instrument(name = "subagent.mailbox.wait", skip(self, call), fields(agent = ?self.id))]
    async fn handle_check_messages(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let params: CheckMessagesParams = deserialize_params(&call.params)?;
        let wait_ms = u64::from(params.wait_ms.unwrap_or(0)).min(self.max_wait_ms);

        let mut rx = self.mailbox_rx.lock().await;
        let mut messages = Vec::new();
        Self::drain_ready(&mut rx, &mut messages);

        if messages.is_empty() && wait_ms > 0 {
            let mut ticker = tokio::time::interval(PROGRESS_TICK);
            ticker.tick().await; // first tick fires immediately — consume it before the loop
            let sleep = tokio::time::sleep(Duration::from_millis(wait_ms));
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => break,
                    recv_result = rx.recv() => {
                        if let Some(msg) = recv_result {
                            messages.push(msg);
                            Self::drain_ready(&mut rx, &mut messages);
                        }
                        break;
                    }
                    _ = ticker.tick() => {
                        crate::agent_loop::record_progress(self.progress_at.as_ref());
                    }
                    () = &mut sleep => break,
                }
            }
        }

        drop(rx);

        // Critic round-4 M8: a `remaining` count computed here would be structurally always
        // 0 outside a same-instant race — both `drain_ready` calls above (the initial drain
        // and, on the wait path, the post-delivery drain) already exhaust the mailbox before
        // this point, so it would read to the LLM as "call me again" when that's essentially
        // never true. Dropped rather than shipped as a misleading field.
        let views: Vec<MessageView> = messages
            .into_iter()
            .map(|m| MessageView {
                body: self.sanitize_body(&m.sender_name, &m.body),
                sender: m.sender_name,
                sent_at: m.sent_at.to_rfc3339(),
            })
            .collect();

        let summary = serde_json::to_string(&serde_json::json!({ "messages": views }))
            .unwrap_or_else(|_| "{\"messages\":[]}".to_owned());

        Ok(Some(tool_output(CHECK_MESSAGES, summary)))
    }

    fn handle_list_peers(&self) -> ToolOutput {
        let views: Vec<PeerView> = self
            .router
            .peers_for(&self.id)
            .into_iter()
            .map(|p| PeerView {
                // Critic round-4 S1: must be the exact string `resolve_target` accepts back
                // as `send_peer_message`'s `target` argument — `AgentId`'s `{:?}` form (e.g.
                // `Task("9f2c-...")`) is not parseable and, worse, `resolve_target` would
                // then fall through to a *name* match, so two sub-agents spawned from the
                // same definition (sharing a display name) become mutually unaddressable.
                // `Task`'s payload *is* the task_id `resolve_target` matches first; a `Root`
                // has no task_id, so its name (already unique — one root per group) is the
                // only resolvable string for it.
                id: match &p.id {
                    AgentId::Task(task_id) => task_id.clone(),
                    AgentId::Root(_) => p.name.clone(),
                },
                name: p.name,
                relation: match p.relation {
                    super::PeerRelation::Parent => "parent",
                    super::PeerRelation::Sibling => "sibling",
                    super::PeerRelation::Child => "child",
                },
            })
            .collect();
        let summary = serde_json::to_string(&views).unwrap_or_else(|_| "[]".to_owned());
        tool_output(LIST_PEERS, summary)
    }

    async fn dispatch(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        match call.tool_id.as_str() {
            SEND_PEER_MESSAGE => self.handle_send_peer_message(call).await,
            CHECK_MESSAGES => self.handle_check_messages(call).await,
            LIST_PEERS => Ok(Some(self.handle_list_peers())),
            _ => unreachable!("dispatch only called for is_peer_tool ids"),
        }
    }
}

impl ErasedToolExecutor for PeerToolExecutor {
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

    fn tool_definitions_erased(&self) -> Vec<ToolDef> {
        let mut defs = self.inner.tool_definitions_erased();
        defs.push(ToolDef {
            id: SEND_PEER_MESSAGE.into(),
            description: "Send a message to another addressable agent (your spawner or a \
                          sibling sub-agent) by task_id or display name. Use list_peers to \
                          discover who you may address. The recipient does not need to \
                          terminate or be respawned to receive it."
                .into(),
            schema: schemars::schema_for!(SendPeerMessageParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        });
        defs.push(ToolDef {
            id: CHECK_MESSAGES.into(),
            description: "Drain your inbound peer-message mailbox. With wait_ms unset or 0, \
                          returns immediately (empty list if nothing is queued). With wait_ms \
                          set, parks for up to that many milliseconds for a message to arrive \
                          before returning, so you can wait for a reply in a single turn."
                .into(),
            schema: schemars::schema_for!(CheckMessagesParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        });
        defs.push(ToolDef {
            id: LIST_PEERS.into(),
            description: "List the addressable agents (your spawner and sibling sub-agents) \
                          you are authorized to message, with their relation to you."
                .into(),
            schema: schemars::schema_for!(ListPeersParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        });
        defs
    }

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
    > {
        if is_peer_tool(call.tool_id.as_str()) {
            return Box::pin(self.dispatch(call));
        }
        self.inner.execute_tool_call_erased(call)
    }

    fn execute_tool_call_confirmed_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
    > {
        if is_peer_tool(call.tool_id.as_str()) {
            return Box::pin(self.dispatch(call));
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
        if is_peer_tool(tool_id) {
            // send_peer_message is side-effecting; check_messages is a destructive drain —
            // retrying either could duplicate a send or silently skip messages already
            // consumed. Neither is safe to retry.
            return false;
        }
        self.inner.is_tool_retryable_erased(tool_id)
    }

    fn requires_confirmation_erased(&self, call: &ToolCall) -> bool {
        if is_peer_tool(call.tool_id.as_str()) {
            return false;
        }
        self.inner.requires_confirmation_erased(call)
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
        if is_peer_tool(tool_id) {
            return false;
        }
        self.inner.is_tool_speculatable_erased(tool_id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use zeph_config::PeerMessagingConfig;
    use zeph_sanitizer::ContentIsolationConfig;
    use zeph_sanitizer::exfiltration::ExfiltrationGuardConfig;

    use super::*;
    use crate::peer::{PeerGroupId, PeerRouter};

    struct NoopExecutor;

    impl ErasedToolExecutor for NoopExecutor {
        fn execute_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn execute_confirmed_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn tool_definitions_erased(&self) -> Vec<ToolDef> {
            vec![]
        }

        fn execute_tool_call_erased<'a>(
            &'a self,
            _call: &'a ToolCall,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
            false
        }

        zeph_tools::erased_tool_executor_no_inner_defaults!();
    }

    fn call(tool_id: &str, params: serde_json::Value) -> ToolCall {
        let params = match params {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        ToolCall {
            tool_id: tool_id.into(),
            params,
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    fn make_executor(
        id: AgentId,
        router: Arc<PeerRouter>,
        rx: mpsc::Receiver<PeerMessage>,
        cancel: CancellationToken,
        progress_at: Option<Arc<AtomicU64>>,
    ) -> PeerToolExecutor {
        PeerToolExecutor::new(
            Arc::new(NoopExecutor),
            id,
            router,
            rx,
            cancel,
            progress_at,
            ContentSanitizer::new(&ContentIsolationConfig::default()),
            ExfiltrationGuard::new(ExfiltrationGuardConfig::default()),
            30_000,
        )
    }

    #[test]
    fn tool_definitions_include_the_three_peer_tools_plus_inner() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (_reg, rx) = router.register(
            AgentId::Task("t1".into()),
            "t1".into(),
            None,
            PeerGroupId::Session,
        );
        let exec = make_executor(
            AgentId::Task("t1".into()),
            router,
            rx,
            CancellationToken::new(),
            None,
        );
        let ids: Vec<String> = exec
            .tool_definitions_erased()
            .into_iter()
            .map(|d| d.id.into_owned())
            .collect();
        assert!(ids.contains(&SEND_PEER_MESSAGE.to_owned()));
        assert!(ids.contains(&CHECK_MESSAGES.to_owned()));
        assert!(ids.contains(&LIST_PEERS.to_owned()));
    }

    #[tokio::test]
    async fn non_peer_tool_is_forwarded_to_inner_unchanged() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (_reg, rx) = router.register(
            AgentId::Task("t1".into()),
            "t1".into(),
            None,
            PeerGroupId::Session,
        );
        let exec = make_executor(
            AgentId::Task("t1".into()),
            router,
            rx,
            CancellationToken::new(),
            None,
        );
        let result = exec
            .execute_tool_call_erased(&call("bash", serde_json::json!({})))
            .await;
        assert!(
            result.unwrap().is_none(),
            "NoopExecutor always returns None"
        );
    }

    #[tokio::test]
    async fn send_peer_message_reports_delivered_true() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (root_reg, root_rx) = router.register(
            AgentId::Root(PeerGroupId::Session),
            "spawner".into(),
            None,
            PeerGroupId::Session,
        );
        drop(root_rx);
        let (a_reg, a_rx) = router.register(
            AgentId::Task("a".into()),
            "a".into(),
            Some(root_reg.id().clone()),
            PeerGroupId::Session,
        );
        let (_b_reg, b_rx) = router.register(
            AgentId::Task("b".into()),
            "b".into(),
            Some(root_reg.id().clone()),
            PeerGroupId::Session,
        );
        let exec_a = make_executor(
            AgentId::Task("a".into()),
            Arc::clone(&router),
            a_rx,
            CancellationToken::new(),
            None,
        );
        let _exec_b = make_executor(
            AgentId::Task("b".into()),
            router,
            b_rx,
            CancellationToken::new(),
            None,
        );
        let out = exec_a
            .execute_tool_call_erased(&call(
                SEND_PEER_MESSAGE,
                serde_json::json!({"target": "b", "body": "hi"}),
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(out.summary.contains("\"delivered\":true"));
        drop(a_reg);
    }

    #[tokio::test]
    async fn send_peer_message_round_trip_addressed_by_list_peers_returned_id() {
        // Critic round-4 item 8: the only prior US-001 coverage sent via
        // `router.send(...)` directly with the raw `task_id`, bypassing the decorator
        // entirely — so S1's `list_peers` Debug-format bug (a value that couldn't be fed
        // back into `send_peer_message`) went uncaught. This test drives the real
        // decorator round trip end to end, and deliberately uses two sub-agents that share
        // the same *display name* (US-001's own "researcher"/"implementer" scenario, both
        // spawned from one `SubAgentDef`) so a name-based `id` would make them mutually
        // unaddressable — proving `list_peers` must return the `task_id`, not the name.
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (root_reg, root_rx) = router.register(
            AgentId::Root(PeerGroupId::Session),
            "spawner".into(),
            None,
            PeerGroupId::Session,
        );
        drop(root_rx);
        let (a_reg, a_rx) = router.register(
            AgentId::Task("task-a".into()),
            "shared-name".into(),
            Some(root_reg.id().clone()),
            PeerGroupId::Session,
        );
        let (_b_reg, b_rx) = router.register(
            AgentId::Task("task-b".into()),
            "shared-name".into(),
            Some(root_reg.id().clone()),
            PeerGroupId::Session,
        );
        let exec_a = make_executor(
            AgentId::Task("task-a".into()),
            Arc::clone(&router),
            a_rx,
            CancellationToken::new(),
            None,
        );
        let exec_b = make_executor(
            AgentId::Task("task-b".into()),
            router,
            b_rx,
            CancellationToken::new(),
            None,
        );

        // Discover the target through list_peers, exactly as an LLM would.
        let peers_out = exec_a
            .execute_tool_call_erased(&call(LIST_PEERS, serde_json::json!({})))
            .await
            .unwrap()
            .unwrap();
        let peers: serde_json::Value = serde_json::from_str(&peers_out.summary).unwrap();
        let sibling_id = peers
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["relation"] == "sibling")
            .expect("sibling listed")["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            sibling_id, "task-b",
            "list_peers must return the resolvable task_id, not a name that collides"
        );

        let send_out = exec_a
            .execute_tool_call_erased(&call(
                SEND_PEER_MESSAGE,
                serde_json::json!({"target": sibling_id, "body": "redirect"}),
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(send_out.summary.contains("\"delivered\":true"));

        let recv_out = exec_b
            .execute_tool_call_erased(&call(CHECK_MESSAGES, serde_json::json!({})))
            .await
            .unwrap()
            .unwrap();
        assert!(recv_out.summary.contains("redirect"));
        drop(a_reg);
    }

    #[tokio::test]
    async fn check_messages_empty_drain_returns_empty_list_not_error() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (_reg, rx) = router.register(
            AgentId::Task("t1".into()),
            "t1".into(),
            None,
            PeerGroupId::Session,
        );
        let exec = make_executor(
            AgentId::Task("t1".into()),
            router,
            rx,
            CancellationToken::new(),
            None,
        );
        let out = exec
            .execute_tool_call_erased(&call(CHECK_MESSAGES, serde_json::json!({})))
            .await
            .unwrap()
            .unwrap();
        assert!(out.summary.contains("\"messages\":[]"));
    }

    #[tokio::test]
    async fn check_messages_preserves_arrival_order() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (root_reg, root_rx) = router.register(
            AgentId::Root(PeerGroupId::Session),
            "spawner".into(),
            None,
            PeerGroupId::Session,
        );
        let (_child_reg, child_rx) = router.register(
            AgentId::Task("child".into()),
            "child".into(),
            Some(root_reg.id().clone()),
            PeerGroupId::Session,
        );
        drop(root_rx);
        router
            .send(root_reg.id(), "child", "first".into())
            .expect("send 1");
        router
            .send(root_reg.id(), "child", "second".into())
            .expect("send 2");

        let exec = make_executor(
            AgentId::Task("child".into()),
            router,
            child_rx,
            CancellationToken::new(),
            None,
        );
        let out = exec
            .execute_tool_call_erased(&call(CHECK_MESSAGES, serde_json::json!({})))
            .await
            .unwrap()
            .unwrap();
        let first_idx = out.summary.find("first").expect("first present");
        let second_idx = out.summary.find("second").expect("second present");
        assert!(first_idx < second_idx, "arrival order must be preserved");
    }

    #[tokio::test(start_paused = true)]
    async fn check_messages_wait_returns_early_on_delivery() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (root_reg, root_rx) = router.register(
            AgentId::Root(PeerGroupId::Session),
            "spawner".into(),
            None,
            PeerGroupId::Session,
        );
        let (_child_reg, child_rx) = router.register(
            AgentId::Task("child".into()),
            "child".into(),
            Some(root_reg.id().clone()),
            PeerGroupId::Session,
        );
        drop(root_rx);
        let exec = make_executor(
            AgentId::Task("child".into()),
            Arc::clone(&router),
            child_rx,
            CancellationToken::new(),
            None,
        );

        let wait = tokio::spawn(async move {
            exec.execute_tool_call_erased(&call(
                CHECK_MESSAGES,
                serde_json::json!({"wait_ms": 30_000}),
            ))
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        router
            .send(root_reg.id(), "child", "reply".into())
            .expect("send while waiting");

        let out = tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("must not hit the outer test timeout")
            .expect("task join")
            .unwrap()
            .unwrap();
        assert!(out.summary.contains("reply"));
    }

    #[tokio::test(start_paused = true)]
    async fn check_messages_wait_times_out_to_empty() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (_reg, rx) = router.register(
            AgentId::Task("t1".into()),
            "t1".into(),
            None,
            PeerGroupId::Session,
        );
        let exec = make_executor(
            AgentId::Task("t1".into()),
            router,
            rx,
            CancellationToken::new(),
            None,
        );
        let check_call = call(CHECK_MESSAGES, serde_json::json!({"wait_ms": 200}));
        let call_fut = exec.execute_tool_call_erased(&check_call);
        tokio::pin!(call_fut);
        tokio::time::advance(Duration::from_millis(250)).await;
        let out = call_fut.await.unwrap().unwrap();
        assert!(out.summary.contains("\"messages\":[]"));
    }

    #[tokio::test(start_paused = true)]
    async fn check_messages_wait_aborts_immediately_on_cancellation() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (_reg, rx) = router.register(
            AgentId::Task("t1".into()),
            "t1".into(),
            None,
            PeerGroupId::Session,
        );
        let cancel = CancellationToken::new();
        let exec = make_executor(AgentId::Task("t1".into()), router, rx, cancel.clone(), None);
        cancel.cancel();
        let out = tokio::time::timeout(
            Duration::from_secs(1),
            exec.execute_tool_call_erased(&call(
                CHECK_MESSAGES,
                serde_json::json!({"wait_ms": 30_000}),
            )),
        )
        .await
        .expect("cancellation must return immediately, not after the 30s wait");
        assert!(out.unwrap().unwrap().summary.contains("\"messages\":[]"));
    }

    #[tokio::test(start_paused = true)]
    async fn check_messages_wait_records_progress_ticks() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (_reg, rx) = router.register(
            AgentId::Task("t1".into()),
            "t1".into(),
            None,
            PeerGroupId::Session,
        );
        // Seed with a sentinel a real `monotonic_millis()` reading can never produce (0 is
        // not safe here — a paused-clock test executes in well under 1ms of real wall-clock
        // time, so `record_progress`'s real-time write can legitimately read back as 0 even
        // though it fired; see `agent_loop::record_progress_tests` for the same pitfall).
        let progress = Arc::new(AtomicU64::new(u64::MAX));
        let exec = make_executor(
            AgentId::Task("t1".into()),
            router,
            rx,
            CancellationToken::new(),
            Some(Arc::clone(&progress)),
        );
        let exec = Arc::new(exec);
        let exec_for_task = Arc::clone(&exec);
        let handle = tokio::spawn(async move {
            let check_call = call(CHECK_MESSAGES, serde_json::json!({"wait_ms": 20_000}));
            exec_for_task.execute_tool_call_erased(&check_call).await
        });
        // `tokio::time::sleep` (unlike a bare `advance`) actually yields to the runtime under
        // `start_paused = true`, letting the spawned task run far enough to register its
        // timers before the virtual clock moves.
        tokio::time::sleep(Duration::from_secs(12)).await;
        assert_ne!(
            progress.load(Ordering::Relaxed),
            u64::MAX,
            "a progress tick must have fired during the wait"
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
        let _ = handle.await;
    }

    #[tokio::test]
    async fn list_peers_returns_only_peers_for_self() {
        let router = PeerRouter::new(PeerMessagingConfig::default(), None);
        let (root_reg, root_rx) = router.register(
            AgentId::Root(PeerGroupId::Session),
            "spawner".into(),
            None,
            PeerGroupId::Session,
        );
        let (child_reg, child_rx) = router.register(
            AgentId::Task("child".into()),
            "child".into(),
            Some(root_reg.id().clone()),
            PeerGroupId::Session,
        );
        drop(root_rx);
        let exec = make_executor(
            child_reg.id().clone(),
            router,
            child_rx,
            CancellationToken::new(),
            None,
        );
        let out = exec
            .execute_tool_call_erased(&call(LIST_PEERS, serde_json::json!({})))
            .await
            .unwrap()
            .unwrap();
        assert!(out.summary.contains("spawner"));
        assert!(out.summary.contains("parent"));
    }
}
