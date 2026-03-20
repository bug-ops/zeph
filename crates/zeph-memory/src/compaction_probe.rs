// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compaction probe: validates summary quality before committing it to the context.
//!
//! Generates factual questions from the messages being compacted, then answers them
//! using only the summary text, and scores the answers against expected values.
//! Returns a [`CompactionProbeResult`] that the caller uses to decide whether to
//! commit or reject the summary.

use std::time::Instant;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, MessagePart, Role};

use crate::error::MemoryError;

// --- Data structures ---

/// A single factual question with the expected answer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProbeQuestion {
    /// Factual question about the compacted messages.
    pub question: String,
    /// Expected correct answer extractable from the original messages.
    pub expected_answer: String,
}

/// Three-tier verdict for compaction probe quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeVerdict {
    /// Score >= `threshold`: summary preserves enough context. Proceed.
    Pass,
    /// Score in [`hard_fail_threshold`, `threshold`): summary is borderline.
    /// Proceed with compaction but log a warning.
    SoftFail,
    /// Score < `hard_fail_threshold`: summary lost critical facts. Block compaction.
    HardFail,
}

/// Full result of a compaction probe run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionProbeResult {
    /// Overall score in [0.0, 1.0].
    pub score: f32,
    /// Per-question breakdown.
    pub questions: Vec<ProbeQuestion>,
    /// LLM answers to the questions (positionally aligned with `questions`).
    pub answers: Vec<String>,
    /// Per-question similarity scores.
    pub per_question_scores: Vec<f32>,
    pub verdict: ProbeVerdict,
    /// Pass threshold used for this run.
    pub threshold: f32,
    /// Hard-fail threshold used for this run.
    pub hard_fail_threshold: f32,
    /// Model name used for the probe.
    pub model: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

// --- Structured LLM output types ---

#[derive(Debug, Deserialize, JsonSchema)]
struct ProbeQuestionsOutput {
    questions: Vec<ProbeQuestion>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProbeAnswersOutput {
    answers: Vec<String>,
}

// --- Scoring ---

/// Refusal indicators: if the actual answer contains any of these, score it 0.0.
const REFUSAL_PATTERNS: &[&str] = &[
    "unknown",
    "not mentioned",
    "not found",
    "n/a",
    "cannot determine",
    "no information",
    "not provided",
    "not specified",
    "not stated",
    "not available",
];

fn is_refusal(text: &str) -> bool {
    let lower = text.to_lowercase();
    REFUSAL_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Normalize a string: lowercase, split on non-alphanumeric chars, keep tokens >= 3 chars.
fn normalize_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(String::from)
        .collect()
}

fn jaccard(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let set_a: std::collections::HashSet<&str> = a.iter().map(String::as_str).collect();
    let set_b: std::collections::HashSet<&str> = b.iter().map(String::as_str).collect();
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        intersection as f32 / union as f32
    }
}

/// Score a single (expected, actual) answer pair using token-set-ratio.
fn score_pair(expected: &str, actual: &str) -> f32 {
    if is_refusal(actual) {
        return 0.0;
    }

    let tokens_e = normalize_tokens(expected);
    let tokens_a = normalize_tokens(actual);

    // Substring boost: if all expected tokens appear in actual, it's an exact match.
    if !tokens_e.is_empty() {
        let set_e: std::collections::HashSet<&str> = tokens_e.iter().map(String::as_str).collect();
        let set_a: std::collections::HashSet<&str> = tokens_a.iter().map(String::as_str).collect();
        if set_e.is_subset(&set_a) {
            return 1.0;
        }
    }

    // Token-set-ratio: max of three Jaccard variants.
    let j_full = jaccard(&tokens_e, &tokens_a);

    // Intersection with each set individually (handles subset relationships).
    let set_e: std::collections::HashSet<&str> = tokens_e.iter().map(String::as_str).collect();
    let set_a: std::collections::HashSet<&str> = tokens_a.iter().map(String::as_str).collect();
    let intersection: Vec<String> = set_e
        .intersection(&set_a)
        .map(|s| (*s).to_owned())
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let j_e = if tokens_e.is_empty() {
        0.0_f32
    } else {
        intersection.len() as f32 / tokens_e.len() as f32
    };
    #[allow(clippy::cast_precision_loss)]
    let j_a = if tokens_a.is_empty() {
        0.0_f32
    } else {
        intersection.len() as f32 / tokens_a.len() as f32
    };

    j_full.max(j_e).max(j_a)
}

