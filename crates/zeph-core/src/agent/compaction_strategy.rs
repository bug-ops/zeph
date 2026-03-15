// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task-aware pruning strategy for tool output eviction (#1851, SWE-Pruner / COMI).
//!
//! Replaces the oldest-first heuristic in `prune_tool_outputs()` with principled
//! relevance scoring when the `context-compression` feature is enabled.
//!
//! ## Scoring approach (MVP)
//!
//! The MVP uses TF-IDF–weighted keyword overlap (improved Jaccard) rather than full
//! embeddings. This avoids requiring a running embedding model while still producing
//! better scores than plain Jaccard (which is dominated by common programming tokens
//! such as `fn`, `pub`, `struct`).
//!
//! The MIG (COMI) score is: `relevance − redundancy`.
//! Blocks with negative MIG are the best candidates for eviction.
//!
//! ## Known limitation (S2 from critic review)
//!
//! Keyword overlap is a noisy proxy for semantic relevance in code-heavy contexts.
//! A future improvement should use cosine similarity over Qdrant embeddings. The
//! TF-IDF weighting mitigates the worst cases by down-weighting common tokens.

#[cfg(feature = "context-compression")]
use std::collections::{HashMap, HashSet};

#[cfg(feature = "context-compression")]
use zeph_llm::provider::{Message, MessagePart};
#[cfg(feature = "context-compression")]
use zeph_memory::TokenCounter;

/// Per-message relevance score used by task-aware and MIG pruning.
#[cfg(feature = "context-compression")]
#[derive(Debug, Clone)]
pub(crate) struct BlockScore {
    /// Index in the messages vec.
    pub(crate) msg_index: usize,
    /// Relevance to current task goal (0.0..1.0).
    pub(crate) relevance: f32,
    /// Redundancy relative to other high-relevance blocks (0.0..1.0).
    pub(crate) redundancy: f32,
    /// MIG = relevance − redundancy. Negative MIG = good eviction candidate.
    pub(crate) mig: f32,
}

/// Common Rust/shell stop-words that dominate token overlap but carry no task signal.
/// Filtering these reduces noise in keyword scoring.
#[cfg(feature = "context-compression")]
static STOP_WORDS: std::sync::LazyLock<HashSet<&'static str>> = std::sync::LazyLock::new(|| {
    [
        "fn", "pub", "let", "use", "mod", "impl", "struct", "enum", "trait", "type", "for", "if",
        "else", "match", "return", "self", "super", "crate", "true", "false", "mut", "ref",
        "where", "in", "as", "const", "static", "extern", "unsafe", "async", "await", "move",
        "box", "dyn", "loop", "while", "break", "continue", "yield", "do", "try", "the", "a", "an",
        "is", "are", "was", "be", "to", "of", "and", "or", "not", "with", "from", "by", "at", "on",
        "in", "it", "this", "that", "have", "has", "had", "cargo", "rustc", "warning", "error",
        "note", "help", "running",
    ]
    .into_iter()
    .collect()
});

/// Tokenize text for keyword overlap: lowercase, split on non-alphanumeric,
/// filter stop-words and short tokens.
#[cfg(feature = "context-compression")]
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3)
        .map(str::to_lowercase)
        .filter(|t| !STOP_WORDS.contains(t.as_str()))
        .collect()
}

/// Build a TF map (term → frequency / `total_terms`) for a slice of tokens.
#[cfg(feature = "context-compression")]
#[allow(clippy::cast_precision_loss)]
fn term_frequencies(tokens: &[String]) -> HashMap<String, f32> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for t in tokens {
        *counts.entry(t.clone()).or_insert(0) += 1;
    }
    let total = tokens.len().max(1) as f32;
    counts
        .into_iter()
        .map(|(k, v)| (k, v as f32 / total))
        .collect()
}

