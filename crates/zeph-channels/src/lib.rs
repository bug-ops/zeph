// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Channel implementations for the Zeph agent.
//!
//! This crate provides the concrete `Channel` implementations used by the
//! Zeph agent loop.  Every implementation wraps a transport-specific I/O
//! mechanism and exposes a uniform async interface that the agent core uses
//! without knowing the underlying delivery medium.
//!
//! # Available channels
//!
//! | Type | Feature | Transport |
//! |------|---------|-----------|
//! | [`CliChannel`] | always | stdin / stdout |
//! | [`telegram::TelegramChannel`] | always | Telegram Bot API via teloxide |
//! | `DiscordChannel` | `discord` | Discord gateway |
//! | `SlackChannel` | `slack` | Slack Events API |
//!
//! # Runtime dispatch
//!
//! [`AnyChannel`] is an enum that implements `Channel` by delegating to the
//! active variant.  The binary selects the variant at startup and passes
//! `AnyChannel` throughout; the agent core never needs to be generic over the
//! channel type.
//!
//! # Shared timeouts
//!
//! [`CONFIRM_TIMEOUT`] and [`ELICITATION_TIMEOUT`] define the deny-on-timeout
//! durations that all remote channel adapters must honour for interactive
//! dialogs.  They are intentionally centralised here so that the behaviour is
//! consistent across Telegram, Discord, and Slack.

mod any;
pub mod cli;
#[cfg(feature = "discord")]
pub mod discord;
pub mod json_cli;
mod line_editor;
pub mod markdown;
#[cfg(feature = "slack")]
pub mod slack;
pub mod telegram;
pub mod telegram_api_ext;
pub mod telegram_moderation;

pub use any::AnyChannel;
pub use cli::CliChannel;
pub use json_cli::JsonCliChannel;

/// Shared timeout for interactive confirmation dialogs across all remote channels.
///
/// Used by Telegram, Discord, and Slack [`Channel::confirm`] implementations to
/// ensure consistent deny-on-timeout behaviour.  When the timeout expires the
/// implementation must return `Ok(false)` (deny), not an error.
///
/// [`Channel::confirm`]: zeph_core::channel::Channel::confirm
pub const CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Per-field timeout for interactive elicitation dialogs on remote channels.
///
/// Used by Telegram, Discord, and Slack [`Channel::elicit`] implementations.
/// Each individual field in the elicitation request resets the timer.  When the
/// timeout expires the implementation must return
/// [`ElicitationResponse::Cancelled`].
///
/// [`Channel::elicit`]: zeph_core::channel::Channel::elicit
/// [`ElicitationResponse::Cancelled`]: zeph_core::channel::ElicitationResponse::Cancelled
pub const ELICITATION_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);
