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
#[non_exhaustive]
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

    /// A transcript's hash chain failed to verify (issue #6360): a definite tamper verdict, a
    /// partial strip of chain metadata, an unverifiable/possibly-re-keyed chain, or a chained
    /// file read with no history-integrity key configured. Distinct from [`SubAgentError::Transcript`]
    /// (JSON-syntax/I-O errors) because a chain break always escalates to a hard failure — even
    /// in `TranscriptReader::load`'s otherwise-lenient mode — since it invalidates trust in
    /// everything downstream of the break, unlike a single malformed line.
    #[error("{0}")]
    Integrity(String),

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

    /// Worktree creation or cwd setup failed during agent spawn.
    ///
    /// This error is returned when `permissions.worktree = true` and the worktree
    /// manager fails to create a dedicated worktree or cannot restore the working
    /// directory.  The agent loop never starts in this case (INV-4).
    #[error("worktree setup failed: {0}")]
    WorktreeSetup(String),

    /// The durable promise layer returned an error during subagent spawn or await.
    ///
    /// Wraps a [`zeph_durable::DurableError`] string so the crate does not take a hard
    /// compile-time dependency on `zeph-durable` in code paths where the feature is disabled
    /// at runtime (the `durable` module is always compiled in but the adapter functions are
    /// only called when `durable.enabled && durable.subagent`).
    #[error("durable error: {0}")]
    Durable(String),
}
