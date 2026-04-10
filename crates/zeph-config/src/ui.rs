// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::default_true;

fn default_acp_agent_name() -> String {
    "zeph".to_owned()
}

fn default_acp_agent_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn default_acp_max_sessions() -> usize {
    4
}

fn default_acp_session_idle_timeout_secs() -> u64 {
    1800
}

fn default_acp_broadcast_capacity() -> usize {
    256
}

fn default_acp_transport() -> AcpTransport {
    AcpTransport::Stdio
}

fn default_acp_http_bind() -> String {
    "127.0.0.1:9800".to_owned()
}

fn default_acp_discovery_enabled() -> bool {
    true
}

fn default_acp_lsp_max_diagnostics_per_file() -> usize {
    20
}

fn default_acp_lsp_max_diagnostic_files() -> usize {
    5
}

fn default_acp_lsp_max_references() -> usize {
    100
}

fn default_acp_lsp_max_workspace_symbols() -> usize {
    50
}

fn default_acp_lsp_request_timeout_secs() -> u64 {
    10
}
fn default_lsp_mcp_server_id() -> String {
    "mcpls".into()
}
fn default_lsp_token_budget() -> usize {
    2000
}
fn default_lsp_max_per_file() -> usize {
    20
}
fn default_lsp_max_symbols() -> usize {
    5
}
fn default_lsp_call_timeout_secs() -> u64 {
    5
}

/// TUI (terminal user interface) configuration, nested under `[tui]` in TOML.
///
/// # Example (TOML)
///
/// ```toml
/// [tui]
/// show_source_labels = true
/// ```
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct TuiConfig {
    /// Show memory source labels (episodic / semantic / graph) in the message view.
    /// Default: `false`.
    #[serde(default)]
    pub show_source_labels: bool,
}

/// ACP server transport mode.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpTransport {
    /// JSON-RPC over stdin/stdout (default, IDE embedding).
    #[default]
    Stdio,
    /// JSON-RPC over HTTP+SSE and WebSocket.
    Http,
    /// Both stdio and HTTP transports active simultaneously.
    Both,
}

/// ACP (Agent Communication Protocol) server configuration, nested under `[acp]` in TOML.
///
/// When `enabled = true`, Zeph exposes an ACP endpoint that IDE integrations (e.g. Zed, VS Code)
/// can connect to for conversational coding assistance. Supports stdio and HTTP transports.
///
/// # Example (TOML)
///
/// ```toml
/// [acp]
/// enabled = true
/// transport = "stdio"
/// agent_name = "zeph"
/// max_sessions = 4
/// ```
#[derive(Clone, Deserialize, Serialize)]
pub struct AcpConfig {
    /// Enable the ACP server. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Agent name advertised in the ACP `initialize` response. Default: `"zeph"`.
    #[serde(default = "default_acp_agent_name")]
    pub agent_name: String,
    /// Agent version advertised in the ACP `initialize` response. Default: crate version.
    #[serde(default = "default_acp_agent_version")]
    pub agent_version: String,
    /// Maximum number of concurrent ACP sessions. Default: `4`.
    #[serde(default = "default_acp_max_sessions")]
    pub max_sessions: usize,
    /// Seconds of inactivity before an idle session is closed. Default: `1800`.
    #[serde(default = "default_acp_session_idle_timeout_secs")]
    pub session_idle_timeout_secs: u64,
    /// Broadcast channel capacity for streaming events. Default: `256`.
    #[serde(default = "default_acp_broadcast_capacity")]
    pub broadcast_capacity: usize,
    /// Path to the ACP permission TOML file controlling per-session tool access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_file: Option<std::path::PathBuf>,
    /// List of `{provider}:{model}` identifiers advertised to the IDE for model switching.
    /// Example: `["claude:claude-sonnet-4-5", "ollama:llama3"]`
    #[serde(default)]
    pub available_models: Vec<String>,
    /// Transport mode: "stdio" (default), "http", or "both".
    #[serde(default = "default_acp_transport")]
    pub transport: AcpTransport,
    /// Bind address for the HTTP transport.
    #[serde(default = "default_acp_http_bind")]
    pub http_bind: String,
    /// Bearer token for HTTP and WebSocket transport authentication.
    /// When set, all /acp and /acp/ws requests must include `Authorization: Bearer <token>`.
    /// Omit for local unauthenticated access. TLS termination is assumed to be handled by a
    /// reverse proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Whether to serve the /.well-known/acp.json agent discovery manifest.
    /// Only effective when transport is "http" or "both". Default: true.
    #[serde(default = "default_acp_discovery_enabled")]
    pub discovery_enabled: bool,
    /// LSP extension configuration (`[acp.lsp]`).
    #[serde(default)]
    pub lsp: AcpLspConfig,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agent_name: default_acp_agent_name(),
            agent_version: default_acp_agent_version(),
            max_sessions: default_acp_max_sessions(),
            session_idle_timeout_secs: default_acp_session_idle_timeout_secs(),
            broadcast_capacity: default_acp_broadcast_capacity(),
            permission_file: None,
            available_models: Vec::new(),
            transport: default_acp_transport(),
            http_bind: default_acp_http_bind(),
            auth_token: None,
            discovery_enabled: default_acp_discovery_enabled(),
            lsp: AcpLspConfig::default(),
        }
    }
}

