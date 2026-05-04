// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `MemCoT` [`SemanticStateAccumulator`]: rolling semantic state buffer for graph recall enrichment.
//!
//! The accumulator distills each LLM turn's assistant response into a short semantic state
//! string. This state is then prepended (as `[state] <buf>`) to graph recall queries in the
//! context assembler, improving retrieval relevance over long multi-turn sessions.
//!
//! # Cost bounds
//!
//! - Distillation is skipped when `min_distill_interval_secs` has not elapsed since last distill.
//! - Distillation is skipped after `max_distills_per_session` spawns in the current session.
//! - Both counters are reset by [`SemanticStateAccumulator::reset_session_counters`] on `/new`.
//! - A hard timeout of `distill_timeout_secs` is applied to every LLM call.
//!
//! # Thread safety
//!
//! All hot-path accesses use lock-free atomics. The state buffer is protected by a `RwLock`
//! and only written to inside the background distill task — never on the agent turn thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;
use zeph_config::MemCotConfig;
use zeph_llm::provider::LlmProvider as _;

use super::metrics;

/// Internal semantic state buffer updated by each distillation run.
#[derive(Default)]
pub(crate) struct SemanticState {
    /// Full-overwrite distillation buffer. The LLM is responsible for retaining important
    /// content from the prior state. Post-truncated to `MemCotConfig::max_state_chars` at a
    /// UTF-8 char boundary if the LLM exceeded the cap.
    pub(crate) buffer: String,
    /// Number of successful distillations since the accumulator was created.
    pub(crate) turn_count: u64,
    /// Unix seconds of last successful distillation.
    pub(crate) updated_at_secs: i64,
}

/// Manages the rolling semantic state buffer for `MemCoT`.
///
/// One instance lives on the agent for the lifetime of the process (created at startup when
/// `memory.memcot.enabled = true`; otherwise `None`).
///
/// # Examples
///
/// ```ignore
/// # use std::sync::Arc;
/// # use zeph_config::MemCotConfig;
/// let cfg = MemCotConfig::default(); // enabled = false
/// let acc = SemanticStateAccumulator::new(Arc::new(cfg));
/// assert!(acc.current_state().await.is_none());
/// ```
pub struct SemanticStateAccumulator {
    cfg: Arc<MemCotConfig>,
    state: Arc<RwLock<SemanticState>>,
    /// Unix seconds of the last distillation spawn (written BEFORE spawn to avoid races).
    pub(crate) last_distill_at_secs: Arc<AtomicI64>,
    /// Number of distillation spawns in the current session.
    pub(crate) distill_count_session: Arc<AtomicU64>,
}

