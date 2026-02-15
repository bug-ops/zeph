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
