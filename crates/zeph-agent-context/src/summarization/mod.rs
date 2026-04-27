// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context summarization pipeline for `zeph-agent-context`.
//!
//! This module organises the three summarization tiers:
//!
//! - **Deferred** (`deferred`) — stores tool-pair summaries on message metadata and
//!   applies them lazily when context pressure rises, preserving provider cache hits.
//! - **Pruning** (`pruning`) — evicts tool output bodies using one of the five
//!   configured strategies (Reactive, `TaskAware`, MIG, Subgoal, `SubgoalMig`).
//! - **Scheduling** (`scheduling`) — dispatches the Soft/Hard/Proactive compaction tiers
//!   and drives non-blocking background goal/subgoal extraction.
//! - **Compaction** (`compaction`) — LLM-based summarization that drains the oldest
//!   messages and reinserts a compact summary.
//!
//! All entry points accept a [`crate::state::ContextSummarizationView`] so the logic
//! contains no `Agent<C>` references and the crate does not depend on `zeph-core`.

pub(crate) mod compaction;
pub(crate) mod deferred;
pub(crate) mod pruning;
pub mod scheduling;

// Re-export the read-only helpers so `zeph-core` integration tests can call
// them via `zeph_agent_context::summarization::*` without duplicating the logic.
pub use deferred::{
    count_deferred_summaries, count_unsummarized_pairs, find_oldest_unsummarized_pair,
};
