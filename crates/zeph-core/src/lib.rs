// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent loop, configuration loading, and context builder.

pub mod agent;
#[allow(clippy::missing_errors_doc, clippy::must_use_candidate)]
pub mod bootstrap;
pub mod channel;
pub mod config;
pub mod config_watcher;
pub mod context;
pub mod cost;
pub mod daemon;
pub mod debug_dump;
pub mod instructions;
pub mod metrics;
pub mod pipeline;
pub mod project;
pub mod redact;
pub mod vault;

#[cfg(feature = "experiments")]
pub mod experiments;

#[cfg(feature = "lsp-context")]
pub mod lsp_hooks;

pub mod orchestration;

pub mod hash;
pub mod http;
pub mod memory_tools;
pub mod overflow_tools;
pub mod sanitizer;
pub mod skill_loader;
pub mod subagent;
pub use zeph_common::text;

#[cfg(test)]
pub mod testing;

pub use agent::Agent;
pub use agent::error::AgentError;
pub use agent::session_config::{AgentSessionConfig, CONTEXT_BUDGET_RESERVE_RATIO};
pub use channel::{
    Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage, LoopbackChannel,
    LoopbackEvent, LoopbackHandle, StopHint, ToolOutputData, ToolOutputEvent, ToolStartData,
    ToolStartEvent,
};
pub use config::{Config, ConfigError};
pub use hash::content_hash;
pub use sanitizer::exfiltration::{
    ExfiltrationEvent, ExfiltrationGuard, ExfiltrationGuardConfig, extract_flagged_urls,
};
pub use sanitizer::{
    ContentIsolationConfig, ContentSanitizer, ContentSource, ContentSourceKind, InjectionFlag,
    QuarantineConfig, SanitizedContent, TrustLevel,
};
pub use skill_loader::SkillLoaderExecutor;
pub use zeph_tools::executor::DiffData;
