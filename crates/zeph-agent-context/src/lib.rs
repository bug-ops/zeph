// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// The graph-retrieval-strategy dispatch in `helpers::fetch_graph_facts` chains several
// nested async fns (`run_graph_strategy`, `run_synapse_strategy`, `run_hybrid_strategy`, ...),
// which pushes the compiler's Future-layout query depth beyond the default limit of 128.
// This only manifests with the `full` feature bundle combined with `postgres` at the
// *workspace* level (`cargo check --workspace --all-targets --no-default-features
// --features full,postgres`) — `full` pulls in additional dependent crates (candle, pdf,
// scheduler, tui, acp, gateway, a2a, otel, prometheus) whose types push the query depth
// further than `postgres` alone. A crate-scoped build (`-p zeph-agent-context --features
// postgres`) does NOT reproduce this; always verify against the workspace+full+postgres
// combination before touching this attribute again.
#![recursion_limit = "256"]

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
pub mod memory_backend;
pub mod retrieved;
pub mod service;
pub mod state;
pub mod summarization;
pub mod type_aware_compose;

pub use compaction::{
    BlockScore, ContentDensity, SubgoalExtractionResult, SubgoalId, SubgoalRegistry, SubgoalState,
    classify_density, extract_scorable_text, partition_by_density, run_focus_auto_consolidation,
    score_blocks_mig, score_blocks_subgoal, score_blocks_subgoal_mig, score_blocks_task_aware,
};
pub use error::ContextError;
pub use helpers::BudgetHint;
pub use service::{ContextService, SemanticRecallParams};
pub use state::{
    CompactionOutcome, CompactionPersistence, CompactionProbeCallback, ContextAssemblyView,
    ContextDelta, ContextSummarizationView, MessageWindowView, MetricsCallback, MetricsCounters,
    ProbeOutcome, ProviderHandles, QdrantPersistFuture, SecurityEventSink, StatusSink,
    ToolOutputArchive, TrustGate,
};

pub use retrieved::{RetrievedContext, collect_retrieved_context};
