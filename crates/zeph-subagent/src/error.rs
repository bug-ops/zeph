// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// All errors that can arise during sub-agent lifecycle operations.
///
/// [`SubAgentError`] is the single error type for the entire `zeph-subagent` crate.
/// Every fallible public function returns `Result<_, SubAgentError>`.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::{SubAgentDef, SubAgentError};
///
/// let err = SubAgentDef::parse("missing frontmatter").unwrap_err();
/// assert!(matches!(err, SubAgentError::Parse { .. }));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum SubAgentError {
    /// Frontmatter parsing failed (malformed YAML/TOML or missing delimiters).
    #[error("parse error in {path}: {reason}")]
    Parse { path: String, reason: String },

    /// Definition semantics are invalid (e.g. empty name, conflicting tool policies).
    #[error("invalid definition: {0}")]
    Invalid(String),

    /// No definition or running agent with the requested name or ID was found.
    #[error("agent not found: {0}")]
    NotFound(String),

    /// The background task could not be spawned (OS or tokio error).
    #[error("spawn failed: {0}")]
    Spawn(String),

    /// The manager's concurrency limit is exhausted; no new agents can be spawned.
    #[error("concurrency limit reached (active: {active}, max: {max})")]
    ConcurrencyLimit { active: usize, max: usize },

    /// The agent loop was cancelled via its [`tokio_util::sync::CancellationToken`].
    #[error("cancelled")]
    Cancelled,

    /// A slash-command string (`/agent`, `/agents`) could not be parsed.
    #[error("invalid command: {0}")]
    InvalidCommand(String),

    /// An I/O operation on a transcript file failed.
    #[error("transcript error: {0}")]
    Transcript(String),

    /// An ID prefix matched more than one transcript; provide a longer prefix.
    #[error("ambiguous id prefix '{0}': matches {1} agents")]
    AmbiguousId(String, usize),

    /// Resume was requested for an agent that is still running.
    #[error("agent '{0}' is still running; cancel it first or wait for completion")]
    StillRunning(String),

    /// A memory directory could not be created or resolved.
    #[error("memory error for agent '{name}': {reason}")]
    Memory { name: String, reason: String },

    /// A filesystem I/O error unrelated to transcripts.
    #[error("I/O error at {path}: {reason}")]
    Io { path: String, reason: String },

    /// The underlying LLM provider returned an error during the agent loop.
    #[error("LLM call failed: {0}")]
    Llm(String),

    /// A channel send (status watch, secret approval) failed.
    #[error("channel send failed: {0}")]
    Channel(String),

    /// The tokio task panicked and the join handle propagated the panic.
    #[error("task panicked: {0}")]
    TaskPanic(String),

    /// The recursion depth for nested sub-agent spawning exceeded the configured limit.
    #[error("max spawn depth exceeded (depth: {depth}, max: {max})")]
    MaxDepthExceeded { depth: u32, max: u32 },
}
