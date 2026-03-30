// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! D-MEM RPE-based tiered graph extraction routing.
//!
//! Computes a heuristic "reward prediction error" (RPE) signal for each incoming turn.
//! Low-RPE turns (predictable, topically continuous, no new entities) skip the expensive
//! MAGMA LLM extraction pipeline. High-RPE turns proceed to full extraction.
//!
//! ## RPE formula
//!
//! ```text
//! RPE = 0.5 * (1 - max_cosine_similarity) + 0.5 * entity_novelty_ratio
//! ```
//!
//! Where:
//! - `max_cosine_similarity` = max cosine similarity between current turn embedding and last N
//!   turn embeddings. High = topically predictable.
//! - `entity_novelty_ratio` = fraction of candidate entity names not seen in recent history.
//!   0.0 if no entities extracted.
//!
//! ## Safety valve
//!
//! To prevent unbounded skipping, `consecutive_skips` is tracked. When it reaches
//! `max_skip_turns`, extraction is forced regardless of RPE score.

use std::collections::VecDeque;

use crate::graph::belief_revision::cosine_similarity;

/// Maximum number of recent turn embeddings to keep for context similarity computation.
pub const RPE_EMBEDDING_BUFFER_SIZE: usize = 10;

/// Number of recent entity names to keep in novelty history.
const ENTITY_HISTORY_SIZE: usize = 200;

/// RPE computation result for a single turn.
#[derive(Debug, Clone)]
pub struct RpeSignal {
    pub rpe_score: f32,
    pub context_similarity: f32,
    pub entity_novelty: f32,
    pub should_extract: bool,
}

/// Stateful RPE router. Tracks recent embeddings and entity history.
///
/// Protected by the caller's synchronization (typically held behind `Arc<Mutex<...>>`
/// at the `SemanticMemory` layer).
pub struct RpeRouter {
    recent_embeddings: VecDeque<Vec<f32>>,
    entity_history: VecDeque<String>,
    consecutive_skips: u32,
    /// RPE below this value → skip extraction. Range: `[0.0, 1.0]`.
    pub threshold: f32,
    /// Force extraction after this many consecutive skips. Default: 5.
    pub max_skip_turns: u32,
}

impl RpeRouter {
    #[must_use]
    pub fn new(threshold: f32, max_skip_turns: u32) -> Self {
        Self {
            recent_embeddings: VecDeque::with_capacity(RPE_EMBEDDING_BUFFER_SIZE),
            entity_history: VecDeque::with_capacity(ENTITY_HISTORY_SIZE),
            consecutive_skips: 0,
            threshold,
            max_skip_turns,
        }
    }

    /// Record a turn embedding. Called even when extraction is skipped, so context similarity
    /// remains up-to-date for the next turn.
    pub fn push_embedding(&mut self, embedding: Vec<f32>) {
        if self.recent_embeddings.len() >= RPE_EMBEDDING_BUFFER_SIZE {
            self.recent_embeddings.pop_front();
        }
        self.recent_embeddings.push_back(embedding);
    }

    /// Record entity names extracted (or candidate names from text) for novelty tracking.
    pub fn push_entities(&mut self, names: &[String]) {
        for name in names {
            if self.entity_history.len() >= ENTITY_HISTORY_SIZE {
                self.entity_history.pop_front();
            }
            self.entity_history.push_back(name.clone());
        }
    }

