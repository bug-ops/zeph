// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task-aware pruning strategy for tool output eviction.
//!
//! Provides relevance scoring, subgoal tracking, density classification, and
//! focus auto-consolidation. These types and functions are used by the
//! [`crate::service::ContextService`] summarization path and are referenced from
//! `zeph-core` via re-export.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};
use zeph_memory::TokenCounter;

// ── Scoring ───────────────────────────────────────────────────────────────────

/// Per-message relevance score used by task-aware and MIG pruning.
#[derive(Debug, Clone)]
pub struct BlockScore {
    /// Index in the messages vec.
    pub msg_index: usize,
    /// Relevance to current task goal (0.0..1.0).
    pub relevance: f32,
    /// Redundancy relative to other high-relevance blocks (0.0..1.0).
    pub redundancy: f32,
    /// MIG = relevance − redundancy. Negative MIG = good eviction candidate.
    pub mig: f32,
}

/// Common Rust/shell stop-words that dominate token overlap but carry no task signal.
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

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3)
        .map(str::to_lowercase)
        .filter(|t| !STOP_WORDS.contains(t.as_str()))
        .collect()
}

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

fn tf_weighted_similarity(tf_a: &HashMap<String, f32>, tf_b: &HashMap<String, f32>) -> f32 {
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
#[must_use]
pub fn extract_scorable_text(msg: &Message) -> String {
    let mut parts_text = String::new();
    for part in &msg.parts {
        match part {
            MessagePart::ToolOutput {
                body, tool_name, ..
            } => {
                parts_text.push_str(tool_name.as_str());
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

/// Score each tool-output message block against the task goal.
#[must_use]
pub fn score_blocks_task_aware(
    messages: &[Message],
    task_goal: &str,
    _tc: &TokenCounter,
) -> Vec<BlockScore> {
    let goal_tokens = tokenize(task_goal);
    let goal_tf = term_frequencies(&goal_tokens);
    let mut scores = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
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
        let relevance = tf_weighted_similarity(&goal_tf, &tf);
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
#[must_use]
pub fn score_blocks_mig(
    messages: &[Message],
    task_goal: Option<&str>,
    tc: &TokenCounter,
) -> Vec<BlockScore> {
    #[allow(clippy::cast_precision_loss)]
    let mut scores = if let Some(goal) = task_goal {
        score_blocks_task_aware(messages, goal, tc)
    } else {
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
            if scores[j].relevance > scores[i].relevance {
                let sim = tf_weighted_similarity(&texts[i], &texts[j]);
                max_redundancy = max_redundancy.max(sim);
            }
        }
        scores[i].redundancy = max_redundancy;
        scores[i].mig = scores[i].relevance - max_redundancy;
    }
    scores
}

/// Score each tool-output message block by subgoal tier membership.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn score_blocks_subgoal(
    messages: &[Message],
    registry: &SubgoalRegistry,
    _tc: &TokenCounter,
) -> Vec<BlockScore> {
    let total = messages.len().max(1) as f32;
    let mut scores = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
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
        let recency = i as f32 / total * 0.05;
        let relevance = match registry.subgoal_state(i) {
            Some(SubgoalState::Active) => 1.0_f32 + recency,
            Some(SubgoalState::Completed) => 0.3_f32 + recency,
            None => 0.1_f32 + recency,
        };
        scores.push(BlockScore {
            msg_index: i,
            relevance,
            redundancy: 0.0,
            mig: relevance,
        });
    }
    scores
}

/// Score tool-output blocks using subgoal tiers combined with MIG redundancy.
#[must_use]
pub fn score_blocks_subgoal_mig(
    messages: &[Message],
    registry: &SubgoalRegistry,
    tc: &TokenCounter,
) -> Vec<BlockScore> {
    let mut scores = score_blocks_subgoal(messages, registry, tc);
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
            if scores[j].relevance > scores[i].relevance {
                let sim = tf_weighted_similarity(&texts[i], &texts[j]);
                max_redundancy = max_redundancy.max(sim);
            }
        }
        scores[i].redundancy = max_redundancy;
        scores[i].mig = scores[i].relevance - max_redundancy;
    }
    scores
}

// ── SubgoalRegistry ───────────────────────────────────────────────────────────

/// Unique identifier for a subgoal within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubgoalId(pub u32);

/// Lifecycle state of a subgoal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubgoalState {
    /// Currently being worked on. Messages tagged with this subgoal are protected.
    Active,
    /// Completed. Messages tagged with this subgoal are candidates for summarization.
    Completed,
}

