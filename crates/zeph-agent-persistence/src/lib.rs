// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent persistence service for Zeph.
//!
//! This crate provides [`service::PersistenceService`] — a stateless façade for loading
//! conversation history from and writing messages to the `SemanticMemory` backend
//! (`SQLite` + Qdrant). It also exposes pure helper functions for:
//!
//! - Tool-pair sanitization ([`sanitize`])
//! - Message embedding decisions and memory writes ([`embed`])
//! - Graph extraction configuration ([`graph`])
//!
//! # Architecture
//!
//! `zeph-agent-persistence` depends on `zeph-memory`, `zeph-llm`, `zeph-context`, `zeph-config`,
//! and `zeph-common`. It does **not** depend on `zeph-core` — this is the core invariant that
//! allows the persistence and tool-dispatch subsystems to evolve independently.
//!
//! `zeph-core` depends on this crate and provides thin shim methods on `Agent<C>` that
//! construct the borrow-lens views (`MemoryPersistenceView`, `SecurityView`, `MetricsView`)
//! from their respective `Agent` fields and delegate to `PersistenceService`.
//!
//! # TODO(critic): consolidate MockChannel/MockToolExecutor into shared zeph-agent-test-helpers
//! crate; tracked separately from #3515/#3516

pub mod embed;
pub mod error;
pub mod graph;
pub mod request;
pub mod sanitize;
pub mod service;
pub mod state;

pub use error::PersistenceError;
pub use request::{LoadHistoryOutcome, PersistMessageOutcome, PersistMessageRequest};
pub use service::PersistenceService;
pub use state::{MemoryPersistenceView, MetricsView, ProviderHandles, SecurityView};
