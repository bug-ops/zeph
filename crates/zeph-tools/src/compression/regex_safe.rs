// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DoS-safe regex compilation (S6 fix).
//!
//! The `regex` crate has no compile-time deadline. `safe_compile` bounds compilation
//! by running it in a `spawn_blocking` task raced against `tokio::time::timeout`.
//!
//! ## Thread cap (H2)
//!
//! `spawn_blocking` threads are not cancelled on timeout — they continue running until
//! the pattern compiles or the blocking thread pool shuts down. To bound the maximum
//! number of simultaneously live compile threads, a global `AtomicUsize` counter gates
//! entry: if `MAX_COMPILE_TASKS` threads are already running, `safe_compile` returns
//! [`CompressionError::CompileTimeout`] immediately without spawning a new one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::CompressionError;

/// Maximum number of regex compile tasks allowed in-flight simultaneously.
const MAX_COMPILE_TASKS: usize = 4;

static ACTIVE_COMPILE_TASKS: AtomicUsize = AtomicUsize::new(0);

/// Compile a regex pattern with `DoS` protection.
///
/// Applies NFA size limit (64 KiB), DFA size limit (1 MiB), and a `timeout_ms`
/// deadline enforced via `spawn_blocking` + `tokio::time::timeout`.
///
/// Returns [`CompressionError::CompileTimeout`] immediately when
/// [`MAX_COMPILE_TASKS`] concurrent compilations are already in-flight.
///
/// On timeout or panic from the blocking task, returns a typed error that allows
/// the evolver's failure counter to distinguish DoS-risk patterns from syntax errors.
///
/// # Errors
///
/// - [`CompressionError::BadPattern`] for syntax errors or task panics.
/// - [`CompressionError::CompileTimeout`] when the in-flight limit is reached or
///   compilation exceeds `timeout_ms`.
pub async fn safe_compile(pat: &str, timeout_ms: u64) -> Result<regex::Regex, CompressionError> {
    // Reject immediately if the thread cap is saturated.
    let prev = ACTIVE_COMPILE_TASKS.fetch_add(1, Ordering::Relaxed);
    if prev >= MAX_COMPILE_TASKS {
        ACTIVE_COMPILE_TASKS.fetch_sub(1, Ordering::Relaxed);
        return Err(CompressionError::CompileTimeout);
    }

    let pat = pat.to_owned();
    let join = tokio::task::spawn_blocking(move || {
        let result = regex::RegexBuilder::new(&pat)
            .size_limit(64 * 1024)
            .dfa_size_limit(1024 * 1024)
            .build();
        // Always decrement the counter when the blocking thread finishes.
        ACTIVE_COMPILE_TASKS.fetch_sub(1, Ordering::Relaxed);
        result
    });

    match tokio::time::timeout(Duration::from_millis(timeout_ms), join).await {
        Err(_elapsed) => Err(CompressionError::CompileTimeout),
        Ok(Err(_join_err)) => Err(CompressionError::BadPattern("compile task panicked".into())),
        Ok(Ok(Err(regex_err))) => Err(CompressionError::BadPattern(regex_err.to_string())),
        Ok(Ok(Ok(re))) => Ok(re),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compiles_simple_pattern() {
        let re = safe_compile(r"\d+", 500).await.unwrap();
        assert!(re.is_match("123"));
    }

    #[tokio::test]
    async fn rejects_invalid_pattern() {
        let err = safe_compile(r"[invalid", 500).await.unwrap_err();
        assert!(matches!(err, CompressionError::BadPattern(_)));
    }
}
