// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Async embedding-based skill matcher with optional two-stage category filtering.
//!
//! [`SkillMatcher`] pre-computes embeddings for all skill descriptions at construction
//! time, then ranks candidates by cosine similarity for each user query.
//!
//! # Two-Stage Matching
//!
//! When skills are organised into categories (`category` frontmatter field) and at least two
//! categories each contain two or more skills, the matcher builds a `CategoryMatcher` that
//! first narrows the candidate pool to the two closest categories before performing fine-grained
//! per-skill scoring. This keeps matching sub-linear as the skill library grows.
//!
//! Stage 1 (optional) — select top-2 categories by centroid cosine similarity.
//! Stage 2 — score all candidates in the selected categories + uncategorized skills.
//!
//! # Confusability Analysis
//!
//! [`SkillMatcher::confusability_report`] performs an O(n²) pairwise similarity scan and
//! reports skill pairs whose cosine similarity exceeds a configurable threshold. Use this
//! during CI or after adding skills to detect ambiguous overlaps.
//!
//! # Examples
//!
//! ```rust,no_run
//! use zeph_skills::matcher::{MatchResult, SkillMatcher, ScoredMatch};
//! use zeph_skills::loader::SkillMeta;
//!
//! async fn example(skills: &[&SkillMeta]) {
//!     let embed_fn = |_text: &str| -> zeph_skills::matcher::EmbedFuture {
//!         Box::pin(async { Ok(vec![0.0f32; 768]) })
//!     };
//!
//!     if let Some(matcher) = SkillMatcher::new(skills, embed_fn).await {
//!         let result = matcher.match_skills(skills.len(), "search the web", 3, true, embed_fn).await;
//!         if let MatchResult::Scored(matches) = result {
//!             for m in &matches {
//!                 println!("skill index {} score {:.3}", m.index, m.score);
//!             }
//!         }
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::embedding::SkillEmbedding;
use crate::error::SkillError;
use crate::loader::SkillMeta;
use futures::stream::{self, StreamExt};

pub use zeph_llm::provider::EmbedFuture;

/// A skill candidate with its position in the original skill slice and cosine similarity score.
///
/// `index` refers to the position in the `&[&SkillMeta]` slice passed to [`SkillMatcher::new`].
#[derive(Debug, Clone)]
pub struct ScoredMatch {
    /// Index into the skill slice originally passed to [`SkillMatcher::new`].
    pub index: usize,
    /// Cosine similarity score in the range `[-1.0, 1.0]`.
    pub score: f32,
}

/// Result of a skill matching attempt, distinguishing infrastructure failures from scored results.
///
/// This type allows callers to apply different policies depending on the outcome:
/// - [`MatchResult::InfraError`] — embedding or backend unavailable; caller may apply a
///   degraded-mode fallback (e.g. inject all skills so the agent remains functional).
/// - [`MatchResult::Scored`] — matcher produced candidates (the vec may be empty when no skills
///   are loaded or all fall below the caller's threshold); the `min_injection_score` gate must be
///   respected.
#[non_exhaustive]
#[must_use]
#[derive(Debug)]
pub enum MatchResult {
    /// Infrastructure failure — embedding call timed out or backend returned an error.
    /// Score information is unavailable; apply degraded-mode fallback.
    InfraError,
    /// Matcher ran successfully and produced scored candidates.
    /// The inner `Vec` may be empty (no skills loaded or none scored above any threshold).
    Scored(Vec<ScoredMatch>),
}

/// LLM-produced structured classification of a user query into a skill name with confidence.
///
/// Used when the agent routes via a classification prompt rather than pure embedding similarity.
/// Deserialized from the LLM's JSON response.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IntentClassification {
    /// Name of the matched skill (from the `name` frontmatter field).
    pub skill_name: String,
    /// Confidence level in `[0.0, 1.0]` as reported by the LLM.
    pub confidence: f32,
    /// Optional extracted parameters (slot-filling), keyed by parameter name.
    #[serde(default)]
    pub params: HashMap<String, String>,
}

/// A pair of skills with similar embeddings.
#[derive(Debug, Clone)]
pub struct ConfusabilityPair {
    /// Name of the first skill in the pair.
    pub skill_a: String,
    /// Name of the second skill in the pair.
    pub skill_b: String,
    /// Cosine similarity between the two skill description embeddings.
    pub similarity: f32,
}

/// Report of all skill pairs whose cosine similarity exceeds a threshold.
#[derive(Debug, Clone)]
pub struct ConfusabilityReport {
    /// Pairs sorted descending by similarity.
    pub pairs: Vec<ConfusabilityPair>,
    /// The threshold used to filter pairs.
    pub threshold: f32,
    /// Skills excluded from the report because their embedding failed.
    pub excluded_skills: Vec<String>,
}

impl fmt::Display for ConfusabilityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.pairs.is_empty() {
            write!(
                f,
                "No confusable skill pairs found above {:.2}.",
                self.threshold
            )?;
        } else {
            writeln!(
                f,
                "Confusability report (threshold: {:.2}):",
                self.threshold
            )?;
            for pair in &self.pairs {
                writeln!(
                    f,
                    "  {} \u{2194} {}: {:.3}",
                    pair.skill_a, pair.skill_b, pair.similarity
                )?;
            }
        }
        if !self.excluded_skills.is_empty() {
            write!(
                f,
                "\nNote: {} skill(s) excluded (embedding unavailable): {}",
                self.excluded_skills.len(),
                self.excluded_skills.join(", ")
            )?;
        }
        Ok(())
    }
}

/// Category-aware index for two-stage skill matching.
///
/// Categories with fewer than 2 embedded skills are treated as uncategorized
/// (their skills always enter Stage 2 directly) to prevent Stage 1 from
/// accidentally excluding singleton-category skills.
#[derive(Debug, Clone)]
struct CategoryMatcher {
    /// Category name → embedding positions (index into `SkillMatcher::embeddings`).
    /// Only categories with ≥ 2 embedded skills are stored here.
    categories: HashMap<String, Vec<usize>>,
    /// Centroid embedding per category.
    centroids: HashMap<String, SkillEmbedding>,
    /// Embedding positions for uncategorized skills or singleton-category skills.
    uncategorized: Vec<usize>,
}