/// Score answers against expected values using token-set-ratio similarity.
///
/// Returns `(per_question_scores, overall_average)`.
#[must_use]
pub fn score_answers(questions: &[ProbeQuestion], answers: &[String]) -> (Vec<f32>, f32) {
    if questions.is_empty() {
        return (vec![], 0.0);
    }
    let scores: Vec<f32> = questions
        .iter()
        .zip(answers.iter().chain(std::iter::repeat(&String::new())))
        .map(|(q, a)| score_pair(&q.expected_answer, a))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let avg = if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f32>() / scores.len() as f32
    };
    (scores, avg)
}

// --- LLM calls ---

/// Truncate tool-result bodies to 500 chars to avoid flooding the probe with raw output.
fn truncate_tool_bodies(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| {
            let mut msg = m.clone();
            for part in &mut msg.parts {
                if let MessagePart::ToolOutput { body, .. } = part {
                    if body.len() <= 500 {
                        continue;
                    }
                    body.truncate(500);
                    body.push('\u{2026}');
                }
            }
            msg.rebuild_content();
            msg
        })
        .collect()
}

/// Generate factual probe questions from the messages being compacted.
///
/// Uses a single LLM call with structured output. Tool-result bodies are
/// truncated to 500 chars to focus on decisions and outcomes rather than raw tool output.
///
/// # Errors
///
/// Returns `MemoryError::Llm` if the LLM call fails.
pub async fn generate_probe_questions(
    provider: &AnyProvider,
    messages: &[Message],
    max_questions: usize,
) -> Result<Vec<ProbeQuestion>, MemoryError> {
    let truncated = truncate_tool_bodies(messages);

    let mut history = String::new();
    for msg in &truncated {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };
        history.push_str(role);
        history.push_str(": ");
        history.push_str(&msg.content);
        history.push('\n');
    }

    let prompt = format!(
        "Given the following conversation excerpt, generate {max_questions} factual questions \
         that test whether a summary preserves the most important concrete details.\n\
         \n\
         Focus on:\n\
         - File paths, function names, struct/enum names that were modified or discussed\n\
         - Architectural or implementation decisions with their rationale\n\
         - Config values, API endpoints, error messages that were significant\n\
         - Action items or next steps agreed upon\n\
         \n\
         Do NOT generate questions about:\n\
         - Raw tool output content (compiler warnings, test output line numbers)\n\
         - Intermediate debugging steps that were superseded\n\
         - Opinions or reasoning that cannot be verified\n\
         \n\
         Each question must have a single unambiguous expected answer extractable from the text.\n\
         \n\
         Conversation:\n{history}\n\
         \n\
         Respond in JSON with schema: {{\"questions\": [{{\"question\": \"...\", \
         \"expected_answer\": \"...\"}}]}}"
    );

    let msgs = [Message {
        role: Role::User,
        content: prompt,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];

    let mut output: ProbeQuestionsOutput = provider
        .chat_typed_erased::<ProbeQuestionsOutput>(&msgs)
        .await
        .map_err(MemoryError::Llm)?;

    // Cap the list to max_questions: a misbehaving LLM could return more.
    output.questions.truncate(max_questions);

    Ok(output.questions)
}

