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

// Re-export experiments module to preserve internal import paths (e.g., `crate::experiments::ExperimentEngine`).
#[cfg(feature = "experiments")]
pub mod experiments {
    pub use zeph_experiments::{
        BenchmarkCase, BenchmarkSet, CaseScore, ConfigSnapshot, EvalError, EvalReport, Evaluator,
        ExperimentEngine, ExperimentResult, ExperimentSessionReport, ExperimentSource,
        GenerationOverrides, GridStep, JudgeOutput, Neighborhood, ParameterKind, ParameterRange,
        Random, SearchSpace, Variation, VariationGenerator, VariationValue,
    };
}

#[cfg(feature = "lsp-context")]
pub mod lsp_hooks;

pub mod orchestration;

pub mod hash;
pub mod http;
pub mod memory_tools;
pub mod overflow_tools;
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
pub use skill_loader::SkillLoaderExecutor;
pub use zeph_sanitizer::exfiltration::{
    ExfiltrationEvent, ExfiltrationGuard, ExfiltrationGuardConfig, extract_flagged_urls,
};
pub use zeph_sanitizer::{
    ContentIsolationConfig, ContentSanitizer, ContentSource, ContentSourceKind, InjectionFlag,
    QuarantineConfig, SanitizedContent, TrustLevel,
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
