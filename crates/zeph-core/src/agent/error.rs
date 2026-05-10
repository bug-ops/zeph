// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Typed orchestration failure.
///
/// Wraps errors from DAG scheduling, planning, and config verification. Each variant
/// preserves the upstream error string because the upstream types (from `zeph-orchestration`)
/// are heterogeneous — they do not share a common `std::error::Error` implementation that
/// would allow `#[from]` chains without loss of information.
#[derive(Debug, thiserror::Error)]
pub enum OrchestrationFailure {
    /// DAG scheduler failed to initialize or resume.
    #[error("scheduler error: {0}")]
    SchedulerInit(String),

    /// Provider/task config verification failed.
    #[error("config verification error: {0}")]
    VerifyConfig(String),

    /// Planner failed to produce a valid task graph.
    #[error("planner error: {0}")]
    PlannerError(String),

    /// DAG reset for retry failed.
    #[error("retry reset error: {0}")]
    RetryReset(String),

    /// Catch-all for orchestration errors not yet mapped to a specific variant.
    #[error("{0}")]
    Generic(String),
}

/// Typed skill file operation failure.
///
/// Returned when skill name validation or skill directory lookup fails.
#[derive(Debug, thiserror::Error)]
pub enum SkillOperationFailure {
    /// Skill name contains path-traversal characters (`/`, `\`, `..`).
    #[error("invalid skill name: {0}")]
    InvalidName(String),

    /// No skill directory found for the given name in any configured path.
    #[error("skill directory not found: {0}")]
    DirectoryNotFound(String),

    /// Catch-all for skill operation errors not yet mapped to a specific variant.
    #[error("{0}")]
    Generic(String),
}