impl SemanticStateAccumulator {
    /// Create a new accumulator.
    ///
    /// The accumulator is always created (even when `cfg.enabled = false`) so that the field
    /// on `MemoryExtractionState` can be `Option<SemanticStateAccumulator>` where `None`
    /// means "not configured" and `Some` means "created but possibly no-op".
    ///
    /// In practice callers wrap in `Option`: the extraction state creates it only when `enabled`.
    #[must_use]
    pub fn new(cfg: Arc<MemCotConfig>) -> Self {
        Self {
            cfg,
            state: Arc::new(RwLock::new(SemanticState::default())),
            last_distill_at_secs: Arc::new(AtomicI64::new(0)),
            distill_count_session: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Return the current semantic state buffer, or `None` if empty.
    pub async fn current_state(&self) -> Option<String> {
        let guard = self.state.read().await;
        if guard.buffer.is_empty() {
            None
        } else {
            Some(guard.buffer.clone())
        }
    }

    /// Reset per-session distillation counters and clear the semantic state buffer.
    ///
    /// Called by the `/new` handler when the user starts a new conversation.
    /// `distill_count_session` and `last_distill_at_secs` are reset to 0.
    /// The state buffer is cleared so stale semantic state from a prior session
    /// does not contaminate graph recall queries in the new session.
    pub async fn reset_session_counters(&self) {
        self.distill_count_session.store(0, Ordering::Relaxed);
        self.last_distill_at_secs.store(0, Ordering::Relaxed);
        self.state.write().await.buffer.clear();
    }

    /// Check all cost gates and, if they pass, spawn a background distillation task.
    ///
    /// Gates checked (in order):
    /// 1. `cfg.enabled` — early exit if `MemCoT` is disabled.
    /// 2. `min_assistant_chars` — skip trivial replies.
    /// 3. `min_distill_interval_secs` — skip if too soon since last distill.
    /// 4. `max_distills_per_session` — skip after session cap.
    ///
    /// When all gates pass, counters are incremented **before** the spawn to avoid
    /// double-spawn races when distill latency is shorter than the next turn.
    pub(crate) fn maybe_enqueue_distill(
        &self,
        assistant_content: &str,
        provider: zeph_llm::any::AnyProvider,
        supervisor_spawn: impl FnOnce(
            &'static str,
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        ),
    ) {
        if !self.cfg.enabled {
            return;
        }

        let content_chars = assistant_content.chars().count();
        if content_chars < self.cfg.min_assistant_chars {
            return;
        }

        let now = unix_now_secs();
        let elapsed = now.saturating_sub(self.last_distill_at_secs.load(Ordering::Relaxed));
        if elapsed < i64::try_from(self.cfg.min_distill_interval_secs).unwrap_or(i64::MAX) {
            metrics::distill_skipped("interval");
            return;
        }
        if self.distill_count_session.load(Ordering::Relaxed) >= self.cfg.max_distills_per_session {
            metrics::distill_skipped("session_cap");
            return;
        }

        // Reserve before spawn to avoid race.
        self.last_distill_at_secs.store(now, Ordering::Relaxed);
        self.distill_count_session.fetch_add(1, Ordering::Relaxed);
        metrics::distill_total();

        let state_arc = Arc::clone(&self.state);
        let cfg = Arc::clone(&self.cfg);
        let content = assistant_content.to_owned();

        let fut = Box::pin(async move {
            let span = tracing::info_span!("core.memcot.distill", result = tracing::field::Empty);
            let _guard = span.enter();

            let prompt = build_distill_prompt(&content, &state_arc).await;
            let msgs = vec![zeph_llm::provider::Message::from_legacy(
                zeph_llm::provider::Role::User,
                prompt,
            )];

            let timeout = Duration::from_secs(cfg.distill_timeout_secs);
            let result = tokio::time::timeout(timeout, provider.chat(&msgs)).await;

            match result {
                Ok(Ok(response)) => {
                    tracing::Span::current().record("result", "ok");
                    let raw = response.trim().to_owned();
                    let cap = cfg.max_state_chars;
                    let new_buf = if raw.chars().count() > cap {
                        let cut = raw.floor_char_boundary(
                            raw.char_indices().nth(cap).map_or(raw.len(), |(i, _)| i),
                        );
                        raw[..cut].to_owned()
                    } else {
                        raw
                    };

                    let mut state = state_arc.write().await;
                    state.buffer = new_buf;
                    state.turn_count = state.turn_count.saturating_add(1);
                    state.updated_at_secs = unix_now_secs();
                    tracing::debug!(
                        turn = state.turn_count,
                        buf_chars = state.buffer.chars().count(),
                        "memcot: distill complete"
                    );
                }
                Ok(Err(e)) => {
                    tracing::Span::current().record("result", "error");
                    metrics::distill_error();
                    tracing::warn!(error = %e, "memcot: distill failed");
                }
                Err(_) => {
                    tracing::Span::current().record("result", "timeout");
                    metrics::distill_timeout();
                    tracing::warn!(
                        timeout_secs = cfg.distill_timeout_secs,
                        "memcot: distill timed out"
                    );
                }
            }
        });

        supervisor_spawn("memcot_distill", fut);
    }
}

/// Scrub characters that could escape delimiters or confuse the LLM parser.
fn scrub_content(s: &str) -> String {
    s.replace(['\n', '\r', '<', '>'], " ")
}

/// Build the distillation prompt from the current assistant turn and prior state.
///
/// Content is wrapped in `<turn>…</turn>` delimiters and scrubbed of `\n\r<>` to prevent
/// prompt injection from raw assistant output.
async fn build_distill_prompt(assistant_content: &str, state: &RwLock<SemanticState>) -> String {
    let prior = {
        let guard = state.read().await;
        guard.buffer.clone()
    };

    let safe_content = scrub_content(assistant_content);

    if prior.is_empty() {
        format!(
            "Summarize the key concepts and entities from the following assistant response \
             in 1-3 short sentences. Focus on what changed or was decided.\n\n\
             <turn>{safe_content}</turn>"
        )
    } else {
        let safe_prior = scrub_content(&prior);
        format!(
            "Update the semantic state by integrating the new assistant response. \
             Keep the most important concepts from the prior state and add new ones. \
             Reply with 1-3 short sentences total.\n\n\
             Prior state: {safe_prior}\n\n\
             <turn>{safe_content}</turn>"
        )
    }
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_provider() -> zeph_llm::any::AnyProvider {
        zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default())
    }

    #[tokio::test]
    async fn accumulator_initial_state_empty() {
        let cfg = Arc::new(MemCotConfig::default());
        let acc = SemanticStateAccumulator::new(cfg);
        assert!(acc.current_state().await.is_none());
    }

    #[tokio::test]
    #[allow(clippy::large_futures)]
    async fn reset_session_counters_clears_state() {
        let cfg = MemCotConfig {
            enabled: true,
            ..MemCotConfig::default()
        };
        let acc = SemanticStateAccumulator::new(Arc::new(cfg));
        acc.distill_count_session.store(42, Ordering::Relaxed);
        acc.last_distill_at_secs.store(9999, Ordering::Relaxed);
        // Pre-populate the state buffer to verify it is cleared on reset.
        acc.state.write().await.buffer = "prior semantic state".to_owned();
        acc.reset_session_counters().await;
        assert_eq!(acc.distill_count_session.load(Ordering::Relaxed), 0);
        assert_eq!(acc.last_distill_at_secs.load(Ordering::Relaxed), 0);
        assert!(
            acc.current_state().await.is_none(),
            "buffer must be cleared on reset"
        );
    }

    #[tokio::test]
    async fn distill_skipped_when_disabled() {
        let cfg = Arc::new(MemCotConfig {
            enabled: false,
            ..MemCotConfig::default()
        });
        let acc = SemanticStateAccumulator::new(cfg);
        let mut spawn_called = false;
        acc.maybe_enqueue_distill("hello", null_provider(), |_name, _fut| {
            spawn_called = true;
        });
        assert!(!spawn_called, "disabled accumulator must never spawn");
    }

    #[tokio::test]
    async fn distill_skipped_when_content_too_short() {
        let cfg = Arc::new(MemCotConfig {
            enabled: true,
            min_assistant_chars: 100,
            min_distill_interval_secs: 0,
            max_distills_per_session: 50,
            ..MemCotConfig::default()
        });
        let acc = SemanticStateAccumulator::new(cfg);
        let mut spawn_called = false;
        acc.maybe_enqueue_distill("hi", null_provider(), |_name, _fut| {
            spawn_called = true;
        });
        assert!(!spawn_called, "should not spawn for short content");
    }

    #[tokio::test]
    async fn distill_skipped_when_session_cap_reached() {
        let cfg = Arc::new(MemCotConfig {
            enabled: true,
            max_distills_per_session: 2,
            min_distill_interval_secs: 0,
            min_assistant_chars: 1,
            ..MemCotConfig::default()
        });
        let acc = SemanticStateAccumulator::new(cfg);
        acc.distill_count_session.store(2, Ordering::Relaxed);
        let mut spawn_called = false;
        acc.maybe_enqueue_distill("hello world!", null_provider(), |_name, _fut| {
            spawn_called = true;
        });
        assert!(!spawn_called, "should not spawn when session cap reached");
    }

    #[tokio::test]
    async fn distill_skipped_when_interval_not_elapsed() {
        let cfg = Arc::new(MemCotConfig {
            enabled: true,
            max_distills_per_session: 50,
            min_distill_interval_secs: 9999,
            min_assistant_chars: 1,
            ..MemCotConfig::default()
        });
        let acc = SemanticStateAccumulator::new(cfg);
        // Simulate a recent distill.
        acc.last_distill_at_secs
            .store(unix_now_secs(), Ordering::Relaxed);
        let mut spawn_called = false;
        acc.maybe_enqueue_distill("hello world!", null_provider(), |_name, _fut| {
            spawn_called = true;
        });
        assert!(!spawn_called, "should not spawn before interval elapses");
    }

    #[tokio::test]
    async fn distill_spawned_when_all_gates_pass() {
        let cfg = Arc::new(MemCotConfig {
            enabled: true,
            max_distills_per_session: 50,
            min_distill_interval_secs: 0,
            min_assistant_chars: 1,
            ..MemCotConfig::default()
        });
        let acc = SemanticStateAccumulator::new(cfg);
        let mut spawn_called = false;
        acc.maybe_enqueue_distill("hello world!", null_provider(), |_name, _fut| {
            spawn_called = true;
        });
        assert!(spawn_called, "should spawn when all gates pass");
        assert_eq!(acc.distill_count_session.load(Ordering::Relaxed), 1);
    }
}
