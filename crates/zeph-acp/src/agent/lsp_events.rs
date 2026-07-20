// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LSP notification handlers for `ZephAcpAgentState`.
//!
//! Groups the `lsp/publishDiagnostics` and `lsp/didSave` ext-notification handlers so the
//! LSP diagnostics-caching surface is isolated from the main agent dispatch logic in
//! [`super`]. Sole call site is `do_ext_notification` in [`super`].

use std::sync::Arc;

use agent_client_protocol as acp;

use super::ZephAcpAgentState;

impl ZephAcpAgentState {
    pub(crate) fn handle_lsp_publish_diagnostics(&self, params: &str) {
        #[derive(serde::Deserialize)]
        struct PublishDiagnosticsParams {
            uri: String,
            #[serde(default)]
            diagnostics: Vec<crate::lsp::LspDiagnostic>,
        }

        match serde_json::from_str::<PublishDiagnosticsParams>(params) {
            Ok(p) => {
                let max = self.lsp_config.max_diagnostics_per_file;
                let mut diags = p.diagnostics;
                diags.truncate(max);
                tracing::debug!(
                    uri = %p.uri,
                    count = diags.len(),
                    "lsp/publishDiagnostics: cached"
                );
                self.diagnostics_cache.write().update(p.uri, diags);
            }
            Err(e) => {
                tracing::warn!(error = %e, "lsp/publishDiagnostics: failed to parse params");
            }
        }
    }

    #[allow(clippy::unused_async)]
    pub(crate) async fn handle_lsp_did_save(
        &self,
        params: &str,
        cx: &acp::ConnectionTo<acp::Client>,
    ) {
        #[derive(serde::Deserialize)]
        struct DidSaveParams {
            uri: String,
        }

        if !self.lsp_config.auto_diagnostics_on_save {
            return;
        }

        let uri = match serde_json::from_str::<DidSaveParams>(params) {
            Ok(p) => p.uri,
            Err(e) => {
                tracing::warn!(error = %e, "lsp/didSave: failed to parse params");
                return;
            }
        };

        let params_json = serde_json::json!({ "uri": &uri });
        let raw = match serde_json::value::to_raw_value(&params_json) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "lsp/didSave: failed to serialize params");
                return;
            }
        };
        let params_value =
            serde_json::from_str::<serde_json::Value>(raw.get()).unwrap_or(serde_json::Value::Null);
        let req = acp::UntypedMessage::new("lsp/diagnostics", params_value).unwrap_or_else(|_| {
            acp::UntypedMessage {
                method: "lsp/diagnostics".to_owned(),
                params: serde_json::Value::Null,
            }
        });
        let timeout = std::time::Duration::from_secs(self.lsp_config.request_timeout_secs);
        // Outbound round-trip inside a notification handler: must use cx.spawn to avoid blocking dispatch.
        let diagnostics_cache = Arc::clone(&self.diagnostics_cache);
        let max = self.lsp_config.max_diagnostics_per_file;
        let cx_inner = cx.clone();
        let uri_clone = uri.clone();
        cx.spawn(async move {
            match tokio::time::timeout(timeout, cx_inner.send_request(req).block_task()).await {
                Ok(Ok(resp)) => {
                    match serde_json::from_value::<Vec<crate::lsp::LspDiagnostic>>(resp) {
                        Ok(mut diags) => {
                            diags.truncate(max);
                            tracing::debug!(
                                uri = %uri_clone,
                                count = diags.len(),
                                "lsp/didSave: fetched diagnostics"
                            );
                            diagnostics_cache.write().update(uri_clone, diags);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "lsp/didSave: failed to parse diagnostics response");
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "lsp/didSave: diagnostics request failed");
                }
                Err(_) => {
                    tracing::warn!(uri = %uri_clone, "lsp/didSave: diagnostics request timed out");
                }
            }
            Ok(())
        }).ok();
    }
}
