// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use crate::bootstrap::VaultArgs;
use zeph_config::VaultBackend;
use zeph_core::config::Config;

/// Error returned by [`load_config_or_default`] when the config file exists but fails to parse.
///
/// Deliberately does *not* print anything itself (unlike the old inline `eprintln!` +
/// `std::process::exit`) — every caller already returns `anyhow::Result<...>`, so the standard
/// `?` operator propagates this up to `main()`, which prints it once via its default `Error:
/// {e:?}` handler and exits non-zero. This is flush-safe by construction: propagating an `Err`
/// unwinds the stack normally, so any `TracingGuards` local (e.g. in `run()`) still drops and
/// flushes before `main()` ever sees the error — no `std::process::exit` call sits between the
/// failure and `?`'s propagation up through `run()`, unlike the pre-#6709 code (#6709).
#[derive(Debug)]
pub struct ConfigLoadError {
    path: PathBuf,
    source: zeph_config::ConfigError,
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The source (`self.source`) is intentionally omitted here — it is exposed via
        // `Error::source()` below, which anyhow's chain rendering already walks and prints once;
        // including it in both places produced a doubled "failed to parse ...: TOML parse
        // error..." followed by a "Caused by:" section repeating the same detail.
        write!(f, "failed to parse config at {}", self.path.display())
    }
}

impl std::error::Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Load config from `path`, falling back to defaults with a notice when the file is absent.
///
/// When the file does **not exist**, prints a notice to stderr and returns
/// `Ok(Config::default())`. When the file exists but fails to parse, returns
/// [`ConfigLoadError`] — propagate it with `?` from any `anyhow::Result`-returning caller.
///
/// # Errors
///
/// Returns [`ConfigLoadError`] when the file exists but its contents fail to parse.
pub fn load_config_or_default(path: &Path) -> Result<Config, ConfigLoadError> {
    if !path.exists() {
        eprintln!(
            "Config file not found at {} — running with defaults. \
             Run 'zeph init' to create one.",
            path.display()
        );
        return Ok(Config::default());
    }
    zeph_config::Config::load(path).map_err(|source| ConfigLoadError {
        path: path.to_owned(),
        source,
    })
}

pub fn resolve_config_path(cli_override: Option<&Path>) -> PathBuf {
    let cwd_default = Path::new("config/default.toml");
    resolve_config_path_impl(
        cli_override,
        |name| std::env::var(name).ok(),
        cwd_default.exists(),
    )
}

fn resolve_config_path_impl(
    cli_override: Option<&Path>,
    get_env: impl Fn(&str) -> Option<String>,
    cwd_default_exists: bool,
) -> PathBuf {
    if let Some(path) = cli_override {
        tracing::debug!("config resolved via CLI flag: {}", path.display());
        return path.to_owned();
    }
    if let Some(val) = get_env("ZEPH_CONFIG") {
        let path = PathBuf::from(&val);
        tracing::debug!(
            "config resolved via ZEPH_CONFIG env var: {}",
            path.display()
        );
        return path;
    }
    if cwd_default_exists {
        tracing::debug!("config resolved via CWD default: config/default.toml");
        return PathBuf::from("config/default.toml");
    }
    let xdg = dirs::config_dir()
        .unwrap_or_else(|| {
            get_env("HOME")
                .map_or_else(|| PathBuf::from("~"), PathBuf::from)
                .join(".config")
        })
        .join("zeph")
        .join("config.toml");
    tracing::debug!("config resolved via XDG fallback: {}", xdg.display());
    xdg
}

/// Parse a vault backend string from a CLI flag or environment variable.
///
/// # Errors
///
/// Returns an error describing the invalid input when `s` is not one of the recognized
/// backend names. Unlike the pre-#5954 behavior, an unrecognized value is never silently
/// downgraded to [`VaultBackend::Env`] — that would defeat the purpose of an explicit
/// `--vault`/`ZEPH_VAULT_BACKEND` override by falling back to the weakest backend.
fn parse_backend_str(s: &str) -> Result<VaultBackend, String> {
    match s {
        "env" => Ok(VaultBackend::Env),
        "age" => Ok(VaultBackend::Age),
        "keyring" => Ok(VaultBackend::Keyring),
        other => Err(format!(
            "unknown vault backend '{other}': expected one of \"env\", \"age\", \"keyring\""
        )),
    }
}

