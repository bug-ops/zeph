// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! STEM (Skill Template Evolution from Mining) — automatic detection of recurring tool-use
//! patterns and conversion to SKILL.md candidates.
//!
//! The STEM pipeline runs periodically (configured via `[skills.stem]`):
//!
//! 1. **Detect** — query `skill_usage_log` for tool sequences that exceed
//!    `min_occurrences` and `min_success_rate` thresholds.
//! 2. **Generate** — call the LLM with [`PATTERN_TO_SKILL_PROMPT_TEMPLATE`] to produce
//!    a SKILL.md body describing the pattern.
//! 3. **Validate** — parse the generated content via [`crate::loader::load_skill_meta_from_str`]
//!    and check for injection patterns via [`crate::scanner::scan_skill_body`].
//! 4. **Queue** — write the candidate to `skill_candidates` for operator review.
//!
//! # Canonical form
//!
//! Tool sequences are stored in their canonical JSON array form (no spaces) to ensure
//! consistent hashing. Use [`normalize_tool_sequence`] and [`sequence_hash`] to produce
//! the canonical representation before DB lookups.
//!
//! # Examples
//!
//! ```rust
//! use zeph_skills::stem::{normalize_tool_sequence, sequence_hash, should_generate_skill, ToolPattern};
//!
//! let seq = normalize_tool_sequence(&["shell", "web_scrape"]);
//! assert_eq!(seq, r#"["shell","web_scrape"]"#);
//!
//! let hash = sequence_hash(&seq);
//! assert_eq!(hash.len(), 16);
//!
//! let pattern = ToolPattern {
//!     tool_sequence: seq,
//!     sequence_hash: hash,
//!     occurrence_count: 5,
//!     success_count: 5,
//! };
//! assert!(should_generate_skill(&pattern, 3, 0.8));
//! ```

/// A recurring tool-use pattern detected from `skill_usage_log`.
#[derive(Debug, Clone)]
pub struct ToolPattern {
    /// Normalized JSON array of tool names (e.g. `["shell","web_scrape"]`).
    pub tool_sequence: String,
    /// Blake3 hex hash of `tool_sequence` (16 chars) — stable DB key.
    pub sequence_hash: String,
    /// Total times this sequence was recorded in the detection window.
    pub occurrence_count: u32,
    /// Number of successful invocations in the detection window.
    pub success_count: u32,
}

impl ToolPattern {
    /// Success rate in `[0.0, 1.0]`. Returns 0.0 when `occurrence_count` is zero.
    #[must_use]
    pub fn success_rate(&self) -> f64 {
        if self.occurrence_count == 0 {
            0.0
        } else {
            f64::from(self.success_count) / f64::from(self.occurrence_count)
        }
    }
}

/// Return `true` when a pattern has met both the occurrence and success-rate thresholds.
#[must_use]
pub fn should_generate_skill(
    pattern: &ToolPattern,
    min_occurrences: u32,
    min_success_rate: f64,
) -> bool {
    pattern.occurrence_count >= min_occurrences && pattern.success_rate() >= min_success_rate
}

