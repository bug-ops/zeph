// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `GoSkills` group-structured skill retrieval.
//!
//! Groups a ranked list of skills into an entry-point + support structure by computing
//! inter-skill cosine similarity on their in-process embeddings. Falls back to a flat
//! list when no pair exceeds the configured threshold.
//!
//! # Flow
//!
//! ```text
//! top-N skills (post-RRF, post-rerank)
//!         │
//!         ▼
//!  group_skills() ──threshold exceeded?──► GroupResult::Grouped(SkillGroup)
//!                                    │
//!                                    └──► GroupResult::Flat(Vec<Skill>)
//! ```

use std::collections::HashMap;

use crate::loader::Skill;

/// Role of a skill within a group.
///
/// `Context` is reserved for future use (e.g., background reference skills) and is
/// not assigned by the MVP grouping algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillRole {
    /// The primary skill that directly handles the user's intent.
    EntryPoint,
    /// A skill that assists the entry point (high cosine similarity).
    Support,
    /// A background context skill (reserved, not assigned in MVP).
    Context,
}

/// A group of semantically related skills with an entry point and support skills.
///
/// `requirements` and `failure_notes` are present for forward-compatibility with the
/// full `GoSkills` spec; they are empty in the MVP and the formatter skips empty blocks.
#[derive(Debug, Clone)]
pub struct SkillGroup {
    /// The primary skill selected as entry point.
    pub entry_point: Skill,
    /// Skills with cosine similarity > threshold relative to the entry point.
    pub support: Vec<Skill>,
    /// Role assignment map from skill name to role.
    pub role_labels: HashMap<String, SkillRole>,
    /// Structured requirements extracted from the skill (empty in MVP).
    pub requirements: Vec<String>,
    /// Failure avoidance notes extracted from the skill (empty in MVP).
    pub failure_notes: Vec<String>,
}

/// Outcome of the grouping step: a formed group or a flat fallback.
#[derive(Debug, Clone)]
pub enum GroupResult {
    /// At least one support skill exceeded the similarity threshold.
    Grouped(Box<SkillGroup>),
    /// No pair exceeded the threshold; use the flat list as-is.
    Flat(Vec<Skill>),
}

