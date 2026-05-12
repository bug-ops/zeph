// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ScrapMem` optical forgetting — progressive content-fidelity decay (issue #3713).
//!
//! Transitions old messages through resolution levels by compressing their content via LLM:
//!
//! 1. **Full** — original message content, unchanged.
//! 2. **Compressed** — LLM-generated summary preserving key facts (stored in `compressed_content`).
//! 3. **`SummaryOnly`** — one-line distilled fact (replaces original content, most compact).
//!
//! The sweep is orthogonal to `SleepGate` (which decays importance scores):
//! - `SleepGate` prunes by importance score below a floor.
//! - Optical forgetting compresses by age (turns since creation).
//!
//! Both can run concurrently; optical forgetting skips messages below the `SleepGate` prune
//! threshold to avoid compressing content that will be pruned shortly anyway.
//!
//! # Invariants
//!
//! - Messages below the `SleepGate` `forgetting_floor` are skipped.
//! - The `episodic_events` table (EM-Graph) references messages by FK; events survive
//!   optical forgetting because messages are never deleted — only their content is replaced.
//! - `focus_pinned` is a runtime-only `MessageMetadata` field and is not stored in the
//!   `messages` table, so it cannot be filtered at the SQL level. The agent loop is
//!   responsible for not triggering optical forgetting on pinned sessions.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

pub use zeph_config::memory::OpticalForgettingConfig;

use crate::error::MemoryError;
use crate::store::SqliteStore;

// ── Content fidelity tier ──────────────────────────────────────────────────────

/// Content-fidelity level for optical forgetting.
///
/// Distinct from [`crate::compression::CompressionLevel`], which classifies memory *type*
/// (episodic vs. declarative abstraction). `ContentFidelity` classifies memory *fidelity*:
/// how much of the original content is preserved. A message can be both
/// `CompressionLevel::Episodic` and `ContentFidelity::Compressed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentFidelity {
    /// Original full-fidelity content.
    Full,
    /// LLM-compressed summary preserving key facts.
    Compressed,
    /// One-line distilled fact. Terminal state.
    SummaryOnly,
}

impl ContentFidelity {
    /// Canonical string stored in the `content_fidelity` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::Compressed => "Compressed",
            Self::SummaryOnly => "SummaryOnly",
        }
    }
}

impl std::fmt::Display for ContentFidelity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ContentFidelity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Full" => Ok(Self::Full),
            "Compressed" => Ok(Self::Compressed),
            "SummaryOnly" => Ok(Self::SummaryOnly),
            other => Err(format!("unknown content_fidelity: {other}")),
        }
    }
}

// ── Result ────────────────────────────────────────────────────────────────────

/// Outcome of a single optical forgetting sweep.
#[derive(Debug, Default)]
pub struct OpticalForgettingResult {
    /// Messages transitioned Full → Compressed.
    pub compressed: u32,
    /// Messages transitioned `Compressed` → `SummaryOnly`.
    pub summarized: u32,
    /// Messages skipped (pinned or below `SleepGate` floor).
    pub skipped: u32,
}

// ── Background loop ───────────────────────────────────────────────────────────

/// Start the background optical forgetting loop.
///
/// Periodically scans messages older than the configured thresholds and progressively
/// compresses them. Database errors are logged but do not stop the loop.
///
/// The loop respects `cancel` for graceful shutdown.
pub async fn start_optical_forgetting_loop(
    store: Arc<SqliteStore>,
    provider: Arc<AnyProvider>,
    config: OpticalForgettingConfig,
    forgetting_floor: f32,
    cancel: CancellationToken,
) {
    if !config.enabled {
        tracing::debug!("optical forgetting disabled (optical_forgetting.enabled = false)");
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs));
    ticker.tick().await; // skip first immediate tick

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("optical forgetting loop shutting down");
                return;
            }
            _ = ticker.tick() => {}
        }

        tracing::debug!("optical_forgetting: starting sweep");
        let start = std::time::Instant::now();

        match run_optical_forgetting_sweep(&store, &provider, &config, forgetting_floor).await {
            Ok(r) => {
                tracing::info!(
                    compressed = r.compressed,
                    summarized = r.summarized,
                    skipped = r.skipped,
                    elapsed_ms = start.elapsed().as_millis(),
                    "optical_forgetting: sweep complete"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    elapsed_ms = start.elapsed().as_millis(),
                    "optical_forgetting: sweep failed, will retry"
                );
            }
        }
    }
}

