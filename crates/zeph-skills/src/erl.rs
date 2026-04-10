// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ERL (Experiential Reflective Learning) — heuristic extraction and injection at skill match time.
//!
//! After a task completes, the agent calls the LLM with [`REFLECTION_EXTRACT_PROMPT_TEMPLATE`]
//! to extract up to three actionable heuristics from the task history. These are stored in
//! `skill_heuristics` (via `zeph-core`) and injected into matching skill prompts as a
//! `## Learned Heuristics` block.
//!
//! # Deduplication
//!
//! Before storing a newly extracted heuristic, [`text_similarity`] compares it against all
//! existing heuristics for the same skill using Jaccard word-set similarity. Heuristics that
//! exceed the deduplication threshold (configurable, default 0.8) are discarded.
//!
//! # Examples
//!
//! ```rust
//! use zeph_skills::erl::{build_reflection_extract_prompt, format_heuristics_section};
//!
//! let prompt = build_reflection_extract_prompt(
//!     "Searched GitHub for Rust crates",
//!     "web_scrape, shell",
//!     "success",
//! );
//! assert!(prompt.contains("Searched GitHub"));
//!
//! let section = format_heuristics_section(&["prefer crates.io over GitHub".into()]);
//! assert!(section.starts_with("## Learned Heuristics"));
//! ```

/// LLM response struct for heuristic extraction.
///
/// Deserialized from the LLM's JSON response to [`REFLECTION_EXTRACT_PROMPT_TEMPLATE`].
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct ReflectionResult {
    /// Extracted heuristics (at most 3 are requested in the prompt).
    pub heuristics: Vec<HeuristicEntry>,
}

/// A single extracted heuristic with an optional skill name association.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct HeuristicEntry {
    /// Concise actionable heuristic text.
    pub text: String,
    /// Skill the heuristic most applies to, or `None` for a general heuristic.
    pub skill_name: Option<String>,
}

/// Prompt template for ERL heuristic extraction.
///
/// Placeholders: `{task_summary}`, `{tool_calls}`, `{outcome}` — substituted via
/// [`build_reflection_extract_prompt`] using `str::replace`.
pub const REFLECTION_EXTRACT_PROMPT_TEMPLATE: &str = "\
A task was completed. Extract transferable heuristics that could help future similar tasks.

Task summary: {task_summary}
Tool calls used: {tool_calls}
Outcome: {outcome}

Extract up to 3 concise, actionable heuristics. For each heuristic, optionally name the \
skill it most applies to (or leave skill_name null for general heuristics).

Respond in JSON:
{\"heuristics\": [{\"text\": \"string\", \"skill_name\": \"string or null\"}, ...]}";

/// Build an ERL reflection extraction prompt.
#[must_use]
pub fn build_reflection_extract_prompt(
    task_summary: &str,
    tool_calls: &str,
    outcome: &str,
) -> String {
    REFLECTION_EXTRACT_PROMPT_TEMPLATE
        .replace("{task_summary}", task_summary)
        .replace("{tool_calls}", tool_calls)
        .replace("{outcome}", outcome)
}

/// Simple text similarity check for deduplication (word-set Jaccard coefficient).
///
/// Returns a value in `[0.0, 1.0]`. Used when no embedding model is available (MVP).
#[must_use]
pub fn text_similarity(a: &str, b: &str) -> f32 {
    let tokens_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let tokens_b: std::collections::HashSet<&str> = b.split_whitespace().collect();
    if tokens_a.is_empty() && tokens_b.is_empty() {
        return 1.0;
    }
    let intersection = tokens_a.intersection(&tokens_b).count();
    let union = tokens_a.union(&tokens_b).count();
    if union == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let result = intersection as f32 / union as f32;
        result
    }
}

/// Format a list of heuristics as a markdown `## Learned Heuristics` section
/// suitable for prepending to skill context.
#[must_use]
pub fn format_heuristics_section(heuristics: &[String]) -> String {
    if heuristics.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Learned Heuristics\n");
    for h in heuristics {
        out.push_str("- ");
        out.push_str(h);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_reflection_extract_prompt_substitutes() {
        let result = build_reflection_extract_prompt("Fix the bug", "shell, git", "success");
        assert!(result.contains("Fix the bug"));
        assert!(result.contains("shell, git"));
        assert!(result.contains("success"));
    }

    #[test]
    fn reflection_result_deserialize() {
        let json = r#"{"heuristics": [{"text": "always test", "skill_name": "git"}]}"#;
        let r: ReflectionResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.heuristics.len(), 1);
        assert_eq!(r.heuristics[0].text, "always test");
        assert_eq!(r.heuristics[0].skill_name.as_deref(), Some("git"));
    }

    #[test]
    fn reflection_result_null_skill_name() {
        let json = r#"{"heuristics": [{"text": "be careful", "skill_name": null}]}"#;
        let r: ReflectionResult = serde_json::from_str(json).unwrap();
        assert!(r.heuristics[0].skill_name.is_none());
    }

    #[test]
    fn text_similarity_identical() {
        assert!((text_similarity("hello world", "hello world") - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn text_similarity_disjoint() {
        assert!((text_similarity("foo bar", "baz qux") - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn text_similarity_partial() {
        let s = text_similarity("hello world", "hello there");
        // intersection={hello}, union={hello,world,there} → 1/3
        assert!(s > 0.0 && s < 1.0);
    }

    #[test]
    fn text_similarity_empty_both() {
        assert!((text_similarity("", "") - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn format_heuristics_section_empty() {
        assert!(format_heuristics_section(&[]).is_empty());
    }

    #[test]
    fn format_heuristics_section_nonempty() {
        let h = vec!["tip one".to_string(), "tip two".to_string()];
        let s = format_heuristics_section(&h);
        assert!(s.contains("## Learned Heuristics"));
        assert!(s.contains("- tip one"));
        assert!(s.contains("- tip two"));
    }
}