/// Top-level error type for the agent loop.
///
/// All fallible agent operations return `Result<T, AgentError>`. Variants are kept
/// typed where the upstream error has a known shape; string-bearing variants only
/// exist where the upstream is a heterogeneous `dyn Error` that cannot be boxed
/// without breaking existing bounds.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] zeph_llm::LlmError),

    #[error(transparent)]
    Channel(#[from] crate::channel::ChannelError),

    #[error(transparent)]
    Memory(#[from] zeph_memory::MemoryError),

    #[error(transparent)]
    Skill(#[from] zeph_skills::SkillError),

    #[error(transparent)]
    Tool(#[from] zeph_tools::executor::ToolError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A `tokio::task::spawn_blocking` call failed to complete (task panicked or was cancelled).
    #[error("blocking task failed: {0}")]
    SpawnBlocking(#[from] tokio::task::JoinError),

    /// Agent received a shutdown signal and exited the run loop cleanly.
    #[error("agent shut down")]
    Shutdown,

    /// The context window was exhausted and could not be compacted further.
    #[error("context exhausted: {0}")]
    ContextExhausted(String),

    /// A tool call exceeded its configured timeout.
    #[error("tool timed out: {tool_name}")]
    ToolTimeout { tool_name: zeph_common::ToolName },

    /// Structured output did not conform to the expected JSON schema.
    #[error("schema validation failed: {0}")]
    SchemaValidation(String),

    /// An orchestration or DAG planning operation failed.
    #[error("orchestration error: {0}")]
    OrchestrationError(#[from] OrchestrationFailure),

    /// An unknown slash command or subcommand was received.
    #[error("unknown command: {0}")]
    UnknownCommand(String),

    /// Skill file operation failed (invalid name or skill not found).
    #[error("skill error: {0}")]
    SkillOperation(#[from] SkillOperationFailure),

    /// Context assembly or index retrieval failed.
    #[error("context error: {0}")]
    ContextError(String),

    /// A database operation in the agent subsystem failed.
    #[error("database error: {0}")]
    Db(String),
}

impl AgentError {
    /// Returns true if this error originates from a context length exceeded condition.
    #[must_use]
    pub fn is_context_length_error(&self) -> bool {
        if let Self::Llm(e) = self {
            return e.is_context_length_error();
        }
        false
    }

    /// Returns true if this error indicates that a beta header was rejected by the API.
    #[must_use]
    pub fn is_beta_header_rejected(&self) -> bool {
        if let Self::Llm(e) = self {
            return e.is_beta_header_rejected();
        }
        false
    }

    /// Returns true if this error is `LlmError::NoProviders` (all configured backends unavailable).
    #[must_use]
    pub fn is_no_providers(&self) -> bool {
        matches!(self, Self::Llm(zeph_llm::LlmError::NoProviders))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_error_detects_context_length_from_llm() {
        let e = AgentError::Llm(zeph_llm::LlmError::ContextLengthExceeded);
        assert!(e.is_context_length_error());
    }

    #[test]
    fn agent_error_detects_context_length_from_typed_variant() {
        // Providers must return ContextLengthExceeded directly, not Other.
        let e = AgentError::Llm(zeph_llm::LlmError::ContextLengthExceeded);
        assert!(e.is_context_length_error());
    }

    #[test]
    fn agent_error_other_with_context_message_not_detected() {
        // The `Other` path no longer triggers context-length classification;
        // providers are responsible for returning ContextLengthExceeded directly.
        let e = AgentError::Llm(zeph_llm::LlmError::Other("context length exceeded".into()));
        assert!(!e.is_context_length_error());
    }

    #[test]
    fn agent_error_non_llm_variant_not_detected() {
        let e = AgentError::ContextError("something went wrong".into());
        assert!(!e.is_context_length_error());
    }

    #[test]
    fn shutdown_variant_display() {
        let e = AgentError::Shutdown;
        assert_eq!(e.to_string(), "agent shut down");
    }

    #[test]
    fn context_exhausted_variant_display() {
        let e = AgentError::ContextExhausted("no space left".into());
        assert!(e.to_string().contains("no space left"));
    }

    #[test]
    fn tool_timeout_variant_display() {
        let e = AgentError::ToolTimeout {
            tool_name: "bash".into(),
        };
        assert!(e.to_string().contains("bash"));
    }

    #[test]
    fn schema_validation_variant_display() {
        let e = AgentError::SchemaValidation("missing field".into());
        assert!(e.to_string().contains("missing field"));
    }

    #[test]
    fn agent_error_detects_beta_header_rejected() {
        let e = AgentError::Llm(zeph_llm::LlmError::BetaHeaderRejected {
            header: "compact-2026-01-12".into(),
        });
        assert!(e.is_beta_header_rejected());
    }

    #[test]
    fn agent_error_non_llm_variant_not_beta_rejected() {
        let e = AgentError::ContextError("something went wrong".into());
        assert!(!e.is_beta_header_rejected());
    }

    #[test]
    fn agent_error_detects_no_providers() {
        let e = AgentError::Llm(zeph_llm::LlmError::NoProviders);
        assert!(e.is_no_providers());
    }

    #[test]
    fn agent_error_non_no_providers_returns_false() {
        let e = AgentError::ContextError("other".into());
        assert!(!e.is_no_providers());
    }

    #[test]
    fn orchestration_error_display() {
        let e =
            AgentError::OrchestrationError(OrchestrationFailure::Generic("planner failed".into()));
        assert!(e.to_string().contains("planner failed"));
    }

    #[test]
    fn orchestration_failure_variants_display() {
        assert!(
            OrchestrationFailure::SchedulerInit("dag error".into())
                .to_string()
                .contains("dag error")
        );
        assert!(
            OrchestrationFailure::VerifyConfig("bad config".into())
                .to_string()
                .contains("bad config")
        );
        assert!(
            OrchestrationFailure::PlannerError("plan failed".into())
                .to_string()
                .contains("plan failed")
        );
        assert!(
            OrchestrationFailure::RetryReset("reset failed".into())
                .to_string()
                .contains("reset failed")
        );
    }

    #[test]
    fn unknown_command_display() {
        let e = AgentError::UnknownCommand("/foo".into());
        assert!(e.to_string().contains("/foo"));
    }

    #[test]
    fn skill_operation_display() {
        let e =
            AgentError::SkillOperation(SkillOperationFailure::DirectoryNotFound("my-skill".into()));
        assert!(e.to_string().contains("my-skill"));
    }

    #[test]
    fn skill_operation_failure_variants_display() {
        assert!(
            SkillOperationFailure::InvalidName("bad/name".into())
                .to_string()
                .contains("bad/name")
        );
        assert!(
            SkillOperationFailure::DirectoryNotFound("foo".into())
                .to_string()
                .contains("foo")
        );
    }
}
