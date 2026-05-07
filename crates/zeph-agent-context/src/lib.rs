// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
// Allow async stubs during scaffold phase — async signatures are load-bearing for callers.
#![allow(clippy::unused_async)]

//! Agent context-assembly service for Zeph.
//!
//! This crate provides [`service::ContextService`] — a stateless façade for all
//! context-assembly operations that were previously implemented directly on `Agent<C>`
//! in `zeph-core`. Extracting this logic means that editing context-assembly code does
//! not trigger recompilation of the tool dispatcher (`zeph-agent-tools`) or the
//! persistence layer (`zeph-agent-persistence`).
//!
//! # Architecture
//!
//! `zeph-agent-context` depends on `zeph-memory`, `zeph-llm`, `zeph-context`,
//! `zeph-config`, `zeph-common`, `zeph-skills`, and `zeph-sanitizer`. It does **not**
//! depend on `zeph-core` — this is the core invariant that keeps context-assembly
//! changes from triggering full workspace rebuilds.
//!
//! `zeph-core` depends on this crate and constructs narrow borrow-lens views
//! ([`state::MessageWindowView`], [`state::ContextAssemblyView`],
//! [`state::ContextSummarizationView`]) from `Agent<C>` field projections, then
//! delegates to `ContextService`.
//!
//! # Features
//!
//! - `index` — enables `zeph-index` integration via the `IndexAccess` trait.

pub mod compaction;
pub mod error;
pub mod helpers;
pub mod retrieved;
pub mod service;
pub mod state;
pub mod summarization;

pub use compaction::{
    BlockScore, ContentDensity, SubgoalExtractionResult, SubgoalId, SubgoalRegistry, SubgoalState,
    classify_density, extract_scorable_text, partition_by_density, run_focus_auto_consolidation,
    score_blocks_mig, score_blocks_subgoal, score_blocks_subgoal_mig, score_blocks_task_aware,
};
pub use error::ContextError;
pub use helpers::BudgetHint;
pub use service::ContextService;
pub use state::{
    CompactionOutcome, CompactionPersistence, CompactionProbeCallback, ContextAssemblyView,
    ContextDelta, ContextSummarizationView, MessageWindowView, MetricsCallback, MetricsCounters,
    ProbeOutcome, ProviderHandles, QdrantPersistFuture, SecurityEventSink, StatusSink,
    ToolOutputArchive, TrustGate,
};

pub use retrieved::{RetrievedContext, collect_retrieved_context};
