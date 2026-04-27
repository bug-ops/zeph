// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Retrieved-memory context extraction.
//!
//! This module will hold the `collect_retrieved_context` function currently in
//! `zeph-core::agent::context::retrieved`, along with the `RetrievedContext` type.
//!
//! # Migration status
//!
//! `collect_retrieved_context` depends on `RetrievedContext` from
//! `zeph-core::quality::pipeline`. That type must be promoted to `zeph-context` or
//! `zeph-common` before the function can move here.
//!
//! # TODO(review): NON-BLOCKER — move `RetrievedContext` to `zeph-context` or
//! `zeph-common`, then migrate `collect_retrieved_context` here in Step 9.
//!
//! This feature is only relevant when the `self-check` feature is enabled.
#![cfg_attr(docsrs, doc(cfg(feature = "self-check")))]