/// A tracked subgoal with message span.
#[derive(Debug, Clone)]
pub struct Subgoal {
    pub id: SubgoalId,
    pub description: String,
    pub state: SubgoalState,
    /// Index of the first message in this subgoal's span.
    pub start_msg_index: usize,
    /// Index of the last message known to belong to this subgoal.
    pub end_msg_index: usize,
}

/// In-memory registry of all subgoals in the current session.
///
/// Not persisted across restarts — subgoal state is transient session data.
#[derive(Debug, Default)]
pub struct SubgoalRegistry {
    pub subgoals: Vec<Subgoal>,
    next_id: u32,
    /// Maps message index → subgoal ID for fast lookup during compaction.
    pub msg_to_subgoal: std::collections::HashMap<usize, SubgoalId>,
    last_tagged_index: usize,
}

impl SubgoalRegistry {
    /// Register a new active subgoal starting at the given message index.
    ///
    /// Auto-completes any existing Active subgoal before creating the new one.
    pub fn push_active(&mut self, description: String, start_msg_index: usize) -> SubgoalId {
        if let Some(active) = self
            .subgoals
            .iter_mut()
            .find(|s| s.state == SubgoalState::Active)
        {
            active.state = SubgoalState::Completed;
        }
        let id = SubgoalId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.subgoals.push(Subgoal {
            id,
            description,
            state: SubgoalState::Active,
            start_msg_index,
            end_msg_index: start_msg_index,
        });
        self.last_tagged_index = start_msg_index.saturating_sub(1);
        id
    }

    /// Mark the current active subgoal as completed and assign an end boundary.
    pub fn complete_active(&mut self, end_msg_index: usize) {
        if let Some(active) = self
            .subgoals
            .iter_mut()
            .find(|s| s.state == SubgoalState::Active)
        {
            active.state = SubgoalState::Completed;
            active.end_msg_index = end_msg_index;
        }
    }

    /// Extend the active subgoal to cover new messages up to `new_end`.
    pub fn extend_active(&mut self, new_end: usize) {
        if let Some(active) = self
            .subgoals
            .iter_mut()
            .find(|s| s.state == SubgoalState::Active)
        {
            active.end_msg_index = new_end;
            let start = self.last_tagged_index.saturating_add(1);
            for idx in start..=new_end {
                self.msg_to_subgoal.insert(idx, active.id);
            }
            if new_end >= start {
                self.last_tagged_index = new_end;
            }
        }
    }

    /// Tag messages in range `[start, end]` with the given subgoal ID.
    pub fn tag_range(&mut self, start: usize, end: usize, id: SubgoalId) {
        for idx in start..=end {
            self.msg_to_subgoal.insert(idx, id);
        }
        if end > self.last_tagged_index {
            self.last_tagged_index = end;
        }
    }

    /// Get the subgoal state for a given message index.
    #[must_use]
    pub fn subgoal_state(&self, msg_index: usize) -> Option<SubgoalState> {
        let sg_id = self.msg_to_subgoal.get(&msg_index)?;
        self.subgoals
            .iter()
            .find(|s| &s.id == sg_id)
            .map(|s| s.state)
    }

    /// Get the current active subgoal (for debug output and TUI metrics).
    #[must_use]
    pub fn active_subgoal(&self) -> Option<&Subgoal> {
        self.subgoals
            .iter()
            .find(|s| s.state == SubgoalState::Active)
    }