impl CategoryMatcher {
    /// Build from completed embeddings. `skills` is the original skill slice passed to
    /// `SkillMatcher::new`; `embeddings` is the successful subset.
    fn build(skills: &[&SkillMeta], embeddings: &[(usize, SkillEmbedding)]) -> Self {
        // Group embedding positions by category.
        let mut by_category: HashMap<String, Vec<usize>> = HashMap::new();
        let mut uncategorized: Vec<usize> = Vec::new();

        for (pos, (skill_idx, _)) in embeddings.iter().enumerate() {
            match skills[*skill_idx].category.as_deref() {
                Some(cat) => by_category.entry(cat.to_string()).or_default().push(pos),
                None => uncategorized.push(pos),
            }
        }

        // Promote singleton categories to uncategorized.
        let mut categories: HashMap<String, Vec<usize>> = HashMap::new();
        for (cat, positions) in by_category {
            if positions.len() >= 2 {
                categories.insert(cat, positions);
            } else {
                uncategorized.extend(positions);
            }
        }

        // Compute centroids for multi-skill categories.
        let mut centroids: HashMap<String, SkillEmbedding> = HashMap::new();
        for (cat, positions) in &categories {
            let dim = embeddings[positions[0]].1.dim();
            let mut centroid = vec![0.0f32; dim];
            for &pos in positions {
                for (c, v) in centroid.iter_mut().zip(embeddings[pos].1.as_ref()) {
                    *c += v;
                }
            }
            #[allow(clippy::cast_precision_loss)]
            let n = positions.len() as f32;
            for c in &mut centroid {
                *c /= n;
            }
            centroids.insert(cat.clone(), SkillEmbedding::from_raw(centroid));
        }

        Self {
            categories,
            centroids,
            uncategorized,
        }
    }

    /// Whether two-stage matching is useful (≥2 categories with ≥2 skills each).
    fn is_useful(&self) -> bool {
        self.categories.len() >= 2
    }

    /// Return embedding positions in the Stage 2 candidate pool for the given query.
    /// Selects top-2 categories by centroid cosine similarity, plus all uncategorized.
    fn candidate_positions(&self, query_vec: &[f32]) -> Vec<usize> {
        // Score categories by centroid similarity.
        let mut cat_scores: Vec<(&str, f32)> = self
            .centroids
            .iter()
            .map(|(cat, centroid)| {
                (
                    cat.as_str(),
                    cosine_similarity(query_vec, centroid.as_ref()),
                )
            })
            .collect();
        cat_scores
            .sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut positions: Vec<usize> = self.uncategorized.clone();
        for (cat, _) in cat_scores.iter().take(2) {
            if let Some(cat_positions) = self.categories.get(*cat) {
                positions.extend_from_slice(cat_positions);
            }
        }
        positions
    }
}

/// Maximum number of trigger embeddings across all skills.
///
/// Prevents a pathological skill library from stalling startup with thousands of embed calls.
const MAX_TOTAL_TRIGGER_EMBEDDINGS: usize = 500;

#[derive(Debug, Clone)]
pub struct SkillMatcher {
    embeddings: Vec<(usize, SkillEmbedding)>,
    /// Trigger embeddings grouped by skill index for O(1) lookup during matching.
    /// Key: skill index (matches `embeddings[i].0`). Value: all trigger phrase embeddings for that skill.
    /// Empty when no skills define triggers.
    trigger_embeddings: HashMap<usize, Vec<SkillEmbedding>>,
    /// Populated when at least 2 multi-skill categories exist.
    category_matcher: Option<CategoryMatcher>,
}

impl SkillMatcher {
    /// Create a matcher by pre-computing embeddings for all skill descriptions and trigger phrases.
    ///
    /// Returns `None` if all description embeddings fail (caller should fall back to all skills).
    #[allow(clippy::too_many_lines)]
    pub async fn new<F>(skills: &[&SkillMeta], embed_fn: F) -> Option<Self>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        type EmbedOutcome = (usize, String, Result<Vec<f32>, Option<zeph_llm::LlmError>>);
        type TriggerOutcome = (usize, Result<Vec<f32>, ()>);

        // Collect raw results without logging per-skill; errors will be summarized below.
        let raw: Vec<EmbedOutcome> = stream::iter(skills.iter().enumerate())
            .map(|(i, skill)| {
                let fut = embed_fn(&skill.description);
                let name = skill.name.clone();
                async move {
                    let result = match tokio::time::timeout(Duration::from_secs(10), fut).await {
                        Ok(Ok(vec)) => Ok(vec),
                        Ok(Err(e)) => Err(Some(e)),
                        Err(_) => Err(None),
                    };
                    (i, name, result)
                }
            })
            .buffer_unordered(20)
            .collect()
            .await;

        let mut embeddings = Vec::new();
        // Captures the provider name from any EmbedUnsupported error; last-wins is fine
        // because all unsupported errors for a given provider share the same string.
        let mut unsupported_provider: Option<String> = None;
        let mut unsupported_count: usize = 0;

        for (i, name, result) in raw {
            match result {
                Ok(vec) => embeddings.push((i, SkillEmbedding::from_raw(vec))),
                Err(Some(zeph_llm::LlmError::EmbedUnsupported { provider })) => {
                    unsupported_provider = Some(provider);
                    unsupported_count += 1;
                }
                Err(None) => {
                    tracing::warn!("embedding timed out for skill '{name}'");
                }
                Err(Some(e)) => {
                    tracing::warn!("failed to embed skill '{name}': {e:#}");
                }
            }
        }

        if unsupported_count > 0
            && let Some(provider) = unsupported_provider
        {
            tracing::info!(
                "skill embeddings skipped: embedding not supported by {provider} \
                 ({unsupported_count} skills affected)"
            );
        }

        if embeddings.is_empty() {
            return None;
        }

        let category_matcher = {
            let cm = CategoryMatcher::build(skills, &embeddings);
            if cm.is_useful() { Some(cm) } else { None }
        };