impl std::fmt::Debug for AcpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpConfig")
            .field("enabled", &self.enabled)
            .field("agent_name", &self.agent_name)
            .field("agent_version", &self.agent_version)
            .field("max_sessions", &self.max_sessions)
            .field("session_idle_timeout_secs", &self.session_idle_timeout_secs)
            .field("broadcast_capacity", &self.broadcast_capacity)
            .field("permission_file", &self.permission_file)
            .field("available_models", &self.available_models)
            .field("transport", &self.transport)
            .field("http_bind", &self.http_bind)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("discovery_enabled", &self.discovery_enabled)
            .field("lsp", &self.lsp)
            .finish()
    }
}

/// Configuration for the ACP LSP extension.
///
/// Controls LSP code intelligence features when connected to an IDE that advertises
/// `meta["lsp"]` capability during ACP `initialize`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcpLspConfig {
    /// Enable LSP extension when the IDE supports it. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Automatically fetch diagnostics when `lsp/didSave` notification is received.
    #[serde(default = "default_true")]
    pub auto_diagnostics_on_save: bool,
    /// Maximum diagnostics to accept per file. Default: 20.
    #[serde(default = "default_acp_lsp_max_diagnostics_per_file")]
    pub max_diagnostics_per_file: usize,
    /// Maximum files in `DiagnosticsCache` (LRU eviction). Default: 5.
    #[serde(default = "default_acp_lsp_max_diagnostic_files")]
    pub max_diagnostic_files: usize,
    /// Maximum reference locations returned. Default: 100.
    #[serde(default = "default_acp_lsp_max_references")]
    pub max_references: usize,
    /// Maximum workspace symbol search results. Default: 50.
    #[serde(default = "default_acp_lsp_max_workspace_symbols")]
    pub max_workspace_symbols: usize,
    /// Timeout in seconds for LSP `ext_method` calls. Default: 10.
    #[serde(default = "default_acp_lsp_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

impl Default for AcpLspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_diagnostics_on_save: true,
            max_diagnostics_per_file: default_acp_lsp_max_diagnostics_per_file(),
            max_diagnostic_files: default_acp_lsp_max_diagnostic_files(),
            max_references: default_acp_lsp_max_references(),
            max_workspace_symbols: default_acp_lsp_max_workspace_symbols(),
            request_timeout_secs: default_acp_lsp_request_timeout_secs(),
        }
    }
}

// ── LSP context injection ─────────────────────────────────────────────────────

/// Minimum diagnostic severity to include in LSP context injection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    #[default]
    Error,
    Warning,
    Info,
    Hint,
}

/// Configuration for the diagnostics-on-save hook (`[agent.lsp.diagnostics]`).
///
/// Flood control relies on `token_budget` in [`LspConfig`], not a per-file count.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DiagnosticsConfig {
    /// Enable automatic diagnostics fetching after the `write` tool.
    pub enabled: bool,
    /// Maximum diagnostics entries per file.
    #[serde(default = "default_lsp_max_per_file")]
    pub max_per_file: usize,
    /// Minimum severity to include.
    #[serde(default)]
    pub min_severity: DiagnosticSeverity,
}
impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_file: default_lsp_max_per_file(),
            min_severity: DiagnosticSeverity::default(),
        }
    }
}

/// Configuration for the hover-on-read hook (`[agent.lsp.hover]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct HoverConfig {
    /// Enable hover info pre-fetch after the `read` tool. Disabled by default.
    pub enabled: bool,
    /// Maximum hover entries per file (Rust-only for MVP).
    #[serde(default = "default_lsp_max_symbols")]
    pub max_symbols: usize,
}
impl Default for HoverConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_symbols: default_lsp_max_symbols(),
        }
    }
}

/// Top-level LSP context injection configuration (`[agent.lsp]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LspConfig {
    /// Enable LSP context injection hooks.
    pub enabled: bool,
    /// MCP server ID to route LSP calls through (default: "mcpls").
    #[serde(default = "default_lsp_mcp_server_id")]
    pub mcp_server_id: String,
    /// Maximum tokens to spend on injected LSP context per turn.
    #[serde(default = "default_lsp_token_budget")]
    pub token_budget: usize,
    /// Timeout in seconds for each MCP LSP call.
    #[serde(default = "default_lsp_call_timeout_secs")]
    pub call_timeout_secs: u64,
    /// Diagnostics-on-save hook configuration.
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
    /// Hover-on-read hook configuration.
    #[serde(default)]
    pub hover: HoverConfig,
}
impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mcp_server_id: default_lsp_mcp_server_id(),
            token_budget: default_lsp_token_budget(),
            call_timeout_secs: default_lsp_call_timeout_secs(),
            diagnostics: DiagnosticsConfig::default(),
            hover: HoverConfig::default(),
        }
    }
}