    /// Rebuild the registry after compaction.
    ///
    /// When `old_compact_end == 0`, repairs shifted indices without dropping subgoals.
    /// When `old_compact_end > 0`, drops subgoals whose entire span was drained.
    pub fn rebuild_after_compaction(&mut self, messages: &[Message], old_compact_end: usize) {
        self.msg_to_subgoal.clear();
        if self.subgoals.is_empty() {
            self.last_tagged_index = 0;
            return;
        }
        if old_compact_end > 0 {
            self.subgoals
                .retain(|s| s.state == SubgoalState::Active || s.end_msg_index >= old_compact_end);
        }
        if self.subgoals.is_empty() {
            self.last_tagged_index = 0;
            return;
        }
        let mut last_idx = 0usize;
        for (i, _msg) in messages.iter().enumerate().skip(1) {
            let id = self
                .subgoals
                .iter()
                .filter(|s| s.state == SubgoalState::Active)
                .find(|s| i >= s.start_msg_index && i <= s.end_msg_index)
                .map(|s| s.id)
                .or_else(|| {
                    self.subgoals
                        .iter()
                        .filter(|s| s.state == SubgoalState::Completed)
                        .find(|s| i >= s.start_msg_index && i <= s.end_msg_index)
                        .map(|s| s.id)
                });
            if let Some(id) = id {
                self.msg_to_subgoal.insert(i, id);
                last_idx = i;
            }
        }
        self.last_tagged_index = last_idx;
    }
}

// ── ContentDensity ────────────────────────────────────────────────────────────

/// Density classification for a message or segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentDensity {
    /// More than 50% of lines are structured (code fences, JSON, lists, shell output).
    High,
    /// 50% or fewer lines are structured.
    Low,
}

/// Classify a message's content density.
#[must_use]
pub fn classify_density(content: &str) -> ContentDensity {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return ContentDensity::Low;
    }
    let structured = lines
        .iter()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("```")
                || trimmed.starts_with("~~~")
                || trimmed.starts_with('{')
                || trimmed.starts_with('[')
                || trimmed.starts_with('|')
                || trimmed.starts_with('$')
                || trimmed.starts_with('>')
                || trimmed.starts_with('#')
                || (line.len() >= 4 && line.starts_with("    "))
        })
        .count();
    #[allow(clippy::cast_precision_loss)]
    let ratio = structured as f32 / lines.len() as f32;
    if ratio > 0.5 {
        ContentDensity::High
    } else {
        ContentDensity::Low
    }
}

/// Partition messages into (high-density, low-density) groups.
#[must_use]
pub fn partition_by_density(messages: &[Message]) -> (Vec<Message>, Vec<Message>) {
    let mut high = Vec::new();
    let mut low = Vec::new();
    for msg in messages {
        if msg.metadata.focus_pinned {
            continue;
        }
        match classify_density(&msg.content) {
            ContentDensity::High => high.push(msg.clone()),
            ContentDensity::Low => low.push(msg.clone()),
        }
    }
    (high, low)
}

// ── SubgoalExtractionResult ───────────────────────────────────────────────────

/// Output of a background subgoal extraction LLM call.
#[derive(Debug)]
pub struct SubgoalExtractionResult {
    /// Current subgoal the agent is working toward.
    pub current: String,
    /// Just-completed subgoal, if the LLM detected a transition (`COMPLETED:` non-NONE).
    pub completed: Option<String>,
}

// ── Focus auto-consolidation ──────────────────────────────────────────────────