        // Build trigger embeddings up to the global cap.
        // Each entry is (skill_index, phrase, embed_future); we flatten all skill triggers first.
        let trigger_tasks: Vec<(usize, String)> = {
            let mut tasks = Vec::new();
            let mut total = 0usize;
            'outer: for (skill_idx, skill) in skills.iter().enumerate() {
                for phrase in &skill.triggers {
                    if total >= MAX_TOTAL_TRIGGER_EMBEDDINGS {
                        tracing::warn!(
                            "trigger embedding cap ({MAX_TOTAL_TRIGGER_EMBEDDINGS}) reached; \
                             remaining trigger phrases skipped"
                        );
                        break 'outer;
                    }
                    tasks.push((skill_idx, phrase.clone()));
                    total += 1;
                }
            }
            tasks
        };

        let trigger_raw: Vec<TriggerOutcome> = stream::iter(trigger_tasks)
            .map(|(skill_idx, phrase)| {
                let fut = embed_fn(&phrase);
                async move {
                    let result = match tokio::time::timeout(Duration::from_secs(10), fut).await {
                        Ok(Ok(vec)) => Ok(vec),
                        Ok(Err(e)) => {
                            tracing::debug!("trigger embed failed for phrase: {e:#}");
                            Err(())
                        }
                        Err(_) => {
                            tracing::debug!("trigger embed timed out");
                            Err(())
                        }
                    };
                    (skill_idx, result)
                }
            })
            .buffer_unordered(20)
            .collect()
            .await;

        let mut trigger_embeddings: HashMap<usize, Vec<SkillEmbedding>> = HashMap::new();
        for (skill_idx, result) in trigger_raw {
            if let Ok(vec) = result {
                trigger_embeddings
                    .entry(skill_idx)
                    .or_default()
                    .push(SkillEmbedding::from_raw(vec));
            }
        }

        Some(Self {
            embeddings,
            trigger_embeddings,
            category_matcher,
        })
    }

    /// Return the embedding vector for skill at the given index, if available.
    #[must_use]
    pub fn skill_embedding(&self, skill_index: usize) -> Option<&[f32]> {
        self.embeddings
            .iter()
            .find(|(idx, _)| *idx == skill_index)
            .map(|(_, v)| v.as_ref())
    }

    /// Match a user query against stored skill embeddings, returning the top-K scored matches
    /// ranked by cosine similarity.
    ///
    /// When `two_stage` is true and a `CategoryMatcher` is available, uses category-first
    /// filtering before fine-grained matching. Falls back to flat matching otherwise.
    ///
    /// Returns [`MatchResult::InfraError`] if the query embedding fails or times out.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "skill.match", skip_all, fields(query_len = %query.len(), candidates = tracing::field::Empty, top_score = tracing::field::Empty))
    )]
    pub async fn match_skills<F>(
        &self,
        count: usize,
        query: &str,
        limit: usize,
        two_stage: bool,
        embed_fn: F,
    ) -> MatchResult
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let _ = count; // total skill count, unused for in-memory matcher
        let query_vec = match tokio::time::timeout(Duration::from_secs(10), embed_fn(query)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!("failed to embed query: {e:#}");
                return MatchResult::InfraError;
            }
            Err(_) => {
                tracing::warn!("embedding timed out for query");
                return MatchResult::InfraError;
            }
        };

        // Two-stage: restrict candidate pool to top-2 categories + uncategorized.
        let candidate_positions: Option<Vec<usize>> = if two_stage {
            self.category_matcher
                .as_ref()
                .map(|cm| cm.candidate_positions(&query_vec))
        } else {
            None
        };

        let mut scored: Vec<ScoredMatch> = match candidate_positions {
            Some(positions) => positions
                .iter()
                .map(|&pos| ScoredMatch {
                    index: self.embeddings[pos].0,
                    score: cosine_similarity(&query_vec, self.embeddings[pos].1.as_ref()),
                })
                .collect(),
            None => self
                .embeddings
                .iter()
                .map(|(idx, emb)| ScoredMatch {
                    index: *idx,
                    score: cosine_similarity(&query_vec, emb.as_ref()),
                })
                .collect(),
        };

        // A5: merge best trigger score into description score via max().
        if !self.trigger_embeddings.is_empty() {
            for m in &mut scored {
                if let Some(triggers) = self.trigger_embeddings.get(&m.index) {
                    let best_trigger = triggers
                        .iter()
                        .map(|emb| cosine_similarity(&query_vec, emb.as_ref()))
                        .fold(f32::NEG_INFINITY, f32::max);

                    if best_trigger > f32::NEG_INFINITY {
                        if best_trigger > m.score + 0.1 {
                            tracing::trace!(
                                skill_index = m.index,
                                trigger_score = best_trigger,
                                description_score = m.score,
                                "trigger score dominates description score"
                            );
                        }
                        m.score = m.score.max(best_trigger);
                    }
                }
            }
        }

        scored.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);

        MatchResult::Scored(scored)
    }

    /// Compute pairwise cosine similarity for all skill pairs with successful embeddings.
    ///
    /// Returns pairs where similarity ≥ `threshold`, sorted descending. Skills that failed
    /// to embed are listed in [`ConfusabilityReport::excluded_skills`].
    ///
    /// This is an O(n²) operation. For large skill libraries, call from a blocking context.
    #[must_use]
    pub fn confusability_report(
        &self,
        skills: &[&SkillMeta],
        threshold: f32,
    ) -> ConfusabilityReport {
        let embedded_indices: std::collections::HashSet<usize> =
            self.embeddings.iter().map(|(i, _)| *i).collect();
        let excluded_skills: Vec<String> = skills
            .iter()
            .enumerate()
            .filter(|(i, _)| !embedded_indices.contains(i))
            .map(|(_, m)| m.name.clone())
            .collect();

        let mut pairs = Vec::new();
        for i in 0..self.embeddings.len() {
            for j in (i + 1)..self.embeddings.len() {
                let sim =
                    cosine_similarity(self.embeddings[i].1.as_ref(), self.embeddings[j].1.as_ref());
                if sim >= threshold {
                    pairs.push(ConfusabilityPair {
                        skill_a: skills[self.embeddings[i].0].name.clone(),
                        skill_b: skills[self.embeddings[j].0].name.clone(),
                        similarity: sim,
                    });
                }
            }
        }
        pairs.sort_unstable_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ConfusabilityReport {
            pairs,
            threshold,
            excluded_skills,
        }
    }
}

