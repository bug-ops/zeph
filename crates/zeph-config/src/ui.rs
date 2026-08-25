// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Component, Path, PathBuf};

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

/// Reserved `[[acp.auth_clients]]` id colliding with the synthesized legacy `auth_token` client.
pub const ACP_AUTH_CLIENT_ID_DEFAULT: &str = "default";
/// Reserved `[[acp.auth_clients]]` id colliding with the unauthenticated/stdio owner bucket.
pub const ACP_AUTH_CLIENT_ID_LOCAL: &str = "acp-local";

/// A single named ACP HTTP/WS bearer-token credential (#5868).
///
/// Each entry authenticates one `Authorization: Bearer <token>` value and, on match, becomes
/// the request's owner identity for ACP session-persistence scoping (`owner_key`). Exactly one
/// of `token` / `token_vault_key` must be set — `token` is inline (parity with the legacy
/// `[acp] auth_token` field), `token_vault_key` resolves the secret from the age vault at
/// startup, mirroring `[serve] auth_token_vault_key`.
#[derive(Clone, Deserialize, Serialize)]
pub struct AcpAuthClient {
    /// Stable owner label surviving token rotation. Must be non-empty, unique among
    /// `auth_clients`, and must not be `"default"` or `"acp-local"` (reserved sentinels).
    pub id: String,
    /// Inline bearer token. Mutually exclusive with `token_vault_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Vault key to resolve the bearer token from at startup. Mutually exclusive with `token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_vault_key: Option<String>,
}

impl std::fmt::Debug for AcpAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpAuthClient")
            .field("id", &self.id)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("token_vault_key", &self.token_vault_key)
            .finish()
    }
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

fn default_acp_elicitation_timeout_secs() -> u64 {
    120
}

fn default_acp_terminal_timeout_secs() -> u64 {
    120
}

fn default_acp_mcp_timeout_secs() -> u64 {
    300
}

fn default_acp_notify_ack_timeout_ms() -> u64 {
    5000
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

/// Auth methods recognised by Zeph's ACP handler.
///
/// PR 4 MVP restricts this to `Agent` only. Future variants (`EnvVar`, `Terminal`) will
/// be added in follow-up issues with their sub-struct payloads.
///
/// # Examples
///
/// ```rust
/// use zeph_config::AcpAuthMethod;
/// use serde_json;
///
/// let m: AcpAuthMethod = serde_json::from_str(r#""agent""#).unwrap();
/// assert_eq!(m, AcpAuthMethod::Agent);
/// assert!(serde_json::from_str::<AcpAuthMethod>(r#""envvar""#).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AcpAuthMethod {
    /// Vault-backed agent auth — the sole supported method in PR 4.
    Agent,
}

impl<'de> serde::Deserialize<'de> for AcpAuthMethod {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "agent" => Ok(Self::Agent),
            other => Err(serde::de::Error::unknown_variant(other, &["agent"])),
        }
    }
}

impl std::fmt::Display for AcpAuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent => f.write_str("agent"),
        }
    }
}

fn default_acp_auth_methods() -> Vec<AcpAuthMethod> {
    vec![AcpAuthMethod::Agent]
}

/// Error returned when parsing an [`AdditionalDir`] fails.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdditionalDirError {
    /// The raw path contains a `..` component.
    #[error("path `{0}` contains `..` traversal")]
    Traversal(PathBuf),
    /// The canonical path is a reserved system or credentials location.
    #[error("path `{0}` is a reserved system or credentials directory")]
    Reserved(PathBuf),
    /// `std::fs::canonicalize` failed.
    #[error("failed to canonicalize `{path}`: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A single entry in the `acp.additional_directories` policy allowlist.
///
/// Constructed via [`Self::parse`], which:
/// 1. Rejects any path containing a `..` component (component-aware check).
/// 2. Expands a leading `~` to the user's home directory.
/// 3. Calls `std::fs::canonicalize`.
/// 4. Rejects paths prefixed by `/proc`, `/sys`, `{HOME}/.ssh`, `{HOME}/.gnupg`, or `{HOME}/.aws`.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_config::AdditionalDir;
///
/// let dir = AdditionalDir::parse("/tmp/workspace").unwrap();
/// assert!(dir.as_path().is_absolute());
/// assert!(AdditionalDir::parse("/proc/self").is_err());
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct AdditionalDir(PathBuf);

