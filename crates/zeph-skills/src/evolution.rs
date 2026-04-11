// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Self-learning skill evolution: failure classification, outcome tracking, and prompt templates.
//!
//! The evolution pipeline consists of three stages:
//!
//! 1. **Classify** — convert a raw tool error into a [`FailureKind`] so the system can
//!    distinguish transient infrastructure failures from systematic skill quality issues.
//! 2. **Record** — store [`SkillOutcome`] events in `skill_usage_log` for Bayesian ranking.
//! 3. **Improve** — if the success rate drops below a threshold, generate an updated skill
//!    body via LLM using [`IMPROVEMENT_PROMPT_TEMPLATE`].
//!
//! Step corrections ([`StepCorrection`]) allow fine-grained recovery: when a specific tool
//! failure pattern is detected, a hint is injected into the next agent turn.

use zeph_common::ToolName;

/// Structured failure classification for tool execution errors.
///
/// Used to decide whether a failure is attributable to the skill (systematic) or to
/// external infrastructure (transient). Only systematic failures trigger skill improvement.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::evolution::FailureKind;
///
/// let kind = FailureKind::from_error("process timed out");
/// assert_eq!(kind, FailureKind::Timeout);
///
/// let kind2 = FailureKind::from_error("permission denied");
/// assert_eq!(kind2, FailureKind::PermissionDenied);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Process exited with a non-zero status code.
    ExitNonzero,
    /// Operation exceeded its time budget.
    Timeout,
    /// OS or policy rejected the operation due to insufficient permissions.
    PermissionDenied,
    /// The skill chose an inappropriate tool or approach for the task.
    WrongApproach,
    /// The operation partially succeeded but did not complete.
    Partial,
    /// The LLM emitted syntactically invalid tool parameters.
    SyntaxError,
    /// Failure cause could not be classified.
    Unknown,
}

impl FailureKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExitNonzero => "exit_nonzero",
            Self::Timeout => "timeout",
            Self::PermissionDenied => "permission_denied",
            Self::WrongApproach => "wrong_approach",
            Self::Partial => "partial",
            Self::SyntaxError => "syntax_error",
            Self::Unknown => "unknown",
        }
    }

    /// Classify a failure kind from an error string heuristic.
    #[must_use]
    pub fn from_error(error: &str) -> Self {
        let lower = error.to_lowercase();
        if lower.contains("timed out") || lower.contains("timeout") {
            Self::Timeout
        } else if lower.contains("permission denied") {
            Self::PermissionDenied
        } else if lower.contains("exit code") {
            Self::ExitNonzero
        } else {
            Self::Unknown
        }
    }
}

impl From<zeph_common::error_taxonomy::ToolErrorCategory> for FailureKind {
    fn from(cat: zeph_common::error_taxonomy::ToolErrorCategory) -> Self {
        use zeph_common::error_taxonomy::ToolErrorCategory as C;
        match cat {
            C::Timeout => Self::Timeout,
            // Quality-attributable: skill chose the wrong approach or wrong tool.
            C::PolicyBlocked | C::ConfirmationRequired | C::ToolNotFound => Self::WrongApproach,
            // LLM-supplied parameters were invalid or mistyped.
            C::InvalidParameters | C::TypeMismatch => Self::SyntaxError,
            // Infrastructure failures and non-quality outcomes are not attributable to the skill.
            C::RateLimited
            | C::ServerError
            | C::NetworkError
            | C::PermanentFailure
            | C::Cancelled => Self::Unknown,
        }
    }
}

/// Pattern that matches a tool failure for step-correction lookup.
///
/// All three fields participate in matching: an empty string means "match any".
/// A failure event must match all non-empty fields to be eligible for correction.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FailurePattern {
    /// Which `FailureKind` this pattern matches (empty string = match any).
    pub failure_kind: String,
    /// Substring match against `error_context` (empty = match any error text).
    pub error_substring: String,
    /// Optional tool name filter (empty string = match any tool).
    pub tool_name: ToolName,
}

/// A step-level correction hint: when a tool failure matches `trigger`,
/// inject `hint` into the next turn's context.
///
/// Corrections are stored per skill in the `skill_step_corrections` DB table
/// and evaluated after each tool failure.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StepCorrection {
    /// Failure pattern that must match for this correction to be applied.
    pub trigger: FailurePattern,
    /// Text to inject into the agent's next turn as a contextual hint.
    pub hint: String,
}

