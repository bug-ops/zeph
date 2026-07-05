// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Add/Merge/Discard decision logic for skill candidates (`AutoSkill A2`).
//!
//! Implements the three-way decision flow used by all skill creation paths:
//! - [`MergeDecision::Add`] — candidate is novel; create a new quarantined skill.
//! - [`MergeDecision::Merge`] — candidate is semantically similar; unify with the nearest skill via LLM.
//! - [`MergeDecision::Discard`] — candidate is a near-exact duplicate; drop it.
//!
//! # Threshold Invariant
//!
//! `merge_threshold` MUST be strictly less than `dedup_threshold`. Callers are responsible for
//! enforcing this at startup (see `LearningConfig` validation). The decision function assumes
//! the invariant holds and does not re-validate it.
//!
//! # Usage
//!
//! ```rust
//! use zeph_skills::merger::{MergeDecision, decide};
//! use zeph_skills::loader::SkillMeta;
//! use std::path::PathBuf;
//!
//! let meta = SkillMeta {
//!     name: "rewrite-text".into(),
//!     description: "Rewrite text professionally.".into(),
//!     version: 2,
//!     source: "trace_extraction".into(),
//!     session_id: None,
//!     compatibility: None,
//!     license: None,
//!     metadata: vec![],
//!     allowed_tools: vec![],
//!     requires_secrets: vec![],
//!     skill_dir: PathBuf::new(),
//!     source_url: None,
//!     git_hash: None,
//!     category: None,
//!     triggers: vec![],
//!     parent_skill: None,
//!     proactive_domain: None,
//!     extensions: None,
//! };
//!
//! let decision = decide(0.80, 0.75, 0.90, true, &meta);
//! assert!(matches!(decision, MergeDecision::Merge { .. }));
//! ```

use crate::loader::SkillMeta;

/// Outcome of the Add/Merge/Discard similarity evaluation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum MergeDecision {
    /// Candidate is novel — create a new quarantined skill with `version = 0`.
    Add,
    /// Candidate is semantically related to an existing skill — merge via LLM.
    Merge {
        /// Name of the nearest existing skill.
        nearest_name: String,
        /// Current version of the nearest skill (`version + 1` will be written after merge).
        nearest_version: u32,
    },
    /// Candidate is a near-exact duplicate — drop it.
    Discard,
}

/// Evaluate the Add/Merge/Discard decision for a candidate skill.
///
/// # Arguments
///
/// * `similarity` — cosine similarity of the candidate to its nearest existing skill (0.0–1.0).
/// * `merge_threshold` — minimum similarity to trigger a merge (typically 0.75).
/// * `dedup_threshold` — minimum similarity to discard as a duplicate (typically 0.90).
/// * `merge_enabled` — when `false`, the merge zone collapses to `Discard`.
/// * `nearest` — metadata of the nearest existing skill (name + version).
///
/// # Decision table
///
/// | Condition | Result |
/// |-----------|--------|
/// | `sim >= dedup_threshold` | `Discard` |
/// | `sim >= merge_threshold` AND `merge_enabled` | `Merge { nearest_name, nearest_version }` |
/// | `sim >= merge_threshold` AND `!merge_enabled` | `Discard` |
/// | `sim < merge_threshold` | `Add` |
///
/// # Examples
///
/// ```rust,ignore
/// use zeph_skills::merger::{MergeDecision, decide};
/// use zeph_skills::loader::SkillMeta;
/// use std::path::PathBuf;
///
/// let meta = SkillMeta {
///     name: "deploy-ci".into(),
///     description: "Deploy CI.".into(),
///     version: 1,
///     source: String::new(),
///     session_id: None,
///     compatibility: None,
///     license: None,
///     metadata: vec![],
///     allowed_tools: vec![],
///     requires_secrets: vec![],
///     skill_dir: PathBuf::new(),
///     source_url: None,
///     git_hash: None,
///     category: None,
///     parent_skill: None,
/// };
///
/// // Near-duplicate → Discard
/// assert_eq!(decide(0.95, 0.75, 0.90, true, &meta), MergeDecision::Discard);
///
/// // Novel → Add
/// assert_eq!(decide(0.40, 0.75, 0.90, true, &meta), MergeDecision::Add);
///
/// // Merge zone, disabled → Discard
/// assert_eq!(decide(0.80, 0.75, 0.90, false, &meta), MergeDecision::Discard);
/// ```
#[must_use]
pub fn decide(
    similarity: f32,
    merge_threshold: f32,
    dedup_threshold: f32,
    merge_enabled: bool,
    nearest: &SkillMeta,
) -> MergeDecision {
    if similarity >= dedup_threshold {
        return MergeDecision::Discard;
    }
    if similarity >= merge_threshold {
        if merge_enabled {
            return MergeDecision::Merge {
                nearest_name: nearest.name.clone(),
                nearest_version: nearest.version,
            };
        }
        return MergeDecision::Discard;
    }
    MergeDecision::Add
}