    /// Compute the RPE signal for the current turn.
    ///
    /// `turn_embedding` — embedding of the current message.
    /// `candidate_entities` — entity names extracted from the current message text (may be empty).
    ///
    /// Returns the RPE signal. When `recent_embeddings` is empty (cold start), returns
    /// `rpe_score = 1.0` and `should_extract = true`.
    #[must_use]
    pub fn compute(&mut self, turn_embedding: &[f32], candidate_entities: &[String]) -> RpeSignal {
        // Safety valve: force extraction after max_skip_turns consecutive skips.
        if self.consecutive_skips >= self.max_skip_turns {
            tracing::debug!(
                consecutive_skips = self.consecutive_skips,
                "D-MEM RPE: safety valve triggered, forcing extraction"
            );
            self.consecutive_skips = 0;
            return RpeSignal {
                rpe_score: 1.0,
                context_similarity: 0.0,
                entity_novelty: 1.0,
                should_extract: true,
            };
        }

        // Cold start: no history yet → always extract.
        if self.recent_embeddings.is_empty() {
            return RpeSignal {
                rpe_score: 1.0,
                context_similarity: 0.0,
                entity_novelty: 1.0,
                should_extract: true,
            };
        }

        // Context similarity: max cosine similarity to recent embeddings.
        let context_similarity = self
            .recent_embeddings
            .iter()
            .map(|emb| cosine_similarity(turn_embedding, emb))
            .fold(0.0f32, f32::max);

        // Entity novelty: fraction of candidate entities not in history.
        let entity_novelty = if candidate_entities.is_empty() {
            0.0
        } else {
            let novel = candidate_entities
                .iter()
                .filter(|e| !self.entity_history.contains(e))
                .count();
            #[allow(clippy::cast_precision_loss)]
            let ratio = novel as f32 / candidate_entities.len() as f32;
            ratio
        };

        let rpe_score = 0.5 * (1.0 - context_similarity) + 0.5 * entity_novelty;
        let should_extract = rpe_score >= self.threshold;

        if should_extract {
            self.consecutive_skips = 0;
        } else {
            self.consecutive_skips += 1;
            tracing::debug!(
                rpe_score,
                context_similarity,
                entity_novelty,
                consecutive_skips = self.consecutive_skips,
                "D-MEM RPE: low surprise, skipping graph extraction"
            );
        }

        RpeSignal {
            rpe_score,
            context_similarity,
            entity_novelty,
            should_extract,
        }
    }
}

// Lowercased known tech-domain terms that would be missed by capitalization heuristic.
const TECH_TERMS: &[&str] = &[
    "rust",
    "python",
    "go",
    "java",
    "kotlin",
    "swift",
    "ruby",
    "scala",
    "elixir",
    "haskell",
    "typescript",
    "javascript",
    "c",
    "c++",
    "cpp",
    "zig",
    "nim",
    "odin",
    "docker",
    "kubernetes",
    "k8s",
    "postgres",
    "sqlite",
    "redis",
    "kafka",
    "nginx",
    "linux",
    "macos",
    "windows",
    "android",
    "ios",
    "git",
    "cargo",
    "npm",
    "pip",
    "gradle",
    "cmake",
];

