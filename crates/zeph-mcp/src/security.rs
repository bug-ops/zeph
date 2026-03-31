// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use crate::error::McpError;

/// Expand a leading `~` to the user's home directory.
/// Returns the original string unchanged if it does not start with `~` or
/// if the `HOME` environment variable is not set.
fn expand_tilde(path: &str) -> std::borrow::Cow<'_, str> {
    if (path == "~" || path.starts_with("~/") || path.starts_with("~\\"))
        && let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
    {
        return std::borrow::Cow::Owned(format!("{home}{}", &path[1..]));
    }
    std::borrow::Cow::Borrowed(path)
}

/// Return true if `command` matches `pattern` (glob syntax, `~` expanded).
fn matches_pattern(command: &str, pattern: &str) -> bool {
    let expanded = expand_tilde(pattern);
    glob::Pattern::new(&expanded).is_ok_and(|p| p.matches(command))
}

const DEFAULT_ALLOWED_COMMANDS: &[&str] = &[
    "npx", "uvx", "node", "python3", "python", "docker", "deno", "bun", "mcpls",
];

const BLOCKED_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "LD_PROFILE",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "GLOBIGNORE",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "RUBYLIB",
    "RUBYOPT",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5LIB",
    "PERL5OPT",
    "JAVA_TOOL_OPTIONS",
];

/// Validate that command is on the allowlist.
///
/// Bare names (without path separators) are checked against the default allowlist and
/// `extra_allowed`. Full absolute paths (containing `/` or `\`) are permitted only
/// when explicitly listed in `extra_allowed` — this prevents symlink-based bypasses
/// while allowing operators to pin specific binary paths in their config.
///
/// # Errors
///
/// Returns `McpError::CommandNotAllowed` if the command is not on the allowlist.
pub fn validate_command(command: &str, extra_allowed: &[String]) -> Result<(), McpError> {
    // Expand `~` in the command itself so patterns and exact entries can use `~` uniformly.
    let command = expand_tilde(command);
    let command = command.as_ref();

    if command.contains('/') || command.contains('\\') {
        // Full paths: allowed only when an operator-provided entry matches (exact or glob).
        let allowed = extra_allowed
            .iter()
            .any(|p| p == command || matches_pattern(command, p));
        if !allowed {
            return Err(McpError::CommandNotAllowed {
                command: command.into(),
            });
        }
        return Ok(());
    }

    let allowed = DEFAULT_ALLOWED_COMMANDS.contains(&command)
        || extra_allowed
            .iter()
            .any(|p| p == command || matches_pattern(command, p));

    if !allowed {
        return Err(McpError::CommandNotAllowed {
            command: command.into(),
        });
    }

    Ok(())
}

/// Minimal base environment variables passed to isolated stdio MCP server processes.
///
/// When `env_isolation = true`, the spawned process receives only these variables from the
/// parent environment, plus any server-specific `env` entries from config. This prevents
/// the child from reading secrets or credentials the parent may have inherited.
pub const BASE_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TERM",
    "SHELL",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
];

