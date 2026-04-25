// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sealed extension trait for router-specific quality-signal methods.
//!
//! [`RouterAware`] exposes the subset of [`crate::router::RouterProvider`] methods that only
//! make sense when the underlying provider is a multi-provider router:
//!
//! - [`RouterAware::set_memory_confidence`] — MAR (Memory-Augmented Routing) signal
//! - [`RouterAware::record_quality_outcome`] — RAPS (Reputation-Aware Provider Selection) signal
//!
//! The trait is **sealed** via the private `Sealed` supertrait. External crates cannot
//! implement it, which prevents accidental silent no-ops on non-router providers.
//!
//! # Import discipline
//!
//! Call sites that need quality signals must import this trait explicitly:
//!
//! ```rust,no_run
//! use zeph_llm::router::RouterAware;
//! use zeph_llm::any::AnyProvider;
//!
//! fn report_quality(provider: &AnyProvider, name: &str, success: bool) {
//!     provider.record_quality_outcome(name, success);
//! }
//! ```
//!
//! When the provider is not a [`crate::router::RouterProvider`], both methods are no-ops that
//! emit a `tracing::trace!` event so the drop is observable in trace JSON.

use crate::any::AnyProvider;
use crate::provider::LlmProvider;
use crate::router::RouterProvider;

mod sealed {
    pub trait Sealed {}
}

/// Extension trait for router-specific quality-signal methods.
///
/// Implemented only on [`crate::router::RouterProvider`] and [`AnyProvider`]. The trait is sealed to
/// prevent external implementations — quality signals are only meaningful for multi-provider
/// routers, and the `AnyProvider` impl provides the correct no-op + trace for non-router
/// variants.
///
/// See the [module documentation](self) for usage.
pub trait RouterAware: sealed::Sealed {
    /// Set the MAR (Memory-Augmented Routing) confidence signal for the current turn.
    ///
    /// Must be called before `chat` / `chat_stream` to influence bandit provider selection.
    /// Pass `None` to disable MAR for this turn.
    ///
    /// No-op (with a `tracing::trace!` event) when the underlying provider is not a router.
    fn set_memory_confidence(&self, confidence: Option<f32>);

    /// Record a semantic quality outcome for the last active sub-provider (RAPS).
    ///
    /// Call only for semantic failures (invalid tool arguments, parse errors).
    /// Do NOT call for network errors, rate limits, or transient I/O failures.
    ///
    /// No-op (with a `tracing::trace!` event) when the underlying provider is not a router
    /// or when reputation scoring is not enabled.
    fn record_quality_outcome(&self, provider_name: &str, success: bool);
}

impl sealed::Sealed for RouterProvider {}

impl RouterAware for RouterProvider {
    fn set_memory_confidence(&self, confidence: Option<f32>) {
        RouterProvider::set_memory_confidence(self, confidence);
    }

    fn record_quality_outcome(&self, provider_name: &str, success: bool) {
        RouterProvider::record_quality_outcome(self, provider_name, success);
    }
}

impl sealed::Sealed for AnyProvider {}

impl RouterAware for AnyProvider {
    fn set_memory_confidence(&self, confidence: Option<f32>) {
        if let AnyProvider::Router(r) = self {
            r.set_memory_confidence(confidence);
        } else {
            tracing::trace!(
                provider_variant = self.name(),
                confidence = ?confidence,
                "set_memory_confidence: no-op (non-router provider; MAR signal requires RouterProvider)"
            );
        }
    }

    fn record_quality_outcome(&self, provider_name: &str, success: bool) {
        if let AnyProvider::Router(p) = self {
            p.record_quality_outcome(provider_name, success);
        } else {
            tracing::trace!(
                provider_name,
                success,
                provider_variant = self.name(),
                "record_quality_outcome: no-op (non-router provider; quality signals require RouterProvider)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeProvider;
    use crate::mock::MockProvider;
    use crate::ollama::OllamaProvider;

    #[test]
    fn router_aware_noop_on_ollama_set_memory_confidence() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        // Must not panic; emits a tracing::trace! but is otherwise a no-op.
        provider.set_memory_confidence(Some(0.9));
    }

    #[test]
    fn router_aware_noop_on_claude_record_quality_outcome() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        // Must not panic; emits a tracing::trace! but is otherwise a no-op.
        provider.record_quality_outcome("claude", true);
    }

    #[test]
    fn router_aware_noop_on_mock_set_memory_confidence() {
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["ok".into()]));
        provider.set_memory_confidence(None);
    }

    #[test]
    fn router_aware_noop_on_mock_record_quality_outcome() {
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["ok".into()]));
        provider.record_quality_outcome("mock", false);
    }
}
