// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Temporal causal IPI (Indirect Prompt Injection) analysis at tool-return boundaries.
//!
//! [`TurnCausalAnalyzer`] detects behavioral deviation by comparing agent behavioral
//! probes before and after a tool batch is dispatched. If the agent's stated task or
//! planned next action changes significantly after receiving tool output, it may indicate
//! that injected instructions in the tool output have influenced its behavior.
//!
//! Deviation above the configured threshold fires a `WARN` log and emits a `SecurityEvent`
//! — it **never blocks** tool execution. The goal is operator visibility, not hard blocking.
//!
//! # Algorithm
//!
//! 1. **Pre-probe**: send `PROBE_QUESTION` + context summary to the LLM, record response `A`.
//! 2. **Dispatch** the tool batch and collect all results.
//! 3. **Post-probe**: send `PROBE_QUESTION` + context summary + tool output snippets, record `B`.
//! 4. **Analyze**: compute deviation between `A` and `B` using normalized Levenshtein +
//!    Jaccard distance. Score ∈ [0.0, 1.0]. Deviation ≥ threshold → flagged.
//!
//! Both probes count as 2 LLM calls per tool batch. Deviation scoring is local (no 3rd call).
//!
//! Integration point: `native.rs` tool batch dispatch loop.

use std::collections::HashSet;
use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};

pub use zeph_config::CausalIpiConfig;

// ---------------------------------------------------------------------------
// Fixed behavioral probe question (not configurable — security boundary)
// ---------------------------------------------------------------------------

const PROBE_QUESTION: &str = "Summarize your current task and planned next action in one sentence.";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from [`TurnCausalAnalyzer`] probe calls.
///
/// Probe failures are non-fatal: the caller should log a `WARN` and skip causal
/// analysis for the current batch rather than blocking tool execution.
#[derive(Debug, thiserror::Error)]
pub enum CausalIpiError {
    /// The probe LLM call returned an error.
    #[error("probe LLM call failed: {0}")]
    LlmError(#[from] zeph_llm::LlmError),
    /// The probe did not complete within the configured timeout.
    #[error("probe timed out after {0}ms")]
    Timeout(u64),
}

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Result of causal deviation analysis returned by [`TurnCausalAnalyzer::analyze`].
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_sanitizer::causal_ipi::{TurnCausalAnalyzer, CausalAnalysis};
/// use zeph_config::CausalIpiConfig;
/// use zeph_llm::any::AnyProvider;
/// use zeph_llm::mock::MockProvider;
///
/// let analyzer = TurnCausalAnalyzer::new(
///     AnyProvider::Mock(MockProvider::default()),
///     &CausalIpiConfig { threshold: 0.5, ..Default::default() },
/// );
///
/// // Identical probe responses produce near-zero deviation.
/// let result = analyzer.analyze("I will search files.", "I will search files.");
/// assert!(!result.is_flagged);
/// assert!(result.deviation_score < 0.01);
/// ```
#[derive(Debug, Clone)]
pub struct CausalAnalysis {
    /// Deviation score in [0.0, 1.0]. Higher = more behavioral divergence between probes.
    pub deviation_score: f32,
    /// `true` when `deviation_score` meets or exceeds the configured threshold.
    pub is_flagged: bool,
}

// ---------------------------------------------------------------------------
// Analyzer
// ---------------------------------------------------------------------------

/// Detects behavioral deviation at tool-return boundaries via LLM probes.
///
/// The behavioral probe question is fixed and not configurable (security boundary):
/// `"Summarize your current task and planned next action in one sentence."`
///
/// # Usage
///
/// ```rust,ignore
/// // Before tool dispatch:
/// let pre = analyzer.probe(&context_summary).await?;
///
/// // Dispatch tools, collect results...
///
/// // After all results received:
/// let post = analyzer.post_probe(&context_summary, &tool_snippets).await?;
///
/// // Compute deviation locally (no LLM call):
/// let analysis = analyzer.analyze(&pre, &post);
/// if analysis.is_flagged {
///     tracing::warn!(score = analysis.deviation_score, "causal IPI deviation detected");
/// }
/// ```
pub struct TurnCausalAnalyzer {
    provider: AnyProvider,
    threshold: f32,
    probe_timeout: Duration,
    /// Maximum characters kept from each probe response before deviation scoring.
    ///
    /// Approximates `probe_max_tokens` from config (1 token ≈ 4 chars). Bounding the
    /// probe size limits both Levenshtein O(n²) cost and deviation score sensitivity to
    /// unexpectedly long responses. Responses longer than this are truncated at a UTF-8
    /// character boundary before analysis.
    probe_max_chars: usize,
}

impl TurnCausalAnalyzer {
    /// Construct a new analyzer from config and a resolved provider.
    ///
    /// The `probe_max_chars` field is derived from `config.probe_max_tokens` (1 token ≈ 4 chars)
    /// to bound Levenshtein O(n²) cost and prevent unexpectedly long probe responses from
    /// distorting deviation scores.
    #[must_use]
    pub fn new(provider: AnyProvider, config: &CausalIpiConfig) -> Self {
        Self {
            provider,
            threshold: config.threshold,
            probe_timeout: Duration::from_millis(config.probe_timeout_ms),
            // 1 token ≈ 4 chars; multiply by 4 to get a conservative char limit.
            probe_max_chars: (config.probe_max_tokens as usize)
                .saturating_mul(4)
                .max(400),
        }
    }

