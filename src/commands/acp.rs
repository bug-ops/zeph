// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use anyhow::Context as _;

use crate::cli::{AcpCommand, AcpSubagentCommand};

/// Handle `zeph acp <subcommand>`.
///
/// # Errors
///
/// Returns an error if the sub-agent fails to spawn, the handshake fails, or the
/// prompt round-trip times out.
pub(crate) async fn handle_acp_command(cmd: AcpCommand) -> anyhow::Result<()> {
    match cmd {
        AcpCommand::RunAgent {
            command,
            prompt,
            cwd,
            timeout,
        } => {
            let span = tracing::info_span!("acp.client.session.run");
            let _enter = span.enter();

            let text = if let Some(p) = prompt {
                p
            } else {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("reading prompt from stdin")?;
                buf
            };

            // `--timeout` sets `prompt_timeout_secs` directly.
            // `handshake_timeout_secs` is capped at 30 s so a large `--timeout` value (e.g. 300 s
            // for a long-running agent) does not extend the connection-setup window indefinitely.
            let cfg = zeph_acp::client::SubagentConfig {
                command,
                process_cwd: cwd.clone(),
                session_cwd: cwd,
                prompt_timeout_secs: timeout,
                handshake_timeout_secs: timeout.min(30),
                auto_approve_permissions: true,
                ..zeph_acp::client::SubagentConfig::default()
            };

            let outcome = zeph_acp::run_session(cfg, text).await?;
            println!("{}", outcome.text);
            tracing::info!(stop_reason = ?outcome.stop_reason, "sub-agent session completed");
            Ok(())
        }
        AcpCommand::Subagent {
            command: AcpSubagentCommand::List,
        } => {
            // Config is not loaded at this point; report that presets must be configured.
            println!("Sub-agent presets are configured under [acp.subagents] in config.toml.");
            println!("Use `zeph acp run-agent --command <CMD> --prompt <TEXT>` for one-shot runs.");
            Ok(())
        }
    }
}
