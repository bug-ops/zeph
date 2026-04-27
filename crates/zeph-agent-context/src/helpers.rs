// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure helper functions for context assembly.
//!
//! This module will hold the logic currently in
//! `zeph-core::agent::context::assembler_helpers` once `MemoryState` is replaced
//! by the borrow-lens view parameters in [`crate::state::ContextAssemblyView`].
//!
//! # Migration status
//!
//! Functions are stubs pending Step 5 of the extraction plan. The originals remain
//! in `zeph-core::agent::context::assembler_helpers` until they can be rewritten to
//! accept `&ContextAssemblyView` instead of `&MemoryState`.
//!
//! # TODO(review): NON-BLOCKER — move `assembler_helpers` functions here in Step 5
//! once `ContextAssemblyView` replaces `MemoryState` in the call sites.
