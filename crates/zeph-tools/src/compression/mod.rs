// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! TACO: self-evolving tool output compression.
//!
//! The core abstraction is [`OutputCompressor`], an async trait that post-processes
//! raw tool output before it is injected into the LLM context. When disabled, the
//! [`IdentityCompressor`] is wired in — its `compress` always returns `Ok(None)` and
//! is zero-cost beyond one virtual call per tool output.
//!
//! ## Architecture
//!
//! - [`OutputCompressor`] — core async trait.
//! - [`IdentityCompressor`] — default no-op.
//! - [`RuleBasedCompressor`] — regex rules loaded from `SQLite`.
//! - [`CompressedExecutor`] — decorator wrapping the root [`crate::ToolExecutor`].
//! - [`CompressionRuleStore`] — SQLite/Postgres-backed rule persistence.
//! - [`safe_compile`] — DoS-safe regex compilation with timeout.
//!
//! ## Invariants
//!
//! - **T1**: `IdentityCompressor` is the only compressor when
//!   `[tools.compression] enabled = false`.
//! - **T4**: Audit logging happens inside individual tool implementations, not inside
//!   `CompressedExecutor`. Because compression wraps *outside* the tool boundary,
//!   audit JSONL always records the raw pre-compression payload.

mod identity;
mod regex_safe;
mod rule_based;
mod store;

pub mod decorator;

pub use decorator::CompressedExecutor;
pub use identity::IdentityCompressor;
pub use regex_safe::safe_compile;
pub use rule_based::RuleBasedCompressor;
pub use store::{CompressionRule, CompressionRuleStore};

use std::future::Future;
use std::pin::Pin;

use zeph_common::ToolName;

/// Error variants for compression operations.
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// The rule's regex pattern string is syntactically invalid.
    #[error("regex compile failed: {0}")]
    BadPattern(String),
    /// Regex compilation exceeded the configured deadline; the rule was skipped.
    #[error("regex compile timed out")]
    CompileTimeout,
    /// A database error occurred while loading or persisting compression rules.
    #[error(transparent)]
    Db(#[from] zeph_db::SqlxError),
}

/// Core abstraction for tool output compression.
///
/// Returning `Ok(None)` means "pass through unchanged". Non-`None` `Ok(Some(s))`
/// replaces the tool output with `s` before it reaches the LLM context.
///
/// Implementors must be `Send + Sync + Debug`.
pub trait OutputCompressor: Send + Sync + std::fmt::Debug {
    /// Attempt to compress `output` for the given `tool_name`.
    ///
    /// Returns `Ok(None)` when no rule matched or compression is not applicable.
    /// Returns `Ok(Some(compressed))` with the replacement string.
    ///
    /// # Errors
    ///
    /// Returns [`CompressionError`] on internal failure. Errors are logged and the
    /// raw output is used as fallback by [`CompressedExecutor`].
    fn compress<'a>(
        &'a self,
        tool_name: &'a ToolName,
        output: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CompressionError>> + Send + 'a>>;

    /// Stable identifier for this compressor (used in logs).
    fn name(&self) -> &'static str;
}