/// Backend selection for the skill embedding matcher.
///
/// `InMemory` uses a pre-computed in-process embedding index; `Qdrant` delegates to a
/// remote Qdrant vector store and requires the `qdrant` feature to be enabled.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum SkillMatcherBackend {
    /// In-process embedding index built at startup from skill descriptions.
    InMemory(SkillMatcher),
    /// Qdrant-backed vector store for large skill libraries (requires `qdrant` feature).
    #[cfg(feature = "qdrant")]
    Qdrant(crate::qdrant_matcher::QdrantSkillMatcher),
}

impl SkillMatcherBackend {
    /// Return the embedding vector for a skill at the given index, if available.
    ///
    /// For [`Self::InMemory`], this is the pre-computed description embedding. For
    /// [`Self::Qdrant`], this is served from a per-turn cache populated by
    /// [`Self::refresh_skill_embeddings`] — `None` if that hasn't been called yet,
    /// `skill_index` wasn't among its candidates, or the fetch failed or returned a partial
    /// result (see issue #5786).
    #[must_use]
    pub fn skill_embedding(&self, skill_index: usize) -> Option<Vec<f32>> {
        match self {
            Self::InMemory(m) => m.skill_embedding(skill_index).map(<[f32]>::to_vec),
            #[cfg(feature = "qdrant")]
            Self::Qdrant(m) => m.skill_embedding(skill_index),
        }
    }

    /// Returns `true` if this backend is a Qdrant vector store.
    #[must_use]
    pub fn is_qdrant(&self) -> bool {
        match self {
            Self::InMemory(_) => false,
            #[cfg(feature = "qdrant")]
            Self::Qdrant(_) => true,
        }
    }

    /// Populate per-skill vectors for `scored` so a subsequent [`Self::skill_embedding`] call
    /// can serve them.
    ///
    /// Callers must pass the **final** candidate set — after any BM25 hybrid-search fusion, if
    /// enabled — since fusion can introduce skill indices outside the original vector-search
    /// top-K that [`Self::match_skills`] alone would have cached.
    ///
    /// No-op for [`Self::InMemory`], whose embeddings are already resident in memory. For
    /// [`Self::Qdrant`], this issues a bounded `get_points` round-trip scoped to `scored` — only
    /// call it when a consumer of [`Self::skill_embedding`] is actually enabled (RL rerank
    /// and/or `GoSkills` grouping), so turns using neither feature don't pay for the extra
    /// Qdrant round-trip (see issue #5786).
    #[cfg_attr(
        not(feature = "qdrant"),
        allow(
            unused_variables,
            clippy::unused_async,
            clippy::unused_async_trait_impl
        )
    )]
    pub async fn refresh_skill_embeddings(&self, meta: &[&SkillMeta], scored: &[ScoredMatch]) {
        match self {
            Self::InMemory(_) => {}
            #[cfg(feature = "qdrant")]
            Self::Qdrant(m) => m.refresh_vector_cache(meta, scored).await,
        }
    }

    /// Match skills by embedding similarity for the given `query`.
    ///
    /// Dispatches to the underlying backend (in-memory or Qdrant). Returns up to `limit`
    /// candidates sorted by descending cosine similarity.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "skill.match", skip_all, fields(query_len = %query.len(), candidates = tracing::field::Empty, top_score = tracing::field::Empty))
    )]
    pub async fn match_skills<F>(
        &self,
        meta: &[&SkillMeta],
        query: &str,
        limit: usize,
        two_stage: bool,
        embed_fn: F,
    ) -> MatchResult
    where
        F: Fn(&str) -> EmbedFuture,
    {
        match self {
            Self::InMemory(m) => {
                m.match_skills(meta.len(), query, limit, two_stage, embed_fn)
                    .await
            }
            #[cfg(feature = "qdrant")]
            Self::Qdrant(m) => m.match_skills(meta, query, limit, embed_fn).await,
        }
    }

    /// Compute the confusability report for the in-memory matcher.
    ///
    /// Offloads the O(n²) computation to a blocking thread pool to avoid stalling the
    /// async runtime. Returns an empty report for the Qdrant backend.
    pub async fn confusability_report(
        &self,
        meta: &[&SkillMeta],
        threshold: f32,
    ) -> ConfusabilityReport {
        match self {
            Self::InMemory(m) => {
                let matcher = m.clone();
                let meta_owned: Vec<crate::loader::SkillMeta> =
                    meta.iter().map(|m| (*m).clone()).collect();
                tokio::task::spawn_blocking(move || {
                    let refs: Vec<&SkillMeta> = meta_owned.iter().collect();
                    matcher.confusability_report(&refs, threshold)
                })
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("confusability_report task panicked: {e}");
                    ConfusabilityReport {
                        pairs: vec![],
                        threshold,
                        excluded_skills: vec![],
                    }
                })
            }
            #[cfg(feature = "qdrant")]
            Self::Qdrant(_) => ConfusabilityReport {
                pairs: vec![],
                threshold,
                excluded_skills: vec![],
            },
        }
    }

    /// Sync skill embeddings. Only performs work for the Qdrant variant.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant sync fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "skill.matcher_sync", skip_all)
    )]
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn sync<F>(
        &mut self,
        meta: &[&SkillMeta],
        embedding_model: &str,
        embed_fn: F,
        on_progress: Option<Box<dyn Fn(usize, usize) + Send>>,
    ) -> Result<(), SkillError>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        match self {
            Self::InMemory(_) => {
                let _ = (meta, embedding_model, &embed_fn, on_progress);
                Ok(())
            }
            #[cfg(feature = "qdrant")]
            Self::Qdrant(m) => {
                m.sync(meta, embedding_model, embed_fn, on_progress).await?;
                Ok(())
            }
        }
    }
}

