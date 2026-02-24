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
    fn agent_error_detects_context_length_from_other_message() {
        let e = AgentError::Llm(zeph_llm::LlmError::Other("context length exceeded".into()));
        assert!(e.is_context_length_error());
    }

    #[test]
    fn agent_error_non_llm_variant_not_detected() {
        let e = AgentError::Other("something went wrong".into());
        assert!(!e.is_context_length_error());
    }
}
