// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Composable sanitization pipeline types.
//!
//! Provides [`Stage`] and [`Pipeline`] for building ordered synchronous processing chains
//! over a [`SanitizeContext`] accumulator. Async layers (guardrail, quarantine) remain
//! separate and are not modeled here.
//!
//! # Examples
//!
//! ```rust
//! use zeph_sanitizer::pipeline::{Pipeline, SanitizeContext, Stage, StageError};
//!
//! struct TrimStage;
//!
//! impl Stage for TrimStage {
//!     fn name(&self) -> &'static str { "trim" }
//!     fn process(&self, mut ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
//!         ctx.content = ctx.content.trim().to_owned();
//!         Ok(ctx)
//!     }
//! }
//!
//! let mut pipeline = Pipeline::new();
//! pipeline.add_stage(TrimStage);
//! let ctx = SanitizeContext::new("  hello  ".to_owned());
//! let result = pipeline.process(ctx).unwrap();
//! assert_eq!(result.content, "hello");
//! ```

use thiserror::Error;

/// Error type returned by a [`Stage`] when processing fails.
///
/// Carries the name of the failing stage and the underlying cause.
#[derive(Debug, Error)]
#[error("stage '{stage}' failed: {source}")]
pub struct StageError {
    /// Name of the stage that produced this error.
    pub stage: &'static str,
    /// Underlying cause.
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl StageError {
    /// Construct a [`StageError`] from a stage name and an arbitrary error.
    pub fn new(
        stage: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            stage,
            source: Box::new(source),
        }
    }
}

/// Accumulator passed through each [`Stage`] in a [`Pipeline`].
///
/// Holds the mutable content string and a flag indicating whether the content
/// was modified in ways relevant to downstream stages (e.g. truncation).
///
/// # Design note
///
/// Only synchronous, regex-based stages (layers 1–2 in the sanitization architecture)
/// operate on this struct. Async stages (guardrail LLM calls, quarantine summarizer)
/// remain outside the pipeline and are invoked directly by `ContentSanitizer`.
#[derive(Debug, Clone)]
pub struct SanitizeContext {
    /// The content string being processed. Each stage may modify this in-place.
    pub content: String,
    /// Set to `true` by any stage that truncates the content.
    pub was_truncated: bool,
}

impl SanitizeContext {
    /// Create a new context wrapping the given content string.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::pipeline::SanitizeContext;
    ///
    /// let ctx = SanitizeContext::new("raw tool output".to_owned());
    /// assert!(!ctx.was_truncated);
    /// ```
    #[must_use]
    pub fn new(content: String) -> Self {
        Self {
            content,
            was_truncated: false,
        }
    }
}

/// A single synchronous processing stage in the sanitization pipeline.
///
/// Implementors receive a [`SanitizeContext`] by value, mutate or replace it,
/// and return it. Returning an error aborts the pipeline.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::pipeline::{SanitizeContext, Stage, StageError};
///
/// struct Noop;
/// impl Stage for Noop {
///     fn name(&self) -> &'static str { "noop" }
///     fn process(&self, ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
///         Ok(ctx)
///     }
/// }
/// ```
pub trait Stage: Send + Sync {
    /// Human-readable name used in logs and [`StageError`] messages.
    fn name(&self) -> &'static str;

    /// Process the context, returning the (possibly modified) context or an error.
    ///
    /// # Errors
    ///
    /// Returns [`StageError`] if this stage cannot process the input.
    fn process(&self, ctx: SanitizeContext) -> Result<SanitizeContext, StageError>;
}