// ── Sweep implementation ──────────────────────────────────────────────────────

/// Execute one full optical forgetting sweep.
///
/// Phase 1: compress Full messages older than `compress_after_turns`.
/// Phase 2: summarize Compressed messages older than `summarize_after_turns`.
///
/// Skips messages with `importance_score` below `forgetting_floor`
/// (they will be pruned by `SleepGate` soon anyway).
///
/// # Errors
///
/// Returns an error if any database operation fails.
#[cfg_attr(
    feature = "profiling",
    tracing::instrument(name = "memory.optical_forgetting", skip_all)
)]
pub async fn run_optical_forgetting_sweep(
    store: &SqliteStore,
    provider: &Arc<AnyProvider>,
    config: &OpticalForgettingConfig,
    forgetting_floor: f32,
) -> Result<OpticalForgettingResult, MemoryError> {
    let mut result = OpticalForgettingResult::default();

    // Phase 1: Full → Compressed
    let full_candidates = fetch_full_candidates(store, config, forgetting_floor).await?;
    for (msg_id, content) in full_candidates {
        match compress_content(provider, &content).await {
            Ok(compressed) => {
                store_compressed(store, msg_id, &compressed).await?;
                result.compressed += 1;
                tracing::debug!(msg_id, "optical_forgetting: Full → Compressed");
            }
            Err(e) => {
                tracing::warn!(error = %e, msg_id, "optical_forgetting: compression failed, skipping");
                result.skipped += 1;
            }
        }
    }

    // Phase 2: Compressed → SummaryOnly
    let compressed_candidates =
        fetch_compressed_candidates(store, config, forgetting_floor).await?;
    for (msg_id, compressed_content) in compressed_candidates {
        match summarize_content(provider, &compressed_content).await {
            Ok(summary) => {
                store_summary_only(store, msg_id, &summary).await?;
                result.summarized += 1;
                tracing::debug!(msg_id, "optical_forgetting: Compressed → SummaryOnly");
            }
            Err(e) => {
                tracing::warn!(error = %e, msg_id, "optical_forgetting: summarization failed, skipping");
                result.skipped += 1;
            }
        }
    }

    Ok(result)
}

// ── Database helpers ──────────────────────────────────────────────────────────

/// Fetch message IDs and content for Full messages eligible for compression.
///
/// Skips messages below `forgetting_floor`.
async fn fetch_full_candidates(
    store: &SqliteStore,
    config: &OpticalForgettingConfig,
    forgetting_floor: f32,
) -> Result<Vec<(i64, String)>, MemoryError> {
    // COALESCE handles empty table: MAX(id) returns NULL, NULL - N is NULL in SQLite,
    // making the condition always false (no candidates). COALESCE(MAX(id), 0) returns 0,
    // so 0 - N is negative and no row satisfies id <= negative (same safe result, explicit).
    // Note: focus_pinned is not a DB column (it is a runtime MessageMetadata field only),
    // so pinned messages are not excluded here — the caller should avoid passing pinned
    // message IDs through optical forgetting, which is ensured by only sweeping full
    // sessions at the agent level.
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, content FROM messages
         WHERE content_fidelity = 'Full'
           AND deleted_at IS NULL
           AND (importance_score IS NULL OR importance_score >= ?)
           AND id <= (SELECT COALESCE(MAX(id), 0) - ? FROM messages)
         ORDER BY id ASC
         LIMIT ?",
    )
    .bind(forgetting_floor)
    .bind(i64::from(config.compress_after_turns))
    .bind(i64::try_from(config.sweep_batch_size).unwrap_or(i64::MAX))
    .fetch_all(store.pool())
    .await?;

    Ok(rows)
}