impl AdditionalDir {
    /// Parse and validate a raw path as a policy allowlist entry.
    ///
    /// # Errors
    ///
    /// Returns [`AdditionalDirError`] on traversal, reserved prefix, or canonicalization failure.
    pub fn parse(raw: impl Into<PathBuf>) -> Result<Self, AdditionalDirError> {
        let raw: PathBuf = raw.into();

        // Expand leading `~`.
        let expanded = if raw.starts_with("~") {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            home.join(raw.strip_prefix("~").unwrap_or(&raw))
        } else {
            raw.clone()
        };

        // Reject `..` components (component-aware, not string-based).
        for component in expanded.components() {
            if component == Component::ParentDir {
                return Err(AdditionalDirError::Traversal(raw));
            }
        }

        let canon =
            std::fs::canonicalize(&expanded).map_err(|e| AdditionalDirError::Canonicalize {
                path: raw.clone(),
                source: e,
            })?;

        // Reject reserved locations.
        let reserved = reserved_prefixes();
        for prefix in &reserved {
            if canon.starts_with(prefix) {
                return Err(AdditionalDirError::Reserved(canon));
            }
        }

        Ok(Self(canon))
    }

    /// Returns the canonicalized path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

fn reserved_prefixes() -> Vec<PathBuf> {
    let mut prefixes = vec![PathBuf::from("/proc"), PathBuf::from("/sys")];
    if let Some(home) = dirs::home_dir() {
        prefixes.push(home.join(".ssh"));
        prefixes.push(home.join(".gnupg"));
        prefixes.push(home.join(".aws"));
    }
    prefixes
}

impl std::fmt::Debug for AdditionalDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AdditionalDir({:?})", self.0)
    }
}

impl std::fmt::Display for AdditionalDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl Serialize for AdditionalDir {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.to_string_lossy().serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for AdditionalDir {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(s).map_err(serde::de::Error::custom)
    }
}

/// Controls how much detail is shown for tool-call messages in the chat view.
///
/// Cycled with the `c` key at runtime; persisted in `[tui].tool_density`.
///
/// # Examples
///
/// ```rust
/// use zeph_config::ToolDensity;
///
/// let d = ToolDensity::default();
/// assert_eq!(d, ToolDensity::Inline);
/// assert_eq!(d.cycle(), ToolDensity::Block);
/// assert_eq!(ToolDensity::Block.cycle(), ToolDensity::Compact);
/// assert_eq!(ToolDensity::Compact.cycle(), ToolDensity::Inline);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ToolDensity {
    /// Single-line summary only (tool name + line count, no output body).
    Compact,
    /// Command line + head/tail-truncated output (default).
    #[default]
    Inline,
    /// Full output body without truncation.
    Block,
}

impl ToolDensity {
    /// Advance to the next density level, wrapping around.
    ///
    /// `Compact` → `Inline` → `Block` → `Compact`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_config::ToolDensity;
    ///
    /// assert_eq!(ToolDensity::Compact.cycle(), ToolDensity::Inline);
    /// assert_eq!(ToolDensity::Inline.cycle(), ToolDensity::Block);
    /// assert_eq!(ToolDensity::Block.cycle(), ToolDensity::Compact);
    /// ```
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Compact => Self::Inline,
            Self::Inline => Self::Block,
            Self::Block => Self::Compact,
        }
    }
}

