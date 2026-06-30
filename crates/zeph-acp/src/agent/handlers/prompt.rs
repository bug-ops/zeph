// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/prompt` (ACP method `"session/prompt"`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `prompt` request.
///
/// When the `unstable-cancel-request` feature is enabled, this bridges the real ACP
/// `$/cancel_request` protocol notification (scoped to this specific JSON-RPC request via
/// [`acp::Responder::cancellation`]) onto the session's existing `cancel_signal: Arc<Notify>` —
/// the same signal `session/cancel` notifies. A short-lived watcher task races the cancellation
/// marker against prompt completion so it never outlives this request.
pub(crate) async fn handle_prompt(
    req: acp::schema::v1::PromptRequest,
    responder: acp::Responder<acp::schema::v1::PromptResponse>,
    #[cfg_attr(not(feature = "unstable-cancel-request"), allow(unused_variables))]
    cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    #[cfg(feature = "unstable-cancel-request")]
    let cancel_request_bridge = spawn_cancel_request_bridge(&req, &responder, &cx, &state);

    let resp = state.do_prompt(req).await?;

    #[cfg(feature = "unstable-cancel-request")]
    drop(cancel_request_bridge);

    responder.respond(resp)
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
