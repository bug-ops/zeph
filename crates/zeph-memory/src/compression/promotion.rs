// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background skill-promotion engine (#3305).
//!
//! [`PromotionEngine`] scans a window of recent episodic messages for clustering
//! patterns and promotes qualifying clusters to SKILL.md files on disk.
//!
//! # C1 fix — session provenance
//!
//! [`PromotionInput`] carries a `conversation_id` field that is absent from
//! [`crate::facade::MemoryMatch`].  This allows the engine to enforce the
//! `min_sessions` heuristic without touching the public recall API.
//!
//! # Dependency inversion
//!
//! To avoid a circular crate dependency (`zeph-memory` ↔ `zeph-skills`), skill
//! generation is delegated to [`SkillWriter`], a trait that callers in
//! `zeph-core` implement using `zeph_skills::generator::SkillGenerator`.
//! This keeps `zeph-memory` free of a direct `zeph-skills` dependency.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};

// ── PromotionInput ────────────────────────────────────────────────────────────

/// A single episodic message prepared for the promotion scan.
///
/// This type carries the `conversation_id` (session provenance) that ordinary
/// [`crate::facade::MemoryMatch`] results do not expose, making it possible to
/// enforce the [`PromotionConfig::min_sessions`] heuristic.
#[derive(Debug, Clone)]
pub struct PromotionInput {
    /// Identifies the individual message for deduplication bookkeeping.
    pub message_id: MessageId,
    /// The session this message belongs to.
    pub conversation_id: ConversationId,
    /// Raw message content.
    pub content: String,
    /// Pre-computed embedding vector.
    ///
    /// When `None`, the scan will skip this row rather than re-embed inline on
    /// the hot path — embedding is expensive and the promotion engine runs in the
    /// background.
    pub embedding: Option<Vec<f32>>,
}

// ── PromotionCandidate ────────────────────────────────────────────────────────

/// A cluster of episodic messages that qualifies for promotion to a SKILL.md.
#[derive(Debug, Clone)]
pub struct PromotionCandidate {
    /// A stable identifier derived from the cluster centroid (SHA-256 hex, truncated).
    pub signature: String,
    /// IDs of the messages in this cluster.
    pub member_ids: Vec<MessageId>,
    /// Distinct sessions that contributed at least one member to this cluster.
    pub session_ids: Vec<ConversationId>,
    /// Average embedding vector of cluster members (centroid).
    pub centroid: Vec<f32>,
}

// ── PromotionConfig ───────────────────────────────────────────────────────────

/// Configuration knobs for [`PromotionEngine`].
///
/// All thresholds have conservative defaults — they should be tuned based on
/// real-world telemetry once the feature is in production.
#[derive(Debug, Clone)]
pub struct PromotionConfig {
    /// Minimum number of cluster members to qualify for promotion. Default: `3`.
    pub min_occurrences: u32,
    /// Minimum number of distinct sessions represented in the cluster. Default: `2`.
    pub min_sessions: u32,
    /// Cosine similarity threshold for clustering. Messages with similarity ≥ this value
    /// to a cluster's centroid are merged into that cluster. Default: `0.85`.
    pub cluster_threshold: f32,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            min_occurrences: 3,
            min_sessions: 2,
            cluster_threshold: 0.85,
        }
    }
}

// ── SkillWriter ───────────────────────────────────────────────────────────────

/// Trait for writing a generated SKILL.md to disk.
///
/// Implemented in `zeph-core` using `zeph_skills::generator::SkillGenerator`.
/// Defined here to avoid a circular crate dependency.
///
/// # Contract
///
/// Implementors must:
/// - Generate a valid SKILL.md from `description`.
/// - Apply any configured evaluator gate before writing.
/// - Return `Ok(())` on success or evaluator rejection (rejection is not an error).
/// - Return `Err` only on hard failures (LLM error, I/O error).
pub trait SkillWriter: Send + Sync {
    /// Generate and persist a SKILL.md from `description`.
    ///
    /// `signature` is used as an idempotency key — callers should ensure the skill
    /// file does not already exist before calling this method.
    ///
    /// # Errors
    ///
    /// Returns an error string on generation or I/O failure.
    fn write_skill(
        &self,
        description: String,
        signature: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>;
}

// ── PromotionEngine ───────────────────────────────────────────────────────────

/// Background engine that scans episodic memory and promotes recurring patterns to skills.
///
/// Runs off the hot path, typically queued to a `JoinSet` at turn boundary.
///
/// # Examples
///
/// ```rust,no_run
/// use std::path::PathBuf;
/// use std::sync::Arc;
/// use zeph_memory::compression::promotion::{PromotionEngine, PromotionConfig};
///
/// # struct MockWriter;
/// # impl zeph_memory::compression::promotion::SkillWriter for MockWriter {
/// #   fn write_skill(&self, _d: String, _s: String)
/// #     -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
/// #   { Box::pin(async { Ok(()) }) }
/// # }
/// let engine = PromotionEngine::new(
///     Arc::new(MockWriter),
///     PromotionConfig::default(),
///     PathBuf::from("/tmp/skills"),
/// );
/// ```
pub struct PromotionEngine {
    writer: Arc<dyn SkillWriter>,
    config: PromotionConfig,
    output_dir: PathBuf,
}

impl PromotionEngine {
    /// Create a new promotion engine.
    ///
    /// `writer` is injected from `zeph-core` and encapsulates `SkillGenerator` +
    /// optional `SkillEvaluator`. `output_dir` is where SKILL.md directories are created.
    #[must_use]
    pub fn new(writer: Arc<dyn SkillWriter>, config: PromotionConfig, output_dir: PathBuf) -> Self {
        Self {
            writer,
            config,
            output_dir,
        }
    }

