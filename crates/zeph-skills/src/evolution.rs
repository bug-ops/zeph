//! Self-learning skill evolution types and prompt templates.

/// Outcome classification for skill-attributed events.
#[derive(Debug, Clone)]
pub enum SkillOutcome {
    Success,
    ToolFailure {
        skill_name: String,
        error_context: String,
        tool_output: String,
    },
    EmptyResponse {
        skill_name: String,
    },
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

/// Aggregated metrics for a skill version.
#[derive(Debug, Clone)]
pub struct SkillMetrics {
    pub skill_name: String,
    pub version: i64,
    pub total: i64,
    pub successes: i64,
    pub failures: i64,
}

impl SkillMetrics {
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

/// Absolute maximum body size to prevent exponential growth across generations.
pub const MAX_BODY_BYTES: usize = 65_536;

/// Validate that the generated body does not exceed 2x the original size
/// and stays within the absolute cap.
#[must_use]
pub fn validate_body_size(original: &str, generated: &str) -> bool {
    generated.len() <= original.len() * 2 && generated.len() <= MAX_BODY_BYTES
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