/// Extract candidate entity names from text using simple heuristics.
///
/// Captures capitalized tokens (length >= 3) that do NOT start the sentence.
/// Also captures lowercase technical terms known to be common entity types (programming
/// languages, tools). This is intentionally cheap — no LLM involved.
///
/// Returns lowercased names for comparison against stored canonical names.
#[must_use]
pub fn extract_candidate_entities(text: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();

    // Track sentence-start positions to avoid capturing "The", "This", etc.
    let mut sentence_starts: std::collections::HashSet<usize> = std::collections::HashSet::new();
    sentence_starts.insert(0);
    let mut prev_ends_sentence = true; // first word is always sentence-start
    for (idx, word) in words.iter().enumerate() {
        if prev_ends_sentence {
            sentence_starts.insert(idx);
        }
        prev_ends_sentence = word.ends_with('.') || word.ends_with('!') || word.ends_with('?');
    }

    // Collect capitalized non-sentence-start words >= 3 chars.
    for (idx, word) in words.iter().enumerate() {
        let clean: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if clean.len() < 3 || sentence_starts.contains(&idx) {
            continue;
        }
        // Skip pure-uppercase acronyms (API, HTTP, JSON).
        if clean.chars().all(char::is_uppercase) && clean.len() <= 5 {
            continue;
        }
        if clean.chars().next().is_some_and(char::is_uppercase) {
            candidates.push(clean.to_lowercase());
        }
    }

    // Add tech-domain terms found in the text (case-insensitive, word-boundary check).
    let text_lower = text.to_lowercase();
    for term in TECH_TERMS {
        let mut start = 0;
        while let Some(pos) = text_lower[start..].find(term) {
            let abs_pos = start + pos;
            let before_ok = abs_pos == 0
                || text_lower
                    .as_bytes()
                    .get(abs_pos - 1)
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_');
            let after_ok = {
                let end = abs_pos + term.len();
                end >= text_lower.len()
                    || text_lower
                        .as_bytes()
                        .get(end)
                        .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_')
            };
            if before_ok && after_ok {
                let t = (*term).to_string();
                if !candidates.contains(&t) {
                    candidates.push(t);
                }
            }
            start = abs_pos + 1;
        }
    }

    // Deduplicate preserving order.
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|c| seen.insert(c.clone()));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_embedding(val: f32, len: usize) -> Vec<f32> {
        vec![val; len]
    }

    #[test]
    fn rpe_cold_start_returns_one() {
        let mut router = RpeRouter::new(0.3, 5);
        let emb = make_embedding(0.5, 4);
        let signal = router.compute(&emb, &[]);
        assert!(signal.should_extract);
        assert!((signal.rpe_score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rpe_high_similarity_low_novelty_skips() {
        let mut router = RpeRouter::new(0.3, 5);
        let emb = make_embedding(1.0, 4);
        // Seed history with identical embedding.
        router.push_embedding(emb.clone());
        router.push_entities(&["rust".to_string()]);

        // Turn with same embedding and known entity → RPE near 0.
        let signal = router.compute(&emb, &["rust".to_string()]);
        // context_similarity = 1.0, entity_novelty = 0.0 → RPE = 0.0
        assert!(!signal.should_extract, "low-RPE turn should be skipped");
        assert!(signal.rpe_score < 0.3);
    }

    #[test]
    fn rpe_low_similarity_high_novelty_extracts() {
        let mut router = RpeRouter::new(0.3, 5);
        let prev = vec![1.0f32, 0.0, 0.0, 0.0];
        router.push_embedding(prev);

        // Orthogonal embedding + all-new entities.
        let curr = vec![0.0f32, 1.0, 0.0, 0.0];
        let signal = router.compute(&curr, &["NewFramework".to_string()]);
        // context_similarity = 0.0, entity_novelty = 1.0 → RPE = 1.0
        assert!(signal.should_extract);
        assert!((signal.rpe_score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rpe_max_skip_turns_forces_extraction() {
        let mut router = RpeRouter::new(0.3, 3);
        let emb = make_embedding(1.0, 4);
        router.push_embedding(emb.clone());
        router.push_entities(&["rust".to_string()]);

        // Force 3 skips.
        router.consecutive_skips = 3;
        let signal = router.compute(&emb, &["rust".to_string()]);
        assert!(signal.should_extract, "safety valve must force extraction");
        assert_eq!(
            router.consecutive_skips, 0,
            "counter must reset after safety valve"
        );
    }

    #[test]
    fn rpe_consecutive_skips_increments() {
        let mut router = RpeRouter::new(0.9, 10); // high threshold → easy to skip
        let emb = make_embedding(1.0, 4);
        router.push_embedding(emb.clone());
        router.push_entities(&["rust".to_string()]);

        let s = router.compute(&emb, &["rust".to_string()]);
        if !s.should_extract {
            assert_eq!(router.consecutive_skips, 1);
        }
    }

    #[test]
    fn extract_candidate_entities_captures_capitalized() {
        let text = "I use Tokio and Axum for async web development.";
        let entities = extract_candidate_entities(text);
        // "Tokio" and "Axum" are mid-sentence capitalized.
        assert!(
            entities.contains(&"tokio".to_string()),
            "expected tokio, got {entities:?}"
        );
        assert!(
            entities.contains(&"axum".to_string()),
            "expected axum, got {entities:?}"
        );
    }

    #[test]
    fn extract_candidate_entities_captures_tech_terms() {
        let text = "I write code in rust and use docker for deployment.";
        let entities = extract_candidate_entities(text);
        assert!(
            entities.contains(&"rust".to_string()),
            "expected rust, got {entities:?}"
        );
        assert!(
            entities.contains(&"docker".to_string()),
            "expected docker, got {entities:?}"
        );
    }

    #[test]
    fn extract_candidate_entities_ignores_sentence_start() {
        let text = "The project uses Rust. The team is growing.";
        let entities = extract_candidate_entities(text);
        // "The" appears at sentence start and should not be captured.
        assert!(!entities.contains(&"the".to_string()));
    }

    #[test]
    fn extract_candidate_entities_no_duplicates() {
        let text = "I use rust and I love rust and rust is great.";
        let entities = extract_candidate_entities(text);
        let count = entities.iter().filter(|e| e.as_str() == "rust").count();
        assert_eq!(
            count, 1,
            "rust should appear exactly once, got {entities:?}"
        );
    }
}
