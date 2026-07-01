// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use anyhow::Context as _;

use crate::bootstrap::{load_config_or_default, resolve_config_path};
use crate::cli::{AcpCommand, AcpModelConfigCommand, AcpSubagentCommand};

/// Handle `zeph acp <subcommand>`.
///
/// # Errors
///
/// Returns an error if the sub-agent fails to spawn, the handshake fails, or the
/// prompt round-trip times out.
pub(crate) async fn handle_acp_command(
    cmd: AcpCommand,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
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
        AcpCommand::ModelConfig {
            command: AcpModelConfigCommand::Show,
        } => {
            let config_file = resolve_config_path(config_path);
            let config = load_config_or_default(&config_file);
            print!("{}", render_model_config_table(&config, &config_file));
            Ok(())
        }
    }
}

/// Render the `zeph acp model-config show` table: the three sampling-temperature presets with
/// `(default)` marked on whichever one matches `config.acp.model_config.default_temperature_preset`,
/// plus a pointer to the config file and key that controls it.
///
/// Extracted as a pure function (rather than inlined `println!` calls) so the #5379 regression
/// test can assert on the exact rendered text instead of only that the handler returns `Ok(())`.
fn render_model_config_table(config: &zeph_core::config::Config, config_path: &Path) -> String {
    use std::fmt::Write as _;

    let default_preset = config.acp.model_config.default_temperature_preset;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ACP model_config presets (session/set_config_option, config_id=\"temperature\"):"
    );
    for preset in [
        zeph_config::AcpTemperaturePreset::Precise,
        zeph_config::AcpTemperaturePreset::Balanced,
        zeph_config::AcpTemperaturePreset::Creative,
    ] {
        let marker = if preset == default_preset {
            "  (default)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  {:<10} temperature = {}{marker}",
            preset.as_str(),
            preset.temperature()
        );
    }
    let _ = writeln!(
        out,
        "Config: {} — [acp.model_config].default_temperature_preset",
        config_path.display()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{handle_acp_command, render_model_config_table};
    use crate::cli::{AcpCommand, AcpModelConfigCommand};

    /// `model-config show` must load the resolved config without error (#5379) — this
    /// exercises the `resolve_config_path` + `load_config_or_default` wiring end to end.
    /// Marker-placement correctness is asserted separately below against
    /// `render_model_config_table` directly, since capturing `stdout` from this handler in a
    /// test is fragile — the pure render function exists specifically to avoid that.
    #[tokio::test]
    async fn model_config_show_loads_config_without_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = zeph_core::config::Config::default();
        config.acp.model_config.default_temperature_preset =
            zeph_config::AcpTemperaturePreset::Creative;
        let toml = toml::to_string_pretty(&config).expect("serialize config");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml).expect("write config");

        let result = handle_acp_command(
            AcpCommand::ModelConfig {
                command: AcpModelConfigCommand::Show,
            },
            Some(&config_path),
        )
        .await;
        assert!(result.is_ok(), "model-config show failed: {result:?}");
    }

    /// The `(default)` marker must land specifically on the `creative` line when
    /// `default_temperature_preset = "creative"` is configured — a regression that drops the
    /// marker entirely, or misplaces it on another preset, must fail this test (#5379).
    #[test]
    fn render_marks_configured_creative_preset_as_default() {
        let mut config = zeph_core::config::Config::default();
        config.acp.model_config.default_temperature_preset =
            zeph_config::AcpTemperaturePreset::Creative;
        let table = render_model_config_table(&config, std::path::Path::new("config.toml"));

        let creative_line = table
            .lines()
            .find(|l| l.contains("creative"))
            .expect("creative line present");
        assert!(
            creative_line.contains("(default)"),
            "creative line must carry the marker: {creative_line:?}"
        );
        for other in ["precise", "balanced"] {
            let line = table.lines().find(|l| l.contains(other)).unwrap();
            assert!(
                !line.contains("(default)"),
                "{other} line must not carry the marker: {line:?}"
            );
        }
    }

    /// With no `[acp.model_config]` override, the built-in default (`balanced`) must carry the
    /// marker — not `precise`/`creative` (#5379).
    #[test]
    fn render_marks_balanced_as_default_when_unconfigured() {
        let config = zeph_core::config::Config::default();
        let table = render_model_config_table(&config, std::path::Path::new("config.toml"));

        let balanced_line = table
            .lines()
            .find(|l| l.contains("balanced"))
            .expect("balanced line present");
        assert!(
            balanced_line.contains("(default)"),
            "balanced line must carry the marker: {balanced_line:?}"
        );
        for other in ["precise", "creative"] {
            let line = table.lines().find(|l| l.contains(other)).unwrap();
            assert!(
                !line.contains("(default)"),
                "{other} line must not carry the marker: {line:?}"
            );
        }
    }
}
