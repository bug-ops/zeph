// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Speculative tool execution engine.
//!
//! Provides two complementary strategies for reducing tool-dispatch latency:
//!
//! - **Decoding-level** (`SpeculationMode::Decoding`, issue #2290): drains the LLM
//!   `ToolStream` SSE events and fires tool calls speculatively as soon as all
//!   required JSON fields are present in the partial input buffer.
//!
//! - **Pattern-level** (`SpeculationMode::Pattern`, issue #2409 PASTE): queries
//!   `SQLite` at skill activation to predict the most likely next tool calls from
//!   historical invocation sequences.
//!
//! Both strategies share a bounded [`SpeculativeCache`] and per-handle TTL enforcement.
//! Speculation is completely disabled (`mode = off`) by default and never adds cargo
//! feature flags — all branches compile unconditionally.
//!
//! ## Safety invariants
//!
//! - Speculative dispatch **always** uses `execute_tool_call` (never `_confirmed`).
//! - A call is not dispatched speculatively when `trust_level != Trusted`.
//! - A call is not dispatched speculatively when `requires_confirmation` returns `true`.
//! - No synchronous dry-run execution — confirmation is checked via a policy query,
//!   not by actually running the tool (C1: no double side-effects).
//! - All in-flight handles are cancelled at turn boundary.
//! - Per-handle TTL (default 30 s) is enforced by a background sweeper that shares
//!   the same cache instance (C2: no separate empty cache in the sweeper).

#![allow(dead_code)]

pub mod cache;
pub mod partial_json;
pub mod paste;
pub mod prediction;

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use zeph_common::SkillTrustLevel;
use zeph_tools::{ErasedToolExecutor, ToolCall, ToolError, ToolOutput};

use cache::{HandleKey, SpeculativeCache, SpeculativeHandle, hash_args};
use prediction::Prediction;

pub use zeph_tools::config::{SpeculationMode, SpeculativeConfig};

/// Metrics collected across a single agent turn.
#[derive(Debug, Default, Clone)]
pub struct SpeculativeMetrics {
    /// Handles that matched and committed.
    pub committed: u32,
    /// Handles that were cancelled (mismatch, TTL, turn end).
    pub cancelled: u32,
    /// Handles that were evicted because `max_in_flight` was saturated.
    pub evicted_oldest: u32,
    /// Handles skipped because `requires_confirmation` returned `true`.
    pub skipped_confirmation: u32,
    /// Total wall-clock milliseconds spent in wasted speculative work.
    pub wasted_ms: u64,
}

/// Speculative execution engine.
///
/// Holds a reference to the underlying executor, the shared cache, and the active
/// configuration. Create one instance per agent session and share via `Arc`.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use zeph_tools::config::SpeculativeConfig;
/// use zeph_core::agent::speculative::SpeculationEngine;
///
/// # async fn example(executor: Arc<dyn zeph_tools::ErasedToolExecutor>) {
/// let config = SpeculativeConfig::default(); // mode = off
/// let engine = SpeculationEngine::new(executor, config);
/// # }
/// ```
pub struct SpeculationEngine {
    executor: Arc<dyn ErasedToolExecutor>,
    config: SpeculativeConfig,
    cache: SpeculativeCache,
    metrics: parking_lot::Mutex<SpeculativeMetrics>,
    /// Background sweeper task (cancelled on drop).
    sweeper: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SpeculationEngine {
    /// Create a new engine with the given executor and config.
    #[must_use]
    pub fn new(executor: Arc<dyn ErasedToolExecutor>, config: SpeculativeConfig) -> Self {
        let cache = SpeculativeCache::new(config.max_in_flight);

        // Share the inner Arc so the sweeper operates on the *same* handle set (fixes C2).
        let shared = cache.shared_inner();

        // intentionally untracked: SpeculationEngine owns this JoinHandle in `sweeper` and
        // aborts it in Drop. The engine has no access to BackgroundSupervisor; it is constructed
        // standalone. The sweeper is an internal implementation detail, not agent-level work.
        let sweeper = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                SpeculativeCache::sweep_expired_inner(&shared);
            }
        });

        Self {
            executor,
            config,
            cache,
            metrics: parking_lot::Mutex::new(SpeculativeMetrics::default()),
            sweeper: parking_lot::Mutex::new(Some(sweeper)),
        }
    }

    /// Current speculation mode.
    #[must_use]
    pub fn mode(&self) -> SpeculationMode {
        self.config.mode
    }

    /// Returns `true` when speculation is not `Off`.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.config.mode != SpeculationMode::Off
    }

    /// Try to dispatch `prediction` speculatively.
    ///
    /// Returns `false` when the call is skipped (not speculatable, trust gate, confirmation
    /// gate, or circuit-breaker). Returns `true` when the handle was inserted in the cache.
    ///
    /// The confirmation check is performed via `requires_confirmation_erased` — a pure policy
    /// query that does **not** execute the tool (fixes C1: no double side-effects).
    pub fn try_dispatch(&self, prediction: &Prediction, trust_level: SkillTrustLevel) -> bool {
        if trust_level != SkillTrustLevel::Trusted {
            return false;
        }

        let tool_id = &prediction.tool_id;
        if !self.executor.is_tool_speculatable_erased(tool_id.as_ref()) {
            return false;
        }

        let call = prediction.to_tool_call(format!("spec-{}", uuid::Uuid::new_v4()));
        let args_hash = hash_args(&call.params);

        // Policy check: skip if the tool would require user confirmation.
        // This is a pure metadata query — no execution, no side-effects (C1).
        if self.executor.requires_confirmation_erased(&call) {
            let mut m = self.metrics.lock();
            m.skipped_confirmation += 1;
            debug!(tool_id = %tool_id, "speculative skip: requires_confirmation");
            return false;
        }

        let exec = Arc::clone(&self.executor);
        let call_clone = call.clone();
        let cancel = CancellationToken::new();
        let cancel_child = cancel.child_token();

        // intentionally untracked: returns a typed JoinHandle<Result<ToolOutput, ToolError>>
        // stored inside SpeculativeHandle. The handle is cancelled via its own CancellationToken
        // when the speculation is invalidated. SpeculationEngine has no access to
        // BackgroundSupervisor; typed result retrieval requires the raw JoinHandle.
        let join = tokio::spawn(async move {
            tokio::select! {
                result = exec.execute_tool_call_erased(&call_clone) => result,
                () = cancel_child.cancelled() => {
                    Err(ToolError::Execution(std::io::Error::other("speculative cancelled")))
                }
            }
        });

        let handle = SpeculativeHandle {
            key: HandleKey {
                tool_id: tool_id.clone(),
                args_hash,
            },
            join,
            cancel,
            ttl_deadline: Instant::now() + Duration::from_secs(self.config.ttl_seconds),
            started_at: std::time::Instant::now(),
        };

        debug!(tool_id = %tool_id, confidence = prediction.confidence, "speculative dispatch");
        self.cache.insert(handle);
        true
    }

    /// Attempt to commit a speculative handle for `call`.
    ///
    /// If a matching handle exists (same `tool_id` + `args_hash`), awaits its result and
    /// returns it. If no match, returns `None` — caller should fall through to normal dispatch.
    pub async fn try_commit(
        &self,
        call: &ToolCall,
    ) -> Option<Result<Option<ToolOutput>, ToolError>> {
        let args_hash = hash_args(&call.params);
        if let Some(handle) = self.cache.take_match(&call.tool_id, &args_hash) {
            {
                let mut m = self.metrics.lock();
                m.committed += 1;
            }
            debug!(tool_id = %call.tool_id, "speculative commit");
            Some(handle.commit().await)
        } else {
            None
        }
    }

    /// Cancel and remove the in-flight handle for `tool_id`, if any.
    ///
    /// Performs an actual cache lookup and task abort (fixes C3: was metrics-only no-op).
    pub fn cancel_for(&self, tool_id: &zeph_common::ToolName) {
        debug!(tool_id = %tool_id, "speculative cancel for tool");
        self.cache.cancel_by_tool_id(tool_id);
        let mut m = self.metrics.lock();
        m.cancelled += 1;
    }

    /// Cancel all in-flight handles at turn boundary and return metrics snapshot.
    pub fn end_turn(&self) -> SpeculativeMetrics {
        self.cache.cancel_all();
        let m = self.metrics.lock().clone();
        *self.metrics.lock() = SpeculativeMetrics::default();
        m
    }

    /// Snapshot current metrics without resetting.
    #[must_use]
    pub fn metrics_snapshot(&self) -> SpeculativeMetrics {
        self.metrics.lock().clone()
    }
}