/// Automatically consolidate low-relevance context into a knowledge-block summary.
///
/// # Errors
///
/// Returns an error if the provider call returns an error or if the 20-second timeout
/// elapses before the provider responds.
pub async fn run_focus_auto_consolidation(
    messages: &[Message],
    min_window: usize,
    provider: impl LlmProvider,
    max_chars: usize,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let _span = tracing::info_span!("ctx.compaction.focus_auto_consolidate").entered();

    if messages.len() < min_window {
        return Ok(None);
    }
    let task_goal = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map_or("", |m| m.content.as_str());
    if task_goal.is_empty() {
        tracing::debug!("focus_auto_consolidation: no user message found, skipping");
        return Ok(None);
    }
    let messages_owned: Vec<Message> = messages.to_vec();
    let task_goal_owned = task_goal.to_string();
    let scores = tokio::task::spawn_blocking(move || {
        let tc = TokenCounter::default();
        score_blocks_mig(
            &messages_owned,
            Some(task_goal_owned.as_str()).filter(|s| !s.is_empty()),
            &tc,
        )
    })
    .await
    .map_err(|e| format!("score_blocks_mig panicked: {e}"))?;

    let low_relevance: HashSet<usize> = scores
        .iter()
        .filter(|s| s.mig <= 0.0)
        .map(|s| s.msg_index)
        .collect();
    let window_indices = find_low_relevance_window(messages, &low_relevance, min_window);
    if window_indices.is_empty() {
        return Ok(None);
    }
    let combined: String = window_indices
        .iter()
        .map(|&i| extract_scorable_text(&messages[i]))
        .collect::<Vec<_>>()
        .join("\n---\n");
    let prompt = format!(
        "Extract up to 10 key facts the agent must remember from the following context. \
         Return bullet points only (one per line, starting with `- `).\n\n{combined}"
    );
    let request = vec![Message::from_legacy(Role::User, &prompt)];
    let raw = tokio::time::timeout(Duration::from_secs(20), provider.chat(&request))
        .await
        .map_err(|_| {
            Box::new(std::io::Error::other(
                "focus auto-consolidation timed out after 20s",
            )) as Box<dyn std::error::Error + Send + Sync>
        })?
        .map_err(|e| {
            Box::new(std::io::Error::other(format!(
                "focus auto-consolidation provider error: {e}"
            ))) as Box<dyn std::error::Error + Send + Sync>
        })?;
    let truncated = if raw.len() <= max_chars {
        raw
    } else {
        let boundary = raw
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= max_chars)
            .last()
            .unwrap_or(0);
        raw[..boundary].to_owned()
    };
    if truncated.is_empty() {
        return Ok(None);
    }
    Ok(Some(truncated))
}

