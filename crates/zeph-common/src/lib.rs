// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared utility functions for Zeph crates.
//!
//! This crate provides pure utility functions (text manipulation, network helpers,
//! sanitization primitives) that are needed by multiple leaf crates. It has no
//! `zeph-*` dependencies and no heavy third-party dependencies.

pub mod net;
pub mod sanitize;
pub mod text;