/// Terminal colour capability override for the TUI theme system.
///
/// `Auto` runs OS-level detection at startup; any other value forces the specified mode
/// and skips detection entirely. Resolution is performed once at TUI startup and stored in
/// the TUI `App` theme.
///
/// # Example (TOML)
///
/// ```toml
/// [tui.theme]
/// color_mode = "truecolor"   # force 24-bit even if $COLORTERM is unset
/// ```
///
/// # Examples
///
/// ```rust
/// use zeph_config::ColorMode;
///
/// let mode: ColorMode = toml::from_str("value = \"auto\"")
///     .map(|t: toml::Table| t["value"].clone().try_into().unwrap())
///     .unwrap();
/// assert_eq!(mode, ColorMode::Auto);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ColorMode {
    /// Run terminal capability detection at startup (default).
    #[default]
    Auto,
    /// Force 24-bit RGB output; skip capability detection.
    Truecolor,
    /// Force RGB → xterm-256 downgrade.
    Ansi256,
    /// Force RGB → ANSI-16 downgrade.
    Ansi16,
    /// Strip all colour; retain text modifiers only (equivalent to `NO_COLOR`).
    Never,
}

/// Theme configuration nested under `[tui.theme]` in TOML.
///
/// # Example (TOML)
///
/// ```toml
/// [tui.theme]
/// name = "zephyr"
/// color_mode = "auto"
/// ```
///
/// # Examples
///
/// ```rust
/// use zeph_config::ThemeConfig;
///
/// let cfg = ThemeConfig::default();
/// assert_eq!(cfg.name, "");
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Named theme preset (e.g. `"zephyr"`, `"gruvbox-dark"`).
    ///
    /// Empty string resolves to the `zephyr` built-in preset.
    pub name: String,
    /// Terminal colour capability override. Default: `auto` (detect at runtime).
    pub color_mode: ColorMode,
}

/// Controls how much animation the TUI renders.
///
/// Set via `[tui] motion = "full" | "minimal" | "off"` in TOML.
/// Default: `full`.
///
/// - `full` — wave animation on the input separator row while busy, no breeze spinner.
/// - `minimal` — animated breeze spinner (current behaviour before #5096), no wave.
/// - `off` — no animation at all; input row is frame-invariant even while busy.
///
/// # Example (TOML)
///
/// ```toml
/// [tui]
/// motion = "minimal"
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Motion {
    /// Wave animation on the input separator row while busy.
    #[default]
    Full,
    /// Animated breeze spinner, no wave.
    Minimal,
    /// No animation; input row is frame-invariant.
    Off,
}

/// Micro-delight toggles for the TUI dashboard (#5104).
///
/// All features default to `true`. The `motion = off` setting in [`TuiConfig`]
/// acts as a master kill-switch that overrides every individual toggle.
///
/// # Example (TOML)
///
/// ```toml
/// [tui.delights]
/// stream_metrics   = true   # tok/s during streaming + TTFT in status bar
/// toasts           = true   # ephemeral overlay notifications
/// completion_flash = true   # accent tint on finished tool groups
/// smooth_scroll    = true   # eased multi-frame scroll on page jumps
/// splash_shimmer   = true   # one-shot gradient sweep across the wordmark
/// ```
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DelightsConfig {
    /// Show tok/s during streaming and TTFT after each turn in the status bar.
    #[serde(default = "default_true")]
    pub stream_metrics: bool,
    /// Ephemeral toast notifications (theme switched, copied, task done).
    #[serde(default = "default_true")]
    pub toasts: bool,
    /// One-frame accent tint when a tool group finishes.
    #[serde(default = "default_true")]
    pub completion_flash: bool,
    /// Eased multi-frame interpolation on page scroll.
    #[serde(default = "default_true")]
    pub smooth_scroll: bool,
    /// One-shot gradient shimmer across the splash wordmark at startup.
    #[serde(default = "default_true")]
    pub splash_shimmer: bool,
}

impl Default for DelightsConfig {
    fn default() -> Self {
        Self {
            stream_metrics: true,
            toasts: true,
            completion_flash: true,
            smooth_scroll: true,
            splash_shimmer: true,
        }
    }
}