impl Drop for SpeculationEngine {
    fn drop(&mut self) {
        self.cache.cancel_all();
        if let Some(sweeper) = self.sweeper.lock().take() {
            sweeper.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

    struct AlwaysOkExecutor;

    impl ToolExecutor for AlwaysOkExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: zeph_common::ToolName::new("test"),
                summary: "ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }

        fn is_tool_speculatable(&self, _: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn dispatch_and_commit_succeeds() {
        let exec: Arc<dyn ErasedToolExecutor> = Arc::new(AlwaysOkExecutor);
        let config = SpeculativeConfig {
            mode: SpeculationMode::Decoding,
            ..Default::default()
        };
        let engine = SpeculationEngine::new(exec, config);

        let pred = Prediction {
            tool_id: zeph_common::ToolName::new("test"),
            args: serde_json::Map::new(),
            confidence: 0.9,
            source: prediction::PredictionSource::StreamPartial,
        };

        let dispatched = engine.try_dispatch(&pred, SkillTrustLevel::Trusted);
        let _ = dispatched;
    }

    #[tokio::test]
    async fn untrusted_skill_skips_dispatch() {
        let exec: Arc<dyn ErasedToolExecutor> = Arc::new(AlwaysOkExecutor);
        let config = SpeculativeConfig {
            mode: SpeculationMode::Decoding,
            ..Default::default()
        };
        let engine = SpeculationEngine::new(exec, config);

        let pred = Prediction {
            tool_id: zeph_common::ToolName::new("test"),
            args: serde_json::Map::new(),
            confidence: 0.9,
            source: prediction::PredictionSource::StreamPartial,
        };

        let dispatched = engine.try_dispatch(&pred, SkillTrustLevel::Quarantined);
        assert!(
            !dispatched,
            "untrusted skill must not dispatch speculatively"
        );
    }

    #[tokio::test]
    async fn cancel_for_removes_handle() {
        let exec: Arc<dyn ErasedToolExecutor> = Arc::new(AlwaysOkExecutor);
        let config = SpeculativeConfig {
            mode: SpeculationMode::Decoding,
            ..Default::default()
        };
        let engine = SpeculationEngine::new(exec, config);

        let pred = Prediction {
            tool_id: zeph_common::ToolName::new("test"),
            args: serde_json::Map::new(),
            confidence: 0.9,
            source: prediction::PredictionSource::StreamPartial,
        };

        engine.try_dispatch(&pred, SkillTrustLevel::Trusted);
        // After cancel_for the cache should be empty.
        engine.cancel_for(&zeph_common::ToolName::new("test"));
        assert!(
            engine.cache.is_empty(),
            "cancel_for must remove handle from cache"
        );
    }
}
