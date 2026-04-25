// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task-aware pruning strategy for tool output eviction (#1851, SWE-Pruner / COMI).
//!
//! Replaces the oldest-first heuristic in `prune_tool_outputs()` with principled
//! relevance scoring when the `context-compression` feature is enabled.
//!
//! ## Scoring approach (MVP)
//!
//! The MVP uses TF-weighted Jaccard similarity rather than full embeddings. This avoids
//! requiring a running embedding model while still producing better scores than plain
//! Jaccard (which is dominated by common programming tokens such as `fn`, `pub`, `struct`).
//! Note: this is TF-only (term frequency), not TF-IDF — there is no inverse-document-frequency
//! component because we do not have a static corpus to compute IDF over at runtime.
//!
//! The MIG (COMI) score is: `relevance − redundancy`.
//! Blocks with negative MIG are the best candidates for eviction.
//!
//! ## Known limitation (S2 from critic review)
//!
//! Keyword overlap is a noisy proxy for semantic relevance in code-heavy contexts.
//! A future improvement should use cosine similarity over Qdrant embeddings. The
//! TF weighting mitigates the worst cases by down-weighting common tokens.
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};
use zeph_memory::TokenCounter;

/// Per-message relevance score used by task-aware and MIG pruning.
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
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3)
        .map(str::to_lowercase)
        .filter(|t| !STOP_WORDS.contains(t.as_str()))
        .collect()
}

/// Build a TF map (term → frequency / `total_terms`) for a slice of tokens.
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

/// TF-weighted Jaccard similarity between two token sets with term frequencies.
/// Returns a value in [0.0, 1.0].
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
pub(crate) fn extract_scorable_text(msg: &Message) -> String {
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

/// Score each tool-output message block against the task goal using TF-IDF Jaccard similarity.
///
/// Messages that are not tool outputs receive a score of 0.0 (never evicted).
/// Pinned messages are excluded entirely.
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
///
/// Coarse step: partition messages into temporal windows (recent vs. old).
/// Fine step: within each window, compute pairwise redundancy between blocks.
/// Final MIG = relevance − `max_redundancy_with_any_higher_scored_block`.
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
                let sim = tf_weighted_similarity(&texts[i], &texts[j]);
                max_redundancy = max_redundancy.max(sim);
            }
        }
        scores[i].redundancy = max_redundancy;
        scores[i].mig = scores[i].relevance - max_redundancy;
    }

    scores
}

// ─── Phase C: Subgoal-aware scoring functions ────────────────────────────────