/// Answer probe questions using only the compaction summary as context.
///
/// # Errors
///
/// Returns `MemoryError::Llm` if the LLM call fails.
pub async fn answer_probe_questions(
    provider: &AnyProvider,
    summary: &str,
    questions: &[ProbeQuestion],
) -> Result<Vec<String>, MemoryError> {
    let mut numbered = String::new();
    for (i, q) in questions.iter().enumerate() {
        use std::fmt::Write as _;
        let _ = writeln!(numbered, "{}. {}", i + 1, q.question);
    }

    let prompt = format!(
        "Given the following summary of a conversation, answer each question using ONLY \
         information present in the summary. If the answer is not in the summary, respond \
         with \"UNKNOWN\".\n\
         \n\
         Summary:\n{summary}\n\
         \n\
         Questions:\n{numbered}\n\
         \n\
         Respond in JSON with schema: {{\"answers\": [\"answer1\", \"answer2\", ...]}}"
    );

    let msgs = [Message {
        role: Role::User,
        content: prompt,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];

    let output: ProbeAnswersOutput = provider
        .chat_typed_erased::<ProbeAnswersOutput>(&msgs)
        .await
        .map_err(MemoryError::Llm)?;

    Ok(output.answers)
}

/// Configuration for the compaction probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionProbeConfig {
    /// Enable compaction probe validation. Default: `false`.
    pub enabled: bool,
    /// Model override for probe LLM calls. Empty string = use the summary provider.
    ///
    /// WARNING: non-Haiku models significantly increase cost per probe.
    /// With Sonnet: ~$0.01–0.03 per probe vs ~$0.001–0.003 with Haiku.
    pub model: String,
    /// Minimum score to pass without warnings. Default: `0.6`.
    /// Scores in [`hard_fail_threshold`, `threshold`) trigger `SoftFail` (warn + proceed).
    pub threshold: f32,
    /// Score below this triggers `HardFail` (block compaction). Default: `0.35`.
    pub hard_fail_threshold: f32,
    /// Maximum number of probe questions to generate. Default: `3`.
    pub max_questions: usize,
    /// Timeout for the entire probe (both LLM calls) in seconds. Default: `15`.
    pub timeout_secs: u64,
}

impl Default for CompactionProbeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            threshold: 0.6,
            hard_fail_threshold: 0.35,
            max_questions: 3,
            timeout_secs: 15,
        }
    }
}

/// Run the compaction probe: generate questions, answer them from the summary, score results.
///
/// Returns `Ok(None)` when:
/// - Probe is disabled (`config.enabled = false`)
/// - The probe times out
/// - Fewer than 2 questions are generated (insufficient statistical power)
///
/// The caller treats `None` as "no opinion" and proceeds with compaction.
///
/// # Errors
///
/// Returns `MemoryError` if an LLM call fails. Callers should treat this as non-fatal
/// and proceed with compaction.
pub async fn validate_compaction(
    provider: &AnyProvider,
    messages: &[Message],
    summary: &str,
    config: &CompactionProbeConfig,
) -> Result<Option<CompactionProbeResult>, MemoryError> {
    if !config.enabled {
        return Ok(None);
    }

    let timeout = std::time::Duration::from_secs(config.timeout_secs);
    let start = Instant::now();

    let result = tokio::time::timeout(timeout, async {
        run_probe(provider, messages, summary, config).await
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = config.timeout_secs,
                "compaction probe timed out — proceeding with compaction"
            );
            Ok(None)
        }
    }
    .map(|opt| {
        opt.map(|mut r| {
            r.duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            r
        })
    })
}

