// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::{default_skill_paths, default_true};
use crate::learning::LearningConfig;
use crate::security::TrustConfig;

fn default_disambiguation_threshold() -> f32 {
    0.05
}

fn default_cosine_weight() -> f32 {
    0.7
}

fn default_hybrid_search() -> bool {
    true
}

fn default_max_active_skills() -> usize {
    5
}

fn default_index_watch() -> bool {
    true
}

fn default_index_search_enabled() -> bool {
    true
}

fn default_index_max_chunks() -> usize {
    12
}

fn default_index_score_threshold() -> f32 {
    0.25
}

fn default_index_budget_ratio() -> f32 {
    0.40
}

fn default_index_repo_map_tokens() -> usize {
    500
}

fn default_repo_map_ttl_secs() -> u64 {
    300
}

fn default_vault_backend() -> String {
    "env".into()
}

fn default_max_daily_cents() -> u32 {
    0
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4317".into()
}

fn default_pid_file() -> String {
    "~/.zeph/zeph.pid".into()
}

fn default_health_interval() -> u64 {
    30
}

fn default_max_restart_backoff() -> u64 {
    60
}

fn default_scheduler_tick_interval() -> u64 {
    60
}

fn default_scheduler_max_tasks() -> usize {
    100
}

fn default_gateway_bind() -> String {
    "127.0.0.1".into()
}

fn default_gateway_port() -> u16 {
    8090
}

fn default_gateway_rate_limit() -> u32 {
    120
}

fn default_gateway_max_body() -> usize {
    1_048_576
}

/// Controls how skills are formatted in the system prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillPromptMode {
    Full,
    Compact,
    #[default]
    Auto,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SkillsConfig {
    #[serde(default = "default_skill_paths")]
    pub paths: Vec<String>,
    #[serde(default = "default_max_active_skills")]
    pub max_active_skills: usize,
    #[serde(default = "default_disambiguation_threshold")]
    pub disambiguation_threshold: f32,
    #[serde(default = "default_cosine_weight")]
    pub cosine_weight: f32,
    #[serde(default = "default_hybrid_search")]
    pub hybrid_search: bool,
    #[serde(default)]
    pub learning: LearningConfig,
    #[serde(default)]
    pub trust: TrustConfig,
    #[serde(default)]
    pub prompt_mode: SkillPromptMode,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct IndexConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_index_search_enabled")]
    pub search_enabled: bool,
    #[serde(default = "default_index_watch")]
    pub watch: bool,
    #[serde(default = "default_index_max_chunks")]
    pub max_chunks: usize,
    #[serde(default = "default_index_score_threshold")]
    pub score_threshold: f32,
    #[serde(default = "default_index_budget_ratio")]
    pub budget_ratio: f32,
    #[serde(default = "default_index_repo_map_tokens")]
    pub repo_map_tokens: usize,
    #[serde(default = "default_repo_map_ttl_secs")]
    pub repo_map_ttl_secs: u64,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            search_enabled: default_index_search_enabled(),
            watch: default_index_watch(),
            max_chunks: default_index_max_chunks(),
            score_threshold: default_index_score_threshold(),
            budget_ratio: default_index_budget_ratio(),
            repo_map_tokens: default_index_repo_map_tokens(),
            repo_map_ttl_secs: default_repo_map_ttl_secs(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VaultConfig {
    #[serde(default = "default_vault_backend")]
    pub backend: String,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            backend: default_vault_backend(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CostConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_daily_cents")]
    pub max_daily_cents: u32,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_daily_cents: default_max_daily_cents(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub exporter: String,
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            exporter: String::new(),
            endpoint: default_otlp_endpoint(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_gateway_bind")]
    pub bind: String,
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_gateway_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_gateway_max_body")]
    pub max_body_size: usize,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_gateway_bind(),
            port: default_gateway_port(),
            auth_token: None,
            rate_limit: default_gateway_rate_limit(),
            max_body_size: default_gateway_max_body(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_pid_file")]
    pub pid_file: String,
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    #[serde(default = "default_max_restart_backoff")]
    pub max_restart_backoff_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pid_file: default_pid_file(),
            health_interval_secs: default_health_interval(),
            max_restart_backoff_secs: default_max_restart_backoff(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_scheduler_tick_interval")]
    pub tick_interval_secs: u64,
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    #[serde(default)]
    pub tasks: Vec<ScheduledTaskConfig>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_interval_secs: default_scheduler_tick_interval(),
            max_tasks: default_scheduler_max_tasks(),
            tasks: Vec::new(),
        }
    }
}

/// Task kind for scheduled tasks.
///
/// Known variants map to built-in handlers; `Custom` accommodates user-defined task types.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskKind {
    MemoryCleanup,
    SkillRefresh,
    HealthCheck,
    UpdateCheck,
    Experiment,
    Custom(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduledTaskConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_at: Option<String>,
    pub kind: ScheduledTaskKind,
    #[serde(default)]
    pub config: serde_json::Value,
}

fn default_trace_service_name() -> String {
    "zeph".into()
}

/// Configuration for OTel-compatible trace dumps (`format = "trace"`).
///
/// When `format = "trace"`, the `TracingCollector` writes a `trace.json` file in OTLP JSON
/// format at session end. Legacy numbered dump files are NOT written by default (C-03).
/// When the `otel` feature is enabled and `otlp_endpoint` is set, spans are also exported
/// via OTLP gRPC.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TraceConfig {
    /// OTLP gRPC endpoint (only used when `otel` feature is enabled).
    /// Defaults to `observability.endpoint` if unset (I-01).
    #[serde(default = "default_otlp_endpoint")]
    pub otlp_endpoint: String,
    /// Service name reported to the `OTel` collector.
    #[serde(default = "default_trace_service_name")]
    pub service_name: String,
    /// Redact sensitive data in span attributes (default: `true`) (C-01).
    #[serde(default = "default_true")]
    pub redact: bool,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: default_otlp_endpoint(),
            service_name: default_trace_service_name(),
            redact: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DebugConfig {
    /// Enable debug dump on startup (CLI `--debug-dump` takes priority).
    pub enabled: bool,
    /// Directory where per-session debug dump subdirectories are created.
    #[serde(default = "crate::defaults::default_debug_output_dir")]
    pub output_dir: std::path::PathBuf,
    /// Output format: `"json"` (default), `"raw"` (API payload), or `"trace"` (OTLP spans).
    pub format: crate::dump_format::DumpFormat,
    /// `OTel` trace configuration (only used when `format = "trace"`).
    pub traces: TraceConfig,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            output_dir: super::defaults::default_debug_output_dir(),
            format: crate::dump_format::DumpFormat::default(),
            traces: TraceConfig::default(),
        }
    }
}