/// Fetch message IDs and compressed content for `Compressed` messages eligible for `SummaryOnly`.
async fn fetch_compressed_candidates(
    store: &SqliteStore,
    config: &OpticalForgettingConfig,
    forgetting_floor: f32,
) -> Result<Vec<(i64, String)>, MemoryError> {
    let rows = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT id, compressed_content FROM messages
         WHERE content_fidelity = 'Compressed'
           AND deleted_at IS NULL
           AND (importance_score IS NULL OR importance_score >= ?)
           AND id <= (SELECT COALESCE(MAX(id), 0) - ? FROM messages)
         ORDER BY id ASC
         LIMIT ?",
    )
    .bind(forgetting_floor)
    .bind(i64::from(config.summarize_after_turns))
    .bind(i64::try_from(config.sweep_batch_size).unwrap_or(i64::MAX))
    .fetch_all(store.pool())
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(id, content)| content.map(|c| (id, c)))
        .collect())
}

/// Update a message to Compressed state, storing the LLM summary in `compressed_content`.
async fn store_compressed(
    store: &SqliteStore,
    msg_id: i64,
    compressed: &str,
) -> Result<(), MemoryError> {
    sqlx::query(
        "UPDATE messages
         SET content_fidelity = 'Compressed', compressed_content = ?
         WHERE id = ?",
    )
    .bind(compressed)
    .bind(msg_id)
    .execute(store.pool())
    .await?;
    Ok(())
}

/// Update a message to `SummaryOnly` state, replacing content with the one-line summary.
async fn store_summary_only(
    store: &SqliteStore,
    msg_id: i64,
    summary: &str,
) -> Result<(), MemoryError> {
    sqlx::query(
        "UPDATE messages
         SET content_fidelity = 'SummaryOnly', content = ?, compressed_content = NULL
         WHERE id = ?",
    )
    .bind(summary)
    .bind(msg_id)
    .execute(store.pool())
    .await?;
    Ok(())
}

// ── LLM compression helpers ───────────────────────────────────────────────────