/// TUI (terminal user interface) configuration, nested under `[tui]` in TOML.
///
/// # Example (TOML)
///
/// ```toml
/// [tui]
/// show_source_labels = true
/// tool_density = "inline"
/// motion = "full"
///
/// [tui.theme]
/// name = "zephyr"
/// color_mode = "auto"
///
/// [tui.delights]
/// stream_metrics   = true
/// toasts           = true
/// completion_flash = true
/// smooth_scroll    = true
/// splash_shimmer   = true
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TuiConfig {
    /// Show memory source labels (episodic / semantic / graph) in the message view.
    /// Default: `false`.
    #[serde(default)]
    pub show_source_labels: bool,
    /// Default tool-output density applied at startup.
    ///
    /// Runtime changes via the `c` key are not persisted back to config.
    /// Default: `inline`.
    #[serde(default)]
    pub tool_density: ToolDensity,
    /// Animation budget for the input separator row.
    ///
    /// `full` = wave (default), `minimal` = breeze spinner, `off` = static.
    #[serde(default)]
    pub motion: Motion,
    /// Fleet panel configuration (auto-refresh interval and max sessions displayed).
    #[serde(default)]
    pub fleet: FleetConfig,
    /// Theme and colour capability configuration.
    #[serde(default)]
    pub theme: ThemeConfig,
    /// Micro-delight toggles (tok/s, toasts, flash, scroll, shimmer). All default `true`.
    ///
    /// `motion = off` overrides all toggles regardless of their individual values.
    #[serde(default)]
    pub delights: DelightsConfig,
    /// Enable opt-in mouse capture at startup.
    ///
    /// When `true`, the terminal forwards scroll-wheel, click, and drag events to
    /// the TUI. Text selection via Shift+drag still works. Default: `false`.
    #[serde(default)]
    pub mouse: bool,
    /// Side-panel vertical sizing strategy (#6675).
    ///
    /// `auto` (default) sizes each unpinned side panel from its own content; `even`
    /// approximates the pre-#6675 behavior of splitting the column equally regardless of
    /// content (same total per slot; exact per-slot remainder placement can differ from the
    /// old cassowary-based split — see `zeph_tui::layout::PanelDemand`'s docs). Runtime-
    /// togglable via `/panel_sizing [auto|even]`.
    #[serde(default)]
    pub panel_sizing: PanelSizingMode,
}

/// Side-panel vertical sizing strategy (see [`TuiConfig::panel_sizing`], #6675).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PanelSizingMode {
    /// Size each unpinned side panel from its own content (`desired_height`), via a
    /// max-min fair water-filling allocator. Leftover space stays blank at the bottom of
    /// the column. Default.
    #[default]
    Auto,
    /// Approximates pre-#6675 behavior: unpinned panels split the column evenly, regardless
    /// of content (same total height per slot; exact remainder placement can differ from
    /// the old cassowary-based split).
    Even,
}

/// Configuration for the TUI fleet panel (#3884).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct FleetConfig {
    /// How often the fleet panel polls the database for updated session data (seconds).
    pub refresh_interval_secs: u64,
    /// Maximum number of sessions to display in the fleet panel.
    pub max_sessions: u32,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 5,
            max_sessions: 50,
        }
    }
}

/// ACP server transport mode.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum AcpTransport {
    /// JSON-RPC over stdin/stdout (default, IDE embedding).
    #[default]
    Stdio,
    /// JSON-RPC over HTTP+SSE and WebSocket.
    Http,
    /// Both stdio and HTTP transports active simultaneously.
    Both,
}

/// Configuration for a named sub-agent preset in `[[acp.subagents.presets]]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SubagentPresetConfig {
    /// Identifier used to reference this preset by name.
    pub name: String,
    /// Shell command string to spawn the sub-agent (e.g. `"cargo run -- --acp"`).
    pub command: String,
    /// Optional working directory for the spawned subprocess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Timeout in seconds for the `initialize` + `session/new` handshake. Default: 30.
    #[serde(default = "default_subagent_handshake_timeout_secs")]
    pub handshake_timeout_secs: u64,
    /// Timeout in seconds for a single prompt round-trip. Default: 600.
    #[serde(default = "default_subagent_prompt_timeout_secs")]
    pub prompt_timeout_secs: u64,
}

/// Configuration block for the `[acp.subagents]` TOML section.
///
/// # Example
///
/// ```toml
/// [acp.subagents]
/// enabled = true
///
/// [[acp.subagents.presets]]
/// name = "inner"
/// command = "cargo run --quiet -- --acp"
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AcpSubagentsConfig {
    /// Whether sub-agent spawning is enabled at runtime. Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Named presets available via CLI (`zeph acp subagent list`) and TUI palette.
    #[serde(default)]
    pub presets: Vec<SubagentPresetConfig>,
}