/// Outcome classification for a skill-attributed agent turn.
///
/// Stored in `skill_usage_log` for Bayesian success-rate estimation used in
/// [`crate::trust_score`] re-ranking.
#[derive(Debug, Clone)]
pub enum SkillOutcome {
    /// The skill completed its task successfully.
    Success,
    /// A tool invoked by the skill exited with an error.
    ToolFailure {
        skill_name: String,
        error_context: String,
        tool_output: String,
        kind: FailureKind,
    },
    /// The LLM produced an empty response when a skill was active.
    EmptyResponse { skill_name: String },
    /// The user explicitly rejected the skill's output.
    UserRejection {
        skill_name: String,
        feedback: String,
    },
}

impl SkillOutcome {
    /// Returns a stable string tag for DB storage.
    #[must_use]
    pub fn outcome_str(&self) -> &str {
        match self {
            Self::Success => "success",
            Self::ToolFailure { .. } => "tool_failure",
            Self::EmptyResponse { .. } => "empty_response",
            Self::UserRejection { .. } => "user_rejection",
        }
    }

    /// Extract the skill name from any non-success variant.
    #[must_use]
    pub fn skill_name(&self) -> Option<&str> {
        match self {
            Self::Success => None,
            Self::ToolFailure { skill_name, .. }
            | Self::EmptyResponse { skill_name }
            | Self::UserRejection { skill_name, .. } => Some(skill_name),
        }
    }
}

/// Aggregated success/failure metrics for a single skill version.
///
/// Loaded from `skill_metrics` via `zeph-core` at improvement-decision time.
#[derive(Debug, Clone)]
pub struct SkillMetrics {
    /// Skill name (matches the `name` frontmatter field).
    pub skill_name: String,
    /// Schema version of the stored skill body.
    pub version: i64,
    /// Total number of invocations recorded.
    pub total: i64,
    /// Number of `Success` outcomes.
    pub successes: i64,
    /// Number of non-success outcomes.
    pub failures: i64,
}

impl SkillMetrics {
    /// Observed success rate in `[0.0, 1.0]`. Returns `0.0` when `total` is zero.
    ///
    /// Prefer [`crate::trust_score::posterior_weight`] for ranking decisions — it applies
    /// a Wilson-score confidence penalty that is more conservative for small sample sizes.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn success_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.successes as f64 / self.total as f64
        }
    }
}

pub const REFLECTION_PROMPT_TEMPLATE: &str = "\
You attempted to help the user with their request using the following skill instructions:

<skill name=\"{name}\">
{body}
</skill>

The attempt failed with this error:
{error_context}

Tool output:
{tool_output}

Analyze what went wrong and suggest an improved approach. \
Then attempt to fulfill the original user request using the improved approach.";

/// Build a reflection prompt by substituting template placeholders.
#[must_use]
pub fn build_reflection_prompt(
    name: &str,
    body: &str,
    error_context: &str,
    tool_output: &str,
) -> String {
    REFLECTION_PROMPT_TEMPLATE
        .replace("{name}", name)
        .replace("{body}", body)
        .replace("{error_context}", error_context)
        .replace("{tool_output}", tool_output)
}

pub const IMPROVEMENT_PROMPT_TEMPLATE: &str = "\
The original skill instructions failed, but an alternative approach succeeded.

Original skill:
<skill name=\"{name}\">
{original_body}
</skill>