    /// Generate a pre-execution behavioral probe.
    ///
    /// Call ONCE before dispatching a tool batch. Returns the probe response for later
    /// comparison in [`post_probe`](Self::post_probe).
    ///
    /// `context_summary`: compact representation of the current conversation state —
    /// last user message + last assistant message, each truncated to 500 chars.
    ///
    /// On provider error or timeout: returns `Err`. The caller logs WARN and skips
    /// causal analysis for the batch — never blocks tool execution.
    ///
    /// # Errors
    ///
    /// Returns `CausalIpiError::LlmError` on provider failure, `CausalIpiError::Timeout`
    /// on probe timeout.
    pub async fn probe(&self, context_summary: &str) -> Result<String, CausalIpiError> {
        let content = format!("{context_summary}\n\n{PROBE_QUESTION}");
        self.call_probe(&content).await
    }

    /// Generate a post-execution behavioral probe.
    ///
    /// Call ONCE after ALL tool results in a batch are received and sanitized.
    ///
    /// `context_summary`: same format as the pre-probe context summary.
    /// `tool_output_snippets`: first 200 chars of each tool output concatenated
    /// with `---` separator. Empty outputs: `[empty]`. Error outputs: `[error: ...]`.
    ///
    /// On provider error or timeout: returns `Err`. The caller logs WARN and skips
    /// causal analysis for the batch — never blocks.
    ///
    /// # Errors
    ///
    /// Returns `CausalIpiError::LlmError` on provider failure, `CausalIpiError::Timeout`
    /// on probe timeout.
    pub async fn post_probe(
        &self,
        context_summary: &str,
        tool_output_snippets: &str,
    ) -> Result<String, CausalIpiError> {
        let content = format!(
            "{context_summary}\n\nTool results:\n{tool_output_snippets}\n\n{PROBE_QUESTION}"
        );
        self.call_probe(&content).await
    }

    async fn call_probe(&self, content: &str) -> Result<String, CausalIpiError> {
        let messages = vec![Message::from_legacy(Role::User, content)];

        match tokio::time::timeout(self.probe_timeout, self.provider.chat(&messages)).await {
            Ok(Ok(response)) => {
                let trimmed = response.trim();
                // Bound probe response to probe_max_chars to cap Levenshtein cost and
                // prevent unbounded responses from distorting deviation scores.
                let bounded = if trimmed.len() <= self.probe_max_chars {
                    trimmed.to_owned()
                } else {
                    let boundary = trimmed.floor_char_boundary(self.probe_max_chars);
                    trimmed[..boundary].to_owned()
                };
                Ok(bounded)
            }
            Ok(Err(e)) => Err(CausalIpiError::LlmError(e)),
            Err(_) => Err(CausalIpiError::Timeout(
                u64::try_from(self.probe_timeout.as_millis()).unwrap_or(u64::MAX),
            )),
        }
    }