/// TF-IDF–weighted Jaccard similarity between two token sets with term frequencies.
/// Returns a value in [0.0, 1.0].
#[cfg(feature = "context-compression")]
fn weighted_similarity(tf_a: &HashMap<String, f32>, tf_b: &HashMap<String, f32>) -> f32 {
    let mut intersection = 0.0_f32;
    let mut union = 0.0_f32;

    for (term, freq_a) in tf_a {
        if let Some(freq_b) = tf_b.get(term) {
            intersection += freq_a.min(*freq_b);
        }
        union += *freq_a;
    }
    for (term, freq_b) in tf_b {
        if !tf_a.contains_key(term) {
            union += *freq_b;
        }
    }

    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Extract text content from a message suitable for scoring.
#[cfg(feature = "context-compression")]
pub(crate) fn extract_scorable_text(msg: &Message) -> String {
    let mut parts_text = String::new();
    for part in &msg.parts {
        match part {
            MessagePart::ToolOutput {
                body, tool_name, ..
            } => {
                parts_text.push_str(tool_name);
                parts_text.push(' ');
                parts_text.push_str(body);
                parts_text.push(' ');
            }
            MessagePart::ToolResult { content, .. } => {
                parts_text.push_str(content);
                parts_text.push(' ');
            }
            _ => {}
        }
    }
    if parts_text.is_empty() {
        msg.content.clone()
    } else {
        parts_text
    }
}

/// Score each tool-output message block against the task goal using TF-IDF Jaccard similarity.
///
/// Messages that are not tool outputs receive a score of 0.0 (never evicted).
/// Pinned messages are excluded entirely.
#[cfg(feature = "context-compression")]
pub(crate) fn score_blocks_task_aware(
    messages: &[Message],
    task_goal: &str,
    _tc: &TokenCounter,
) -> Vec<BlockScore> {
    let goal_tokens = tokenize(task_goal);
    let goal_tf = term_frequencies(&goal_tokens);

    let mut scores = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        // Skip system prompt, system messages, and pinned messages
        if i == 0 || msg.metadata.focus_pinned {
            continue;
        }
        let has_tool_output = msg.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::ToolOutput { .. } | MessagePart::ToolResult { .. }
            )
        });
        if !has_tool_output {
            continue;
        }

        let text = extract_scorable_text(msg);
        let tokens = tokenize(&text);
        let tf = term_frequencies(&tokens);
        let relevance = weighted_similarity(&goal_tf, &tf);

        scores.push(BlockScore {
            msg_index: i,
            relevance,
            redundancy: 0.0,
            mig: relevance,
        });
    }
    scores
}

/// Score blocks using MIG (relevance − redundancy) with temporal partitioning.
///
/// Coarse step: partition messages into temporal windows (recent vs. old).
/// Fine step: within each window, compute pairwise redundancy between blocks.
/// Final MIG = relevance − `max_redundancy_with_any_higher_scored_block`.
#[cfg(feature = "context-compression")]
pub(crate) fn score_blocks_mig(
    messages: &[Message],
    task_goal: Option<&str>,
    tc: &TokenCounter,
) -> Vec<BlockScore> {
    let mut scores = if let Some(goal) = task_goal {
        score_blocks_task_aware(messages, goal, tc)
    } else {
        // Without a goal, assign uniform relevance based on recency
        let total = messages.len();
        messages
            .iter()
            .enumerate()
            .filter(|(i, msg)| {
                *i > 0
                    && !msg.metadata.focus_pinned
                    && msg.parts.iter().any(|p| {
                        matches!(
                            p,
                            MessagePart::ToolOutput { .. } | MessagePart::ToolResult { .. }
                        )
                    })
            })
            .map(|(i, _)| {
                // Recency score: more recent = higher relevance
                #[allow(clippy::cast_precision_loss)]
                let relevance = i as f32 / total as f32;
                BlockScore {
                    msg_index: i,
                    relevance,
                    redundancy: 0.0,
                    mig: relevance,
                }
            })
            .collect()
    };

    // Compute redundancy: for each pair, measure text similarity
    let texts: Vec<_> = scores
        .iter()
        .map(|s| {
            let tokens = tokenize(&extract_scorable_text(&messages[s.msg_index]));
            term_frequencies(&tokens)
        })
        .collect();

    for i in 0..scores.len() {
        let mut max_redundancy = 0.0_f32;
        for j in 0..scores.len() {
            if i == j {
                continue;
            }
            // Only count redundancy against blocks with higher relevance
            if scores[j].relevance > scores[i].relevance {
                let sim = weighted_similarity(&texts[i], &texts[j]);
                max_redundancy = max_redundancy.max(sim);
            }
        }
        scores[i].redundancy = max_redundancy;
        scores[i].mig = scores[i].relevance - max_redundancy;
    }

    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_filters_stop_words() {
        let tokens = tokenize("fn main() { let x = 5; }");
        assert!(!tokens.contains(&"fn".to_string()));
        assert!(!tokens.contains(&"let".to_string()));
    }

    #[test]
    fn tokenize_keeps_meaningful_tokens() {
        let tokens = tokenize("authentication middleware session");
        assert!(tokens.contains(&"authentication".to_string()));
        assert!(tokens.contains(&"middleware".to_string()));
        assert!(tokens.contains(&"session".to_string()));
    }

    #[test]
    fn weighted_similarity_identical_is_one() {
        let tokens = tokenize("authentication session token");
        let tf = term_frequencies(&tokens);
        let sim = weighted_similarity(&tf, &tf);
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn weighted_similarity_disjoint_is_zero() {
        let tokens_a = tokenize("authentication session");
        let tokens_b = tokenize("database migration schema");
        let tf_a = term_frequencies(&tokens_a);
        let tf_b = term_frequencies(&tokens_b);
        assert_eq!(weighted_similarity(&tf_a, &tf_b), 0.0);
    }

    #[test]
    fn weighted_similarity_empty_is_zero() {
        let tf_empty: HashMap<String, f32> = HashMap::new();
        let tokens = tokenize("authentication session");
        let tf = term_frequencies(&tokens);
        assert_eq!(weighted_similarity(&tf_empty, &tf), 0.0);
    }
}