/// Normalize a slice of tool names into a compact JSON array string suitable for DB storage.
///
/// The resulting string is canonical: `["tool_a","tool_b"]` with no spaces.
/// This prevents index mismatches due to whitespace differences.
#[must_use]
pub fn normalize_tool_sequence(tools: &[&str]) -> String {
    let inner = tools
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

/// Compute a 16-character blake3 hex hash of the normalized tool sequence.
#[must_use]
pub fn sequence_hash(normalized: &str) -> String {
    let hash = blake3::hash(normalized.as_bytes());
    hash.to_hex()[..16].to_string()
}

/// Prompt template for STEM skill generation from a recurring tool pattern.
///
/// Placeholders: `{tool_sequence}`, `{sample_contexts}` — substituted via
/// [`build_pattern_to_skill_prompt`] using `str::replace`.
pub const PATTERN_TO_SKILL_PROMPT_TEMPLATE: &str = "\
A recurring tool-use pattern has been detected. Generate a SKILL.md body that encapsulates \
this pattern as a reusable skill.

Tool sequence: {tool_sequence}
Sample task contexts:
{sample_contexts}

Output a SKILL.md body in markdown format with bash code blocks. Include:
- A brief description of what the skill does.
- Usage instructions for when to apply this skill.
- The tool sequence to follow.

The skill body must contain at most 3 top-level sections (## headers). Be concise.
Only output the skill body (no frontmatter, no explanation).";

/// Build a STEM pattern-to-skill prompt.
#[must_use]
pub fn build_pattern_to_skill_prompt(tool_sequence: &str, sample_contexts: &[String]) -> String {
    let contexts = if sample_contexts.is_empty() {
        "(no sample contexts available)".to_string()
    } else {
        sample_contexts
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {c}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    PATTERN_TO_SKILL_PROMPT_TEMPLATE
        .replace("{tool_sequence}", tool_sequence)
        .replace("{sample_contexts}", &contexts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_rate_zero_occurrences() {
        let p = ToolPattern {
            tool_sequence: "[]".into(),
            sequence_hash: "abc".into(),
            occurrence_count: 0,
            success_count: 0,
        };
        assert!((p.success_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn success_rate_partial() {
        let p = ToolPattern {
            tool_sequence: r#"["shell"]"#.into(),
            sequence_hash: "abc".into(),
            occurrence_count: 4,
            success_count: 3,
        };
        assert!((p.success_rate() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn should_generate_skill_threshold_met() {
        let p = ToolPattern {
            tool_sequence: r#"["shell","web_scrape"]"#.into(),
            sequence_hash: "abc".into(),
            occurrence_count: 5,
            success_count: 5,
        };
        assert!(should_generate_skill(&p, 3, 0.8));
    }

    #[test]
    fn should_generate_skill_too_few_occurrences() {
        let p = ToolPattern {
            tool_sequence: r#"["shell"]"#.into(),
            sequence_hash: "abc".into(),
            occurrence_count: 2,
            success_count: 2,
        };
        assert!(!should_generate_skill(&p, 3, 0.8));
    }

    #[test]
    fn should_generate_skill_low_success_rate() {
        let p = ToolPattern {
            tool_sequence: r#"["shell"]"#.into(),
            sequence_hash: "abc".into(),
            occurrence_count: 5,
            success_count: 2,
        };
        assert!(!should_generate_skill(&p, 3, 0.8));
    }

    #[test]
    fn normalize_tool_sequence_compact() {
        let seq = normalize_tool_sequence(&["shell", "web_scrape"]);
        assert_eq!(seq, r#"["shell","web_scrape"]"#);
    }

    #[test]
    fn normalize_tool_sequence_empty() {
        assert_eq!(normalize_tool_sequence(&[]), "[]");
    }

    #[test]
    fn sequence_hash_length() {
        let h = sequence_hash(r#"["shell"]"#);
        assert_eq!(h.len(), 16);
    }

    #[test]
    fn sequence_hash_deterministic() {
        let h1 = sequence_hash(r#"["shell","web"]"#);
        let h2 = sequence_hash(r#"["shell","web"]"#);
        assert_eq!(h1, h2);
    }

    #[test]
    fn build_pattern_to_skill_prompt_substitutes() {
        let result = build_pattern_to_skill_prompt(
            r#"["shell","web_scrape"]"#,
            &["search the web".to_string()],
        );
        assert!(result.contains(r#"["shell","web_scrape"]"#));
        assert!(result.contains("search the web"));
    }

    #[test]
    fn build_pattern_to_skill_prompt_no_contexts() {
        let result = build_pattern_to_skill_prompt(r#"["shell"]"#, &[]);
        assert!(result.contains("no sample contexts"));
    }
}
