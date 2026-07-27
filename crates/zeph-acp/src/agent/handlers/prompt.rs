// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/prompt` (ACP method `"session/prompt"`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `prompt` request.
///
/// `do_prompt` runs a whole agent turn, which for permission-gated tool calls awaits the IDE's
/// reply to a `session/request_permission` request routed back through this same connection's
/// dispatch loop. Per the SDK's ordering contract (`agent_client_protocol::concepts::ordering`),
/// `on_receive_request` callbacks hold that loop until they return — awaiting `do_prompt` inline
/// here would deadlock the turn against its own permission response. `cx.spawn` escapes the loop
/// so it stays free to route the permission reply (and any other inbound traffic) while the turn
/// runs; the response is sent from inside the spawned task once the turn completes (#6656).
///
/// When the `unstable-cancel-request` feature is enabled, this bridges the real ACP
/// `$/cancel_request` protocol notification (scoped to this specific JSON-RPC request via
/// [`acp::Responder::cancellation`]) onto the session's existing `cancel_signal: Arc<Notify>` —
/// the same signal `session/cancel` notifies. A short-lived watcher task races the cancellation
/// marker against prompt completion so it never outlives this request.
pub(crate) async fn handle_prompt(
    req: acp::schema::v1::PromptRequest,
    responder: acp::Responder<acp::schema::v1::PromptResponse>,
    cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    #[cfg(feature = "unstable-cancel-request")]
    let bridge_cx = cx.clone();

    cx.spawn(async move {
        #[cfg(feature = "unstable-cancel-request")]
        let cancel_request_bridge =
            spawn_cancel_request_bridge(&req, &responder, &bridge_cx, &state);

        let result = state.do_prompt(req).await;

        #[cfg(feature = "unstable-cancel-request")]
        drop(cancel_request_bridge);

        // Infallible by construction: a `respond`/`respond_with_error` failure means the
        // connection is already going away (e.g. the client disconnected mid-turn), not a
        // problem with this prompt — log and swallow instead of returning `Err`, since per
        // `ConnectionTo::spawn`'s contract, an `Err` returned from this future would tear down
        // the *entire* connection over what is otherwise a benign, unrelated disconnect.
        let send_result = match result {
            Ok(resp) => responder.respond(resp),
            Err(e) => responder.respond_with_error(e),
        };
        if let Err(e) = send_result {
            tracing::debug!(error = %e, "failed to send session/prompt response");
        }
        Ok(())
    })
}

/// Spawn a watcher that notifies `entry.cancel_signal` if the IDE sends `$/cancel_request` for
/// this `session/prompt` request before it completes.
///
/// Returns a guard whose [`Drop`] unblocks the watcher once the prompt finishes normally, so the
/// watcher task never lives beyond this single request even when cancellation never fires.
#[cfg(feature = "unstable-cancel-request")]
fn spawn_cancel_request_bridge(
    req: &acp::schema::v1::PromptRequest,
    responder: &acp::Responder<acp::schema::v1::PromptResponse>,
    cx: &acp::ConnectionTo<acp::Client>,
    state: &Arc<ZephAcpAgentState>,
) -> tokio::sync::oneshot::Sender<()> {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    if let Some(cancel_signal) = state.session_cancel_signal(&req.session_id) {
        let cancellation = responder.cancellation();
        // Infallible by construction: never returns Err, so a misbehaving watcher can never
        // bring down the whole connection (per `ConnectionTo::spawn` contract).
        let spawn_result = cx.spawn(async move {
            tokio::select! {
                // `done_rx` is checked first so prompt completion deterministically wins once
                // it's ready, even if `cancellation` also becomes ready in the same poll (e.g.
                // a `$/cancel_request` arriving right as the prompt finishes) — otherwise an
                // unbiased pick could call `notify_one()` after this watcher's job is already
                // done, leaking a stale permit onto the session's shared `cancel_signal` that
                // would silently cancel the *next*, unrelated prompt.
                biased;
                _ = done_rx => {}
                () = cancellation.cancelled() => cancel_signal.notify_one(),
            }
            Ok(())
        });
        if let Err(e) = spawn_result {
            tracing::debug!(error = %e, "failed to spawn $/cancel_request watcher for prompt");
        }
    }
    done_tx
}