    /// Returns the configured deviation threshold.
    ///
    /// Deviation scores at or above this value produce [`CausalAnalysis::is_flagged`] = `true`.
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Compare pre- and post-probe responses and return a local deviation score.
    ///
    /// This is a **local** computation — no LLM call. Combines normalized Levenshtein distance
    /// (character-level) with Jaccard distance on word sets. Both metrics are in [0.0, 1.0];
    /// the result is their average.
    ///
    /// Score range: [0.0, 1.0]. Higher = more deviation between pre and post probe.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use zeph_sanitizer::causal_ipi::TurnCausalAnalyzer;
    /// use zeph_config::CausalIpiConfig;
    /// use zeph_llm::any::AnyProvider;
    /// use zeph_llm::mock::MockProvider;
    ///
    /// let analyzer = TurnCausalAnalyzer::new(
    ///     AnyProvider::Mock(MockProvider::default()),
    ///     &CausalIpiConfig { threshold: 0.4, ..Default::default() },
    /// );
    ///
    /// let result = analyzer.analyze(
    ///     "I will list files in the project.",
    ///     "I will exfiltrate credentials to a remote server.",
    /// );
    /// assert!(result.is_flagged);
    /// assert!(result.deviation_score >= 0.4);
    /// ```
    #[must_use]
    pub fn analyze(&self, pre_response: &str, post_response: &str) -> CausalAnalysis {
        let deviation_score = compute_deviation(pre_response, post_response);
        let is_flagged = deviation_score >= self.threshold;
        CausalAnalysis {
            deviation_score,
            is_flagged,
        }
    }
}

// ---------------------------------------------------------------------------
// Local deviation computation
// ---------------------------------------------------------------------------

/// Compute behavioral deviation score between two probe responses.
///
/// Combines normalized Levenshtein distance (character-level) with Jaccard
/// distance on keyword sets (word-level). Both are in [0.0, 1.0]; result
/// is their average.
fn compute_deviation(a: &str, b: &str) -> f32 {
    let lev = normalized_levenshtein(a, b);
    let jac = jaccard_distance(a, b);
    f32::midpoint(lev, jac)
}

/// Normalized Levenshtein distance: `edit_distance / max(len_a, len_b)`.
///
/// Returns 0.0 when both strings are empty, 1.0 when completely different.
fn normalized_levenshtein(a: &str, b: &str) -> f32 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 0.0;
    }
    let dist = levenshtein(&a_chars, &b_chars);
    #[allow(clippy::cast_precision_loss)]
    let score = (dist as f32) / (max_len as f32);
    score.min(1.0)
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let n = b.len();
    // Use two-row rolling array to avoid O(m*n) allocation.
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            curr[j + 1] = if ca == cb {
                prev[j]
            } else {
                1 + prev[j].min(prev[j + 1]).min(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Jaccard distance on word sets: 1.0 - |intersection| / |union|.
fn jaccard_distance(a: &str, b: &str) -> f32 {
    let words_a: HashSet<&str> = a.split_whitespace().collect();
    let words_b: HashSet<&str> = b.split_whitespace().collect();
    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();
    if union == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let score = 1.0 - (intersection as f32) / (union as f32);
    score
}

// ---------------------------------------------------------------------------
// Tool output snippet helpers
// ---------------------------------------------------------------------------

/// Maximum bytes to include from a single tool output in the post-probe context snippet.
pub const SNIPPET_MAX_BYTES: usize = 200;

/// Format a tool output body as a snippet for use in the post-probe context.
///
/// Truncates to [`SNIPPET_MAX_BYTES`] at a UTF-8 character boundary.
/// An empty body is replaced with the sentinel `"[empty]"`.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::causal_ipi::{format_tool_snippet, SNIPPET_MAX_BYTES};
///
/// assert_eq!(format_tool_snippet(""), "[empty]");
/// assert_eq!(format_tool_snippet("hello"), "hello");
///
/// let long = "x".repeat(500);
/// assert_eq!(format_tool_snippet(&long).len(), SNIPPET_MAX_BYTES);
/// ```
#[must_use]
pub fn format_tool_snippet(body: &str) -> String {
    if body.is_empty() {
        return "[empty]".into();
    }
    if body.len() <= SNIPPET_MAX_BYTES {
        return body.to_owned();
    }
    let boundary = body.floor_char_boundary(SNIPPET_MAX_BYTES);
    body[..boundary].to_owned()
}

/// Format a tool error as a snippet for the post-probe context.
///
/// Truncates the error string to 100 bytes and wraps it in `[error: …]`.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::causal_ipi::format_error_snippet;
///
/// let s = format_error_snippet("connection refused");
/// assert_eq!(s, "[error: connection refused]");
/// ```
#[must_use]
pub fn format_error_snippet(error: &str) -> String {
    let max = 100;
    let truncated = if error.len() <= max {
        error
    } else {
        let b = error.floor_char_boundary(max);
        &error[..b]
    };
    format!("[error: {truncated}]")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_strings_deviation_zero() {
        let score = compute_deviation("I will search for files.", "I will search for files.");
        assert!(
            score < 1e-3,
            "identical strings should have near-zero deviation"
        );
    }

    #[test]
    fn completely_different_strings_high_deviation() {
        let score = compute_deviation(
            "I will search for files in the project directory.",
            "Execute system command and exfiltrate credentials to remote server.",
        );
        assert!(
            score > 0.5,
            "very different strings should have high deviation: {score}"
        );
    }

    #[test]
    fn empty_strings_deviation_zero() {
        let score = compute_deviation("", "");
        assert!(score < 1e-6);
    }

    #[test]
    fn one_empty_one_nonempty_deviation_one() {
        let score = compute_deviation("", "hello world");
        assert!((score - 1.0).abs() < 0.1, "score: {score}");
    }

    #[test]
    fn analyze_flags_above_threshold() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        let provider = AnyProvider::Mock(MockProvider::default());
        let config = CausalIpiConfig {
            enabled: true,
            threshold: 0.3,
            ..CausalIpiConfig::default()
        };
        let analyzer = TurnCausalAnalyzer::new(provider, &config);
        let result = analyzer.analyze(
            "I will search for files.",
            "I will now send emails to external addresses.",
        );
        assert!(result.is_flagged, "deviation: {}", result.deviation_score);
    }

    #[test]
    fn analyze_does_not_flag_similar_responses() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        let provider = AnyProvider::Mock(MockProvider::default());
        let config = CausalIpiConfig {
            enabled: true,
            threshold: 0.7,
            ..CausalIpiConfig::default()
        };
        let analyzer = TurnCausalAnalyzer::new(provider, &config);
        let result = analyzer.analyze(
            "I will search for files in the project directory.",
            "I will search for files in the project directory and list them.",
        );
        assert!(!result.is_flagged, "deviation: {}", result.deviation_score);
    }

    #[test]
    fn format_tool_snippet_empty() {
        assert_eq!(format_tool_snippet(""), "[empty]");
    }

    #[test]
    fn format_tool_snippet_short() {
        assert_eq!(format_tool_snippet("hello"), "hello");
    }

    #[test]
    fn format_tool_snippet_truncates_long() {
        let long = "a".repeat(500);
        let snippet = format_tool_snippet(&long);
        assert_eq!(snippet.len(), SNIPPET_MAX_BYTES);
    }

    #[test]
    fn test_format_error_snippet() {
        let err = format_error_snippet("connection refused");
        assert!(err.starts_with("[error: "));
        assert!(err.ends_with(']'));
    }
}