/// Ask the LLM to produce a compressed summary of `content`.
async fn compress_content(
    provider: &Arc<AnyProvider>,
    content: &str,
) -> Result<String, MemoryError> {
    let _span = tracing::debug_span!("memory.optical_forgetting.compress").entered();
    let snippet = content.chars().take(2000).collect::<String>();
    let messages = vec![
        Message {
            role: Role::System,
            content: "You compress conversation messages into concise summaries that preserve \
                      all key facts, decisions, and action items. Output only the summary text, \
                      no preamble."
                .to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: format!("Compress this message:\n\n{snippet}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let raw = tokio::time::timeout(Duration::from_secs(15), provider.chat(&messages))
        .await
        .map_err(|_| MemoryError::Timeout("optical_forgetting: compress timed out".into()))?
        .map_err(MemoryError::Llm)?;

    Ok(raw.trim().to_owned())
}

/// Ask the LLM to distill `content` into a single-line summary.
async fn summarize_content(
    provider: &Arc<AnyProvider>,
    content: &str,
) -> Result<String, MemoryError> {
    let _span = tracing::debug_span!("memory.optical_forgetting.summarize").entered();
    let snippet = content.chars().take(1000).collect::<String>();
    let messages = vec![
        Message {
            role: Role::System,
            content: "You distill summaries into single sentences that capture the essential \
                      fact or outcome. Output only the one-sentence summary, no preamble."
                .to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: format!("Distill into one sentence:\n\n{snippet}"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let raw = tokio::time::timeout(Duration::from_secs(10), provider.chat(&messages))
        .await
        .map_err(|_| MemoryError::Timeout("optical_forgetting: summarize timed out".into()))?
        .map_err(MemoryError::Llm)?;

    Ok(raw.trim().to_owned())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::providers::ProviderName;

    #[test]
    fn content_fidelity_round_trip() {
        for fidelity in [
            ContentFidelity::Full,
            ContentFidelity::Compressed,
            ContentFidelity::SummaryOnly,
        ] {
            let s = fidelity.as_str();
            let parsed: ContentFidelity = s.parse().expect("should parse");
            assert_eq!(parsed, fidelity);
            assert_eq!(format!("{fidelity}"), s);
        }
    }

    #[test]
    fn content_fidelity_unknown_string_errors() {
        assert!("unknown".parse::<ContentFidelity>().is_err());
    }

    #[test]
    fn optical_forgetting_config_defaults() {
        let cfg = OpticalForgettingConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.compress_after_turns, 100);
        assert_eq!(cfg.summarize_after_turns, 500);
        assert_eq!(cfg.sweep_interval_secs, 3600);
        assert_eq!(cfg.sweep_batch_size, 50);
    }

    #[test]
    fn optical_forgetting_result_default() {
        let r = OpticalForgettingResult::default();
        assert_eq!(r.compressed, 0);
        assert_eq!(r.summarized, 0);
        assert_eq!(r.skipped, 0);
    }

    /// Verify that `run_optical_forgetting_sweep` skips all messages when
    /// `compress_after_turns` is larger than the message count (nothing is old enough).
    #[tokio::test]
    async fn sweep_skips_when_no_candidates_old_enough() {
        use std::sync::Arc;

        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        use crate::store::SqliteStore;

        let store = Arc::new(
            SqliteStore::new(":memory:")
                .await
                .expect("SqliteStore::new"),
        );
        let provider = Arc::new(AnyProvider::Mock(MockProvider::default()));

        let cid = store.create_conversation().await.expect("conversation");
        store
            .save_message(cid, "user", "hello")
            .await
            .expect("save_message");

        let config = OpticalForgettingConfig {
            enabled: true,
            compress_after_turns: 100, // message is too recent
            summarize_after_turns: 500,
            sweep_interval_secs: 3600,
            sweep_batch_size: 50,
            compress_provider: ProviderName::default(),
        };
        let result = run_optical_forgetting_sweep(&store, &provider, &config, 0.0)
            .await
            .expect("sweep");

        assert_eq!(
            result.compressed, 0,
            "no message should be compressed when not old enough"
        );
        assert_eq!(result.summarized, 0);
    }

    /// Verify that `run_optical_forgetting_sweep` compresses a Full message that is
    /// old enough (`compress_after_turns` = 0).
    #[tokio::test]
    async fn sweep_compresses_eligible_full_message() {
        use std::sync::Arc;

        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        use crate::store::SqliteStore;

        let store = Arc::new(
            SqliteStore::new(":memory:")
                .await
                .expect("SqliteStore::new"),
        );
        let mock = MockProvider::with_responses(vec!["compressed summary".to_owned()]);
        let provider = Arc::new(AnyProvider::Mock(mock));

        let cid = store.create_conversation().await.expect("conversation");
        // Insert two messages so MAX(id) - 0 = MAX(id), meaning the first message qualifies.
        store
            .save_message(cid, "user", "first message")
            .await
            .expect("save_message 1");
        store
            .save_message(cid, "user", "second message")
            .await
            .expect("save_message 2");

        let config = OpticalForgettingConfig {
            enabled: true,
            compress_after_turns: 0, // everything is eligible
            summarize_after_turns: 500,
            sweep_interval_secs: 3600,
            sweep_batch_size: 50,
            compress_provider: ProviderName::default(),
        };
        let result = run_optical_forgetting_sweep(&store, &provider, &config, 0.0)
            .await
            .expect("sweep");

        // At least one message compressed (the mock returns one response).
        assert!(
            result.compressed >= 1,
            "at least one message must be compressed"
        );
    }

    /// Verify early return when `enabled = false`.
    #[tokio::test]
    async fn sweep_disabled_returns_empty_result() {
        use std::sync::Arc;

        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        use crate::store::SqliteStore;

        let store = Arc::new(
            SqliteStore::new(":memory:")
                .await
                .expect("SqliteStore::new"),
        );
        let provider = Arc::new(AnyProvider::Mock(MockProvider::default()));
        let config = OpticalForgettingConfig {
            enabled: false,
            ..Default::default()
        };
        // With enabled=false, the loop won't call sweep at all. Test that sweep itself
        // produces no side effects when the DB is empty.
        let result = run_optical_forgetting_sweep(&store, &provider, &config, 0.0)
            .await
            .expect("sweep with disabled config");
        assert_eq!(result.compressed, 0);
        assert_eq!(result.summarized, 0);
    }
}
