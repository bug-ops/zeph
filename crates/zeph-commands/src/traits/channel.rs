// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Re-exports [`ChannelSink`] as a sub-trait module for consistency.
//!
//! All command handlers that produce output use [`ChannelSink`] from `CommandContext::sink`.

pub use crate::sink::ChannelSink;
