// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context budget, lifecycle management, compaction strategy, and context assembler for Zeph.
//!
//! This crate contains the **stateless and data-only** parts of context management extracted
//! from `zeph-core`. It has no dependency on `zeph-core` — callers in `zeph-core` implement
//! the [`input::IndexAccess`] trait for their own types and populate
//! [`input::ContextMemoryView`] before each assembly pass.
//!
//! # Modules
//!
//! - [`budget`] — [`budget::ContextBudget`] and [`budget::BudgetAllocation`]
//! - [`manager`] — [`manager::ContextManager`] state machine and [`manager::CompactionState`]
//! - [`assembler`] — [`assembler::ContextAssembler`] parallel fetch coordinator
//! - [`input`] — [`input::ContextAssemblyInput`], [`input::ContextMemoryView`], traits
//! - [`slot`] — [`slot::ContextSlot`], [`slot::CompactionOutcome`], helper functions
//! - [`summarization`] — pure prompt-building helpers for context compaction
//! - [`compression_feedback`] — context-loss detection and failure classification
//! - [`microcompact`] — low-value tool detection helpers for time-based microcompact
//! - [`error`] — [`error::ContextError`]

pub mod assembler;
pub mod budget;
pub mod compression_feedback;
pub mod error;
pub mod input;
pub mod manager;
pub mod microcompact;
pub mod slot;
pub mod summarization;
pub mod typed_page;
