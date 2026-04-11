// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Batch 1 slash command handler implementations.
//!
//! Each module contains a single command handler struct implementing [`CommandHandler<C>`].
//! These are self-contained commands that do not require delegation to `Agent<C>` methods.
//!
//! [`CommandHandler<C>`]: super::command_registry::CommandHandler

pub(super) mod clear;
pub(super) mod clear_queue;
pub(super) mod debug_dump;
pub(super) mod dump_format;
pub(super) mod exit;
pub(super) mod log;