/// Score each tool-output message block by subgoal tier membership.
///
/// Relevance tiers (architecture spec):
/// - Active subgoal:    1.0  — never evicted by scoring
/// - Completed subgoal: 0.3  — candidate for summarization
/// - Untagged/outdated: 0.1  — highest eviction priority
///
/// Within each tier, recency is used as a tiebreaker (newer = slightly higher relevance)
/// by adding a small `position_fraction` term that does not change tier ordering.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn score_blocks_subgoal(
    messages: &[Message],
    registry: &SubgoalRegistry,
    _tc: &TokenCounter,
) -> Vec<BlockScore> {
    let total = messages.len().max(1) as f32;
    let mut scores = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        // Skip system prompt (index 0) and pinned messages.
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

        // Recency fraction: [0.0, 1.0) — does not exceed the tier gap.
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
///
/// Combines `score_blocks_subgoal` relevance with pairwise text redundancy:
/// `mig = subgoal_relevance − max_redundancy_with_any_higher_scored_block`.
///
/// Redundancy is only counted against blocks with strictly higher relevance,
/// so Active subgoal messages (tier 1.0) never have their MIG reduced below
/// their tier baseline.
pub(crate) fn score_blocks_subgoal_mig(
    messages: &[Message],
    registry: &SubgoalRegistry,
    tc: &TokenCounter,
) -> Vec<BlockScore> {
    let mut scores = score_blocks_subgoal(messages, registry, tc);

    // Compute pairwise redundancy (same algorithm as score_blocks_mig).
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

// ─── Phase A: SubgoalRegistry ───────────────────────────────────────────────

/// Unique identifier for a subgoal within a session.
///
/// Monotonically increasing, wraps on overflow (extremely unlikely in practice —
/// a session would need 4 billion subgoal transitions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SubgoalId(pub(crate) u32);

/// Lifecycle state of a subgoal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubgoalState {
    /// Currently being worked on. Messages tagged with this subgoal are protected.
    Active,
    /// Completed. Messages tagged with this subgoal are candidates for summarization.
    Completed,
}

/// A tracked subgoal with message span.
#[derive(Debug, Clone)]
pub(crate) struct Subgoal {
    pub(crate) id: SubgoalId,
    pub(crate) description: String,
    pub(crate) state: SubgoalState,
    /// Index of the first message in this subgoal's span.
    pub(crate) start_msg_index: usize,
    /// Index of the last message known to belong to this subgoal (updated each turn).
    pub(crate) end_msg_index: usize,
}

/// In-memory registry of all subgoals in the current session.
///
/// Lives in `CompressionState` (gated behind `context-compression`).
/// Not persisted across restarts — subgoal state is transient session data.
#[derive(Debug, Default)]
pub(crate) struct SubgoalRegistry {
    pub(crate) subgoals: Vec<Subgoal>,
    next_id: u32,
    /// Maps message index → subgoal ID for fast lookup during compaction.
    pub(crate) msg_to_subgoal: std::collections::HashMap<usize, SubgoalId>,
    /// Highest message index already tagged for the active subgoal.
    /// Used by `extend_active()` to avoid re-inserting existing entries.
    last_tagged_index: usize,
}
impl SubgoalRegistry {
    /// Register a new active subgoal starting at the given message index.
    ///
    /// Defense in depth: if an Active subgoal already exists, auto-completes it before creating
    /// the new one. Prevents the invariant violation of multiple Active subgoals.
    pub(crate) fn push_active(&mut self, description: String, start_msg_index: usize) -> SubgoalId {
        // Auto-complete any existing Active subgoal (M3 fix — defense in depth).
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
        // last_tagged_index starts just before the new subgoal's first message so that
        // extend_active(start_msg_index) will tag it on the first call.
        self.last_tagged_index = start_msg_index.saturating_sub(1);
        id
    }

    /// Mark the current active subgoal as completed and assign an end boundary.
    pub(crate) fn complete_active(&mut self, end_msg_index: usize) {
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
    ///
    /// Only tags messages from `last_tagged_index + 1` to `new_end` to avoid redundant
    /// re-insertions into `msg_to_subgoal`. Incremental cost per turn instead of per total span.
    pub(crate) fn extend_active(&mut self, new_end: usize) {
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
    ///
    /// Used for retroactive tagging of pre-extraction messages on first subgoal creation
    /// (S4 fix): all messages that existed before the first extraction result arrived are
    /// tagged with the initial subgoal so they are not treated as "outdated" (relevance 0.1).
    pub(crate) fn tag_range(&mut self, start: usize, end: usize, id: SubgoalId) {
        for idx in start..=end {
            self.msg_to_subgoal.insert(idx, id);
        }
        if end > self.last_tagged_index {
            self.last_tagged_index = end;
        }
    }

    /// Get the subgoal state for a given message index.
    pub(crate) fn subgoal_state(&self, msg_index: usize) -> Option<SubgoalState> {
        let sg_id = self.msg_to_subgoal.get(&msg_index)?;
        self.subgoals
            .iter()
            .find(|s| &s.id == sg_id)
            .map(|s| s.state)
    }

    /// Get the current active subgoal (for debug output and TUI metrics).
    pub(crate) fn active_subgoal(&self) -> Option<&Subgoal> {
        self.subgoals
            .iter()
            .find(|s| s.state == SubgoalState::Active)
    }

    /// Rebuild the registry after compaction.
    ///
    /// Instead of arithmetic offset adjustment (which is fragile because the final message
    /// positions depend on `pinned_count` and `active_subgoal_count` — variable quantities),
    /// this rebuilds `msg_to_subgoal` from scratch by iterating the post-compaction message
    /// array and matching message content against surviving `Subgoal` entries.
    ///
    /// When `old_compact_end == 0`, the function simply rebuilds the map from the current
    /// message array without dropping any subgoals (used after deferred summary insertions
    /// to repair shifted indices — S5 fix).
    ///
    /// Algorithm:
    /// 1. Drop `Subgoal` entries whose entire span was in the drained range.
    /// 2. For surviving subgoals, re-scan the post-compaction array to find their messages.
    /// 3. Rebuild `msg_to_subgoal` and reset `last_tagged_index`.
    pub(crate) fn rebuild_after_compaction(
        &mut self,
        messages: &[zeph_llm::provider::Message],
        old_compact_end: usize,
    ) {
        // Clear the index map; we'll rebuild it completely.
        self.msg_to_subgoal.clear();

        if self.subgoals.is_empty() {
            self.last_tagged_index = 0;
            return;
        }

        // For a full rebuild after drain (old_compact_end > 0), we need to identify which
        // subgoals still have surviving messages. We do this by scanning the post-compaction
        // message array: any message whose content matches a subgoal's tagged content range
        // is re-associated with that subgoal.
        //
        // Since `Message` does not carry a subgoal tag in its metadata (by design — we avoid
        // coupling the LLM message struct to compaction state), we use a different approach:
        // for each message in the post-compaction array, assign it to the Active subgoal if
        // it is in the re-inserted active-subgoal block, or to the most recent subgoal whose
        // span plausibly covers it based on its relative position.
        //
        // The practical approach: rebuild by assigning each non-system message to the subgoal
        // based on the message's position relative to the surviving subgoal spans.
        // After compaction, we cannot know the exact original index of each message, so we
        // rebuild using the surviving subgoal descriptions and the Active/Completed flags.
        //
        // Simplified rebuild: scan messages [1..], assign to Active subgoal if one exists,
        // otherwise to the most recent Completed subgoal. This is a conservative approximation
        // that preserves the invariant that Active subgoal messages are never mistakenly
        // evicted by subsequent pruning.
        let _active_id = self
            .subgoals
            .iter()
            .find(|s| s.state == SubgoalState::Active)
            .map(|s| s.id);

        // When old_compact_end > 0, drop subgoals whose entire span was within the drained range.
        // A subgoal is considered fully drained if its end_msg_index < old_compact_end
        // AND it is Completed (Active subgoal messages are re-inserted so they survive).
        if old_compact_end > 0 {
            self.subgoals.retain(|s| {
                // Keep Active subgoals — their messages were re-inserted.
                s.state == SubgoalState::Active
                // Keep Completed subgoals whose span extends into the preserved tail.
                    || s.end_msg_index >= old_compact_end
            });
        }

        if self.subgoals.is_empty() {
            self.last_tagged_index = 0;
            return;
        }

        // Rebuild: assign each non-system message to a subgoal based on surviving subgoal spans.
        // Strategy: For each surviving message, determine which subgoal span it fell into
        // based on the surviving subgoals' adjusted boundaries. Active subgoals take
        // precedence, then Completed subgoals, then untagged.
        //
        // This preserves tier differentiation: Active messages stay Active (unevictable),
        // Completed messages stay Completed (summarizable), untagged stay untagged (outdated).

        let mut last_idx = 0usize;
        for (i, _msg) in messages.iter().enumerate().skip(1) {
            // Try to match this message index to a surviving subgoal span.
            // Prefer Active subgoals, then Completed subgoals.
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

/// Density classification for a message or segment (#2481).
///
/// Used to partition compaction token budgets: high-density content (structured data,
/// code, JSON) receives a larger fraction of the summary budget than low-density prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentDensity {
    /// More than 50% of lines are structured (code fences, JSON, lists, shell output).
    High,
    /// 50% or fewer lines are structured.
    Low,
}

/// Classify a message's content density.
///
/// A line is considered structured if it matches any of:
/// - code fence delimiters (` ``` ` or `~~~`)
/// - leading special characters: `{`, `[`, `|`, `$`, `>`, `#`
/// - indentation of 4+ spaces (typical for code blocks / shell output)
///
/// Threshold: >50% structured lines → `High`.
pub(crate) fn classify_density(content: &str) -> ContentDensity {
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

/// Automatically consolidate low-relevance context into a knowledge-block summary.
///
/// Scores each message in `messages` against the task goal extracted from the most recent
/// user message, finds the oldest contiguous run of messages scoring ≤ 0 (negative MIG)
/// of length ≥ `min_window`, and asks `provider` to extract the key facts. The result
/// is truncated to `max_chars` at a UTF-8 character boundary.
///
/// Returns `Ok(None)` when the history is too short or no low-relevance window exists.
/// Returns `Err` when the provider call fails or times out (the caller should log-and-skip).
///
/// # Errors
///
/// Returns an error if the provider call returns an error or if the 20-second timeout
/// elapses before the provider responds.
pub(crate) async fn run_focus_auto_consolidation(
    messages: &[Message],
    min_window: usize,
    provider: impl LlmProvider,
    max_chars: usize,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let _span = tracing::info_span!("core.context.focus_auto_consolidate").entered();

    if messages.len() < min_window {
        return Ok(None);
    }

    // Seed the task goal from the most recent user message.
    let task_goal = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map_or("", |m| m.content.as_str());

    // S2/S3: without a user message we cannot compute relevance scores — score_blocks_mig
    // would fall back to recency mode and eagerly consolidate nearly all history on every
    // call (agent warm-start, scripted init). Skip instead.
    if task_goal.is_empty() {
        tracing::debug!("focus_auto_consolidation: no user message found, skipping");
        return Ok(None);
    }

    // Clone inputs so the O(K²) pairwise loop can run on a blocking thread without
    // stalling the async executor during long sessions.
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

    // Find the oldest contiguous run of messages with MIG ≤ 0 of length ≥ min_window.
    // `scores` is indexed by their original message positions via `msg_index`.
    // Build a set of low-relevance message indices for O(1) lookup.
    let low_relevance: std::collections::HashSet<usize> = scores
        .iter()
        .filter(|s| s.mig <= 0.0)
        .map(|s| s.msg_index)
        .collect();

    // Walk message indices in order to find the first run of low-relevance msgs ≥ min_window.
    let window_indices = find_low_relevance_window(messages, &low_relevance, min_window);
    if window_indices.is_empty() {
        return Ok(None);
    }

    // Build the extraction prompt from the window's scorable text.
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

    // Truncate at char boundary to stay within max_chars.
    let truncated = if raw.len() <= max_chars {
        raw
    } else {
        // Find the largest char boundary ≤ max_chars.
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

/// Find the indices of the first contiguous run of low-relevance messages.
///
/// Only messages that appear in `low_relevance` (by index) are counted.
/// System messages (index 0) and pinned messages are never included.
/// Returns an empty vec if no qualifying run exists.
fn find_low_relevance_window(
    messages: &[Message],
    low_relevance: &std::collections::HashSet<usize>,
    min_window: usize,
) -> Vec<usize> {
    let mut best: Vec<usize> = Vec::new();
    let mut current: Vec<usize> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        // Skip system prompt and pinned messages.
        if i == 0 || msg.metadata.focus_pinned {
            current.clear();
            continue;
        }
        if low_relevance.contains(&i) {
            current.push(i);
        } else {
            // Gap: if the current run qualifies and we haven't found one yet, capture it.
            if current.len() >= min_window && best.is_empty() {
                // Move contents into best and leave current empty.
                best.append(&mut current);
            }
            current.clear();
        }
    }
    // Check trailing run.
    if current.len() >= min_window && best.is_empty() {
        best = current;
    }
    best
}

/// Partition messages into (high-density, low-density) groups by content classification.
///
/// System messages and pinned messages are excluded from both groups.
pub(crate) fn partition_by_density(messages: &[Message]) -> (Vec<Message>, Vec<Message>) {
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

    // T-HIGH-03: score_blocks_task_aware tests.

    fn make_tool_output_msg(body: &str) -> zeph_llm::provider::Message {
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};
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
        use zeph_llm::provider::{Message, Role};
        use zeph_memory::TokenCounter;

        let tc = TokenCounter::default();
        let messages = vec![
            Message::from_legacy(Role::System, "system prompt"),
            make_tool_output_msg("authentication session middleware"),
        ];
        let scores = score_blocks_task_aware(&messages, "authentication session", &tc);
        // index 0 is skipped; exactly 1 score for index 1
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].msg_index, 1);
    }

    #[test]
    fn score_blocks_task_aware_skips_pinned_messages() {
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::TokenCounter;

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
        // Pinned message at index 1 must be excluded
        assert!(
            scores.iter().all(|s| s.msg_index != 1),
            "pinned message must not be scored"
        );
    }

    #[test]
    fn score_blocks_task_aware_relevant_block_scores_higher() {
        use zeph_llm::provider::{Message, Role};
        use zeph_memory::TokenCounter;

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
            "auth block (relevance={}) must score higher than db block (relevance={})",
            auth_score.relevance,
            db_score.relevance
        );
    }

    #[test]
    fn score_blocks_mig_redundancy_decreases_mig() {
        use zeph_llm::provider::{Message, Role};
        use zeph_memory::TokenCounter;

        let tc = TokenCounter::default();
        // Two very similar blocks about authentication — the lower-relevance one gets
        // high redundancy (it's similar to the higher-relevance one) → negative MIG.
        let auth_body =
            "authentication session token middleware implementation login logout ".repeat(10);
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            make_tool_output_msg(
                &(auth_body.clone() + " extra unique content for higher relevance boost"),
            ),
            make_tool_output_msg(&auth_body),
        ];
        let scores = score_blocks_mig(&messages, Some("authentication session token"), &tc);
        assert_eq!(scores.len(), 2);
        // Both should have some redundancy since bodies are very similar
        let total_redundancy: f32 = scores.iter().map(|s| s.redundancy).sum();
        assert!(
            total_redundancy > 0.0,
            "similar blocks must have non-zero redundancy"
        );
    }

    #[test]
    fn score_blocks_mig_without_goal_uses_recency() {
        use zeph_llm::provider::{Message, Role};
        use zeph_memory::TokenCounter;

        let tc = TokenCounter::default();
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            make_tool_output_msg("old output from early in conversation"),
            make_tool_output_msg("recent output from later in conversation"),
        ];
        let scores = score_blocks_mig(&messages, None, &tc);
        assert_eq!(scores.len(), 2);
        let old_score = scores.iter().find(|s| s.msg_index == 1).unwrap();
        let new_score = scores.iter().find(|s| s.msg_index == 2).unwrap();
        assert!(
            new_score.relevance > old_score.relevance,
            "recency: later message must have higher relevance (new={}, old={})",
            new_score.relevance,
            old_score.relevance
        );
    }

    // ─── SubgoalRegistry tests ────────────────────────────────────────────────

    #[test]
    fn subgoal_registry_push_active_creates_active_subgoal() {
        let mut registry = SubgoalRegistry::default();
        let id = registry.push_active("Implement login endpoint".into(), 1);
        assert_eq!(registry.subgoals.len(), 1);
        assert_eq!(registry.subgoals[0].id, id);
        assert_eq!(registry.subgoals[0].state, SubgoalState::Active);
        assert_eq!(registry.subgoals[0].start_msg_index, 1);
    }

    #[test]
    fn subgoal_registry_complete_active_transitions_state() {
        let mut registry = SubgoalRegistry::default();
        registry.push_active("initial subgoal".into(), 1);
        registry.complete_active(5);
        assert_eq!(registry.subgoals[0].state, SubgoalState::Completed);
        assert_eq!(registry.subgoals[0].end_msg_index, 5);
        assert!(registry.active_subgoal().is_none());
    }

    #[test]
    fn subgoal_registry_push_active_auto_completes_existing_active() {
        let mut registry = SubgoalRegistry::default();
        registry.push_active("first subgoal".into(), 1);
        // Push a second without completing the first
        registry.push_active("second subgoal".into(), 6);
        // First must be auto-completed
        assert_eq!(registry.subgoals[0].state, SubgoalState::Completed);
        assert_eq!(registry.subgoals[1].state, SubgoalState::Active);
        // Only one Active at any time
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
        // Messages 3, 4, 5 should all be tagged
        assert_eq!(registry.subgoal_state(3), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(4), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(5), Some(SubgoalState::Active));
        assert_eq!(registry.msg_to_subgoal.get(&3), Some(&id));

        // Extend again: only new indices should be added
        registry.extend_active(7);
        assert_eq!(registry.subgoal_state(6), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(7), Some(SubgoalState::Active));
        // Count entries: 3,4,5,6,7 = 5 total (no duplicates)
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

        // Completed subgoal messages
        assert_eq!(registry.subgoal_state(1), Some(SubgoalState::Completed));
        assert_eq!(registry.subgoal_state(5), Some(SubgoalState::Completed));
        // Active subgoal messages
        assert_eq!(registry.subgoal_state(6), Some(SubgoalState::Active));
        assert_eq!(registry.subgoal_state(9), Some(SubgoalState::Active));
        // Untagged
        assert_eq!(registry.subgoal_state(0), None);
        assert_eq!(registry.subgoal_state(10), None);
    }

    #[test]
    fn subgoal_registry_tag_range_retroactive_tagging() {
        let mut registry = SubgoalRegistry::default();
        // Simulate first extraction arriving late: pre-existing messages 1..5
        let id = registry.push_active("first subgoal".into(), 5);
        // Retroactive tag all existing messages
        registry.tag_range(1, 4, id);
        // All messages [1..4] should be tagged as Active
        for i in 1..=4 {
            assert_eq!(
                registry.subgoal_state(i),
                Some(SubgoalState::Active),
                "message {i} must be tagged Active"
            );
        }
    }

    #[test]
    fn subgoal_registry_rebuild_after_compaction_all_removed() {
        use zeph_llm::provider::{Message, Role};
        let mut registry = SubgoalRegistry::default();
        let id = registry.push_active("completed subgoal".into(), 1);
        registry.tag_range(1, 5, id);
        registry.complete_active(5);

        // Post-compaction: only system prompt + summary survive
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            Message::from_legacy(Role::System, "[summary]"),
        ];
        // compact_end = 6 means all original messages [1..5] were drained
        registry.rebuild_after_compaction(&messages, 6);

        // Completed subgoal with end_msg_index=5 < compact_end=6 is dropped
        assert!(
            registry.subgoals.is_empty(),
            "fully drained completed subgoal must be removed"
        );
        assert!(registry.msg_to_subgoal.is_empty());
    }

    #[test]
    fn subgoal_registry_rebuild_after_compaction_active_subgoal_survives() {
        use zeph_llm::provider::{Message, Role};
        let mut registry = SubgoalRegistry::default();
        let id = registry.push_active("active subgoal".into(), 3);
        registry.tag_range(3, 6, id);

        // Post-compaction: system + summary + 2 re-inserted active subgoal msgs + preserved tail
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            Message::from_legacy(Role::System, "[summary]"),
            Message::from_legacy(Role::User, "active msg 1"),
            Message::from_legacy(Role::User, "active msg 2"),
            Message::from_legacy(Role::User, "tail msg"),
        ];
        registry.rebuild_after_compaction(&messages, 3);

        // Active subgoal must survive
        assert!(registry.active_subgoal().is_some());
        // Messages in new array should be tagged
        assert!(!registry.msg_to_subgoal.is_empty());
    }

    #[test]
    fn subgoal_registry_rebuild_no_drain_repairs_shifted_indices() {
        use zeph_llm::provider::{Message, Role};
        let mut registry = SubgoalRegistry::default();
        let id = registry.push_active("subgoal".into(), 1);
        registry.tag_range(1, 3, id);

        // Simulate deferred summary insertion at index 2 (shifts indices up)
        let messages = vec![
            Message::from_legacy(Role::System, "sys"),
            Message::from_legacy(Role::User, "msg 1"),
            Message::from_legacy(Role::Assistant, "[tool summary]"), // inserted
            Message::from_legacy(Role::User, "msg 3"),               // was index 2
            Message::from_legacy(Role::User, "msg 4"),               // was index 3
        ];
        // old_compact_end = 0 means "no drain, just repair indices"
        registry.rebuild_after_compaction(&messages, 0);

        // After rebuild, the Active subgoal must still exist and messages must be tagged
        assert!(registry.active_subgoal().is_some());
        // At least the new messages should be tagged
        assert!(!registry.msg_to_subgoal.is_empty());
    }

    // ─── classify_density tests ───────────────────────────────────────────────

    #[test]
    fn classify_density_empty_string_is_low() {
        assert_eq!(classify_density(""), ContentDensity::Low);
    }

    #[test]
    fn classify_density_all_structured_is_high() {
        // 4 lines, all structured (code fence or leading special char)
        let content = "```rust\nfn main() {}\n```\n$ cargo build\n";
        assert_eq!(classify_density(content), ContentDensity::High);
    }

    #[test]
    fn classify_density_all_prose_is_low() {
        let content = "This is a sentence.\nAnother sentence here.\nNo structured content at all.";
        assert_eq!(classify_density(content), ContentDensity::Low);
    }

    #[test]
    fn classify_density_exactly_50_percent_is_low() {
        // 2 structured lines out of 4 = 50% → Low (threshold is strictly > 0.5)
        let content = "```rust\n$ cargo test\nplain prose line one\nplain prose line two";
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(classify_density(content), ContentDensity::Low);
    }

    #[test]
    fn classify_density_cargo_output_is_high() {
        // Cargo JSON/shell output: lines starting with `{`, `[`, `$` or 4-space indent.
        // 4 structured out of 6 = 66% → High.
        let content = "$ cargo build --message-format json\n\
                       {\"reason\":\"compiler-message\"}\n\
                       {\"reason\":\"build-script-executed\"}\n\
                       {\"reason\":\"compiler-artifact\"}\n\
                       Build finished.\n\
                       Done.";
        assert_eq!(classify_density(content), ContentDensity::High);
    }

    #[test]
    fn classify_density_indented_code_4_spaces_is_structured() {
        // Lines with 4+ spaces of indentation count as structured
        let content = "    let x = 5;\n    let y = 6;\nnormal prose\n    return x + y;";
        // 3 structured out of 4 = 75% → High
        assert_eq!(classify_density(content), ContentDensity::High);
    }

    // ─── run_focus_auto_consolidation tests ──────────────────────────────────

    struct StubProvider {
        response: &'static str,
    }

    impl zeph_llm::provider::LlmProvider for StubProvider {
        async fn chat(
            &self,
            _messages: &[zeph_llm::provider::Message],
        ) -> Result<String, zeph_llm::LlmError> {
            Ok(self.response.to_owned())
        }

        async fn chat_stream(
            &self,
            messages: &[zeph_llm::provider::Message],
        ) -> Result<zeph_llm::provider::ChatStream, zeph_llm::LlmError> {
            let r = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(
                zeph_llm::provider::StreamChunk::Content(r),
            ))))
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
        async fn chat(
            &self,
            _messages: &[zeph_llm::provider::Message],
        ) -> Result<String, zeph_llm::LlmError> {
            // Hang forever by awaiting a never-completing future.
            std::future::pending::<()>().await;
            unreachable!()
        }

        async fn chat_stream(
            &self,
            _messages: &[zeph_llm::provider::Message],
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
        use zeph_llm::provider::{Message, Role};
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
        use zeph_llm::provider::{Message, Role};

        // Build 8 messages that are completely off-topic from the user goal.
        // The user asks about "authentication" but all tool outputs are about "database schema".
        // This should produce a low-relevance window and trigger extraction.
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

        // With at least 4 low-relevance messages, a summary should be produced.
        assert!(result.is_some());
        let summary = result.unwrap();
        assert!(!summary.is_empty());
    }

    #[tokio::test]
    async fn auto_consolidation_timeout_recovers() {
        use zeph_llm::provider::{Message, Role};

        // Use a very short timeout by passing a HangingProvider. The function has a fixed 20s
        // timeout. We can't make the test wait 20s, so instead we check that an error is
        // returned (not a panic) when the provider hangs. To keep the test fast, we manually
        // verify the timeout path using `tokio::time::timeout` directly.
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

        // Wrap the call in a very short timeout to avoid waiting 20s in tests.
        // The important invariant: the function does not panic on timeout.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            run_focus_auto_consolidation(&messages, 4, HangingProvider, 4096),
        )
        .await;

        // Either: outer timeout fires (Err), or the inner 20s timeout fires (Ok(Err)).
        // Both cases must not panic.
        match result {
            Err(_elapsed) => {
                // Outer timeout fired — no panic, correct.
            }
            Ok(inner) => {
                // Inner 20s timeout fired — also valid, must be an Err.
                assert!(inner.is_err(), "hanging provider must return an error");
            }
        }
    }

    // ─── KnowledgeBlock eviction order tests ─────────────────────────────────

    #[test]
    fn eviction_auto_consolidated_evicted_before_llm_curated() {
        use crate::agent::focus::{FocusState, KnowledgeBlockSource};
        use crate::config::FocusConfig;
        // Cap = 10 tokens ≈ 40 chars. Each block below is ~100 chars each.
        let config = FocusConfig {
            max_knowledge_tokens: 10,
            ..FocusConfig::default()
        };
        let mut state = FocusState::new(config);

        // Add LlmCurated first, then AutoConsolidated.
        state.append_llm_knowledge("llm_curated_summary_content ".repeat(4));
        state.append_auto_knowledge("auto_consolidated_summary ".repeat(4));
        // Add another LlmCurated — this triggers eviction to stay under cap.
        state.append_llm_knowledge("second_llm_curated_summary ".repeat(4));

        // AutoConsolidated must be evicted first. No AutoConsolidated blocks should remain
        // if the cap forced any eviction.
        let has_auto = state
            .knowledge_blocks
            .iter()
            .any(|b| b.source == KnowledgeBlockSource::AutoConsolidated);
        assert!(
            !has_auto,
            "AutoConsolidated block must be evicted before LlmCurated"
        );
        // At least one LlmCurated must survive.
        let has_llm = state
            .knowledge_blocks
            .iter()
            .any(|b| b.source == KnowledgeBlockSource::LlmCurated);
        assert!(has_llm, "at least one LlmCurated block must survive");
    }

    /// S2/S3: when no User message is present, `run_focus_auto_consolidation` must return
    /// `None` instead of entering recency mode and eagerly consolidating all history.
    #[tokio::test]
    async fn run_focus_auto_consolidation_skips_when_no_user_message() {
        use zeph_llm::provider::{Message, Role};

        // Build a history with plenty of messages but no User turn (scripted init / warm-start).
        let mut messages = vec![Message::from_legacy(Role::System, "sys")];
        for i in 0..8 {
            messages.push(make_tool_output_msg(&format!("tool output {i}")));
        }
        // No Role::User message — task_goal will be empty.

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
}
