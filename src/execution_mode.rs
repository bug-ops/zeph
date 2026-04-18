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
pub(crate) struct ExecutionMode {
    pub(crate) bare: bool,
    pub(crate) json: bool,
    pub(crate) auto: bool,
}

impl ExecutionMode {
    /// Merge CLI flags with config defaults. CLI flags take priority.
    pub(crate) fn from_cli_and_config(cli: &Cli, cfg: &Config) -> Self {
        Self {
            bare: cli.bare || cfg.cli.bare,
            json: cli.json || cfg.cli.json,
            auto: cli.auto || cfg.cli.auto,
        }
    }
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
        assert!(!mode.bare && !mode.json && !mode.auto);
    }

    #[test]
    fn cli_bare_flag_activates_bare() {
        let mut cli = default_cli();
        cli.bare = true;
        let mode = ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(mode.bare);
        assert!(!mode.json);
        assert!(!mode.auto);
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
        assert!(!mode.bare && !mode.json && !mode.auto);
    }
}
