// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;

use super::{Agent, error::AgentError};
use crate::channel::Channel;

/// Format the `additional_directories` allowlist for display.
pub(super) fn format_acp_dirs(cfg: &zeph_config::AcpConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "ACP additional_directories allowlist:");
    if cfg.additional_directories.is_empty() {
        let _ = writeln!(out, "  (none configured)");
    } else {
        for dir in &cfg.additional_directories {
            let _ = writeln!(out, "  {dir}");
        }
    }
    out.trim_end().to_owned()
}

/// Format the `auth_methods` list for display.
pub(super) fn format_acp_auth_methods(cfg: &zeph_config::AcpConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "ACP auth_methods:");
    if cfg.auth_methods.is_empty() {
        let _ = writeln!(out, "  (none configured)");
    } else {
        for method in &cfg.auth_methods {
            let _ = writeln!(out, "  {method}");
        }
    }
    out.trim_end().to_owned()
}

/// Format the ACP server status summary.
pub(super) fn format_acp_status(cfg: &zeph_config::AcpConfig, is_acp_session: bool) -> String {
    let mut out = String::new();
    let enabled = if cfg.enabled { "enabled" } else { "disabled" };
    let _ = writeln!(out, "ACP: {enabled}");
    let _ = writeln!(out, "transport:       {:?}", cfg.transport);
    let _ = writeln!(out, "agent_name:      {}", cfg.agent_name);
    let _ = writeln!(out, "agent_version:   {}", cfg.agent_version);
    let _ = writeln!(out, "max_sessions:    {}", cfg.max_sessions);
    let _ = writeln!(out, "http_bind:       {}", cfg.http_bind);
    let _ = writeln!(out, "discovery:       {}", cfg.discovery_enabled);
    let _ = writeln!(out, "message_ids:     {}", cfg.message_ids_enabled);
    let _ = writeln!(
        out,
        "this session:    {}",
        if is_acp_session {
            "ACP client"
        } else {
            "non-ACP"
        }
    );
    out.trim_end().to_owned()
}

/// Pure dispatcher — separated from `Agent` for unit testing.
pub(super) fn dispatch_acp(
    cfg: &zeph_config::AcpConfig,
    is_acp_session: bool,
    args: &str,
) -> Result<String, AgentError> {
    match args.trim() {
        "dirs" => Ok(format_acp_dirs(cfg)),
        "auth-methods" => Ok(format_acp_auth_methods(cfg)),
        "status" => Ok(format_acp_status(cfg, is_acp_session)),
        "" => Ok(
            "Usage: /acp <subcommand>\n\nSubcommands:\n  dirs          List additional_directories allowlist\n  auth-methods  List advertised auth methods\n  status        Show ACP server configuration summary"
                .to_owned(),
        ),
        other => Err(AgentError::UnknownCommand(format!(
            "Unknown /acp subcommand: {other}. Valid subcommands: dirs, auth-methods, status"
        ))),
    }
}

impl<C: Channel> Agent<C> {
    /// Dispatch `/acp [dirs|auth-methods|status]` and return a display string.
    pub(super) fn handle_acp_as_string(&mut self, args: &str) -> Result<String, AgentError> {
        dispatch_acp(&self.runtime.acp_config, self.security.is_acp_session, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_default() -> zeph_config::AcpConfig {
        zeph_config::AcpConfig::default()
    }

    fn cfg_with_dirs(dirs: &[&str]) -> zeph_config::AcpConfig {
        let mut cfg = cfg_default();
        cfg.additional_directories = dirs
            .iter()
            .map(|p| {
                zeph_config::AdditionalDir::parse(
                    std::path::Path::new(p)
                        .canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(p)),
                )
                .unwrap_or_else(|_| panic!("failed to parse {p}"))
            })
            .collect();
        cfg
    }

    #[test]
    fn dirs_empty() {
        let out = format_acp_dirs(&cfg_default());
        assert!(out.contains("(none configured)"), "got: {out}");
    }

    #[test]
    fn dirs_populated() {
        let cfg = cfg_with_dirs(&["/tmp"]);
        let out = format_acp_dirs(&cfg);
        assert!(out.contains("/tmp"), "got: {out}");
        assert!(!out.contains("(none configured)"), "got: {out}");
    }

    #[test]
    fn auth_methods_default() {
        let out = format_acp_auth_methods(&cfg_default());
        assert!(out.contains("agent"), "got: {out}");
        assert!(!out.contains("Agent"), "got: {out}");
    }

    #[test]
    fn auth_methods_empty() {
        let mut cfg = cfg_default();
        cfg.auth_methods.clear();
        let out = format_acp_auth_methods(&cfg);
        assert!(out.contains("(none configured)"), "got: {out}");
    }

    #[test]
    fn status_disabled() {
        let out = format_acp_status(&cfg_default(), false);
        assert!(out.contains("ACP: disabled"), "got: {out}");
        assert!(out.contains("non-ACP"), "got: {out}");
    }

    #[test]
    fn status_enabled_acp_session() {
        let mut cfg = cfg_default();
        cfg.enabled = true;
        let out = format_acp_status(&cfg, true);
        assert!(out.contains("ACP: enabled"), "got: {out}");
        assert!(out.contains("ACP client"), "got: {out}");
    }

    #[test]
    fn empty_args_returns_help() {
        let out = dispatch_acp(&cfg_default(), false, "").unwrap();
        assert!(out.contains("Usage: /acp"), "got: {out}");
        assert!(out.contains("dirs"), "got: {out}");
        assert!(out.contains("auth-methods"), "got: {out}");
        assert!(out.contains("status"), "got: {out}");
    }

    #[test]
    fn unknown_subcommand_returns_err() {
        let err = dispatch_acp(&cfg_default(), false, "bogus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "got: {msg}");
        assert!(
            !msg.contains("\"bogus\""),
            "should not quote arg, got: {msg}"
        );
        assert!(
            msg.contains("dirs"),
            "should list valid subcommands, got: {msg}"
        );
    }

    #[test]
    fn whitespace_args_returns_help() {
        let out = dispatch_acp(&cfg_default(), false, "   ").unwrap();
        assert!(out.contains("Usage: /acp"), "got: {out}");
    }
}
