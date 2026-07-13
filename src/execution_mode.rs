// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-scoped operational flags derived from CLI args and `[cli]` config section.

use zeph_core::config::Config;

use crate::cli::Cli;

/// Session-scoped mode flags resolved at startup from CLI args and `[cli]` config.
///
/// CLI flags take priority: a flag absent on the command line (defaults to `false`)
/// falls back to the config value. A config value of `true` therefore activates
/// the mode even when the flag is not passed — useful for scripting environments.
#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // runtime state — boolean flags are idiomatic here
pub(crate) struct ExecutionMode {
    pub(crate) bare: bool,
    pub(crate) safe_mode: bool,
    pub(crate) json: bool,
    pub(crate) auto: bool,
}

impl ExecutionMode {
    /// Merge CLI flags with config defaults. CLI flags take priority.
    pub(crate) fn from_cli_and_config(cli: &Cli, cfg: &Config) -> Self {
        Self {
            bare: cli.bare || cfg.cli.bare,
            safe_mode: cli.safe_mode || cfg.cli.safe_mode,
            json: cli.json || cfg.cli.json,
            auto: cli.auto || cfg.cli.auto,
        }
    }
}

/// Resolve the `--safe-mode` CLI flag against the `ZEPH_SAFE_MODE` environment variable.
///
/// Used by session entry points (`daemon`, `acp`, `serve`) that call `AppBuilder::new`
/// directly, before config is loaded — so only the env var (not `config.cli.safe_mode`) is
/// available to OR against the flag at this point. `AppBuilder::new` itself ORs the resolved
/// value into `config.cli.safe_mode` (which the `Config::load` env overlay may have already
/// set), so all three sources — flag, env, and any config default — converge downstream
/// regardless of which combination triggered activation here.
pub(crate) fn resolve_safe_mode_flag(cli_flag: bool) -> bool {
    cli_flag
        || std::env::var("ZEPH_SAFE_MODE")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_cli() -> Cli {
        Cli::default()
    }

    #[test]
    fn all_false_by_default() {
        let mode = ExecutionMode::default();
        assert!(!mode.bare && !mode.safe_mode && !mode.json && !mode.auto);
    }

    #[test]
    fn cli_bare_flag_activates_bare() {
        let mut cli = default_cli();
        cli.bare = true;
        let mode = ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(mode.bare);
        assert!(!mode.safe_mode);
        assert!(!mode.json);
        assert!(!mode.auto);
    }

    #[test]
    fn cli_safe_mode_flag_activates_safe_mode() {
        let mut cli = default_cli();
        cli.safe_mode = true;
        let mode = ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(mode.safe_mode);
        assert!(!mode.bare);
    }

    #[test]
    fn config_safe_mode_activates_safe_mode() {
        let mut cfg = Config::default();
        cfg.cli.safe_mode = true;
        let mode = ExecutionMode::from_cli_and_config(&default_cli(), &cfg);
        assert!(mode.safe_mode);
        assert!(!mode.bare);
    }

    #[test]
    fn bare_and_safe_mode_compose_independently() {
        let mut cli = default_cli();
        cli.bare = true;
        cli.safe_mode = true;
        let mode = ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(mode.bare);
        assert!(mode.safe_mode);
    }

    #[test]
    fn resolve_safe_mode_flag_true_short_circuits_env_read() {
        // `true` must not depend on `ZEPH_SAFE_MODE` being unset — safe to run unguarded.
        assert!(resolve_safe_mode_flag(true));
    }

    #[test]
    #[serial_test::serial(zeph_safe_mode_env)]
    // std::env::set_var / remove_var are unsafe in Rust 2024 edition; guarded by #[serial]
    // above (mirrors src/bootstrap/tests.rs's identical precedent).
    #[allow(unsafe_code)]
    fn resolve_safe_mode_flag_reads_env_when_flag_false() {
        // SAFETY: guarded by `#[serial]` on this env-var-scoped lock name — no other test in
        // this process touches `ZEPH_SAFE_MODE` concurrently.
        unsafe {
            std::env::set_var("ZEPH_SAFE_MODE", "true");
        }
        assert!(resolve_safe_mode_flag(false));
        unsafe {
            std::env::remove_var("ZEPH_SAFE_MODE");
        }
        assert!(!resolve_safe_mode_flag(false));
    }

    #[test]
    fn cli_json_flag_activates_json() {
        let mut cli = default_cli();
        cli.json = true;
        let mode = ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(mode.json);
        assert!(!mode.bare);
    }

    #[test]
    fn cli_auto_flag_activates_auto() {
        let mut cli = default_cli();
        cli.auto = true;
        let mode = ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(mode.auto);
    }

    #[test]
    fn config_bare_activates_bare_mode() {
        let mut cfg = Config::default();
        cfg.cli.bare = true;
        let mode = ExecutionMode::from_cli_and_config(&default_cli(), &cfg);
        assert!(mode.bare);
        assert!(!mode.json);
    }

    #[test]
    fn config_json_activates_json_mode() {
        let mut cfg = Config::default();
        cfg.cli.json = true;
        let mode = ExecutionMode::from_cli_and_config(&default_cli(), &cfg);
        assert!(mode.json);
    }

    #[test]
    fn config_auto_activates_auto_mode() {
        let mut cfg = Config::default();
        cfg.cli.auto = true;
        let mode = ExecutionMode::from_cli_and_config(&default_cli(), &cfg);
        assert!(mode.auto);
    }

    #[test]
    fn cli_overrides_config_when_both_false_still_false() {
        let mode = ExecutionMode::from_cli_and_config(&default_cli(), &Config::default());
        assert!(!mode.bare && !mode.safe_mode && !mode.json && !mode.auto);
    }
}