pub use zeph_common::math::cosine_similarity;

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn make_meta(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: description.into(),
            ..Default::default()
        }
    }

    fn make_meta_with_category(name: &str, description: &str, category: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: description.into(),
            category: Some(category.into()),
            ..Default::default()
        }
    }

    fn scored(r: MatchResult) -> Vec<ScoredMatch> {
        match r {
            MatchResult::Scored(v) => v,
            MatchResult::InfraError => panic!("expected Scored, got InfraError"),
        }
    }

    fn embed_fn_mapping(text: &str) -> EmbedFuture {
        let vec = match text {
            "alpha" => vec![1.0, 0.0, 0.0],
            "beta" => vec![0.0, 1.0, 0.0],
            "gamma" => vec![0.0, 0.0, 1.0],
            "query" => vec![0.9, 0.1, 0.0],
            _ => vec![0.0, 0.0, 0.0],
        };
        Box::pin(async move { Ok(vec) })
    }

    fn embed_fn_constant(text: &str) -> EmbedFuture {
        let _ = text;
        Box::pin(async { Ok(vec![1.0, 0.0]) })
    }

    fn embed_fn_fail(text: &str) -> EmbedFuture {
        let _ = text;
        Box::pin(async { Err(zeph_llm::LlmError::Other("error".into())) })
    }

    #[tokio::test]
    async fn test_match_skills_returns_top_k() {
        let metas = [
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let match_results = scored(
            skill_matcher
                .match_skills(refs.len(), "query", 2, false, embed_fn_mapping)
                .await,
        );

        assert_eq!(match_results.len(), 2);
        assert_eq!(match_results[0].index, 0); // "a" / "alpha"
        assert_eq!(match_results[1].index, 1); // "b" / "beta"
        assert!(match_results[0].score >= match_results[1].score);
    }

    #[tokio::test]
    async fn test_match_skills_empty_skills() {
        let refs: Vec<&SkillMeta> = Vec::new();
        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    async fn test_match_skills_single_skill() {
        let metas = [make_meta("only", "the only skill")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let match_results = scored(
            skill_matcher
                .match_skills(refs.len(), "query", 5, false, embed_fn_constant)
                .await,
        );

        assert_eq!(match_results.len(), 1);
        assert_eq!(match_results[0].index, 0);
    }

    #[tokio::test]
    async fn test_matcher_new_returns_none_on_failure() {
        let metas = [make_meta("fail", "will fail")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_fail).await;
        assert!(matcher.is_none());
    }

    fn embed_fn_unsupported(text: &str) -> EmbedFuture {
        let _ = text;
        Box::pin(async {
            Err(zeph_llm::LlmError::EmbedUnsupported {
                provider: "claude".into(),
            })
        })
    }

    #[tokio::test]
    async fn test_matcher_new_returns_none_when_all_unsupported() {
        let metas = [
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        // All embeddings fail with EmbedUnsupported — matcher must return None
        // and must not produce 3 individual warnings (only 1 summary).
        let matcher = SkillMatcher::new(&refs, embed_fn_unsupported).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_unsupported_emits_single_info_not_per_skill() {
        let metas = [
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let _ = SkillMatcher::new(&refs, embed_fn_unsupported).await;

        // Summary log must be present from the correct module.
        assert!(logs_contain(
            "zeph_skills::matcher: skill embeddings skipped"
        ));
        // Must be INFO level, not WARN — prevents regression to warn!.
        assert!(!logs_contain(
            "WARN zeph_skills::matcher: skill embeddings skipped"
        ));
        // Per-skill EmbedUnsupported must NOT be logged individually (the fix for #1387).
        assert!(!logs_contain("failed to embed skill"));
    }

    #[tokio::test]
    async fn test_matcher_new_partial_unsupported_falls_back_to_supported() {
        let metas = [make_meta("good", "alpha"), make_meta("bad", "bad skill")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let embed_fn = |text: &str| -> EmbedFuture {
            if text == "alpha" {
                Box::pin(async { Ok(vec![1.0, 0.0]) })
            } else {
                Box::pin(async {
                    Err(zeph_llm::LlmError::EmbedUnsupported {
                        provider: "claude".into(),
                    })
                })
            }
        };

        let matcher = SkillMatcher::new(&refs, embed_fn).await.unwrap();
        assert_eq!(matcher.embeddings.len(), 1);
        assert_eq!(matcher.embeddings[0].0, 0);
    }

    #[tokio::test]
    async fn test_matcher_skips_failed_embeddings() {
        let metas = [
            make_meta("good", "good skill"),
            make_meta("bad", "bad skill"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let embed_fn = |text: &str| -> EmbedFuture {
            if text == "bad skill" {
                Box::pin(async { Err(zeph_llm::LlmError::Other("embed failed".into())) })
            } else {
                Box::pin(async { Ok(vec![1.0, 0.0]) })
            }
        };

        let matcher = SkillMatcher::new(&refs, embed_fn).await.unwrap();
        assert_eq!(matcher.embeddings.len(), 1);
        assert_eq!(matcher.embeddings[0].0, 0);
    }

    #[tokio::test]
    async fn test_match_skills_returns_all_when_k_larger() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let match_results = scored(
            skill_matcher
                .match_skills(refs.len(), "query", 100, false, embed_fn_constant)
                .await,
        );

        assert_eq!(match_results.len(), 2);
    }

    #[tokio::test]
    async fn test_match_skills_query_embed_fails() {
        let metas = [make_meta("a", "alpha")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let result = skill_matcher
            .match_skills(refs.len(), "query", 5, false, embed_fn_fail)
            .await;

        assert_matches!(result, MatchResult::InfraError);
    }

    #[test]
    fn cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_empty_vectors() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_both_zero() {
        let a = vec![0.0, 0.0];
        let b = vec![0.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_parallel() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![2.0, 4.0, 6.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn match_skills_limit_zero() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let match_results = scored(
            skill_matcher
                .match_skills(refs.len(), "query", 0, false, embed_fn_constant)
                .await,
        );

        assert!(match_results.is_empty());
    }

    #[tokio::test]
    async fn match_skills_preserves_ranking() {
        let metas = [
            make_meta("far", "gamma"),
            make_meta("close", "alpha"),
            make_meta("mid", "beta"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let match_results = scored(
            skill_matcher
                .match_skills(refs.len(), "query", 3, false, embed_fn_mapping)
                .await,
        );

        assert_eq!(match_results.len(), 3);
        assert_eq!(match_results[0].index, 1); // "close" / "alpha" is closest to "query"
    }

    #[test]
    fn matcher_backend_in_memory_is_not_qdrant() {
        let matcher = SkillMatcher {
            embeddings: vec![(0, SkillEmbedding::from_raw(vec![1.0, 0.0]))],
            trigger_embeddings: HashMap::new(),
            category_matcher: None,
        };
        let backend = SkillMatcherBackend::InMemory(matcher);
        assert!(!backend.is_qdrant());
    }

    #[tokio::test]
    async fn backend_in_memory_sync_is_noop() {
        let matcher = SkillMatcher {
            embeddings: vec![],
            trigger_embeddings: HashMap::new(),
            category_matcher: None,
        };
        let mut backend = SkillMatcherBackend::InMemory(matcher);
        let metas: Vec<&SkillMeta> = vec![];
        let result = backend.sync(&metas, "model", embed_fn_constant, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn backend_in_memory_match_skills() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let inner = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let backend = SkillMatcherBackend::InMemory(inner);
        let matches = scored(
            backend
                .match_skills(&refs, "query", 5, false, embed_fn_constant)
                .await,
        );
        assert_eq!(matches.len(), 2);
    }

    #[tokio::test]
    async fn backend_in_memory_skill_embedding_returns_owned_vec() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let inner = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let backend = SkillMatcherBackend::InMemory(inner);

        let emb = backend
            .skill_embedding(0)
            .expect("skill 0 must be embedded");
        assert_eq!(emb, vec![1.0, 0.0, 0.0]); // "alpha"
    }

    #[tokio::test]
    async fn backend_in_memory_skill_embedding_out_of_range_is_none() {
        let metas = [make_meta("a", "alpha")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let inner = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let backend = SkillMatcherBackend::InMemory(inner);

        assert!(backend.skill_embedding(99).is_none());
    }

    #[tokio::test]
    async fn backend_in_memory_refresh_skill_embeddings_is_noop() {
        // In-memory embeddings are already resident; refresh_skill_embeddings must not alter
        // skill_embedding()'s behavior (still served from the pre-computed index, not a cache).
        let metas = [make_meta("a", "alpha")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let inner = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let backend = SkillMatcherBackend::InMemory(inner);

        let scored = vec![ScoredMatch {
            index: 0,
            score: 0.9,
        }];
        backend.refresh_skill_embeddings(&refs, &scored).await;

        assert_eq!(backend.skill_embedding(0), Some(vec![1.0, 0.0, 0.0]));
    }

    #[test]
    fn matcher_debug() {
        let matcher = SkillMatcher {
            embeddings: vec![(0, SkillEmbedding::from_raw(vec![1.0]))],
            trigger_embeddings: HashMap::new(),
            category_matcher: None,
        };
        let dbg = format!("{matcher:?}");
        assert!(dbg.contains("SkillMatcher"));
    }

    #[test]
    fn backend_debug() {
        let matcher = SkillMatcher {
            embeddings: vec![],
            trigger_embeddings: HashMap::new(),
            category_matcher: None,
        };
        let backend = SkillMatcherBackend::InMemory(matcher);
        let dbg = format!("{backend:?}");
        assert!(dbg.contains("InMemory"));
    }

    #[test]
    fn scored_match_clone_and_debug() {
        let sm = ScoredMatch {
            index: 0,
            score: 0.95,
        };
        let cloned = sm.clone();
        assert_eq!(cloned.index, 0);
        assert!((cloned.score - 0.95).abs() < f32::EPSILON);
        let dbg = format!("{sm:?}");
        assert!(dbg.contains("ScoredMatch"));
    }

    #[test]
    fn intent_classification_deserialize() {
        let json = r#"{"skill_name":"git","confidence":0.9,"params":{"branch":"main"}}"#;
        let ic: IntentClassification = serde_json::from_str(json).unwrap();
        assert_eq!(ic.skill_name, "git");
        assert!((ic.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(ic.params.get("branch").unwrap(), "main");
    }

    #[test]
    fn intent_classification_deserialize_without_params() {
        let json = r#"{"skill_name":"test","confidence":0.5}"#;
        let ic: IntentClassification = serde_json::from_str(json).unwrap();
        assert_eq!(ic.skill_name, "test");
        assert!(ic.params.is_empty());
    }

    #[test]
    fn intent_classification_json_schema() {
        let schema = schemars::schema_for!(IntentClassification);
        let json = serde_json::to_string(&schema).unwrap();
        assert!(json.contains("skill_name"));
        assert!(json.contains("confidence"));
    }

    #[test]
    fn intent_classification_rejects_missing_required_fields() {
        let json = r#"{"confidence":0.5}"#;
        let result: Result<IntentClassification, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn scored_match_delta_threshold_zero_disables_disambiguation() {
        // With threshold = 0.0 the condition `(scores[0] - scores[1]) < threshold`
        // evaluates to `delta < 0.0`. For any pair of sorted (descending) scores the
        // delta is always >= 0.0, so this threshold effectively disables disambiguation.
        let threshold = 0.0_f32;

        let high = ScoredMatch {
            index: 0,
            score: 0.90,
        };
        let low = ScoredMatch {
            index: 1,
            score: 0.89,
        };
        let delta = high.score - low.score; // 0.01

        assert!(
            delta >= 0.0,
            "delta between sorted scores is always non-negative"
        );
        assert!(
            delta >= threshold,
            "with threshold=0.0 disambiguation must NOT be triggered"
        );
    }

    #[test]
    fn scored_match_delta_at_threshold_boundary() {
        let threshold = 0.05_f32;

        // delta clearly above threshold => not ambiguous
        let high = ScoredMatch {
            index: 0,
            score: 0.90,
        };
        let low = ScoredMatch {
            index: 1,
            score: 0.80,
        };
        assert!((high.score - low.score) >= threshold);

        // delta clearly below threshold => ambiguous
        let close = ScoredMatch {
            index: 2,
            score: 0.89,
        };
        assert!((high.score - close.score) < threshold);
    }

    #[tokio::test]
    async fn match_skills_returns_scores() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let match_results = scored(
            skill_matcher
                .match_skills(refs.len(), "query", 2, false, embed_fn_mapping)
                .await,
        );

        assert_eq!(match_results.len(), 2);
        assert!(match_results[0].score > 0.0);
        assert!(match_results[0].score >= match_results[1].score);
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn scored_match_score_preserved(index in 0usize..100, score in -1.0f32..=1.0) {
            let m = ScoredMatch { index, score };
            // score stored exactly as provided; f32 round-trip is identity
            assert!((m.score - score).abs() < f32::EPSILON);
            assert_eq!(m.index, index);
        }

        #[test]
        fn cosine_similarity_within_bounds(
            a in proptest::collection::vec(-1.0f32..=1.0, 1..10),
            b in proptest::collection::vec(-1.0f32..=1.0, 1..10),
        ) {
            if a.len() == b.len() {
                let result = cosine_similarity(&a, &b);
                // cosine similarity is in [-1, 1], allow small floating-point slack
                assert!((-1.01..=1.01).contains(&result), "got {result}");
            }
        }
    }

    #[tokio::test]
    async fn two_stage_matching_uses_categories() {
        // Skills: 2 "web" + 2 "data", query matches "web" category.
        let metas = [
            make_meta_with_category("web-a", "alpha", "web"),
            make_meta_with_category("web-b", "beta", "web"),
            make_meta_with_category("data-a", "gamma", "data"),
            make_meta_with_category("data-b", "delta", "data"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        // Two-stage should still return results (not crash or empty).
        let results = scored(
            matcher
                .match_skills(refs.len(), "query", 4, true, embed_fn_mapping)
                .await,
        );
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn two_stage_falls_back_when_no_categories() {
        // All skills without category → two-stage falls back to flat.
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        // category_matcher is None when <2 multi-skill categories.
        assert!(matcher.category_matcher.is_none());
        let results = scored(
            matcher
                .match_skills(refs.len(), "query", 2, true, embed_fn_mapping)
                .await,
        );
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn two_stage_singleton_category_goes_to_uncategorized() {
        // One category with 1 skill, one with 2 → singleton is uncategorized.
        let metas = [
            make_meta_with_category("lone", "alpha", "solo"),
            make_meta_with_category("pair-a", "beta", "pair"),
            make_meta_with_category("pair-b", "gamma", "pair"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        // Only 1 multi-skill category ("pair") → category_matcher is None (not useful).
        assert!(matcher.category_matcher.is_none());
    }

    #[test]
    fn confusability_report_empty_when_threshold_high() {
        let matcher = SkillMatcher {
            embeddings: vec![
                (0, SkillEmbedding::from_raw(vec![1.0, 0.0])),
                (1, SkillEmbedding::from_raw(vec![0.0, 1.0])),
            ],
            trigger_embeddings: HashMap::new(),
            category_matcher: None,
        };
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let report = matcher.confusability_report(&refs, 0.99);
        assert!(report.pairs.is_empty());
        assert!(report.excluded_skills.is_empty());
    }

    #[test]
    fn confusability_report_finds_similar_pair() {
        let v = vec![1.0_f32, 0.0, 0.0];
        let matcher = SkillMatcher {
            embeddings: vec![
                (0, SkillEmbedding::from_raw(v.clone())),
                (1, SkillEmbedding::from_raw(v)),
            ],
            trigger_embeddings: HashMap::new(),
            category_matcher: None,
        };
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let report = matcher.confusability_report(&refs, 0.9);
        assert_eq!(report.pairs.len(), 1);
        assert!((report.pairs[0].similarity - 1.0).abs() < 1e-5);
    }

    #[test]
    fn confusability_report_tracks_excluded_skills() {
        // embeddings only contains index 0; index 1 has no embedding.
        let matcher = SkillMatcher {
            embeddings: vec![(0, SkillEmbedding::from_raw(vec![1.0, 0.0]))],
            trigger_embeddings: HashMap::new(),
            category_matcher: None,
        };
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let report = matcher.confusability_report(&refs, 0.5);
        assert_eq!(report.excluded_skills, vec!["b".to_string()]);
    }

    #[test]
    fn confusability_report_display_clean() {
        let report = ConfusabilityReport {
            pairs: vec![],
            threshold: 0.85,
            excluded_skills: vec![],
        };
        let s = report.to_string();
        assert!(s.contains("0.85"));
    }

    #[test]
    fn confusability_report_display_with_pairs() {
        let report = ConfusabilityReport {
            pairs: vec![ConfusabilityPair {
                skill_a: "web-search".into(),
                skill_b: "web-scrape".into(),
                similarity: 0.91,
            }],
            threshold: 0.85,
            excluded_skills: vec![],
        };
        let s = report.to_string();
        assert!(s.contains("web-search"));
        assert!(s.contains("web-scrape"));
        assert!(s.contains("0.910"));
    }

    #[test]
    fn confusability_report_display_with_excluded_skills() {
        let report = ConfusabilityReport {
            pairs: vec![],
            threshold: 0.85,
            excluded_skills: vec!["embed-failed".to_string(), "timeout-skill".to_string()],
        };
        let s = report.to_string();
        assert!(s.contains("embed-failed"));
        assert!(s.contains("timeout-skill"));
        assert!(s.contains("2 skill(s) excluded"));
    }

    #[test]
    fn confusability_report_display_with_pairs_and_excluded() {
        let report = ConfusabilityReport {
            pairs: vec![ConfusabilityPair {
                skill_a: "web-search".into(),
                skill_b: "web-scrape".into(),
                similarity: 0.91,
            }],
            threshold: 0.85,
            excluded_skills: vec!["no-embed".to_string()],
        };
        let s = report.to_string();
        assert!(s.contains("web-search"));
        assert!(s.contains("no-embed"));
        assert!(s.contains("1 skill(s) excluded"));
    }

    #[tokio::test]
    async fn two_stage_category_matcher_is_some_with_two_categories() {
        let metas = [
            make_meta_with_category("web-a", "alpha", "web"),
            make_meta_with_category("web-b", "beta", "web"),
            make_meta_with_category("data-a", "gamma", "data"),
            make_meta_with_category("data-b", "delta", "data"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        assert!(
            matcher.category_matcher.is_some(),
            "expected CategoryMatcher with 2 multi-skill categories"
        );
    }

    #[tokio::test]
    async fn two_stage_mixed_categorized_and_uncategorized_single_category() {
        // 2 skills in one category + 1 uncategorized → only 1 multi-skill category → not useful.
        let metas = [
            make_meta_with_category("web-a", "alpha", "web"),
            make_meta_with_category("web-b", "beta", "web"),
            make_meta("no-cat", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        assert!(
            matcher.category_matcher.is_none(),
            "only 1 multi-skill category is not useful for two-stage"
        );
    }

    #[tokio::test]
    async fn two_stage_result_count_within_flat_count() {
        let metas = [
            make_meta_with_category("web-a", "alpha", "web"),
            make_meta_with_category("web-b", "beta", "web"),
            make_meta_with_category("data-a", "gamma", "data"),
            make_meta_with_category("data-b", "delta", "data"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();

        let flat = scored(
            matcher
                .match_skills(refs.len(), "alpha", 4, false, embed_fn_mapping)
                .await,
        );
        let two = scored(
            matcher
                .match_skills(refs.len(), "alpha", 4, true, embed_fn_mapping)
                .await,
        );

        // Top result must be the same regardless of strategy.
        assert_eq!(flat[0].index, two[0].index);
        // Two-stage must not return more results than flat.
        assert!(two.len() <= flat.len());
    }

    // ── MatchResult contract tests (issue #3034) ─────────────────────────────
    // These tests verify the three cases that assembly.rs depends on:
    //   InfraError  → caller applies all-skills fallback
    //   Scored([])  → caller must NOT apply fallback (gate respected)
    //   Scored([…]) → caller filters by min_injection_score normally

    #[tokio::test]
    async fn match_result_infra_error_on_embed_failure() {
        // Embedding the query fails → MatchResult::InfraError must be returned,
        // signalling assembly.rs to apply the degraded-mode all-skills fallback.
        let metas = [make_meta("a", "alpha")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();

        let result = matcher
            .match_skills(refs.len(), "query", 5, false, embed_fn_fail)
            .await;

        assert!(
            matches!(result, MatchResult::InfraError),
            "embed failure must produce InfraError"
        );
    }

    #[tokio::test]
    async fn match_result_scored_empty_is_not_infra_error() {
        // Matcher succeeds but limit=0 produces an empty candidate list.
        // assembly.rs must treat this as Scored([]) — no fallback, gate respected.
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();

        let result = matcher
            .match_skills(refs.len(), "query", 0, false, embed_fn_constant)
            .await;

        // Must be Scored, not InfraError — empty list is a valid scorer outcome.
        assert!(
            matches!(result, MatchResult::Scored(ref v) if v.is_empty()),
            "successful match with limit=0 must produce Scored([])"
        );
    }

    #[tokio::test]
    async fn match_result_scored_nonempty_carries_candidates() {
        // Normal path: matcher succeeds and returns non-empty candidates.
        // assembly.rs must apply min_injection_score filter to these results.
        let metas = [
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();

        let result = matcher
            .match_skills(refs.len(), "query", 2, false, embed_fn_mapping)
            .await;

        assert!(
            matches!(result, MatchResult::Scored(ref v) if !v.is_empty()),
            "successful match with results must produce Scored([…])"
        );
    }

    // --- A5: trigger embedding tests ---

    fn make_meta_with_triggers(name: &str, description: &str, triggers: Vec<&str>) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: description.into(),
            triggers: triggers.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    /// Skill with no description match but exact trigger match should score higher than
    /// a semantically similar skill with no triggers.
    #[tokio::test]
    async fn trigger_score_dominates_description_score() {
        // embed_fn_mapping:
        //   "alpha" → [1,0,0], "beta" → [0,1,0], "gamma" → [0,0,1], "query" → [0.9,0.1,0]
        // Skill 0 ("a") description "alpha" → high cosine with "query" (≈0.9)
        // Skill 1 ("b") description "beta"  → low cosine with "query" (≈0.1)
        //   but trigger "alpha" → [1,0,0] → cosine with "query" ≈0.9
        //
        // Skill 1 should match at least as well as skill 0 via its trigger.
        let meta0 = make_meta("a", "alpha");
        let meta1 = make_meta_with_triggers("b", "beta", vec!["alpha"]);
        let refs: Vec<&SkillMeta> = vec![&meta0, &meta1];

        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        // Confirm trigger embeddings were stored: 1 skill (index 1) with 1 trigger.
        assert_eq!(matcher.trigger_embeddings.len(), 1);
        assert!(matcher.trigger_embeddings.contains_key(&1)); // skill index 1

        let match_results = scored(
            matcher
                .match_skills(refs.len(), "query", 2, false, embed_fn_mapping)
                .await,
        );
        // Both should be at or above the description score for skill 0 (~0.9).
        // Skill 1 should now have score ≈ 0.9 via trigger, not ≈ 0.1 via description.
        let score1 = match_results
            .iter()
            .find(|m| m.index == 1)
            .map_or(0.0, |m| m.score);
        assert!(
            score1 >= 0.85,
            "trigger should lift skill 1 score to ~0.9, got {score1:.3}",
        );
    }

    /// Trigger embeddings that fail (error returned) should be skipped silently.
    #[tokio::test]
    async fn trigger_embed_failure_skipped_silently() {
        let meta = make_meta_with_triggers("a", "alpha", vec!["will-fail-trigger"]);
        let refs: Vec<&SkillMeta> = vec![&meta];

        // embed_fn fails for anything not in the mapping (returns zero vec, which is fine;
        // in practice we want to test the fail path — use embed_fn_fail for the trigger).
        let embed_fn = |text: &str| -> EmbedFuture {
            if text == "will-fail-trigger" {
                Box::pin(async { Err(zeph_llm::LlmError::Other("trigger embed fail".into())) })
            } else {
                // description "alpha" succeeds
                embed_fn_mapping(text)
            }
        };

        let matcher = SkillMatcher::new(&refs, embed_fn).await.unwrap();
        // Trigger embed failed → trigger_embeddings should be empty.
        assert!(matcher.trigger_embeddings.is_empty());
    }

    /// Global trigger cap (500) is enforced: if total triggers across all skills exceed 500,
    /// only the first 500 are embedded and the rest are silently dropped.
    #[tokio::test]
    async fn global_trigger_cap_limits_embeddings() {
        // Build skills with enough triggers to exceed the 500-cap.
        // Each skill has 20 triggers (the per-skill max); 26 skills × 20 = 520 total.
        let trigger_phrase = "trigger phrase number";
        let metas: Vec<SkillMeta> = (0..26_usize)
            .map(|i| {
                let triggers: Vec<&str> = (0..20).map(|_| trigger_phrase).collect();
                make_meta_with_triggers(&format!("skill-{i:02}"), "alpha", triggers)
            })
            .collect();
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();

        // Total trigger embeddings stored across all skills must be ≤ 500.
        let total: usize = matcher.trigger_embeddings.values().map(Vec::len).sum();
        assert!(
            total <= MAX_TOTAL_TRIGGER_EMBEDDINGS,
            "total trigger embeddings {total} exceeds cap {MAX_TOTAL_TRIGGER_EMBEDDINGS}"
        );
    }
}
