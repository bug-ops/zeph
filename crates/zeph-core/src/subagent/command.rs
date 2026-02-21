use super::error::SubAgentError;

/// Typed representation of a parsed `/agent` CLI command.
#[derive(Debug, PartialEq)]
pub enum AgentCommand {
    List,
    Spawn { name: String, prompt: String },
    Background { name: String, prompt: String },
    Status,
    Cancel { id: String },
    Approve { id: String },
    Deny { id: String },
}

impl AgentCommand {
    /// Parse from raw input text.
    ///
    /// The input must start with `/agent`. Everything after that prefix is
    /// interpreted as `<subcommand> [args]`.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::InvalidCommand`] if:
    /// - `input` does not start with `/agent`
    /// - the subcommand is missing (empty after prefix)
    /// - required arguments are missing
    /// - the subcommand is not recognised
    pub fn parse(input: &str) -> Result<Self, SubAgentError> {
        let rest = input
            .strip_prefix("/agent")
            .ok_or_else(|| SubAgentError::InvalidCommand("input must start with /agent".into()))?
            .trim();

        if rest.is_empty() {
            return Err(SubAgentError::InvalidCommand(
                "usage: /agent <list|spawn|bg|status|cancel|approve|deny> [args]".into(),
            ));
        }

        let (cmd, args) = rest.split_once(' ').unwrap_or((rest, ""));
        let cmd = cmd.trim();
        let args = args.trim();

        match cmd {
            "list" => Ok(Self::List),
            "status" => Ok(Self::Status),
            "spawn" | "bg" => {
                let (name, prompt) = args.split_once(' ').ok_or_else(|| {
                    SubAgentError::InvalidCommand(format!("usage: /agent {cmd} <name> <prompt>"))
                })?;
                let name = name.trim().to_owned();
                let prompt = prompt.trim().to_owned();
                if name.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "sub-agent name must not be empty".into(),
                    ));
                }
                if prompt.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "prompt must not be empty".into(),
                    ));
                }
                if cmd == "bg" {
                    Ok(Self::Background { name, prompt })
                } else {
                    Ok(Self::Spawn { name, prompt })
                }
            }
            "cancel" => {
                if args.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "usage: /agent cancel <id>".into(),
                    ));
                }
                Ok(Self::Cancel {
                    id: args.to_owned(),
                })
            }
            "approve" => {
                if args.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "usage: /agent approve <id>".into(),
                    ));
                }
                Ok(Self::Approve {
                    id: args.to_owned(),
                })
            }
            "deny" => {
                if args.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "usage: /agent deny <id>".into(),
                    ));
                }
                Ok(Self::Deny {
                    id: args.to_owned(),
                })
            }
            other => Err(SubAgentError::InvalidCommand(format!(
                "unknown subcommand '{other}'; try: list, spawn, bg, status, cancel, approve, deny"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list() {
        assert_eq!(
            AgentCommand::parse("/agent list").unwrap(),
            AgentCommand::List
        );
    }

    #[test]
    fn parse_status() {
        assert_eq!(
            AgentCommand::parse("/agent status").unwrap(),
            AgentCommand::Status
        );
    }

    #[test]
    fn parse_spawn() {
        let cmd = AgentCommand::parse("/agent spawn helper do something useful").unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Spawn {
                name: "helper".into(),
                prompt: "do something useful".into(),
            }
        );
    }

    #[test]
    fn parse_bg() {
        let cmd = AgentCommand::parse("/agent bg reviewer check the code").unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Background {
                name: "reviewer".into(),
                prompt: "check the code".into(),
            }
        );
    }

    #[test]
    fn parse_cancel() {
        let cmd = AgentCommand::parse("/agent cancel abc123").unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Cancel {
                id: "abc123".into()
            }
        );
    }

    #[test]
    fn parse_approve() {
        let cmd = AgentCommand::parse("/agent approve task-1").unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Approve {
                id: "task-1".into()
            }
        );
    }

    #[test]
    fn parse_deny() {
        let cmd = AgentCommand::parse("/agent deny task-2").unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Deny {
                id: "task-2".into()
            }
        );
    }

    #[test]
    fn parse_wrong_prefix_returns_error() {
        let err = AgentCommand::parse("/foo list").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_empty_after_prefix_returns_usage() {
        let err = AgentCommand::parse("/agent").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_whitespace_only_after_prefix_returns_usage() {
        let err = AgentCommand::parse("/agent   ").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_unknown_subcommand_returns_error() {
        let err = AgentCommand::parse("/agent frobnicate").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("frobnicate")));
    }

    #[test]
    fn parse_spawn_missing_prompt_returns_error() {
        let err = AgentCommand::parse("/agent spawn helper").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_spawn_missing_name_and_prompt_returns_error() {
        let err = AgentCommand::parse("/agent spawn").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_cancel_missing_id_returns_error() {
        let err = AgentCommand::parse("/agent cancel").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_approve_missing_id_returns_error() {
        let err = AgentCommand::parse("/agent approve").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_deny_missing_id_returns_error() {
        let err = AgentCommand::parse("/agent deny").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_extra_whitespace_trimmed() {
        // Extra spaces around subcommand and args should be handled gracefully.
        let cmd = AgentCommand::parse("/agent  cancel  deadbeef").unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Cancel {
                id: "deadbeef".into()
            }
        );
    }

    #[test]
    fn parse_spawn_prompt_with_spaces_preserved() {
        let cmd =
            AgentCommand::parse("/agent spawn bot review the PR and suggest improvements").unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Spawn {
                name: "bot".into(),
                prompt: "review the PR and suggest improvements".into(),
            }
        );
    }
}
