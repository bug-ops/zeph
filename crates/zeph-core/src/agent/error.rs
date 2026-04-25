// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
    OrchestrationError(String),

    /// An unknown slash command or subcommand was received.
    #[error("unknown command: {0}")]
    UnknownCommand(String),

    /// Skill file operation failed (invalid name or skill not found).
    #[error("skill error: {0}")]
    SkillOperation(String),

    /// Context assembly or index retrieval failed.
    #[error("context error: {0}")]
    ContextError(String),

    /// Catch-all for errors that do not yet have a specific typed variant.
    ///
    /// # Deprecation
    ///
    /// Prefer adding a typed variant over using `Other`. This variant exists for
    /// backward compatibility and will be removed once all callsites are migrated.
    #[error("{0}")]
    Other(String),
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
        let e = AgentError::Other("something went wrong".into());
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
        let e = AgentError::Other("something went wrong".into());
        assert!(!e.is_beta_header_rejected());
    }

    #[test]
    fn agent_error_detects_no_providers() {
        let e = AgentError::Llm(zeph_llm::LlmError::NoProviders);
        assert!(e.is_no_providers());
    }

    #[test]
    fn agent_error_non_no_providers_returns_false() {
        let e = AgentError::Other("other".into());
        assert!(!e.is_no_providers());
    }

    #[test]
    fn orchestration_error_display() {
        let e = AgentError::OrchestrationError("planner failed".into());
        assert!(e.to_string().contains("planner failed"));
    }

    #[test]
    fn unknown_command_display() {
        let e = AgentError::UnknownCommand("/foo".into());
        assert!(e.to_string().contains("/foo"));
    }

    #[test]
    fn skill_operation_display() {
        let e = AgentError::SkillOperation("skill not found".into());
        assert!(e.to_string().contains("skill not found"));
    }
}