Failed approach error: {error_context}
Successful approach: {successful_response}
{user_feedback_section}
Generate an improved version of the skill instructions that incorporates the lesson \
learned. Keep the same format (markdown with bash code blocks). Be concise.
The improved skill body must contain at most 3 top-level sections (## headers). \
Keep it focused and concise.
Only output the improved skill body (no frontmatter, no explanation).";

/// Build an improvement prompt by substituting template placeholders.
#[must_use]
pub fn build_improvement_prompt(
    name: &str,
    original_body: &str,
    error_context: &str,
    successful_response: &str,
    user_feedback: Option<&str>,
) -> String {
    let feedback_section = user_feedback.map_or_else(String::new, |fb| {
        format!("\nUser feedback on the current skill:\n{fb}\n")
    });
    IMPROVEMENT_PROMPT_TEMPLATE
        .replace("{name}", name)
        .replace("{original_body}", original_body)
        .replace("{error_context}", error_context)
        .replace("{successful_response}", successful_response)
        .replace("{user_feedback_section}", &feedback_section)
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct SkillEvaluation {
    pub should_improve: bool,
    pub issues: Vec<String>,
    pub severity: String,
}

pub const EVALUATION_PROMPT_TEMPLATE: &str = "\
Evaluate whether the following skill needs improvement based on the error context.

<skill name=\"{name}\">
{body}
</skill>

Error context: {error_context}
Tool output: {tool_output}
Current success rate: {success_rate}%

Determine if this is a systematic skill problem (should_improve: true) \
or a transient issue like network timeout, rate limit, etc. (should_improve: false).

Respond in JSON with fields: should_improve (bool), issues (list of strings), severity (\"low\", \"medium\", or \"high\").";

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn build_evaluation_prompt(
    name: &str,
    body: &str,
    error_context: &str,
    tool_output: &str,
    metrics: &SkillMetrics,
) -> String {
    let rate = format!("{:.0}", metrics.success_rate() * 100.0);
    EVALUATION_PROMPT_TEMPLATE
        .replace("{name}", name)
        .replace("{body}", body)
        .replace("{error_context}", error_context)
        .replace("{tool_output}", tool_output)
        .replace("{success_rate}", &rate)
}

/// Domain gate prompt template for evaluating whether an auto-generated skill body stays
/// within the domain of the original skill.
///
/// Placeholders: `{description}`, `{name}`, `{body}` — substituted via
/// [`build_domain_gate_prompt`] using `str::replace` (not `format!()`).
///
/// The JSON example in the template uses literal curly braces, which is safe here
/// because the string is never passed through `format!()`.
pub const DOMAIN_GATE_PROMPT_TEMPLATE: &str = "\
Evaluate whether the following auto-generated skill version stays within \
the domain of the original skill.

Original skill description: {description}
Original skill name: {name}

Generated skill body:
<skill>
{body}
</skill>

Respond in JSON: {\"domain_relevant\": bool, \"reasoning\": string}
Return domain_relevant=true only if the generated body is focused on the \
same domain as the original skill description. Return false if it drifts \
into unrelated topics or adds capabilities beyond the original scope.";

/// LLM response for the domain evaluation gate.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct DomainGateResult {
    pub domain_relevant: bool,
    pub reasoning: String,
}

/// Build a domain gate prompt by substituting template placeholders.
///
/// Uses [`str::replace`] rather than `format!()` to avoid interpreting the JSON
/// example braces in the template as format arguments.
#[must_use]
pub fn build_domain_gate_prompt(name: &str, description: &str, body: &str) -> String {
    DOMAIN_GATE_PROMPT_TEMPLATE
        .replace("{description}", description)
        .replace("{name}", name)
        .replace("{body}", body)
}

// --- ARISE: trace-based improvement ---

/// Prompt template for ARISE trace-based skill improvement.
///
/// Placeholders: `{name}`, `{original_body}`, `{tool_trace}` — substituted via
/// [`build_trace_improvement_prompt`] using `str::replace`.
pub const TRACE_IMPROVEMENT_PROMPT_TEMPLATE: &str = "\
The following skill was used to complete a task successfully using multiple tools.

<skill name=\"{name}\">
{original_body}
</skill>

Successful tool call sequence:
{tool_trace}

Generate an improved version of the skill instructions that captures the successful pattern.
Keep the same markdown format with bash code blocks. Be concise.
The improved skill body must contain at most 3 top-level sections (## headers).
Only output the improved skill body (no frontmatter, no explanation).";

/// Build an ARISE trace improvement prompt.
#[must_use]
pub fn build_trace_improvement_prompt(name: &str, original_body: &str, tool_trace: &str) -> String {
    TRACE_IMPROVEMENT_PROMPT_TEMPLATE
        .replace("{name}", name)
        .replace("{original_body}", original_body)
        .replace("{tool_trace}", tool_trace)
}

/// Prompt template for extracting step corrections from a failure+success trace pair.
///
/// Placeholders: `{name}`, `{failure_error}`, `{failure_tool}`, `{successful_approach}`.
pub const CORRECTION_EXTRACTION_PROMPT_TEMPLATE: &str = "\
A skill named \"{name}\" encountered a tool failure, then succeeded on retry.

Tool that failed: {failure_tool}
Failure error: {failure_error}
Successful approach: {successful_approach}

Extract a concise correction hint that could help avoid this failure in the future.
Respond in JSON with fields:
  failure_kind: one of \"exit_nonzero\", \"timeout\", \"permission_denied\", \"wrong_approach\", \"syntax_error\", \"unknown\"
  error_substring: a short distinctive substring from the error (empty string if none)
  hint: a 1-2 sentence actionable correction hint
Example: {\"failure_kind\": \"exit_nonzero\", \"error_substring\": \"not a git repo\", \"hint\": \"Run git init before any git commands.\"}";

/// Response from the ARISE correction extraction LLM call.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct CorrectionExtractionResult {
    pub failure_kind: String,
    #[serde(default)]
    pub error_substring: String,
    pub hint: String,
}

/// Build a correction extraction prompt.
#[must_use]
pub fn build_correction_extraction_prompt(
    name: &str,
    failure_error: &str,
    failure_tool: &str,
    successful_approach: &str,
) -> String {
    CORRECTION_EXTRACTION_PROMPT_TEMPLATE
        .replace("{name}", name)
        .replace("{failure_error}", failure_error)
        .replace("{failure_tool}", failure_tool)
        .replace("{successful_approach}", successful_approach)
}

/// Absolute maximum body size to prevent exponential growth across generations.
pub const MAX_BODY_BYTES: usize = 65_536;

/// Validate that the generated body does not exceed 2x the original size
/// and stays within the absolute cap.
#[must_use]
pub fn validate_body_size(original: &str, generated: &str) -> bool {
    generated.len() <= original.len() * 2 && generated.len() <= MAX_BODY_BYTES
}

/// Validate that the generated body contains at most `max_sections` top-level
/// markdown sections (lines starting with `"## "`).
///
/// Only H2 headers are counted; H1 (`# `) and H3+ (`### `) are ignored.
///
/// # Known limitation
///
/// Lines starting with `"## "` inside fenced code blocks (` ``` `) are also counted.
/// For MVP this is acceptable — SKILL.md bodies rarely contain code blocks with headers.
#[must_use]
pub fn validate_body_sections(body: &str, max_sections: u32) -> bool {
    let count = body.lines().filter(|l| l.starts_with("## ")).count();
    count <= max_sections as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_str_variants() {
        assert_eq!(SkillOutcome::Success.outcome_str(), "success");
        assert_eq!(
            SkillOutcome::ToolFailure {
                skill_name: "git".into(),
                error_context: "err".into(),
                tool_output: "out".into(),
                kind: FailureKind::Unknown,
            }
            .outcome_str(),
            "tool_failure"
        );
        assert_eq!(
            SkillOutcome::EmptyResponse {
                skill_name: "git".into(),
            }
            .outcome_str(),
            "empty_response"
        );
        assert_eq!(
            SkillOutcome::UserRejection {
                skill_name: "git".into(),
                feedback: "bad".into(),
            }
            .outcome_str(),
            "user_rejection"
        );
    }

    #[test]
    fn skill_name_extraction() {
        assert!(SkillOutcome::Success.skill_name().is_none());
        assert_eq!(
            SkillOutcome::ToolFailure {
                skill_name: "docker".into(),
                error_context: String::new(),
                tool_output: String::new(),
                kind: FailureKind::Unknown,
            }
            .skill_name(),
            Some("docker")
        );
        assert_eq!(
            SkillOutcome::EmptyResponse {
                skill_name: "git".into(),
            }
            .skill_name(),
            Some("git")
        );
        assert_eq!(
            SkillOutcome::UserRejection {
                skill_name: "sql".into(),
                feedback: String::new(),
            }
            .skill_name(),
            Some("sql")
        );
    }

    #[test]
    fn success_rate_zero_total() {
        let m = SkillMetrics {
            skill_name: "x".into(),
            version: 1,
            total: 0,
            successes: 0,
            failures: 0,
        };
        assert!((m.success_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn success_rate_all_success() {
        let m = SkillMetrics {
            skill_name: "x".into(),
            version: 1,
            total: 10,
            successes: 10,
            failures: 0,
        };
        assert!((m.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn success_rate_all_failures() {
        let m = SkillMetrics {
            skill_name: "x".into(),
            version: 1,
            total: 5,
            successes: 0,
            failures: 5,
        };
        assert!((m.success_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn success_rate_mixed() {
        let m = SkillMetrics {
            skill_name: "x".into(),
            version: 1,
            total: 4,
            successes: 3,
            failures: 1,
        };
        assert!((m.success_rate() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn build_reflection_prompt_substitutes() {
        let result = build_reflection_prompt("git", "do git stuff", "exit code 1", "fatal: error");
        assert!(result.contains("<skill name=\"git\">"));
        assert!(result.contains("do git stuff"));
        assert!(result.contains("exit code 1"));
        assert!(result.contains("fatal: error"));
    }

    #[test]
    fn build_improvement_prompt_without_feedback() {
        let result = build_improvement_prompt("git", "original body", "the error", "the fix", None);
        assert!(result.contains("<skill name=\"git\">"));
        assert!(result.contains("original body"));
        assert!(result.contains("the error"));
        assert!(result.contains("the fix"));
        assert!(!result.contains("User feedback"));
    }

    #[test]
    fn build_improvement_prompt_with_feedback() {
        let result = build_improvement_prompt(
            "git",
            "original body",
            "the error",
            "the fix",
            Some("please fix the commit flow"),
        );
        assert!(result.contains("User feedback on the current skill:"));
        assert!(result.contains("please fix the commit flow"));
    }

    #[test]
    fn validate_body_size_within_limit() {
        assert!(validate_body_size("12345", "1234567890"));
    }

    #[test]
    fn validate_body_size_exceeds_limit() {
        assert!(!validate_body_size("12345", "12345678901"));
    }

    #[test]
    fn validate_body_size_empty_original() {
        assert!(validate_body_size("", ""));
        assert!(!validate_body_size("", "x"));
    }

    #[test]
    fn build_evaluation_prompt_substitutes() {
        let metrics = SkillMetrics {
            skill_name: "git".into(),
            version: 1,
            total: 10,
            successes: 7,
            failures: 3,
        };
        let result =
            build_evaluation_prompt("git", "do git stuff", "exit code 1", "fatal", &metrics);
        assert!(result.contains("<skill name=\"git\">"));
        assert!(result.contains("do git stuff"));
        assert!(result.contains("exit code 1"));
        assert!(result.contains("fatal"));
        assert!(result.contains("70%"));
    }

    #[test]
    fn skill_evaluation_deserialize() {
        let json = r#"{"should_improve": true, "issues": ["bad pattern"], "severity": "high"}"#;
        let eval: SkillEvaluation = serde_json::from_str(json).unwrap();
        assert!(eval.should_improve);
        assert_eq!(eval.issues.len(), 1);
        assert_eq!(eval.severity, "high");
    }

    #[test]
    fn skill_evaluation_skip() {
        let json = r#"{"should_improve": false, "issues": [], "severity": "low"}"#;
        let eval: SkillEvaluation = serde_json::from_str(json).unwrap();
        assert!(!eval.should_improve);
        assert!(eval.issues.is_empty());
    }

    #[test]
    fn validate_body_size_absolute_cap() {
        let large_original = "x".repeat(40_000);
        let large_generated = "x".repeat(70_000);
        // Within 2x but exceeds MAX_BODY_BYTES (65536)
        assert!(!validate_body_size(&large_original, &large_generated));
    }

    #[test]
    fn validate_body_sections_within_limit() {
        let body = "## Setup\ndo stuff\n## Usage\nmore stuff\n";
        assert!(validate_body_sections(body, 3));
    }

    #[test]
    fn validate_body_sections_at_limit() {
        let body = "## Setup\n## Usage\n## Tips\n";
        assert!(validate_body_sections(body, 3));
    }

    #[test]
    fn validate_body_sections_exceeds_limit() {
        let body = "## A\n## B\n## C\n## D\n";
        assert!(!validate_body_sections(body, 3));
    }

    #[test]
    fn validate_body_sections_no_sections() {
        let body = "Just some text without any headers.\n";
        assert!(validate_body_sections(body, 3));
    }

    #[test]
    fn validate_body_sections_h1_not_counted() {
        let body = "# Title\n## Section\n### Subsection\n";
        // Only "## Section" is counted; H1 and H3+ are not.
        assert!(validate_body_sections(body, 1));
    }

    #[test]
    fn domain_gate_result_deserialize() {
        let json = r#"{"domain_relevant": true, "reasoning": "matches original domain"}"#;
        let result: DomainGateResult = serde_json::from_str(json).unwrap();
        assert!(result.domain_relevant);
        assert_eq!(result.reasoning, "matches original domain");
    }

    #[test]
    fn domain_gate_result_false() {
        let json = r#"{"domain_relevant": false, "reasoning": "drifted to unrelated topic"}"#;
        let result: DomainGateResult = serde_json::from_str(json).unwrap();
        assert!(!result.domain_relevant);
    }

    #[test]
    fn build_domain_gate_prompt_substitutes() {
        let result = build_domain_gate_prompt(
            "git-helper",
            "Git workflow assistant",
            "## Usage\nRun git commands",
        );
        assert!(result.contains("git-helper"));
        assert!(result.contains("Git workflow assistant"));
        assert!(result.contains("## Usage\nRun git commands"));
        // Ensure the JSON example braces are preserved literally.
        assert!(result.contains("{\"domain_relevant\""));
    }

    #[test]
    fn improvement_prompt_includes_section_limit() {
        assert!(
            IMPROVEMENT_PROMPT_TEMPLATE.contains("at most 3 top-level sections"),
            "IMPROVEMENT_PROMPT_TEMPLATE must mention the section limit"
        );
    }

    // Priority 2: SkillEvaluation deserialization edge cases

    #[test]
    fn skill_evaluation_missing_severity_fails() {
        let json = r#"{"should_improve": true, "issues": ["bad pattern"]}"#;
        let result: Result<SkillEvaluation, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "expected error when severity field is missing"
        );
    }

    #[test]
    fn skill_evaluation_should_improve_as_string_fails() {
        let json = r#"{"should_improve": "true", "issues": [], "severity": "low"}"#;
        let result: Result<SkillEvaluation, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "expected error when should_improve is a string"
        );
    }

    #[test]
    fn skill_evaluation_extra_unknown_fields_succeeds() {
        let json =
            r#"{"should_improve": false, "issues": [], "severity": "low", "extra_field": 42}"#;
        let result: SkillEvaluation = serde_json::from_str(json).unwrap();
        assert!(!result.should_improve);
        assert_eq!(result.severity, "low");
    }

    // Priority 3: Proptest

    use proptest::prelude::*;

    #[test]
    fn failure_kind_from_error_timeout() {
        assert_eq!(
            FailureKind::from_error("operation timed out"),
            FailureKind::Timeout
        );
        assert_eq!(
            FailureKind::from_error("timeout after 30s"),
            FailureKind::Timeout
        );
    }

    #[test]
    fn failure_kind_from_error_permission_denied() {
        assert_eq!(
            FailureKind::from_error("error: permission denied"),
            FailureKind::PermissionDenied
        );
    }

    #[test]
    fn failure_kind_from_error_exit_nonzero() {
        assert_eq!(
            FailureKind::from_error("command failed [exit code 1]"),
            FailureKind::ExitNonzero
        );
        assert_eq!(
            FailureKind::from_error("exit code 128"),
            FailureKind::ExitNonzero
        );
    }

    #[test]
    fn failure_kind_from_error_unknown() {
        assert_eq!(
            FailureKind::from_error("something went wrong"),
            FailureKind::Unknown
        );
        assert_eq!(FailureKind::from_error(""), FailureKind::Unknown);
    }

    #[test]
    fn failure_kind_as_str_roundtrip() {
        assert_eq!(FailureKind::ExitNonzero.as_str(), "exit_nonzero");
        assert_eq!(FailureKind::Timeout.as_str(), "timeout");
        assert_eq!(FailureKind::PermissionDenied.as_str(), "permission_denied");
        assert_eq!(FailureKind::WrongApproach.as_str(), "wrong_approach");
        assert_eq!(FailureKind::Partial.as_str(), "partial");
        assert_eq!(FailureKind::SyntaxError.as_str(), "syntax_error");
        assert_eq!(FailureKind::Unknown.as_str(), "unknown");
    }

    #[test]
    fn failure_kind_from_tool_error_category_key_mappings() {
        use zeph_common::error_taxonomy::ToolErrorCategory as C;
        assert_eq!(FailureKind::from(C::Timeout), FailureKind::Timeout);
        assert_eq!(
            FailureKind::from(C::PolicyBlocked),
            FailureKind::WrongApproach
        );
        assert_eq!(
            FailureKind::from(C::ToolNotFound),
            FailureKind::WrongApproach
        );
        assert_eq!(
            FailureKind::from(C::InvalidParameters),
            FailureKind::SyntaxError
        );
        assert_eq!(FailureKind::from(C::TypeMismatch), FailureKind::SyntaxError);
        assert_eq!(FailureKind::from(C::RateLimited), FailureKind::Unknown);
        assert_eq!(FailureKind::from(C::ServerError), FailureKind::Unknown);
        assert_eq!(FailureKind::from(C::NetworkError), FailureKind::Unknown);
        assert_eq!(FailureKind::from(C::PermanentFailure), FailureKind::Unknown);
        assert_eq!(
            FailureKind::from(C::ConfirmationRequired),
            FailureKind::WrongApproach
        );
        assert_eq!(FailureKind::from(C::Cancelled), FailureKind::Unknown);
    }

    // D2Skill serde round-trips and prompt substitution

    #[test]
    fn step_correction_serde_roundtrip() {
        let original = StepCorrection {
            trigger: FailurePattern {
                failure_kind: "exit_nonzero".to_string(),
                error_substring: "not a git repo".to_string(),
                tool_name: "".into(),
            },
            hint: "Run git init before any git commands.".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: StepCorrection = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn failure_pattern_serde_roundtrip() {
        let original = FailurePattern {
            failure_kind: "timeout".to_string(),
            error_substring: "timed out".to_string(),
            tool_name: "shell".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: FailurePattern = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn correction_extraction_result_deserialize_valid() {
        let json = r#"{
            "failure_kind": "exit_nonzero",
            "error_substring": "not a git repo",
            "hint": "Run git init before any git commands."
        }"#;
        let result: CorrectionExtractionResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.failure_kind, "exit_nonzero");
        assert_eq!(result.error_substring, "not a git repo");
        assert_eq!(result.hint, "Run git init before any git commands.");
    }

    #[test]
    fn correction_extraction_result_missing_optional_field() {
        // error_substring has #[serde(default)] so it may be absent
        let json = r#"{"failure_kind": "unknown", "hint": "retry with sudo"}"#;
        let result: CorrectionExtractionResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.failure_kind, "unknown");
        assert!(result.error_substring.is_empty());
    }

    #[test]
    fn correction_extraction_result_deserialize_invalid() {
        let json = r#"{"failure_kind": 42, "hint": "bad"}"#;
        let result: Result<CorrectionExtractionResult, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "expected error for invalid failure_kind type"
        );
    }

    #[test]
    fn build_correction_extraction_prompt_substitutes() {
        let result = build_correction_extraction_prompt(
            "git-helper",
            "command not found",
            "shell",
            "installed git first",
        );
        assert!(result.contains("git-helper"));
        assert!(result.contains("command not found"));
        assert!(result.contains("shell"));
        assert!(!result.contains("{name}"));
        assert!(!result.contains("{failure_error}"));
    }

    proptest! {
        #[test]
        fn build_evaluation_prompt_never_panics(
            name in ".*",
            body in ".*",
            desc in ".*",
            total in 0i64..=1000,
            successes in 0i64..=1000,
        ) {
            let failures = total - successes.min(total);
            let metrics = SkillMetrics {
                skill_name: name.clone(),
                version: 1,
                total,
                successes: successes.min(total),
                failures,
            };
            let _ = build_evaluation_prompt(&name, &body, &desc, "", &metrics);
        }
    }
}