    /// Scan a recent-episodic window and return clusters that qualify for promotion.
    ///
    /// Clustering is greedy: each message is assigned to the first cluster whose centroid
    /// has cosine similarity ≥ `config.cluster_threshold`; if no cluster matches, a new
    /// cluster is created. A cluster qualifies when both `min_occurrences` and
    /// `min_sessions` are satisfied.
    ///
    /// Messages without embeddings (`embedding == None`) are silently skipped.
    ///
    /// # Panics
    ///
    /// Does not panic in practice — the `unwrap` on `embedding` is guarded by the
    /// `filter(|p| p.embedding.is_some())` step immediately above.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Promotion`] if embeddings have inconsistent dimensions.
    #[tracing::instrument(name = "memory.compression.promote.scan", skip_all,
                          fields(window_len = window.len()))]
    pub async fn scan(
        &self,
        window: &[PromotionInput],
    ) -> Result<Vec<PromotionCandidate>, MemoryError> {
        // Filter to messages that have embeddings.
        let embeds: Vec<&PromotionInput> =
            window.iter().filter(|p| p.embedding.is_some()).collect();

        if embeds.is_empty() {
            return Ok(vec![]);
        }

        // Safety: filtered to `is_some()` above.
        let dim = embeds[0].embedding.as_ref().unwrap().len();

        // Greedy centroid clustering.
        struct Cluster {
            centroid: Vec<f32>,
            member_ids: Vec<MessageId>,
            session_ids: HashSet<ConversationId>,
        }

        let mut clusters: Vec<Cluster> = Vec::new();

        for input in &embeds {
            let emb = input.embedding.as_ref().unwrap();
            if emb.len() != dim {
                return Err(MemoryError::Promotion(format!(
                    "embedding dimension mismatch: expected {dim}, got {}",
                    emb.len()
                )));
            }

            // Find the first cluster within the similarity threshold.
            let mut assigned = false;
            for cluster in &mut clusters {
                let sim = cosine_similarity(emb, &cluster.centroid);
                if sim >= self.config.cluster_threshold {
                    // Update centroid (running average).
                    #[allow(clippy::cast_precision_loss)]
                    let n = cluster.member_ids.len() as f32;
                    for (c, v) in cluster.centroid.iter_mut().zip(emb.iter()) {
                        *c = (*c * n + v) / (n + 1.0);
                    }
                    cluster.member_ids.push(input.message_id);
                    cluster.session_ids.insert(input.conversation_id);
                    assigned = true;
                    break;
                }
            }
            if !assigned {
                clusters.push(Cluster {
                    centroid: emb.clone(),
                    member_ids: vec![input.message_id],
                    session_ids: std::iter::once(input.conversation_id).collect(),
                });
            }
        }

        // Filter clusters that meet both thresholds.
        let candidates = clusters
            .into_iter()
            .filter(|c| {
                u32::try_from(c.member_ids.len()).unwrap_or(u32::MAX) >= self.config.min_occurrences
                    && u32::try_from(c.session_ids.len()).unwrap_or(u32::MAX)
                        >= self.config.min_sessions
            })
            .map(|c| {
                let signature = cluster_signature(&c.centroid);
                PromotionCandidate {
                    signature,
                    member_ids: c.member_ids,
                    session_ids: c.session_ids.into_iter().collect(),
                    centroid: c.centroid,
                }
            })
            .collect();

        Ok(candidates)
    }

    /// Generate and persist a SKILL.md for `candidate`. Idempotent by signature.
    ///
    /// On evaluator rejection the method returns `Ok(())` — rejection is a normal outcome.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Promotion`] on generation, evaluator, or disk-write failure.
    #[tracing::instrument(name = "memory.compression.promote.persist", skip_all,
                          fields(signature = %candidate.signature))]
    pub async fn promote(&self, candidate: &PromotionCandidate) -> Result<(), MemoryError> {
        // Idempotency: skip if already exists.
        let skill_name = format!("promoted-pattern-{}", &candidate.signature[..12]);
        let skill_dir = self.output_dir.join(&skill_name);
        if skill_dir.exists() {
            tracing::debug!(signature = %candidate.signature, "promotion candidate already exists, skipping");
            return Ok(());
        }

        let member_count = candidate.member_ids.len();
        let session_count = candidate.session_ids.len();
        let description = format!(
            "Recurring procedural pattern detected across {member_count} messages in \
             {session_count} sessions. Generate a concise SKILL.md capturing the common \
             tool-use pattern or workflow. Signature: {}.",
            candidate.signature
        );

        self.writer
            .write_skill(description, candidate.signature.clone())
            .await
            .map_err(MemoryError::Promotion)
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

/// Compute cosine similarity between two equal-length vectors.
/// Returns `0.0` when either vector is zero-length or the norm is zero.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a < f32::EPSILON || norm_b < f32::EPSILON {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

/// Derive a stable signature from a centroid vector using SHA-256 hex.
fn cluster_signature(centroid: &[f32]) -> String {
    use std::hash::Hash;
    // Use a simple FNV-like hash of the quantised centroid to avoid
    // a heavy crypto dependency — this is a deduplication key, not a security hash.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for v in centroid {
        let bits = v.to_bits();
        bits.hash(&mut hasher);
    }
    let h = std::hash::Hasher::finish(&hasher);
    format!("{h:016x}")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingWriter {
        written: Mutex<Vec<String>>,
    }

    impl SkillWriter for RecordingWriter {
        fn write_skill(
            &self,
            description: String,
            _signature: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
        {
            self.written.lock().unwrap().push(description);
            Box::pin(async { Ok(()) })
        }
    }

    fn make_input(id: i64, cid: i64, content: &str, emb: Vec<f32>) -> PromotionInput {
        PromotionInput {
            message_id: MessageId(id),
            conversation_id: ConversationId(cid),
            content: content.to_string(),
            embedding: Some(emb),
        }
    }

    fn unit_vec(n: usize, val: f32) -> Vec<f32> {
        let mut v = vec![0.0_f32; n];
        v[0] = val;
        // Normalise to unit length.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn scan_returns_empty_for_no_embeddings() {
        let writer = Arc::new(RecordingWriter {
            written: Mutex::new(vec![]),
        });
        let engine =
            PromotionEngine::new(writer, PromotionConfig::default(), PathBuf::from("/tmp"));
        let window = vec![PromotionInput {
            message_id: MessageId(1),
            conversation_id: ConversationId(1),
            content: "hello".into(),
            embedding: None,
        }];
        let candidates = engine.scan(&window).await.unwrap();
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn scan_qualifies_cluster_meeting_thresholds() {
        let writer = Arc::new(RecordingWriter {
            written: Mutex::new(vec![]),
        });
        let config = PromotionConfig {
            min_occurrences: 3,
            min_sessions: 2,
            cluster_threshold: 0.90,
        };
        let engine = PromotionEngine::new(writer, config, PathBuf::from("/tmp"));

        // 4 nearly identical vectors from 3 distinct sessions.
        let base = unit_vec(4, 1.0);
        let window = vec![
            make_input(1, 1, "a", base.clone()),
            make_input(2, 1, "b", base.clone()),
            make_input(3, 2, "c", base.clone()),
            make_input(4, 3, "d", base.clone()),
        ];
        let candidates = engine.scan(&window).await.unwrap();
        assert_eq!(candidates.len(), 1, "expected 1 qualifying cluster");
        let c = &candidates[0];
        assert_eq!(c.member_ids.len(), 4);
        assert_eq!(c.session_ids.len(), 3);
    }

    #[tokio::test]
    async fn scan_rejects_cluster_below_min_sessions() {
        let writer = Arc::new(RecordingWriter {
            written: Mutex::new(vec![]),
        });
        let config = PromotionConfig {
            min_occurrences: 3,
            min_sessions: 2,
            cluster_threshold: 0.90,
        };
        let engine = PromotionEngine::new(writer, config, PathBuf::from("/tmp"));

        // 4 messages but all from the same session.
        let base = unit_vec(4, 1.0);
        let window = (1..=4)
            .map(|i| make_input(i, 1, "x", base.clone()))
            .collect::<Vec<_>>();
        let candidates = engine.scan(&window).await.unwrap();
        assert!(
            candidates.is_empty(),
            "should reject cluster with only 1 session"
        );
    }

    #[tokio::test]
    async fn scan_errors_on_dimension_mismatch() {
        let writer = Arc::new(RecordingWriter {
            written: Mutex::new(vec![]),
        });
        let engine =
            PromotionEngine::new(writer, PromotionConfig::default(), PathBuf::from("/tmp"));

        let window = vec![
            make_input(1, 1, "a", vec![1.0, 0.0, 0.0]),
            make_input(2, 2, "b", vec![0.0, 1.0]), // wrong dimension
        ];
        let result = engine.scan(&window).await;
        assert!(result.is_err(), "expected error on dimension mismatch");
    }
}
