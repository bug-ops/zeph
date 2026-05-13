// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// All errors that can originate from the `zeph-skills` crate.
///
/// Use `thiserror` source chaining to preserve the original cause where available.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::SkillError;
///
/// fn check(name: &str) -> Result<(), SkillError> {
///     if name.is_empty() {
///         return Err(SkillError::Invalid("skill name must not be empty".into()));
///     }
///     Ok(())
/// }
///
/// assert!(check("").is_err());
/// assert!(check("my-skill").is_ok());
/// ```
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// Filesystem or IO failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Qdrant client error (boxed to keep the variant size small).
    #[cfg(feature = "qdrant")]
    #[error("Qdrant error: {0}")]
    Qdrant(#[from] Box<qdrant_client::QdrantError>),

    /// JSON serialization or deserialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Integer type-conversion overflow, e.g. when converting `usize` to `u64`.
    #[error("integer conversion: {0}")]
    IntConversion(#[from] std::num::TryFromIntError),

    /// `notify`/debouncer watcher initialization or watch failure.
    #[error("watcher error: {0}")]
    Watcher(#[from] notify::Error),

    /// Skill frontmatter or content failed validation (name format, description length, etc.).
    #[error("invalid skill: {0}")]
    Invalid(String),

    /// A skill with the given name was not found in the registry.
    #[error("skill not found: {0}")]
    NotFound(String),

    /// A skill with the given name already exists at the target location.
    #[error("skill already exists: {0}")]
    AlreadyExists(String),

    /// `git clone` subprocess exited non-zero or could not be spawned.
    #[error("git clone failed: {0}")]
    GitCloneFailed(String),

    /// Filesystem copy of skill files failed.
    #[error("copy failed: {0}")]
    CopyFailed(String),

    /// An LLM call exceeded its configured timeout.
    #[error("skill generation timed out after {0}ms")]
    Timeout(u64),

    /// Catch-all for errors that do not fit the above categories.
    #[error("{0}")]
    Other(String),
}
