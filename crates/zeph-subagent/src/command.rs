// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Typed parsers for `/agent` and `/agents` slash commands.
//!
//! [`AgentCommand`] handles runtime operations on running agents (spawn, cancel, etc.)
//! and `@agent_name` mention syntax.
//!
//! [`AgentsCommand`] handles definition CRUD operations (`/agents list`, `/agents create`, …).

use super::error::SubAgentError;

/// Typed representation of a parsed `/agents` command for definition CRUD operations.
///
/// Separate from [`AgentCommand`] (runtime operations like spawn/cancel) to avoid
/// namespace collision between running-agent management and definition management.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::AgentsCommand;
///
/// let cmd = AgentsCommand::parse("/agents list").unwrap();
/// assert_eq!(cmd, AgentsCommand::List);
///
/// let cmd = AgentsCommand::parse("/agents show reviewer").unwrap();
/// assert_eq!(cmd, AgentsCommand::Show { name: "reviewer".to_owned() });
/// ```
#[derive(Debug, PartialEq)]
pub enum AgentsCommand {
    /// List all discovered sub-agent definitions.
    List,
    /// Show full details of a definition.
    Show { name: String },
    /// Create a new definition.
    Create { name: String },
    /// Edit an existing definition.
    Edit { name: String },
    /// Delete a definition.
    Delete { name: String },
}

impl AgentsCommand {
    /// Parse from raw input text starting with `/agents`.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::InvalidCommand`] if parsing fails.
    pub fn parse(input: &str) -> Result<Self, SubAgentError> {
        let rest = input
            .strip_prefix("/agents")
            .ok_or_else(|| SubAgentError::InvalidCommand("input must start with /agents".into()))?
            .trim();

        if rest.is_empty() {
            return Err(SubAgentError::InvalidCommand(
                "usage: /agents <list|show|create|edit|delete> [args]".into(),
            ));
        }

        let (cmd, args) = rest.split_once(' ').unwrap_or((rest, ""));
        let cmd = cmd.trim();
        let args = args.trim();

        match cmd {
            "list" => Ok(Self::List),
            "show" => {
                if args.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "usage: /agents show <name>".into(),
                    ));
                }
                Ok(Self::Show {
                    name: args.to_owned(),
                })
            }
            "create" => {
                if args.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "usage: /agents create <name>".into(),
                    ));
                }
                Ok(Self::Create {
                    name: args.to_owned(),
                })
            }
            "edit" => {
                if args.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "usage: /agents edit <name>".into(),
                    ));
                }
                Ok(Self::Edit {
                    name: args.to_owned(),
                })
            }
            "delete" => {
                if args.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "usage: /agents delete <name>".into(),
                    ));
                }
                Ok(Self::Delete {
                    name: args.to_owned(),
                })
            }
            other => Err(SubAgentError::InvalidCommand(format!(
                "unknown subcommand '{other}'; try: list, show, create, edit, delete"
            ))),
        }
    }
}

