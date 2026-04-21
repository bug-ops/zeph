// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for extension method dispatch (ACP `on_receive_dispatch`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an unrecognised ACP dispatch message.
pub(crate) async fn handle_dispatch(
    message: acp::Dispatch,
    cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<acp::Handled<acp::Dispatch>> {
    match message {
        acp::Dispatch::Request(ref raw, ref _responder) => {
            let method = raw.method().to_owned();
            let params = raw.params().clone();
            let ext_req = build_ext_request(&method, &params)?;
            let resp = state.do_ext_method(ext_req).await?;
            // Serialize ExtResponse to serde_json::Value for UntypedMessage responder.
            let resp_value = serde_json::from_str::<serde_json::Value>(resp.0.get())
                .unwrap_or(serde_json::Value::Null);
            if let acp::Dispatch::Request(_, responder) = message {
                responder.respond(resp_value)?;
            }
            Ok(acp::Handled::Yes)
        }
        acp::Dispatch::Notification(ref raw) => {
            let method = raw.method().to_owned();
            let params = raw.params().clone();
            let ext_notif = build_ext_notification(&method, &params)?;
            state.do_ext_notification(ext_notif, &cx).await?;
            Ok(acp::Handled::Yes)
        }
        acp::Dispatch::Response(result, router) => Ok(acp::Handled::No {
            message: acp::Dispatch::Response(result, router),
            retry: false,
        }),
    }
}

fn build_ext_request(
    method: &str,
    params: &serde_json::Value,
) -> acp::Result<acp::schema::ExtRequest> {
    let raw = serde_json::value::to_raw_value(params)
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    Ok(acp::schema::ExtRequest::new(
        method,
        std::sync::Arc::from(raw),
    ))
}

fn build_ext_notification(
    method: &str,
    params: &serde_json::Value,
) -> acp::Result<acp::schema::ExtNotification> {
    let raw = serde_json::value::to_raw_value(params)
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    Ok(acp::schema::ExtNotification::new(
        method,
        std::sync::Arc::from(raw),
    ))
}
