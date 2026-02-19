use std::collections::HashMap;

use crate::error::McpError;

const DEFAULT_ALLOWED_COMMANDS: &[&str] = &[
    "npx", "uvx", "node", "python3", "python", "docker", "deno", "bun",
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

/// Validate that command is a bare name on the allowlist.
///
/// Rejects commands containing path separators to prevent symlink-based bypasses.
///
/// # Errors
///
/// Returns `McpError::CommandNotAllowed` if the command is not on the allowlist.
pub fn validate_command(command: &str, extra_allowed: &[String]) -> Result<(), McpError> {
    if command.contains('/') || command.contains('\\') {
        return Err(McpError::CommandNotAllowed {
            command: command.into(),
        });
    }

    let allowed =
        DEFAULT_ALLOWED_COMMANDS.contains(&command) || extra_allowed.iter().any(|c| c == command);

    if !allowed {
        return Err(McpError::CommandNotAllowed {
            command: command.into(),
        });
    }

    Ok(())
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
}