fn default_subagent_handshake_timeout_secs() -> u64 {
    30
}

fn default_subagent_prompt_timeout_secs() -> u64 {
    600
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
    /// Named ACP HTTP/WS bearer-token clients (#5868), for genuine multi-tenant/multi-window
    /// isolation of persisted session listing. Coexists with the legacy `auth_token` field,
    /// which is synthesized as a client with id `"default"`. See [`AcpAuthClient`].
    #[serde(default)]
    pub auth_clients: Vec<AcpAuthClient>,
    /// Whether to serve the /.well-known/acp.json agent discovery manifest.
    /// Only effective when transport is "http" or "both". Default: true.
    #[serde(default = "default_acp_discovery_enabled")]
    pub discovery_enabled: bool,
    /// LSP extension configuration (`[acp.lsp]`).
    #[serde(default)]
    pub lsp: AcpLspConfig,
    /// Allowlist of workspace directories that ACP clients may reference in session requests.
    ///
    /// Paths are canonicalized at config load; traversal (`..`) and reserved locations
    /// (`/proc`, `/sys`, `~/.ssh`, `~/.gnupg`, `~/.aws`) are rejected with an error.
    /// An empty list means clients may not request any additional directories beyond the
    /// session `cwd`.
    ///
    /// This is a **policy** allowlist, not a protocol advertisement: the agent never returns
    /// `additional_directories` in any response; instead it validates each session request's
    /// `additional_directories` field against this list and rejects with `invalid_params`
    /// on any violation.
    #[serde(default)]
    pub additional_directories: Vec<AdditionalDir>,
    /// Auth methods advertised in the ACP `initialize` response.
    ///
    /// PR 4 MVP accepts only `"agent"`. Config load fails on any other value so drift
    /// from the schema is detected at startup rather than silently ignored.
    #[serde(default = "default_acp_auth_methods")]
    pub auth_methods: Vec<AcpAuthMethod>,
    /// Echo `PromptRequest.message_id` onto `PromptResponse.user_message_id` and every
    /// streamed chunk, enabling IDE-side correlation.
    ///
    /// Default: `true`.
    #[serde(default = "default_true")]
    pub message_ids_enabled: bool,
    /// Sub-agent delegation configuration (`[acp.subagents]`).
    #[serde(default)]
    pub subagents: AcpSubagentsConfig,
    /// Timeout configuration for ACP operations (`[acp.timeouts]`).
    #[serde(default)]
    pub timeouts: AcpTimeoutsConfig,
    /// Model-related configuration parameters (`[acp.model_config]`), advertised to IDE
    /// clients via the `model_config` `session/set_config_option` category (schema 1.1.0+).
    #[serde(default)]
    pub model_config: AcpModelConfigConfig,
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
            auth_clients: Vec::new(),
            discovery_enabled: default_acp_discovery_enabled(),
            lsp: AcpLspConfig::default(),
            additional_directories: Vec::new(),
            auth_methods: default_acp_auth_methods(),
            message_ids_enabled: true,
            subagents: AcpSubagentsConfig::default(),
            timeouts: AcpTimeoutsConfig::default(),
            model_config: AcpModelConfigConfig::default(),
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
            .field("auth_clients", &self.auth_clients)
            .field("discovery_enabled", &self.discovery_enabled)
            .field("lsp", &self.lsp)
            .field("additional_directories", &self.additional_directories)
            .field("auth_methods", &self.auth_methods)
            .field("message_ids_enabled", &self.message_ids_enabled)
            .field("subagents", &self.subagents)
            .field("timeouts", &self.timeouts)
            .field("model_config", &self.model_config)
            .finish()
    }
}

