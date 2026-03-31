// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Channel implementations for the Zeph agent.

mod any;
pub mod cli;
#[cfg(feature = "discord")]
pub mod discord;
mod line_editor;
pub mod markdown;
#[cfg(feature = "slack")]
pub mod slack;
pub mod telegram;

pub use any::AnyChannel;
pub use cli::CliChannel;

/// Shared timeout for interactive confirmation dialogs across all remote channels.
///
/// Used by Telegram, Discord, and Slack `confirm()` implementations to ensure
/// consistent deny-on-timeout behavior.
pub const CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Per-field timeout for interactive elicitation dialogs on remote channels (Telegram, etc.).
pub const ELICITATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