fn find_low_relevance_window(
    messages: &[Message],
    low_relevance: &HashSet<usize>,
    min_window: usize,
) -> Vec<usize> {
    let mut best: Vec<usize> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if i == 0 || msg.metadata.focus_pinned {
            current.clear();
            continue;
        }
        if low_relevance.contains(&i) {
            current.push(i);
        } else {
            if current.len() >= min_window && best.is_empty() {
                best.append(&mut current);
            }
            current.clear();
        }
    }
    if current.len() >= min_window && best.is_empty() {
        best = current;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
    fn tf_weighted_similarity_identical_is_one() {
        let tokens = tokenize("authentication session token");
        let tf = term_frequencies(&tokens);
        let sim = tf_weighted_similarity(&tf, &tf);
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn tf_weighted_similarity_disjoint_is_zero() {
        let tokens_a = tokenize("authentication session");
        let tokens_b = tokenize("database migration schema");
        let tf_a = term_frequencies(&tokens_a);
        let tf_b = term_frequencies(&tokens_b);
        assert!(tf_weighted_similarity(&tf_a, &tf_b).abs() < f32::EPSILON);
    }

    #[test]
    fn tf_weighted_similarity_empty_is_zero() {
        let tf_empty: HashMap<String, f32> = HashMap::new();
        let tokens = tokenize("authentication session");
        let tf = term_frequencies(&tokens);
        assert!(tf_weighted_similarity(&tf_empty, &tf).abs() < f32::EPSILON);
    }

    fn make_tool_output_msg(body: &str) -> Message {
        use zeph_llm::provider::{MessageMetadata, MessagePart};
        let mut msg = Message {
            role: Role::User,
            content: body.to_string(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: body.to_string(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        msg.rebuild_content();
        msg
    }

    #[test]
    fn score_blocks_task_aware_skips_system_prompt() {
        let tc = TokenCounter::default();
        let messages = vec![
            Message::from_legacy(Role::System, "system prompt"),
            make_tool_output_msg("authentication session middleware"),
        ];
        let scores = score_blocks_task_aware(&messages, "authentication session", &tc);
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].msg_index, 1);
    }

    #[test]
    fn score_blocks_task_aware_skips_pinned_messages() {
        use zeph_llm::provider::MessageMetadata;
        let tc = TokenCounter::default();
        let mut pinned_meta = MessageMetadata::focus_pinned();
        pinned_meta.focus_pinned = true;
        let pinned = Message {
            role: Role::System,
            content: "authentication session knowledge".to_string(),
            parts: vec![],
            metadata: pinned_meta,
        };
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            pinned,
            make_tool_output_msg("authentication session"),
        ];
        let scores = score_blocks_task_aware(&messages, "authentication session", &tc);
        assert!(scores.iter().all(|s| s.msg_index != 1));
    }

    #[test]
    fn score_blocks_task_aware_relevant_block_scores_higher() {
        let tc = TokenCounter::default();
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            make_tool_output_msg("authentication middleware session token implementation"),
            make_tool_output_msg("database schema migration foreign key index"),
        ];
        let scores = score_blocks_task_aware(&messages, "authentication session token", &tc);
        assert_eq!(scores.len(), 2);
        let auth_score = scores.iter().find(|s| s.msg_index == 1).unwrap();
        let db_score = scores.iter().find(|s| s.msg_index == 2).unwrap();
        assert!(
            auth_score.relevance > db_score.relevance,
            "auth block must score higher than db block"
        );
    }

    #[test]
    fn subgoal_registry_push_active_creates_active_subgoal() {
        let mut registry = SubgoalRegistry::default();
        let id = registry.push_active("Implement login endpoint".into(), 1);
        assert_eq!(registry.subgoals.len(), 1);
        assert_eq!(registry.subgoals[0].id, id);
        assert_eq!(registry.subgoals[0].state, SubgoalState::Active);
    }

    #[test]
    fn subgoal_registry_complete_active_transitions_state() {
        let mut registry = SubgoalRegistry::default();
        registry.push_active("initial subgoal".into(), 1);
        registry.complete_active(5);
        assert_eq!(registry.subgoals[0].state, SubgoalState::Completed);
        assert!(registry.active_subgoal().is_none());
    }

    #[test]
    fn subgoal_registry_push_active_auto_completes_existing_active() {
        let mut registry = SubgoalRegistry::default();
        registry.push_active("first subgoal".into(), 1);
        registry.push_active("second subgoal".into(), 6);
        assert_eq!(registry.subgoals[0].state, SubgoalState::Completed);
        assert_eq!(registry.subgoals[1].state, SubgoalState::Active);
        let active_count = registry
            .subgoals
            .iter()
            .filter(|s| s.state == SubgoalState::Active)
            .count();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn subgoal_registry_extend_active_tags_incrementally() {
        let mut registry = SubgoalRegistry::default();
        let id = registry.push_active("subgoal".into(), 3);
        registry.extend_active(5);
        assert_eq!(registry.subgoal_state(3), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(4), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(5), Some(SubgoalState::Active));
        assert_eq!(registry.msg_to_subgoal.get(&3), Some(&id));
        registry.extend_active(7);
        assert_eq!(registry.subgoal_state(6), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(7), Some(SubgoalState::Active));
        assert_eq!(registry.msg_to_subgoal.len(), 5);
    }

    #[test]
    fn subgoal_registry_subgoal_state_returns_correct_tier() {
        let mut registry = SubgoalRegistry::default();
        registry.push_active("completed subgoal".into(), 1);
        registry.tag_range(1, 5, SubgoalId(0));
        registry.complete_active(5);
        registry.push_active("active subgoal".into(), 6);
        registry.extend_active(9);
        assert_eq!(registry.subgoal_state(1), Some(SubgoalState::Completed));
        assert_eq!(registry.subgoal_state(6), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(0), None);
    }

    #[test]
    fn classify_density_empty_string_is_low() {
        assert_eq!(classify_density(""), ContentDensity::Low);
    }

    #[test]
    fn classify_density_all_structured_is_high() {
        let content = "```rust\nfn main() {}\n```\n$ cargo build\n";
        assert_eq!(classify_density(content), ContentDensity::High);
    }

    #[test]
    fn classify_density_all_prose_is_low() {
        let content = "This is a sentence.\nAnother sentence here.\nNo structured content at all.";
        assert_eq!(classify_density(content), ContentDensity::Low);
    }

    // ─── run_focus_auto_consolidation tests ──────────────────────────────────

    struct StubProvider {
        response: &'static str,
    }

    impl zeph_llm::provider::LlmProvider for StubProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, zeph_llm::LlmError> {
            Ok(self.response.to_owned())
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
        ) -> Result<zeph_llm::provider::ChatStream, zeph_llm::LlmError> {
            let r = self.chat(messages).await?;
            Ok(Box::pin(futures::stream::once(async move {
                Ok::<_, zeph_llm::LlmError>(zeph_llm::provider::StreamChunk::Content(r))
            })))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, zeph_llm::LlmError> {
            Ok(vec![])
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "stub"
        }
    }

    struct HangingProvider;

    impl zeph_llm::provider::LlmProvider for HangingProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, zeph_llm::LlmError> {
            std::future::pending::<()>().await;
            unreachable!()
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
        ) -> Result<zeph_llm::provider::ChatStream, zeph_llm::LlmError> {
            std::future::pending::<()>().await;
            unreachable!()
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, zeph_llm::LlmError> {
            Ok(vec![])
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "hanging"
        }
    }

    #[tokio::test]
    async fn run_focus_auto_consolidation_returns_none_for_small_history() {
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            make_tool_output_msg("some tool output here"),
        ];
        // min_window = 6, but only 2 messages → None.
        let result = run_focus_auto_consolidation(
            &messages,
            6,
            StubProvider {
                response: "- fact one",
            },
            4096,
        )
        .await
        .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_focus_auto_consolidation_produces_summary() {
        let mut messages = vec![Message::from_legacy(Role::System, "sys")];
        for _ in 0..6 {
            messages.push(make_tool_output_msg(
                "database schema migration foreign key index",
            ));
        }
        messages.push(Message::from_legacy(
            Role::User,
            "Help me with authentication",
        ));

        let result = run_focus_auto_consolidation(
            &messages,
            4,
            StubProvider {
                response: "- database schema uses foreign keys",
            },
            4096,
        )
        .await
        .unwrap();

        assert!(result.is_some());
        let summary = result.unwrap();
        assert!(!summary.is_empty());
    }

    #[tokio::test]
    async fn run_focus_auto_consolidation_skips_when_no_user_message() {
        // S2/S3: when no User message is present, must return None instead of
        // entering recency mode and eagerly consolidating all history.
        let mut messages = vec![Message::from_legacy(Role::System, "sys")];
        for i in 0..8 {
            messages.push(make_tool_output_msg(&format!("tool output {i}")));
        }

        let result = run_focus_auto_consolidation(
            &messages,
            4,
            StubProvider {
                response: "- should not be reached",
            },
            4096,
        )
        .await
        .unwrap();

        assert!(
            result.is_none(),
            "must return None when no user message is present (S2/S3)"
        );
    }

    #[tokio::test]
    async fn auto_consolidation_timeout_recovers() {
        let mut messages = vec![Message::from_legacy(Role::System, "sys")];
        for _ in 0..6 {
            messages.push(make_tool_output_msg(
                "database schema migration foreign key index",
            ));
        }
        messages.push(Message::from_legacy(
            Role::User,
            "Help me with authentication",
        ));

        // Wrap in a short timeout to avoid waiting the full 20s internal timeout.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            run_focus_auto_consolidation(&messages, 4, HangingProvider, 4096),
        )
        .await;

        // Either: outer timeout fires (Err), or inner 20s timeout fires (Ok(Err)).
        // Both cases must not panic.
        match result {
            Err(_elapsed) => {
                // Outer timeout fired — no panic, correct.
            }
            Ok(inner) => {
                assert!(inner.is_err(), "hanging provider must return an error");
            }
        }
    }
}