/// Typed representation of a parsed `/agent` CLI command or `@agent` mention.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::AgentCommand;
///
/// let cmd = AgentCommand::parse("/agent spawn helper fix the bug", &[]).unwrap();
/// assert_eq!(cmd, AgentCommand::Spawn {
///     name: "helper".to_owned(),
///     prompt: "fix the bug".to_owned(),
/// });
///
/// // @mention syntax routes to known agents.
/// let known = vec!["reviewer".to_owned()];
/// let cmd = AgentCommand::parse("@reviewer check the PR", &known).unwrap();
/// assert_eq!(cmd, AgentCommand::Mention {
///     agent: "reviewer".to_owned(),
///     prompt: "check the PR".to_owned(),
/// });
/// ```
#[derive(Debug, PartialEq)]
pub enum AgentCommand {
    /// List all running sub-agent tasks.
    List,
    /// Spawn a foreground sub-agent and block until it completes.
    Spawn { name: String, prompt: String },
    /// Spawn a background sub-agent that runs independently.
    Background { name: String, prompt: String },
    /// Show a brief status summary of all running agents.
    Status,
    /// Cancel a running agent by task ID.
    Cancel { id: String },
    /// Approve a pending vault secret request for a running agent.
    Approve { id: String },
    /// Deny a pending vault secret request for a running agent.
    Deny { id: String },
    /// Foreground spawn triggered by `@agent_name <prompt>` mention syntax.
    Mention { agent: String, prompt: String },
    /// Resume a previously completed sub-agent session by ID prefix.
    Resume { id: String, prompt: String },
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
    ///
    /// Also handles `@agent_name prompt` mention syntax when `known_agents`
    /// contains a match. If `@` prefix is present but the agent is unknown,
    /// returns `Err` so the caller can fall back to file-reference handling.
    pub fn parse(input: &str, known_agents: &[String]) -> Result<Self, SubAgentError> {
        if input.starts_with('@') {
            return Self::parse_mention(input, known_agents);
        }

        let rest = input
            .strip_prefix("/agent")
            .ok_or_else(|| {
                SubAgentError::InvalidCommand("input must start with /agent or @".into())
            })?
            .trim();

        if rest.is_empty() {
            return Err(SubAgentError::InvalidCommand(
                "usage: /agent <list|spawn|bg|resume|status|cancel|approve|deny> [args]".into(),
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
            "resume" => {
                let (id, prompt) = args.split_once(' ').ok_or_else(|| {
                    SubAgentError::InvalidCommand("usage: /agent resume <id> <prompt>".into())
                })?;
                let id = id.trim().to_owned();
                let prompt = prompt.trim().to_owned();
                if id.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "agent id must not be empty".into(),
                    ));
                }
                // Require at least 4 characters to prevent accidental mass-match or session
                // enumeration via very short prefixes.
                if id.len() < 4 {
                    return Err(SubAgentError::InvalidCommand(
                        "agent id prefix must be at least 4 characters".into(),
                    ));
                }
                if prompt.is_empty() {
                    return Err(SubAgentError::InvalidCommand(
                        "prompt must not be empty".into(),
                    ));
                }
                Ok(Self::Resume { id, prompt })
            }
            other => Err(SubAgentError::InvalidCommand(format!(
                "unknown subcommand '{other}'; try: list, spawn, bg, resume, status, cancel, approve, deny"
            ))),
        }
    }

    /// Parse an `@agent_name <prompt>` mention from raw input.
    ///
    /// Returns `Ok(Mention { agent, prompt })` if `input` starts with `@` and the
    /// token after `@` matches one of `known_agents`. Returns
    /// [`SubAgentError::InvalidCommand`] if:
    /// - `input` does not start with `@`
    /// - the agent name token is empty (bare `@`)
    /// - the named agent is not in `known_agents` — caller should fall back to
    ///   other `@` handling such as file references
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::InvalidCommand`] on any parse failure.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_subagent::AgentCommand;
    ///
    /// let known = vec!["helper".to_owned()];
    /// let cmd = AgentCommand::parse_mention("@helper fix this", &known).unwrap();
    /// assert_eq!(cmd, AgentCommand::Mention {
    ///     agent: "helper".to_owned(),
    ///     prompt: "fix this".to_owned(),
    /// });
    ///
    /// // Unknown agents are rejected so callers can fall back to file-reference handling.
    /// assert!(AgentCommand::parse_mention("@unknown do work", &known).is_err());
    /// ```
    pub fn parse_mention(input: &str, known_agents: &[String]) -> Result<Self, SubAgentError> {
        let rest = input
            .strip_prefix('@')
            .ok_or_else(|| SubAgentError::InvalidCommand("input must start with @".into()))?;

        if rest.is_empty() || rest.starts_with(' ') {
            return Err(SubAgentError::InvalidCommand(
                "bare '@' is not a valid agent mention".into(),
            ));
        }

        let (agent_token, prompt) = rest.split_once(' ').unwrap_or((rest, ""));
        let agent = agent_token.trim().to_owned();

        if !known_agents.iter().any(|n| n == &agent) {
            return Err(SubAgentError::InvalidCommand(format!(
                "@{agent} is not a known sub-agent"
            )));
        }

        Ok(Self::Mention {
            agent,
            prompt: prompt.trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list() {
        assert_eq!(
            AgentCommand::parse("/agent list", &[]).unwrap(),
            AgentCommand::List
        );
    }

    #[test]
    fn parse_status() {
        assert_eq!(
            AgentCommand::parse("/agent status", &[]).unwrap(),
            AgentCommand::Status
        );
    }

    #[test]
    fn parse_spawn() {
        let cmd = AgentCommand::parse("/agent spawn helper do something useful", &[]).unwrap();
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
        let cmd = AgentCommand::parse("/agent bg reviewer check the code", &[]).unwrap();
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
        let cmd = AgentCommand::parse("/agent cancel abc123", &[]).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Cancel {
                id: "abc123".into()
            }
        );
    }

    #[test]
    fn parse_approve() {
        let cmd = AgentCommand::parse("/agent approve task-1", &[]).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Approve {
                id: "task-1".into()
            }
        );
    }

    #[test]
    fn parse_deny() {
        let cmd = AgentCommand::parse("/agent deny task-2", &[]).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Deny {
                id: "task-2".into()
            }
        );
    }

    #[test]
    fn parse_wrong_prefix_returns_error() {
        let err = AgentCommand::parse("/foo list", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_empty_after_prefix_returns_usage() {
        let err = AgentCommand::parse("/agent", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_whitespace_only_after_prefix_returns_usage() {
        let err = AgentCommand::parse("/agent   ", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_unknown_subcommand_returns_error() {
        let err = AgentCommand::parse("/agent frobnicate", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("frobnicate")));
    }

    #[test]
    fn parse_spawn_missing_prompt_returns_error() {
        let err = AgentCommand::parse("/agent spawn helper", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_spawn_missing_name_and_prompt_returns_error() {
        let err = AgentCommand::parse("/agent spawn", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_cancel_missing_id_returns_error() {
        let err = AgentCommand::parse("/agent cancel", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_approve_missing_id_returns_error() {
        let err = AgentCommand::parse("/agent approve", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_deny_missing_id_returns_error() {
        let err = AgentCommand::parse("/agent deny", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_extra_whitespace_trimmed() {
        // Extra spaces around subcommand and args should be handled gracefully.
        let cmd = AgentCommand::parse("/agent  cancel  deadbeef", &[]).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Cancel {
                id: "deadbeef".into()
            }
        );
    }

    #[test]
    fn parse_spawn_prompt_with_spaces_preserved() {
        let cmd = AgentCommand::parse(
            "/agent spawn bot review the PR and suggest improvements",
            &[],
        )
        .unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Spawn {
                name: "bot".into(),
                prompt: "review the PR and suggest improvements".into(),
            }
        );
    }

    // ── parse_mention() tests ─────────────────────────────────────────────────

    fn known() -> Vec<String> {
        vec!["reviewer".into(), "helper".into()]
    }

    #[test]
    fn mention_known_agent_with_prompt() {
        let cmd = AgentCommand::parse_mention("@reviewer review this PR", &known()).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Mention {
                agent: "reviewer".into(),
                prompt: "review this PR".into(),
            }
        );
    }

    #[test]
    fn mention_known_agent_without_prompt() {
        let cmd = AgentCommand::parse_mention("@helper", &known()).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Mention {
                agent: "helper".into(),
                prompt: String::new(),
            }
        );
    }

    #[test]
    fn mention_unknown_agent_returns_error() {
        let err = AgentCommand::parse_mention("@unknown-thing do work", &known()).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("unknown-thing")));
    }

    #[test]
    fn mention_bare_at_returns_error() {
        let err = AgentCommand::parse_mention("@", &known()).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn mention_at_with_space_returns_error() {
        let err = AgentCommand::parse_mention("@ something", &known()).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn mention_wrong_prefix_returns_error() {
        let err = AgentCommand::parse_mention("reviewer do work", &known()).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn mention_empty_known_agents_always_fails() {
        let err = AgentCommand::parse_mention("@reviewer do work", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    // ── parse() unified entry point with @ ──────────────────────────────────

    #[test]
    fn parse_dispatches_at_mention_to_parse_mention() {
        let cmd = AgentCommand::parse("@reviewer review this PR", &known()).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Mention {
                agent: "reviewer".into(),
                prompt: "review this PR".into(),
            }
        );
    }

    #[test]
    fn parse_at_unknown_agent_returns_error() {
        let err = AgentCommand::parse("@unknown test", &known()).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_at_with_empty_known_returns_error() {
        let err = AgentCommand::parse("@reviewer test", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    // ── parse resume ─────────────────────────────────────────────────────────

    #[test]
    fn parse_resume() {
        let cmd = AgentCommand::parse("/agent resume deadbeef continue the analysis", &[]).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Resume {
                id: "deadbeef".into(),
                prompt: "continue the analysis".into(),
            }
        );
    }

    #[test]
    fn parse_resume_missing_prompt_returns_error() {
        let err = AgentCommand::parse("/agent resume deadbeef", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_resume_missing_id_and_prompt_returns_error() {
        let err = AgentCommand::parse("/agent resume", &[]).unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    #[test]
    fn parse_resume_unknown_subcommand_hint() {
        let err = AgentCommand::parse("/agent frobnicate", &[]).unwrap_err();
        if let SubAgentError::InvalidCommand(msg) = err {
            assert!(
                msg.contains("resume"),
                "hint should mention 'resume': {msg}"
            );
        } else {
            panic!("expected InvalidCommand");
        }
    }

    #[test]
    fn parse_resume_prompt_with_spaces_preserved() {
        let cmd = AgentCommand::parse("/agent resume abc123 do more work and fix the issue", &[])
            .unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Resume {
                id: "abc123".into(),
                prompt: "do more work and fix the issue".into(),
            }
        );
    }

    #[test]
    fn parse_resume_id_too_short_returns_error() {
        // id "abc" has only 3 chars — below the 4-char minimum.
        let err = AgentCommand::parse("/agent resume abc continue", &[]).unwrap_err();
        assert!(
            matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("4 characters")),
            "expected min-length error, got: {err:?}"
        );
    }

    #[test]
    fn parse_resume_id_exactly_four_chars_is_accepted() {
        let cmd = AgentCommand::parse("/agent resume abcd continue the work", &[]).unwrap();
        assert_eq!(
            cmd,
            AgentCommand::Resume {
                id: "abcd".into(),
                prompt: "continue the work".into(),
            }
        );
    }

    #[test]
    fn parse_resume_whitespace_only_prompt_returns_error() {
        // After split_once, prompt is "   " which trims to "".
        let err = AgentCommand::parse("/agent resume deadbeef    ", &[]).unwrap_err();
        // Either split_once returns None (no space after id) or prompt trims to empty.
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }

    // ── AgentsCommand (definition CRUD) ────────────────────────────────────

    #[test]
    fn agents_parse_list() {
        assert_eq!(
            AgentsCommand::parse("/agents list").unwrap(),
            AgentsCommand::List
        );
    }

    #[test]
    fn agents_parse_show() {
        let cmd = AgentsCommand::parse("/agents show code-reviewer").unwrap();
        assert_eq!(
            cmd,
            AgentsCommand::Show {
                name: "code-reviewer".into()
            }
        );
    }

    #[test]
    fn agents_parse_create() {
        let cmd = AgentsCommand::parse("/agents create my-agent").unwrap();
        assert_eq!(
            cmd,
            AgentsCommand::Create {
                name: "my-agent".into()
            }
        );
    }

    #[test]
    fn agents_parse_edit() {
        let cmd = AgentsCommand::parse("/agents edit reviewer").unwrap();
        assert_eq!(
            cmd,
            AgentsCommand::Edit {
                name: "reviewer".into()
            }
        );
    }

    #[test]
    fn agents_parse_delete() {
        let cmd = AgentsCommand::parse("/agents delete reviewer").unwrap();
        assert_eq!(
            cmd,
            AgentsCommand::Delete {
                name: "reviewer".into()
            }
        );
    }

    #[test]
    fn agents_parse_missing_subcommand_returns_usage() {
        let err = AgentsCommand::parse("/agents").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn agents_parse_show_missing_name_returns_usage() {
        let err = AgentsCommand::parse("/agents show").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn agents_parse_unknown_subcommand_returns_error() {
        let err = AgentsCommand::parse("/agents frobnicate").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(ref m) if m.contains("frobnicate")));
    }

    #[test]
    fn agents_parse_wrong_prefix_returns_error() {
        let err = AgentsCommand::parse("/agent list").unwrap_err();
        assert!(matches!(err, SubAgentError::InvalidCommand(_)));
    }
}
