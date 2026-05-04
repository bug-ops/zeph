// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `MemCoT` semantic state accumulation (issues #3574 / #3575).
//!
//! When `memory.memcot.enabled = true`, the agent maintains a short rolling buffer of
//! distilled conceptual state. This buffer is prepended to graph-recall queries so that
//! retrieval stays relevant across long multi-turn sessions.
//!
//! When `enabled = false` (default), this entire module is a no-op: no allocations, no
//! LLM calls, no overhead.

pub(crate) mod accumulator;
mod metrics;

pub use accumulator::SemanticStateAccumulator;