async fn run_probe(
    provider: &AnyProvider,
    messages: &[Message],
    summary: &str,
    config: &CompactionProbeConfig,
) -> Result<Option<CompactionProbeResult>, MemoryError> {
    let questions = generate_probe_questions(provider, messages, config.max_questions).await?;

    if questions.len() < 2 {
        tracing::debug!(
            count = questions.len(),
            "compaction probe: fewer than 2 questions generated — skipping probe"
        );
        return Ok(None);
    }

    let answers = answer_probe_questions(provider, summary, &questions).await?;

    let (per_question_scores, score) = score_answers(&questions, &answers);

    let verdict = if score >= config.threshold {
        ProbeVerdict::Pass
    } else if score >= config.hard_fail_threshold {
        ProbeVerdict::SoftFail
    } else {
        ProbeVerdict::HardFail
    };

    let model = provider.name().to_owned();

    Ok(Some(CompactionProbeResult {
        score,
        questions,
        answers,
        per_question_scores,
        verdict,
        threshold: config.threshold,
        hard_fail_threshold: config.hard_fail_threshold,
        model,
        duration_ms: 0, // filled in by validate_compaction
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- score_answers tests ---

    #[test]
    fn score_perfect_match() {
        let q = vec![ProbeQuestion {
            question: "What crate is used?".into(),
            expected_answer: "thiserror".into(),
        }];
        let a = vec!["thiserror".into()];
        let (scores, avg) = score_answers(&q, &a);
        assert_eq!(scores.len(), 1);
        assert!((avg - 1.0).abs() < 0.01, "expected ~1.0, got {avg}");
    }

    #[test]
    fn score_complete_mismatch() {
        let q = vec![ProbeQuestion {
            question: "What file was modified?".into(),
            expected_answer: "src/auth.rs".into(),
        }];
        let a = vec!["definitely not in the summary".into()];
        let (scores, avg) = score_answers(&q, &a);
        assert_eq!(scores.len(), 1);
        // Very low overlap expected.
        assert!(avg < 0.5, "expected low score, got {avg}");
    }

    #[test]
    fn score_refusal_is_zero() {
        let q = vec![ProbeQuestion {
            question: "What was the decision?".into(),
            expected_answer: "Use thiserror for typed errors".into(),
        }];
        for refusal in &[
            "UNKNOWN",
            "not mentioned",
            "N/A",
            "cannot determine",
            "No information",
        ] {
            let a = vec![(*refusal).to_owned()];
            let (_, avg) = score_answers(&q, &a);
            assert!(avg < 0.01, "expected 0 for refusal '{refusal}', got {avg}");
        }
    }

    #[test]
    fn score_paraphrased_answer_above_half() {
        // "thiserror was chosen for error types" vs "Use thiserror for typed errors"
        // Shared tokens: "thiserror", "error" (and maybe "for"/"types"/"typed" with >=3 chars)
        let q = vec![ProbeQuestion {
            question: "What error handling crate was chosen?".into(),
            expected_answer: "Use thiserror for typed errors in library crates".into(),
        }];
        let a = vec!["thiserror was chosen for error types in library crates".into()];
        let (_, avg) = score_answers(&q, &a);
        assert!(avg > 0.5, "expected >0.5 for paraphrase, got {avg}");
    }

    #[test]
    fn score_empty_strings() {
        let q = vec![ProbeQuestion {
            question: "What?".into(),
            expected_answer: String::new(),
        }];
        let a = vec![String::new()];
        let (scores, avg) = score_answers(&q, &a);
        assert_eq!(scores.len(), 1);
        // Both empty — jaccard of two empty sets returns 1.0 (exact match).
        assert!(
            (avg - 1.0).abs() < 0.01,
            "expected 1.0 for empty vs empty, got {avg}"
        );
    }

    #[test]
    fn score_empty_questions_list() {
        let (scores, avg) = score_answers(&[], &[]);
        assert!(scores.is_empty());
        assert!((avg - 0.0).abs() < 0.01);
    }

    #[test]
    fn score_file_path_exact() {
        let q = vec![ProbeQuestion {
            question: "Which file was modified?".into(),
            expected_answer: "crates/zeph-memory/src/compaction_probe.rs".into(),
        }];
        let a = vec!["The file crates/zeph-memory/src/compaction_probe.rs was modified.".into()];
        let (_, avg) = score_answers(&q, &a);
        // Substring boost should fire: all expected tokens present in actual.
        assert!(
            avg > 0.8,
            "expected high score for file path match, got {avg}"
        );
    }

    #[test]
    fn score_unicode_input() {
        let q = vec![ProbeQuestion {
            question: "Что было изменено?".into(),
            expected_answer: "файл config.toml".into(),
        }];
        let a = vec!["config.toml был изменён".into()];
        // Just verify no panic; score may vary.
        let (scores, _) = score_answers(&q, &a);
        assert_eq!(scores.len(), 1);
    }

    // --- verdict threshold tests ---

    #[test]
    fn verdict_thresholds() {
        let config = CompactionProbeConfig::default();

        // Pass >= 0.6
        let score = 0.7_f32;
        let verdict = if score >= config.threshold {
            ProbeVerdict::Pass
        } else if score >= config.hard_fail_threshold {
            ProbeVerdict::SoftFail
        } else {
            ProbeVerdict::HardFail
        };
        assert_eq!(verdict, ProbeVerdict::Pass);

        // SoftFail [0.35, 0.6)
        let score = 0.5_f32;
        let verdict = if score >= config.threshold {
            ProbeVerdict::Pass
        } else if score >= config.hard_fail_threshold {
            ProbeVerdict::SoftFail
        } else {
            ProbeVerdict::HardFail
        };
        assert_eq!(verdict, ProbeVerdict::SoftFail);

        // HardFail < 0.35
        let score = 0.2_f32;
        let verdict = if score >= config.threshold {
            ProbeVerdict::Pass
        } else if score >= config.hard_fail_threshold {
            ProbeVerdict::SoftFail
        } else {
            ProbeVerdict::HardFail
        };
        assert_eq!(verdict, ProbeVerdict::HardFail);
    }

    // --- config defaults ---

    #[test]
    fn config_defaults() {
        let c = CompactionProbeConfig::default();
        assert!(!c.enabled);
        assert!(c.model.is_empty());
        assert!((c.threshold - 0.6).abs() < 0.001);
        assert!((c.hard_fail_threshold - 0.35).abs() < 0.001);
        assert_eq!(c.max_questions, 3);
        assert_eq!(c.timeout_secs, 15);
    }

    // --- serde round-trip ---

    #[test]
    fn config_serde_round_trip() {
        let original = CompactionProbeConfig {
            enabled: true,
            model: "claude-haiku-4-5-20251001".into(),
            threshold: 0.65,
            hard_fail_threshold: 0.4,
            max_questions: 5,
            timeout_secs: 20,
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: CompactionProbeConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(restored.enabled);
        assert_eq!(restored.model, "claude-haiku-4-5-20251001");
        assert!((restored.threshold - 0.65).abs() < 0.001);
    }

    #[test]
    fn probe_result_serde_round_trip() {
        let result = CompactionProbeResult {
            score: 0.75,
            questions: vec![ProbeQuestion {
                question: "What?".into(),
                expected_answer: "thiserror".into(),
            }],
            answers: vec!["thiserror".into()],
            per_question_scores: vec![1.0],
            verdict: ProbeVerdict::Pass,
            threshold: 0.6,
            hard_fail_threshold: 0.35,
            model: "haiku".into(),
            duration_ms: 1234,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let restored: CompactionProbeResult = serde_json::from_str(&json).expect("deserialize");
        assert!((restored.score - 0.75).abs() < 0.001);
        assert_eq!(restored.verdict, ProbeVerdict::Pass);
    }

    // --- fewer answers than questions (LLM returned truncated list) ---

    #[test]
    fn score_fewer_answers_than_questions() {
        let questions = vec![
            ProbeQuestion {
                question: "What crate?".into(),
                expected_answer: "thiserror".into(),
            },
            ProbeQuestion {
                question: "What file?".into(),
                expected_answer: "src/lib.rs".into(),
            },
            ProbeQuestion {
                question: "What decision?".into(),
                expected_answer: "use async traits".into(),
            },
        ];
        // LLM only returned 1 answer for 3 questions.
        let answers = vec!["thiserror".into()];
        let (scores, avg) = score_answers(&questions, &answers);
        // scores must have the same length as questions (missing answers → empty string → ~0).
        assert_eq!(scores.len(), 3);
        // First answer is a perfect match.
        assert!(
            (scores[0] - 1.0).abs() < 0.01,
            "first score should be ~1.0, got {}",
            scores[0]
        );
        // Missing answers score 0 (empty string vs non-empty expected).
        assert!(
            scores[1] < 0.5,
            "second score should be low for missing answer, got {}",
            scores[1]
        );
        assert!(
            scores[2] < 0.5,
            "third score should be low for missing answer, got {}",
            scores[2]
        );
        // Average is dragged down by the two missing answers.
        assert!(
            avg < 0.5,
            "average should be below 0.5 with 2 missing answers, got {avg}"
        );
    }

    // --- exact boundary values for threshold ---

    #[test]
    fn verdict_boundary_at_threshold() {
        let config = CompactionProbeConfig::default();

        // Exactly at pass threshold → Pass.
        let score = config.threshold;
        let verdict = if score >= config.threshold {
            ProbeVerdict::Pass
        } else if score >= config.hard_fail_threshold {
            ProbeVerdict::SoftFail
        } else {
            ProbeVerdict::HardFail
        };
        assert_eq!(verdict, ProbeVerdict::Pass);

        // One ULP below pass threshold, above hard-fail → SoftFail.
        let score = config.threshold - f32::EPSILON;
        let verdict = if score >= config.threshold {
            ProbeVerdict::Pass
        } else if score >= config.hard_fail_threshold {
            ProbeVerdict::SoftFail
        } else {
            ProbeVerdict::HardFail
        };
        assert_eq!(verdict, ProbeVerdict::SoftFail);

        // Exactly at hard-fail threshold → SoftFail (boundary is inclusive).
        let score = config.hard_fail_threshold;
        let verdict = if score >= config.threshold {
            ProbeVerdict::Pass
        } else if score >= config.hard_fail_threshold {
            ProbeVerdict::SoftFail
        } else {
            ProbeVerdict::HardFail
        };
        assert_eq!(verdict, ProbeVerdict::SoftFail);

        // One ULP below hard-fail threshold → HardFail.
        let score = config.hard_fail_threshold - f32::EPSILON;
        let verdict = if score >= config.threshold {
            ProbeVerdict::Pass
        } else if score >= config.hard_fail_threshold {
            ProbeVerdict::SoftFail
        } else {
            ProbeVerdict::HardFail
        };
        assert_eq!(verdict, ProbeVerdict::HardFail);
    }

    // --- config partial deserialization (serde default fields) ---

    #[test]
    fn config_partial_json_uses_defaults() {
        // Only `enabled` is specified; all other fields must fall back to defaults via #[serde(default)].
        let json = r#"{"enabled": true}"#;
        let c: CompactionProbeConfig =
            serde_json::from_str(json).expect("deserialize partial json");
        assert!(c.enabled);
        assert!(c.model.is_empty());
        assert!((c.threshold - 0.6).abs() < 0.001);
        assert!((c.hard_fail_threshold - 0.35).abs() < 0.001);
        assert_eq!(c.max_questions, 3);
        assert_eq!(c.timeout_secs, 15);
    }

    #[test]
    fn config_empty_json_uses_all_defaults() {
        let c: CompactionProbeConfig = serde_json::from_str("{}").expect("deserialize empty json");
        assert!(!c.enabled);
        assert!(c.model.is_empty());
    }
}
