// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/cocoon` slash-command handler for the agent loop.

use std::fmt::Write as _;

use tracing::Instrument as _;

use super::Agent;
use super::error::AgentError;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Handle `/cocoon <subcommand>` and return formatted output.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::UnknownCommand`] on unknown subcommands.
    /// Returns [`AgentError::ContextError`] when no Cocoon provider is configured.
    pub(super) async fn handle_cocoon_as_string(
        &mut self,
        args: &str,
    ) -> Result<String, AgentError> {
        match args.trim() {
            "status" => self.cocoon_status().await,
            "models" => self.cocoon_models().await,
            "" => Ok("Usage: /cocoon <subcommand>\n\n\
                 Subcommands:\n\
                 \x20 status  Show sidecar connection state, worker count, and TON balance\n\
                 \x20 models  List available models"
                .to_owned()),
            other => Err(AgentError::UnknownCommand(format!(
                "Unknown /cocoon subcommand: {other}. Available: status, models"
            ))),
        }
    }

    async fn cocoon_status(&mut self) -> Result<String, AgentError> {
        let client = self.build_cocoon_client()?;
        async {
            match client.health_check().await {
                Ok(health) => {
                    let proxy = if health.proxy_connected {
                        "connected"
                    } else {
                        "disconnected"
                    };
                    let mut out = format!(
                        "Cocoon sidecar status:\n\
                         \x20 Proxy: {proxy}\n\
                         \x20 Workers: {}",
                        health.worker_count
                    );
                    if let Some(balance) = health.ton_balance {
                        let _ = write!(out, "\n  TON balance: {balance:.4}");
                    }
                    Ok(out)
                }
                Err(_) => Ok("Cocoon: sidecar unreachable".to_owned()),
            }
        }
        .instrument(tracing::info_span!("tui.cocoon.status"))
        .await
    }

    async fn cocoon_models(&mut self) -> Result<String, AgentError> {
        let client = self.build_cocoon_client()?;
        async {
            match client.list_models().await {
                Ok(models) if models.is_empty() => Ok("Cocoon: no models available".to_owned()),
                Ok(models) => {
                    let mut list = String::new();
                    for m in &models {
                        let _ = writeln!(list, "  - {m}");
                    }
                    Ok(format!("Cocoon models ({}):\n{list}", models.len()))
                }
                Err(_) => Ok("Cocoon: sidecar unreachable".to_owned()),
            }
        }
        .instrument(tracing::info_span!("tui.cocoon.models"))
        .await
    }

    fn build_cocoon_client(&mut self) -> Result<zeph_llm::cocoon::CocoonClient, AgentError> {
        let cocoon_entry = self
            .runtime
            .providers
            .provider_pool
            .iter()
            .find(|p| p.provider_type == zeph_config::ProviderKind::Cocoon)
            .ok_or_else(|| {
                AgentError::ContextError(
                    "No Cocoon provider configured in [[llm.providers]]".to_owned(),
                )
            })?;

        let base_url = cocoon_entry
            .cocoon_client_url
            .as_deref()
            .unwrap_or("http://localhost:10000");

        let access_hash = self
            .runtime
            .providers
            .provider_config_snapshot
            .as_ref()
            .and_then(|s| s.cocoon_access_hash.as_deref().map(str::to_owned));

        Ok(zeph_llm::cocoon::CocoonClient::new(
            base_url,
            access_hash,
            std::time::Duration::from_secs(10),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(args: &str) -> Result<String, AgentError> {
        // Pure parser test — no agent or HTTP needed.
        match args.trim() {
            "" => Ok("Usage: /cocoon <subcommand>\n\n\
                 Subcommands:\n\
                 \x20 status  Show sidecar connection state, worker count, and TON balance\n\
                 \x20 models  List available models"
                .to_owned()),
            other => Err(AgentError::UnknownCommand(format!(
                "Unknown /cocoon subcommand: {other}. Available: status, models"
            ))),
        }
    }

    #[test]
    fn empty_args_returns_usage() {
        let out = dispatch("").unwrap();
        assert!(out.contains("Usage: /cocoon"), "got: {out}");
        assert!(out.contains("status"), "got: {out}");
        assert!(out.contains("models"), "got: {out}");
    }

    #[test]
    fn whitespace_args_returns_usage() {
        let out = dispatch("   ").unwrap();
        assert!(out.contains("Usage: /cocoon"), "got: {out}");
    }

    #[test]
    fn unknown_subcommand_returns_err() {
        let err = dispatch("bogus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "got: {msg}");
        assert!(msg.contains("status"), "got: {msg}");
        assert!(msg.contains("models"), "got: {msg}");
    }
}