impl AcpConfig {
    /// Validate `auth_token` / `auth_clients` coexistence (#5868).
    ///
    /// Checks only what is decidable from the config file alone — `id` uniqueness, the
    /// reserved-sentinel rule, exactly-one-of `token`/`token_vault_key` per entry, non-empty
    /// inline tokens, and duplicate *inline* tokens (including the legacy `auth_token`).
    /// Vault-resolved tokens are cross-checked for collisions at startup instead (after the
    /// vault is unlocked), since that value isn't known here.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message describing the first violation found.
    pub fn validate_auth_clients(&self) -> Result<(), String> {
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_inline_tokens = std::collections::HashSet::new();

        if let Some(ref token) = self.auth_token {
            if token.trim().is_empty() {
                return Err("[acp] auth_token must not be empty or whitespace-only".to_owned());
            }
            seen_inline_tokens.insert(token.as_str());
        }

        for client in &self.auth_clients {
            if client.id.is_empty() {
                return Err("[[acp.auth_clients]] entry has an empty id".to_owned());
            }
            if client.id == ACP_AUTH_CLIENT_ID_DEFAULT || client.id == ACP_AUTH_CLIENT_ID_LOCAL {
                return Err(format!(
                    "[[acp.auth_clients]] id {:?} is reserved (collides with the legacy \
                     auth_token client or the unauthenticated/stdio owner bucket)",
                    client.id
                ));
            }
            if client.id.contains(':') {
                return Err(format!(
                    "[[acp.auth_clients]] id {:?} must not contain ':'",
                    client.id
                ));
            }
            if !seen_ids.insert(client.id.as_str()) {
                return Err(format!(
                    "[[acp.auth_clients]] id {:?} is duplicated",
                    client.id
                ));
            }
            match (&client.token, &client.token_vault_key) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "[[acp.auth_clients]] id {:?} sets both token and token_vault_key; \
                         exactly one must be set",
                        client.id
                    ));
                }
                (None, None) => {
                    return Err(format!(
                        "[[acp.auth_clients]] id {:?} sets neither token nor token_vault_key; \
                         exactly one must be set",
                        client.id
                    ));
                }
                (Some(token), None) => {
                    if token.trim().is_empty() {
                        return Err(format!(
                            "[[acp.auth_clients]] id {:?} has an empty or whitespace-only token",
                            client.id
                        ));
                    }
                    if !seen_inline_tokens.insert(token.as_str()) {
                        return Err(format!(
                            "[[acp.auth_clients]] id {:?} has a token that collides with \
                             another configured client's inline token",
                            client.id
                        ));
                    }
                }
                (None, Some(_)) => {}
            }
        }

        Ok(())
    }
}

/// Sampling-temperature preset for ACP `model_config` session options.
///
/// Maps a discrete, IDE-friendly selector (`"precise"` | `"balanced"` | `"creative"`) onto a
/// concrete sampling temperature, since the ACP `SessionConfigOption` select type only
/// supports discrete values, not a free-form numeric input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpTemperaturePreset {
    /// Low temperature (0.2) — more deterministic, focused completions.
    Precise,
    /// Moderate temperature (0.7) — balanced determinism and variety. Default.
    #[default]
    Balanced,
    /// High temperature (1.0) — more varied, exploratory completions.
    Creative,
}

impl AcpTemperaturePreset {
    /// Returns the concrete sampling temperature for this preset.
    #[must_use]
    pub fn temperature(self) -> f64 {
        match self {
            Self::Precise => 0.2,
            Self::Balanced => 0.7,
            Self::Creative => 1.0,
        }
    }

    /// Returns the ACP wire identifier for this preset (`"precise"` | `"balanced"` | `"creative"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Precise => "precise",
            Self::Balanced => "balanced",
            Self::Creative => "creative",
        }
    }
}

impl std::str::FromStr for AcpTemperaturePreset {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "precise" => Ok(Self::Precise),
            "balanced" => Ok(Self::Balanced),
            "creative" => Ok(Self::Creative),
            _ => Err(()),
        }
    }
}

/// Model-related configuration parameters configuration, nested under `[acp.model_config]`.
///
/// Backs the ACP `model_config` `session/set_config_option` category (schema 1.1.0+), which is
/// distinct from the `model` category: `model` selects which model is active, `model_config`
/// adjusts a parameter (e.g. sampling temperature) of the currently selected model.
///
/// # Example (TOML)
///
/// ```toml
/// [acp.model_config]
/// default_temperature_preset = "balanced"
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AcpModelConfigConfig {
    /// Default sampling-temperature preset applied to new ACP sessions. Default: `"balanced"`.
    #[serde(default)]
    pub default_temperature_preset: AcpTemperaturePreset,
}

