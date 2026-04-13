// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Zeph core agent: multi-model inference, semantic memory, skills orchestration, and tool execution.
//!
//! This crate provides the [`Agent`] struct — the autonomous AI system at the heart of Zeph.
//! It integrates LLM providers (Claude, `OpenAI`, Ollama, Candle), semantic memory (Qdrant),
//! skill registry and matching, tool execution (shell, web, custom), MCP client support, and
//! security/compliance subsystems into a single composable agent framework.
//!
//! # Usage
//!
//! The main entry point is [`Agent::new`] or [`Agent::new_with_registry_arc`]. After creating
//! an agent, call [`Agent::run`] to execute the main loop.
//! Always call [`Agent::shutdown`] before dropping to persist state.
//!
//! See the `bootstrap` module in the `zeph` binary crate for config loading and provider setup examples.
//!
//! # Key Components
//!
//! - [`Agent`] — Main struct that runs the agent loop
//! - [`Channel`] — Abstraction for user interaction (send/receive messages and events)
//! - [`channel::ChannelMessage`] — Structured messages flowing to/from the user
//! - `config` — Configuration schema (LLM providers, memory, skills, etc.)
//! - `agent::session_config` — Per-session configuration (budget, timeouts, etc.)
//! - `agent::context` — Context assembly and token budgeting utilities
//! - [`pipeline`] — Structured execution pipelines for complex workflows
//! - [`project`] — Project indexing and semantic retrieval
//! - [`memory_tools`] — Memory search and management utilities
//!
//! Note: The `bootstrap` module (`AppBuilder`, provider setup, etc.) lives in the `zeph` binary crate.
//!
//! # Architecture
//!
//! The agent operates as a **single-turn finite state machine** that processes each user
//! message through a series of stages:
//!
//! 1. **Input** — Receive user message via channel
//! 2. **Context assembly** — Build prompt from conversation history, memory, and skills
//! 3. **LLM inference** — Call the model with multi-tool calling support
//! 4. **Tool execution** — Run tool calls concurrently with streaming output
//! 5. **Feedback loop** — Feed tool results back to LLM for synthesis
//! 6. **Output** — Send agent response via channel
//! 7. **Persistence** — Save messages and state (async, deferred)
//!
//! All async operations (`await` points) are bounded with timeouts to prevent stalls.
//!
//! # Channel Contract
//!
//! Implementing the [`Channel`] trait allows the agent to integrate with any I/O system:
//!
//! - **CLI** — `cargo run -- --config config.toml`
//! - **Telegram** — Bot interface with streaming updates
//! - **TUI** — Multi-panel dashboard with real-time metrics
//! - **HTTP gateway** — Webhook ingestion and agent event streaming
//! - **Custom** — Implement [`Channel`] for domain-specific systems
//!
//! # Feature Flags
//!
//! - `candle` — Local inference via Candle (default off, requires CUDA/Metal)
//! - `classifiers` — ML-based content classification and trust scoring
//! - `metal` — Candle with Metal acceleration (macOS)
//! - `cuda` — Candle with CUDA acceleration (Linux/Windows)
//! - `scheduler` — Cron-based periodic task scheduler

pub mod agent;
#[cfg(feature = "profiling-alloc")]
pub mod alloc_layer;
pub mod channel;
pub mod config;
pub mod config_watcher;
pub mod context;
pub mod cost;
pub mod daemon;
pub mod debug_dump;
pub mod file_watcher;
pub mod instructions;
pub mod instrumented_channel;
pub mod metrics;
#[cfg(feature = "profiling")]
pub mod metrics_bridge;
pub mod pipeline;
pub mod project;
pub mod provider_factory;
pub mod redact;
#[cfg(feature = "sysinfo")]
pub mod system_metrics;

pub mod http;
pub mod lsp_hooks;
pub mod memory_tools;
pub mod overflow_tools;
pub mod runtime_layer;
pub mod skill_loader;
pub mod task_supervisor;
pub use zeph_common::text;

#[cfg(test)]
pub mod testing;

pub use agent::Agent;
pub use agent::error::AgentError;
pub use agent::session_config::{AgentSessionConfig, CONTEXT_BUDGET_RESERVE_RATIO};
pub use agent::state::AdversarialPolicyInfo;
pub use agent::state::ProviderConfigSnapshot;
pub use channel::{
    Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage, LoopbackChannel,
    LoopbackEvent, LoopbackHandle, StopHint, ToolOutputData, ToolOutputEvent, ToolStartData,
    ToolStartEvent,
};
pub use config::{Config, ConfigError};
pub use skill_loader::SkillLoaderExecutor;
pub use task_supervisor::{
    BlockingError, BlockingHandle, RestartPolicy, TaskDescriptor, TaskHandle, TaskSnapshot,
    TaskStatus, TaskSupervisor,
};
pub use zeph_common::hash::blake3_hex as content_hash;
pub use zeph_sanitizer::exfiltration::{
    ExfiltrationEvent, ExfiltrationGuard, ExfiltrationGuardConfig, extract_flagged_urls,
};
pub use zeph_sanitizer::{
    ContentIsolationConfig, ContentSanitizer, ContentSource, ContentSourceKind, ContentTrustLevel,
    InjectionFlag, QuarantineConfig, SanitizedContent,
};
pub use zeph_tools::executor::DiffData;

// Re-export vault module to preserve internal import paths (e.g., `crate::vault::VaultProvider`).
pub mod vault {
    pub use zeph_vault::{
        AgeVaultError, AgeVaultProvider, ArcAgeVaultProvider, EnvVaultProvider, Secret, VaultError,
        VaultProvider, default_vault_dir,
    };

    #[cfg(any(test, feature = "mock"))]
    pub use zeph_vault::MockVaultProvider;
}