/// An ordered pipeline of synchronous [`Stage`]s.
///
/// Stages are executed in insertion order. The first stage error aborts the
/// pipeline and is returned to the caller.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::pipeline::{Pipeline, SanitizeContext, Stage, StageError};
///
/// struct UpperStage;
/// impl Stage for UpperStage {
///     fn name(&self) -> &'static str { "upper" }
///     fn process(&self, mut ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
///         ctx.content = ctx.content.to_uppercase();
///         Ok(ctx)
///     }
/// }
///
/// let mut pipeline = Pipeline::new();
/// pipeline.add_stage(UpperStage);
/// let result = pipeline.process(SanitizeContext::new("hello".to_owned())).unwrap();
/// assert_eq!(result.content, "HELLO");
/// ```
pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    /// Create an empty pipeline.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::pipeline::Pipeline;
    ///
    /// let pipeline = Pipeline::new();
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self { stages: Vec::new() }
    }

    /// Append a stage to the end of the pipeline.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::pipeline::{Pipeline, SanitizeContext, Stage, StageError};
    ///
    /// struct Noop;
    /// impl Stage for Noop {
    ///     fn name(&self) -> &'static str { "noop" }
    ///     fn process(&self, ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
    ///         Ok(ctx)
    ///     }
    /// }
    ///
    /// let mut pipeline = Pipeline::new();
    /// pipeline.add_stage(Noop);
    /// ```
    pub fn add_stage(&mut self, stage: impl Stage + 'static) {
        self.stages.push(Box::new(stage));
    }

    /// Run the context through all stages in order.
    ///
    /// Returns the final context after all stages succeed, or the first
    /// [`StageError`] encountered.
    ///
    /// # Errors
    ///
    /// Returns the error from the first failing stage, aborting subsequent stages.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::pipeline::{Pipeline, SanitizeContext};
    ///
    /// let pipeline = Pipeline::new();
    /// let ctx = SanitizeContext::new("content".to_owned());
    /// let out = pipeline.process(ctx).unwrap();
    /// assert_eq!(out.content, "content");
    /// ```
    pub fn process(&self, mut ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
        for stage in &self.stages {
            ctx = stage.process(ctx)?;
        }
        Ok(ctx)
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AppendStage(&'static str);

    impl Stage for AppendStage {
        fn name(&self) -> &'static str {
            "append"
        }

        fn process(&self, mut ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
            ctx.content.push_str(self.0);
            Ok(ctx)
        }
    }

    struct FailStage;

    impl Stage for FailStage {
        fn name(&self) -> &'static str {
            "fail"
        }

        fn process(&self, _ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
            Err(StageError::new(
                "fail",
                std::io::Error::other("intentional"),
            ))
        }
    }

    #[test]
    fn empty_pipeline_passes_through() {
        let pipeline = Pipeline::new();
        let ctx = SanitizeContext::new("hello".to_owned());
        let out = pipeline.process(ctx).unwrap();
        assert_eq!(out.content, "hello");
        assert!(!out.was_truncated);
    }

    #[test]
    fn stages_run_in_order() {
        let mut pipeline = Pipeline::new();
        pipeline.add_stage(AppendStage(" world"));
        pipeline.add_stage(AppendStage("!"));
        let out = pipeline
            .process(SanitizeContext::new("hello".to_owned()))
            .unwrap();
        assert_eq!(out.content, "hello world!");
    }

    #[test]
    fn error_aborts_pipeline() {
        let mut pipeline = Pipeline::new();
        pipeline.add_stage(FailStage);
        pipeline.add_stage(AppendStage(" unreachable"));
        let err = pipeline
            .process(SanitizeContext::new("x".to_owned()))
            .unwrap_err();
        assert!(err.to_string().contains("fail"));
    }

    #[test]
    fn truncated_flag_propagates() {
        struct TruncateStage;
        impl Stage for TruncateStage {
            fn name(&self) -> &'static str {
                "truncate"
            }

            fn process(&self, mut ctx: SanitizeContext) -> Result<SanitizeContext, StageError> {
                ctx.content.truncate(3);
                ctx.was_truncated = true;
                Ok(ctx)
            }
        }

        let mut pipeline = Pipeline::new();
        pipeline.add_stage(TruncateStage);
        let out = pipeline
            .process(SanitizeContext::new("hello".to_owned()))
            .unwrap();
        assert!(out.was_truncated);
        assert_eq!(out.content, "hel");
    }
}