/// Timeout configuration for ACP operations.
///
/// These values replace the previously hardcoded 120-second defaults for terminal
/// and elicitation operations, and the 300-second default for MCP bridge calls.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcpTimeoutsConfig {
    /// Timeout in seconds for elicitation requests sent to the IDE. Default: 120.
    #[serde(default = "default_acp_elicitation_timeout_secs")]
    pub elicitation_secs: u64,
    /// Timeout in seconds for terminal command execution. Default: 120.
    #[serde(default = "default_acp_terminal_timeout_secs")]
    pub terminal_secs: u64,
    /// Timeout in seconds for MCP bridge operations. Default: 300.
    #[serde(default = "default_acp_mcp_timeout_secs")]
    pub mcp_secs: u64,
    /// Maximum time in milliseconds to wait for a notification ack from the IDE client.
    ///
    /// If the IDE client does not acknowledge a session notification within this window,
    /// `send_notification` returns an error instead of blocking indefinitely. Default: 5000.
    #[serde(default = "default_acp_notify_ack_timeout_ms")]
    pub notify_ack_timeout_ms: u64,
}

impl Default for AcpTimeoutsConfig {
    fn default() -> Self {
        Self {
            elicitation_secs: default_acp_elicitation_timeout_secs(),
            terminal_secs: default_acp_terminal_timeout_secs(),
            mcp_secs: default_acp_mcp_timeout_secs(),
            notify_ack_timeout_ms: default_acp_notify_ack_timeout_ms(),
        }
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
#[non_exhaustive]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_auth_method_unknown_variant_fails() {
        assert!(serde_json::from_str::<AcpAuthMethod>(r#""bearer""#).is_err());
        assert!(serde_json::from_str::<AcpAuthMethod>(r#""envvar""#).is_err());
        assert!(serde_json::from_str::<AcpAuthMethod>(r#""Agent""#).is_err());
    }

    #[test]
    fn acp_auth_method_known_variant_succeeds() {
        let m = serde_json::from_str::<AcpAuthMethod>(r#""agent""#).unwrap();
        assert_eq!(m, AcpAuthMethod::Agent);
    }

    // ── AcpConfig::validate_auth_clients (#5868) ──────────────────────────────

    fn client(id: &str, token: &str) -> AcpAuthClient {
        AcpAuthClient {
            id: id.to_owned(),
            token: Some(token.to_owned()),
            token_vault_key: None,
        }
    }

    #[test]
    fn validate_auth_clients_empty_config_ok() {
        assert!(AcpConfig::default().validate_auth_clients().is_ok());
    }

    #[test]
    fn validate_auth_clients_legacy_auth_token_only_ok() {
        let cfg = AcpConfig {
            auth_token: Some("secret".to_owned()),
            ..AcpConfig::default()
        };
        assert!(cfg.validate_auth_clients().is_ok());
    }

    #[test]
    fn validate_auth_clients_single_client_ok() {
        let cfg = AcpConfig {
            auth_clients: vec![client("alice", "token-a")],
            ..AcpConfig::default()
        };
        assert!(cfg.validate_auth_clients().is_ok());
    }

    #[test]
    fn validate_auth_clients_coexist_with_legacy_ok() {
        let cfg = AcpConfig {
            auth_token: Some("legacy".to_owned()),
            auth_clients: vec![client("alice", "token-a"), client("bob", "token-b")],
            ..AcpConfig::default()
        };
        assert!(cfg.validate_auth_clients().is_ok());
    }

    #[test]
    fn validate_auth_clients_rejects_reserved_id_default() {
        let cfg = AcpConfig {
            auth_clients: vec![client(ACP_AUTH_CLIENT_ID_DEFAULT, "token-a")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("reserved"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_reserved_id_acp_local() {
        let cfg = AcpConfig {
            auth_clients: vec![client(ACP_AUTH_CLIENT_ID_LOCAL, "token-a")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("reserved"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_duplicate_id() {
        let cfg = AcpConfig {
            auth_clients: vec![client("alice", "token-a"), client("alice", "token-b")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("duplicated"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_id_containing_colon() {
        let cfg = AcpConfig {
            auth_clients: vec![client("alice:2", "token-a")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("':'"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_empty_id() {
        let cfg = AcpConfig {
            auth_clients: vec![client("", "token-a")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("empty id"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_neither_token_nor_vault_key() {
        let cfg = AcpConfig {
            auth_clients: vec![AcpAuthClient {
                id: "alice".to_owned(),
                token: None,
                token_vault_key: None,
            }],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("neither"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_both_token_and_vault_key() {
        let cfg = AcpConfig {
            auth_clients: vec![AcpAuthClient {
                id: "alice".to_owned(),
                token: Some("token-a".to_owned()),
                token_vault_key: Some("ZEPH_ACP_TOKEN_ALICE".to_owned()),
            }],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("both"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_vault_key_only_ok() {
        let cfg = AcpConfig {
            auth_clients: vec![AcpAuthClient {
                id: "alice".to_owned(),
                token: None,
                token_vault_key: Some("ZEPH_ACP_TOKEN_ALICE".to_owned()),
            }],
            ..AcpConfig::default()
        };
        assert!(cfg.validate_auth_clients().is_ok());
    }

    #[test]
    fn validate_auth_clients_rejects_duplicate_inline_tokens_across_clients() {
        let cfg = AcpConfig {
            auth_clients: vec![client("alice", "shared"), client("bob", "shared")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("collides"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_inline_token_colliding_with_legacy_default_token() {
        let cfg = AcpConfig {
            auth_token: Some("shared".to_owned()),
            auth_clients: vec![client("alice", "shared")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("collides"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_empty_legacy_auth_token() {
        let cfg = AcpConfig {
            auth_token: Some(String::new()),
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_whitespace_only_legacy_auth_token() {
        let cfg = AcpConfig {
            auth_token: Some("   ".to_owned()),
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_empty_inline_client_token() {
        let cfg = AcpConfig {
            auth_clients: vec![client("alice", "")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn validate_auth_clients_rejects_whitespace_only_inline_client_token() {
        let cfg = AcpConfig {
            auth_clients: vec![client("alice", "   ")],
            ..AcpConfig::default()
        };
        let err = cfg.validate_auth_clients().unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn additional_dir_rejects_dotdot_traversal() {
        let result = AdditionalDir::parse(std::path::PathBuf::from("/tmp/../etc"));
        assert!(
            matches!(result, Err(AdditionalDirError::Traversal(_))),
            "expected Traversal, got {result:?}"
        );
    }

    #[test]
    fn additional_dir_rejects_proc() {
        // /proc must exist on Linux CI; skip on macOS if not present.
        if !std::path::Path::new("/proc").exists() {
            return;
        }
        let result = AdditionalDir::parse(std::path::PathBuf::from("/proc/self"));
        assert!(
            matches!(result, Err(AdditionalDirError::Reserved(_))),
            "expected Reserved, got {result:?}"
        );
    }

    #[test]
    fn additional_dir_rejects_ssh() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
        let ssh = std::path::PathBuf::from(format!("{home}/.ssh"));
        if !ssh.exists() {
            return;
        }
        let result = AdditionalDir::parse(ssh.clone());
        assert!(
            matches!(result, Err(AdditionalDirError::Reserved(_))),
            "expected Reserved for {ssh:?}, got {result:?}"
        );
    }

    #[test]
    fn additional_dir_accepts_tmp() {
        let tmp = std::env::temp_dir();
        // tempdir always exists; /tmp is not reserved.
        match AdditionalDir::parse(tmp.clone()) {
            Ok(dir) => {
                // canonicalized path stored correctly
                assert!(dir.as_path().is_absolute());
            }
            Err(AdditionalDirError::Canonicalize { .. }) => {
                // temp_dir may be a symlink that canonicalizes to something else — acceptable
            }
            Err(e) => panic!("unexpected error for {tmp:?}: {e:?}"),
        }
    }
}