/// Find the nearest neighbor in `existing` to `candidate_emb` by cosine similarity.
///
/// Returns `None` when `existing` is empty, or `Some((meta, similarity))` for the
/// closest match.
///
/// # Examples
///
/// ```rust,ignore
/// use zeph_skills::merger::find_nearest;
/// use zeph_skills::embedding::SkillEmbedding;
/// use zeph_skills::loader::SkillMeta;
/// use std::path::PathBuf;
///
/// let meta = SkillMeta {
///     name: "existing".into(),
///     description: "desc".into(),
///     version: 0,
///     source: String::new(),
///     session_id: None,
///     compatibility: None,
///     license: None,
///     metadata: vec![],
///     allowed_tools: vec![],
///     requires_secrets: vec![],
///     skill_dir: PathBuf::new(),
///     source_url: None,
///     git_hash: None,
///     category: None,
///     parent_skill: None,
/// };
/// let emb = SkillEmbedding::from_raw(vec![1.0, 0.0, 0.0]);
/// let candidate = SkillEmbedding::from_raw(vec![1.0, 0.0, 0.0]);
/// let existing = vec![(meta, emb)];
/// let result = find_nearest(&candidate, &existing);
/// let (found_meta, sim) = result.unwrap();
/// assert_eq!(found_meta.name, "existing");
/// assert!((sim - 1.0).abs() < 1e-5);
/// ```
#[must_use]
pub fn find_nearest<'a>(
    candidate_emb: &crate::embedding::SkillEmbedding,
    existing: &'a [(SkillMeta, crate::embedding::SkillEmbedding)],
) -> Option<(&'a SkillMeta, f32)> {
    existing
        .iter()
        .map(|(meta, emb)| {
            let sim = zeph_common::math::cosine_similarity(candidate_emb.as_ref(), emb.as_ref());
            (meta, sim)
        })
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::embedding::SkillEmbedding;

    fn make_meta(name: &str, version: u32) -> SkillMeta {
        SkillMeta {
            name: name.to_string(),
            description: "desc".into(),
            version,
            source: String::new(),
            session_id: None,
            compatibility: None,
            license: None,
            metadata: vec![],
            allowed_tools: vec![],
            requires_secrets: vec![],
            skill_dir: PathBuf::new(),
            source_url: None,
            git_hash: None,
            category: None,
            triggers: vec![],
            parent_skill: None,
            proactive_domain: None,
            extensions: None,
        }
    }

    #[test]
    fn decide_discard_above_dedup_threshold() {
        let meta = make_meta("existing", 2);
        assert_eq!(
            decide(0.95, 0.75, 0.90, true, &meta),
            MergeDecision::Discard
        );
    }

    #[test]
    fn decide_discard_at_exact_dedup_threshold() {
        let meta = make_meta("existing", 2);
        assert_eq!(
            decide(0.90, 0.75, 0.90, true, &meta),
            MergeDecision::Discard
        );
    }

    #[test]
    fn decide_merge_in_merge_zone_enabled() {
        let meta = make_meta("rewrite-text", 3);
        let decision = decide(0.80, 0.75, 0.90, true, &meta);
        assert_eq!(
            decision,
            MergeDecision::Merge {
                nearest_name: "rewrite-text".into(),
                nearest_version: 3
            }
        );
    }

    #[test]
    fn decide_discard_in_merge_zone_disabled() {
        // spec 057 AC: merge_enabled=false, sim=0.80 → Discard
        let meta = make_meta("deploy-ci", 1);
        assert_eq!(
            decide(0.80, 0.75, 0.90, false, &meta),
            MergeDecision::Discard
        );
    }

    #[test]
    fn decide_add_below_merge_threshold() {
        let meta = make_meta("any", 0);
        assert_eq!(decide(0.40, 0.75, 0.90, true, &meta), MergeDecision::Add);
        assert_eq!(decide(0.40, 0.75, 0.90, false, &meta), MergeDecision::Add);
    }

    #[test]
    fn decide_add_at_merge_threshold_boundary() {
        // sim == merge_threshold is in the merge zone, not Add zone
        let meta = make_meta("skill", 0);
        assert_eq!(
            decide(0.75, 0.75, 0.90, true, &meta),
            MergeDecision::Merge {
                nearest_name: "skill".into(),
                nearest_version: 0
            }
        );
    }

    #[test]
    fn find_nearest_empty_returns_none() {
        let emb = SkillEmbedding::from_raw(vec![1.0, 0.0]);
        assert!(find_nearest(&emb, &[]).is_none());
    }

    #[test]
    fn find_nearest_returns_closest() {
        let candidate = SkillEmbedding::from_raw(vec![1.0, 0.0, 0.0]);
        let meta_a = make_meta("far", 0);
        let meta_b = make_meta("close", 1);
        let existing = vec![
            (meta_a, SkillEmbedding::from_raw(vec![0.0, 1.0, 0.0])),
            (meta_b, SkillEmbedding::from_raw(vec![1.0, 0.0, 0.0])),
        ];
        let (found, sim) = find_nearest(&candidate, &existing).unwrap();
        assert_eq!(found.name, "close");
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn find_nearest_single_entry() {
        let candidate = SkillEmbedding::from_raw(vec![1.0, 0.0]);
        let meta = make_meta("only", 5);
        let existing = vec![(meta, SkillEmbedding::from_raw(vec![1.0, 0.0]))];
        let (found, _) = find_nearest(&candidate, &existing).unwrap();
        assert_eq!(found.name, "only");
        assert_eq!(found.version, 5);
    }
}