/// Priority: CLI flag > `ZEPH_VAULT_*` env > config.vault.* > defaults
///
/// # Errors
///
/// Returns an error when `cli_backend` or the `ZEPH_VAULT_BACKEND` environment variable
/// is set to an unrecognized backend name (see [`parse_backend_str`]).
pub fn parse_vault_args(
    config: &Config,
    cli_backend: Option<&str>,
    cli_key_path: Option<&Path>,
    cli_vault_path: Option<&Path>,
) -> Result<VaultArgs, String> {
    let env_backend = std::env::var("ZEPH_VAULT_BACKEND").ok();
    let backend = match cli_backend.or(env_backend.as_deref()) {
        Some(s) => parse_backend_str(s)?,
        None => config.vault.backend,
    };

    let env_key = std::env::var("ZEPH_VAULT_KEY").ok();
    let default_dir = zeph_core::vault::default_vault_dir();
    let key_path = cli_key_path
        .map(|p| p.to_string_lossy().into_owned())
        .or(env_key)
        .or_else(|| {
            if backend == VaultBackend::Age {
                Some(
                    default_dir
                        .join("vault-key.txt")
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            }
        });

    let env_vault = std::env::var("ZEPH_VAULT_PATH").ok();
    let vault_path = cli_vault_path
        .map(|p| p.to_string_lossy().into_owned())
        .or(env_vault)
        .or_else(|| {
            if backend == VaultBackend::Age {
                Some(
                    default_dir
                        .join("secrets.age")
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            }
        });

    Ok(VaultArgs {
        backend,
        key_path,
        vault_path,
    })
}

/// Resolve the CLI-overridable vault key/vault-file paths as concrete [`PathBuf`]s, falling back
/// to `default_vault_dir()` when no `--vault-key`/`--vault-path` override is given (or
/// `parse_vault_args` fails, e.g. an unrecognized `--vault` backend, or the resolved backend is
/// not `age`).
///
/// Never fails the caller — mirrors the graceful-degrade posture every vault-key consumer in this
/// codebase already commits to (an absent or misconfigured vault disables the dependent feature
/// for this run rather than aborting startup).
///
/// Shared by every vault-key consumer that resolves paths outside of full `AppBuilder`
/// construction — the history-chain integrity bootstrap (`runner.rs`) and the `zeph durable`
/// key-material loaders (`commands/durable.rs`) — so `--vault-key`/`--vault-path` overrides are
/// honored consistently everywhere a key is loaded, matching the resolution `doctor.rs`/`vault.rs`
/// already use post-#6499 (#6547, #6548).
pub fn resolve_vault_paths(
    config: &Config,
    cli_backend: Option<&str>,
    cli_key_path: Option<&Path>,
    cli_vault_path: Option<&Path>,
) -> (PathBuf, PathBuf) {
    let default_dir = zeph_core::vault::default_vault_dir();
    let (resolved_key, resolved_vault) =
        parse_vault_args(config, cli_backend, cli_key_path, cli_vault_path)
            .map(|va| (va.key_path, va.vault_path))
            .unwrap_or_default();
    (
        resolved_key.map_or_else(|| default_dir.join("vault-key.txt"), PathBuf::from),
        resolved_vault.map_or_else(|| default_dir.join("secrets.age"), PathBuf::from),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn cli_override_takes_precedence() {
        let path = Path::new("/custom/config.toml");
        let result = resolve_config_path_impl(Some(path), no_env, false);
        assert_eq!(result, PathBuf::from("/custom/config.toml"));
    }

    #[test]
    fn env_var_used_when_no_cli() {
        let result = resolve_config_path_impl(
            None,
            |name| {
                if name == "ZEPH_CONFIG" {
                    Some("/env/config.toml".to_owned())
                } else {
                    None
                }
            },
            false,
        );
        assert_eq!(result, PathBuf::from("/env/config.toml"));
    }

    #[test]
    fn cwd_default_returned_when_exists() {
        let result = resolve_config_path_impl(None, no_env, true);
        assert_eq!(result, PathBuf::from("config/default.toml"));
    }

    #[test]
    fn xdg_fallback_path_constructed() {
        // dirs::config_dir() reads the real environment (HOME / XDG_CONFIG_HOME).
        // We only assert the path ends with the expected platform-independent suffix.
        let result = resolve_config_path_impl(None, no_env, false);
        assert!(
            result.ends_with("zeph/config.toml"),
            "unexpected path: {}",
            result.display()
        );
    }

    #[test]
    fn xdg_fallback_matches_wizard_default() {
        // The runtime loader and the init wizard must propose the same path so that
        // a config written by `zeph --init` is found automatically at startup.
        let runtime_path = resolve_config_path_impl(None, no_env, false);
        let wizard_path = crate::init::wizard_default_config_path();
        assert_eq!(
            runtime_path, wizard_path,
            "init wizard default path and runtime XDG fallback diverged"
        );
    }

    #[test]
    fn load_config_or_default_missing_file_returns_defaults() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();
        // Drop the file so the path no longer exists.
        drop(tmp);
        assert!(!path.exists(), "temp file must be gone before the test");
        let cfg = load_config_or_default(&path).expect("missing file falls back to defaults");
        // A default config has the expected default agent name.
        let default_cfg = zeph_core::config::Config::default();
        assert_eq!(cfg.agent.name, default_cfg.agent.name);
    }

    #[test]
    fn load_config_or_default_parse_failure_returns_err() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "this is not valid = = toml").unwrap();
        let err = load_config_or_default(tmp.path())
            .expect_err("malformed TOML must be reported, not silently defaulted");
        assert!(
            err.to_string().contains("failed to parse config at"),
            "unexpected error message: {err}"
        );
    }
}