/// Compute inter-skill cosine similarity and form a group if the threshold is met.
///
/// `skills` must be the already-ranked top-N skills (post-RRF, post-rerank).
/// `skill_indices` maps positions in `skills` to the original skill-store indices used
/// by `get_embedding`.
/// `get_embedding` is called with the original skill-store index and must return the
/// in-process embedding slice, or `None` when unavailable (e.g., Qdrant backend).
///
/// Returns [`GroupResult::Grouped`] when the entry point (`skills[0]`) has at least one
/// support skill whose cosine similarity exceeds `threshold`. Returns
/// [`GroupResult::Flat`] when `skills.len() < 2`, any required embedding is missing,
/// or no candidate exceeds the threshold.
///
/// The threshold comparison uses strict `>` (not `>=`) to prevent zero-vector false
/// matches when `threshold = 0.0`.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::group::{GroupResult, group_skills};
///
/// // With fewer than 2 skills the result is always flat.
/// let result = group_skills(&[], &[], |_| None::<&[f32]>, 0.5);
/// assert!(matches!(result, GroupResult::Flat(_)));
/// ```
#[must_use]
pub fn group_skills<'e, F>(
    skills: &[Skill],
    skill_indices: &[usize],
    get_embedding: F,
    threshold: f32,
) -> GroupResult
where
    F: Fn(usize) -> Option<&'e [f32]>,
{
    if skills.len() < 2 {
        return GroupResult::Flat(skills.to_vec());
    }

    let entry_idx = skill_indices.first().copied().unwrap_or(0);
    let Some(entry_embed) = get_embedding(entry_idx) else {
        return GroupResult::Flat(skills.to_vec());
    };

    let mut support = Vec::new();
    let mut role_labels = HashMap::new();
    role_labels.insert(skills[0].name().to_string(), SkillRole::EntryPoint);

    for (pos, skill) in skills[1..].iter().enumerate() {
        let Some(&idx) = skill_indices.get(pos + 1) else {
            continue;
        };
        let Some(embed) = get_embedding(idx) else {
            continue;
        };
        let sim = zeph_common::math::cosine_similarity(entry_embed, embed);
        if sim > threshold {
            role_labels.insert(skill.name().to_string(), SkillRole::Support);
            support.push(skill.clone());
        }
    }

    if support.is_empty() {
        GroupResult::Flat(skills.to_vec())
    } else {
        GroupResult::Grouped(Box::new(SkillGroup {
            entry_point: skills[0].clone(),
            support,
            role_labels,
            requirements: Vec::new(),
            failure_notes: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::loader::{Skill, SkillMeta};

    fn make_skill(name: &str) -> Skill {
        Skill {
            meta: SkillMeta {
                name: name.into(),
                description: String::new(),
                compatibility: None,
                license: None,
                metadata: Vec::new(),
                allowed_tools: Vec::new(),
                requires_secrets: Vec::new(),
                skill_dir: PathBuf::new(),
                source_url: None,
                git_hash: None,
                category: None,
            },
            body: String::new(),
        }
    }

    // Static embedding storage used in tests — avoids lifetime issues with closures.
    static EMBED_A: &[f32] = &[1.0, 0.0, 0.0];
    static EMBED_B: &[f32] = &[0.9, 0.1, 0.0]; // high similarity to A
    static EMBED_C: &[f32] = &[0.0, 0.0, 1.0]; // low similarity to A
    static EMBED_D: &[f32] = &[0.95, 0.05, 0.0]; // high similarity to A
    static EMBED_E: &[f32] = &[0.0, 1.0, 0.0]; // orthogonal to A

    fn embedding_for_index(idx: usize) -> Option<&'static [f32]> {
        match idx {
            0 => Some(EMBED_A),
            1 => Some(EMBED_B),
            2 => Some(EMBED_C),
            3 => Some(EMBED_D),
            4 => Some(EMBED_E),
            _ => None,
        }
    }

    #[test]
    fn empty_skills_returns_flat() {
        let result = group_skills(&[], &[], embedding_for_index, 0.5);
        assert!(matches!(result, GroupResult::Flat(v) if v.is_empty()));
    }

    #[test]
    fn single_skill_returns_flat() {
        let skills = vec![make_skill("a")];
        let result = group_skills(&skills, &[0], embedding_for_index, 0.5);
        assert!(matches!(result, GroupResult::Flat(v) if v.len() == 1));
    }

    #[test]
    fn all_below_threshold_returns_flat() {
        // EMBED_A vs EMBED_C: cosine = 0.0
        let skills = vec![make_skill("a"), make_skill("c")];
        let result = group_skills(&skills, &[0, 2], embedding_for_index, 0.5);
        assert!(matches!(result, GroupResult::Flat(_)));
    }

    #[test]
    fn one_above_threshold_returns_grouped() {
        // EMBED_A vs EMBED_B: cosine ≈ 0.994
        let skills = vec![make_skill("a"), make_skill("b")];
        let result = group_skills(&skills, &[0, 1], embedding_for_index, 0.5);
        let GroupResult::Grouped(g) = result else {
            panic!("expected Grouped");
        };
        assert_eq!(g.entry_point.name(), "a");
        assert_eq!(g.support.len(), 1);
        assert_eq!(g.support[0].name(), "b");
        assert_eq!(g.role_labels[g.entry_point.name()], SkillRole::EntryPoint);
        assert_eq!(g.role_labels[g.support[0].name()], SkillRole::Support);
    }

    #[test]
    fn mixed_similarity_only_high_ones_become_support() {
        // indices: 0=A, 1=B(high), 2=C(low), 3=D(high)
        let skills = vec![
            make_skill("a"),
            make_skill("b"),
            make_skill("c"),
            make_skill("d"),
        ];
        let result = group_skills(&skills, &[0, 1, 2, 3], embedding_for_index, 0.5);
        let GroupResult::Grouped(g) = result else {
            panic!("expected Grouped");
        };
        assert_eq!(g.support.len(), 2);
        let support_names: Vec<&str> = g.support.iter().map(Skill::name).collect();
        assert!(support_names.contains(&"b"));
        assert!(support_names.contains(&"d"));
        assert!(!support_names.contains(&"c"));
    }

    #[test]
    fn missing_entry_embedding_returns_flat() {
        let skills = vec![make_skill("x"), make_skill("y")];
        // index 99 → no embedding
        let result = group_skills(&skills, &[99, 1], embedding_for_index, 0.5);
        assert!(matches!(result, GroupResult::Flat(_)));
    }

    #[test]
    fn threshold_zero_cosine_zero_not_grouped() {
        // EMBED_A vs EMBED_E: cosine = 0.0 — NOT > 0.0, so flat
        let skills = vec![make_skill("a"), make_skill("e")];
        let result = group_skills(&skills, &[0, 4], embedding_for_index, 0.0);
        assert!(matches!(result, GroupResult::Flat(_)));
    }

    #[test]
    fn threshold_zero_positive_similarity_is_grouped() {
        // EMBED_A vs EMBED_B: cosine ≈ 0.994 > 0.0 → grouped
        let skills = vec![make_skill("a"), make_skill("b")];
        let result = group_skills(&skills, &[0, 1], embedding_for_index, 0.0);
        assert!(matches!(result, GroupResult::Grouped(_)));
    }

    #[test]
    fn missing_support_embedding_skipped_others_still_grouped() {
        // index 5 → no embedding for "e_no_embed"
        let skills = vec![make_skill("a"), make_skill("b"), make_skill("e_no_embed")];
        let result = group_skills(&skills, &[0, 1, 5], embedding_for_index, 0.5);
        // "b" has similarity; "e_no_embed" is skipped — group still formed
        let GroupResult::Grouped(g) = result else {
            panic!("expected Grouped");
        };
        assert_eq!(g.support.len(), 1);
        assert_eq!(g.support[0].name(), "b");
    }

    #[test]
    fn group_result_contains_empty_requirements_and_failure_notes() {
        let skills = vec![make_skill("a"), make_skill("b")];
        let result = group_skills(&skills, &[0, 1], embedding_for_index, 0.5);
        let GroupResult::Grouped(g) = result else {
            panic!("expected Grouped");
        };
        assert!(g.requirements.is_empty());
        assert!(g.failure_notes.is_empty());
    }

    #[test]
    fn context_role_variant_exists() {
        // Ensure Context variant is present for forward-compat (not assigned by MVP)
        assert!(matches!(SkillRole::Context, SkillRole::Context));
    }
}