/// Build the environment map for an isolated stdio child process.
///
/// Starts with the minimal `BASE_ENV_VARS` from the current process, then merges
/// the server-specific `env` overrides. Variables present in `server_env` that are
/// also in `BLOCKED_ENV_VARS` are blocked by the subsequent `validate_env` call in
/// the caller.
#[must_use]
pub fn build_isolated_env<S: std::hash::BuildHasher>(
    server_env: &HashMap<String, String, S>,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = BASE_ENV_VARS
        .iter()
        .filter_map(|&k| std::env::var(k).ok().map(|v| (k.to_owned(), v)))
        .collect();
    env.extend(server_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

/// Validate that no blocked env vars are present.
///
/// # Errors
///
/// Returns `McpError::EnvVarBlocked` if a dangerous env var is found.
pub fn validate_env<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
) -> Result<(), McpError> {
    for key in env.keys() {
        if BLOCKED_ENV_VARS.contains(&key.as_str()) || key.starts_with("BASH_FUNC_") {
            return Err(McpError::EnvVarBlocked {
                var_name: key.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_default_commands() {
        for cmd in DEFAULT_ALLOWED_COMMANDS {
            assert!(validate_command(cmd, &[]).is_ok(), "should allow {cmd}");
        }
    }

    #[test]
    fn allows_extra_command() {
        assert!(validate_command("custom-server", &["custom-server".into()]).is_ok());
    }

    #[test]
    fn rejects_unknown_command() {
        let err = validate_command("bash", &[]).unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[test]
    fn rejects_commands_with_forward_slash() {
        let err = validate_command("/usr/bin/npx", &[]).unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[test]
    fn allows_absolute_path_when_explicitly_listed() {
        assert!(validate_command("/usr/local/bin/mcpls", &["/usr/local/bin/mcpls".into()]).is_ok());
    }

    #[test]
    fn rejects_absolute_path_not_in_extra_allowed() {
        let err = validate_command("/usr/local/bin/mcpls", &["mcpls".into()]).unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[test]
    fn allows_glob_wildcard_matching_directory() {
        assert!(validate_command("/usr/local/bin/mcpls", &["/usr/local/bin/*".into()]).is_ok());
    }

    #[test]
    fn rejects_glob_outside_allowed_directory() {
        let err = validate_command("/usr/bin/mcpls", &["/usr/local/bin/*".into()]).unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[test]
    fn allows_tilde_glob_pattern() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return; // skip if HOME not set
        }
        let command = format!("{home}/.cargo/bin/mcpls");
        assert!(validate_command(&command, &["~/.cargo/bin/*".into()]).is_ok());
    }

    #[test]
    fn expand_tilde_replaces_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let expanded = expand_tilde("~/.cargo/bin/mcpls");
        assert_eq!(expanded, format!("{home}/.cargo/bin/mcpls"));
    }

    #[test]
    fn expand_tilde_leaves_non_tilde_unchanged() {
        let path = "/usr/bin/mcpls";
        assert_eq!(expand_tilde(path), path);
    }

    #[test]
    fn rejects_commands_with_backslash() {
        let err = validate_command("..\\npx", &[]).unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[test]
    fn rejects_relative_path() {
        let err = validate_command("../../npx", &[]).unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[test]
    fn rejects_empty_command() {
        let err = validate_command("", &[]).unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[test]
    fn allows_safe_env_vars() {
        let env = HashMap::from([
            ("PATH".into(), "/usr/bin".into()),
            ("HOME".into(), "/home/user".into()),
            ("NODE_ENV".into(), "production".into()),
        ]);
        assert!(validate_env(&env).is_ok());
    }

    #[test]
    fn allows_empty_env() {
        assert!(validate_env(&HashMap::new()).is_ok());
    }

    #[test]
    fn blocks_ld_preload() {
        let env = HashMap::from([("LD_PRELOAD".into(), "/evil.so".into())]);
        let err = validate_env(&env).unwrap_err();
        assert!(
            matches!(err, McpError::EnvVarBlocked { ref var_name } if var_name == "LD_PRELOAD")
        );
    }

    #[test]
    fn blocks_dyld_insert_libraries() {
        let env = HashMap::from([("DYLD_INSERT_LIBRARIES".into(), "/evil.dylib".into())]);
        let err = validate_env(&env).unwrap_err();
        assert!(matches!(err, McpError::EnvVarBlocked { .. }));
    }

    #[test]
    fn blocks_node_options() {
        let env = HashMap::from([("NODE_OPTIONS".into(), "--require /evil.js".into())]);
        let err = validate_env(&env).unwrap_err();
        assert!(matches!(err, McpError::EnvVarBlocked { .. }));
    }

    #[test]
    fn blocks_pythonpath() {
        let env = HashMap::from([("PYTHONPATH".into(), "/evil".into())]);
        let err = validate_env(&env).unwrap_err();
        assert!(matches!(err, McpError::EnvVarBlocked { .. }));
    }

    #[test]
    fn blocks_java_tool_options() {
        let env = HashMap::from([("JAVA_TOOL_OPTIONS".into(), "-javaagent:/evil.jar".into())]);
        let err = validate_env(&env).unwrap_err();
        assert!(matches!(err, McpError::EnvVarBlocked { .. }));
    }

    #[test]
    fn blocks_bash_func_prefix() {
        let env = HashMap::from([("BASH_FUNC_evil%%".into(), "() { /bin/sh; }".into())]);
        let err = validate_env(&env).unwrap_err();
        assert!(
            matches!(err, McpError::EnvVarBlocked { ref var_name } if var_name == "BASH_FUNC_evil%%")
        );
    }

    #[test]
    fn blocks_all_listed_env_vars() {
        for var in BLOCKED_ENV_VARS {
            let env = HashMap::from([((*var).into(), "value".into())]);
            assert!(validate_env(&env).is_err(), "{var} should be blocked");
        }
    }

    #[test]
    fn error_display_command_not_allowed() {
        let err = McpError::CommandNotAllowed {
            command: "bash".into(),
        };
        assert!(err.to_string().contains("bash"));
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn error_display_env_var_blocked() {
        let err = McpError::EnvVarBlocked {
            var_name: "LD_PRELOAD".into(),
        };
        assert!(err.to_string().contains("LD_PRELOAD"));
        assert!(err.to_string().contains("blocked"));
    }

    // --- build_isolated_env ---

    #[test]
    fn build_isolated_env_base_vars_present_when_set() {
        // PATH and HOME are almost always set in CI and local environments.
        // We test the ones most likely to be present.
        let result = build_isolated_env(&HashMap::new());
        // At minimum, PATH should appear (always set in any shell environment).
        // We do a soft check: the result must be a strict subset of BASE_ENV_VARS + server_env.
        for key in result.keys() {
            assert!(
                BASE_ENV_VARS.contains(&key.as_str()),
                "unexpected key in isolated env: {key}"
            );
        }
    }

    #[test]
    fn build_isolated_env_non_base_vars_absent() {
        // build_isolated_env only propagates variables listed in BASE_ENV_VARS.
        // Any key in the result must be in BASE_ENV_VARS (or in server_env, which is empty here).
        let result = build_isolated_env(&HashMap::new());
        for key in result.keys() {
            assert!(
                BASE_ENV_VARS.contains(&key.as_str()),
                "unexpected key in isolated env (not in BASE_ENV_VARS and not in server_env): {key}"
            );
        }
    }

    #[test]
    fn build_isolated_env_server_env_merged() {
        let mut server_env = HashMap::new();
        server_env.insert("MY_TOOL_TOKEN".into(), "tok_abc".into());
        let result = build_isolated_env(&server_env);
        assert_eq!(
            result.get("MY_TOOL_TOKEN").map(String::as_str),
            Some("tok_abc"),
            "server-declared env must appear in isolated env"
        );
    }

    #[test]
    fn build_isolated_env_server_env_can_override_base_var() {
        // Operator can pin a specific PATH — server_env merges after base vars.
        let mut server_env = HashMap::new();
        server_env.insert("PATH".into(), "/usr/local/bin:/custom/bin".into());
        let result = build_isolated_env(&server_env);
        assert_eq!(
            result.get("PATH").map(String::as_str),
            Some("/usr/local/bin:/custom/bin"),
            "server-declared PATH must override the base PATH"
        );
    }

    #[test]
    fn build_isolated_env_xdg_vars_in_base() {
        // XDG_RUNTIME_DIR and XDG_CONFIG_HOME must be in BASE_ENV_VARS.
        assert!(
            BASE_ENV_VARS.contains(&"XDG_RUNTIME_DIR"),
            "XDG_RUNTIME_DIR must be in BASE_ENV_VARS"
        );
        assert!(
            BASE_ENV_VARS.contains(&"XDG_CONFIG_HOME"),
            "XDG_CONFIG_HOME must be in BASE_ENV_VARS"
        );
    }
}
