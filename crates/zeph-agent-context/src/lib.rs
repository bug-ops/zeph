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
//! - `self-check` — gates retrieved-memory mirror types for the MARCH self-check pipeline.
//! - `index` — enables `zeph-index` integration via the `IndexAccess` trait.

pub mod error;
pub mod helpers;
pub(crate) mod retrieved;
pub mod service;
pub mod state;

pub use error::ContextError;
pub use service::ContextService;
pub use state::{
    ContextAssemblyView, ContextSummarizationView, MessageWindowView, MetricsCounters,
    ProviderHandles,
};
