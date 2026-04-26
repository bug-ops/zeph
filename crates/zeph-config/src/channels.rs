// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::defaults::default_true;
use crate::providers::ProviderName;

pub use crate::mcp_security::ToolSecurityMeta;

// ── MCP trust and policy types (moved from zeph-mcp) ─────────────────────────

/// Trust level for an MCP server connection.
///
/// Controls SSRF validation, tool filtering, and data-flow policy enforcement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTrustLevel {
    /// Full trust — all tools exposed, SSRF check skipped. Use for operator-controlled servers.
    Trusted,
    /// Default. SSRF enforced. Tools exposed with a warning when allowlist is empty.
    #[default]
    Untrusted,
    /// Strict sandboxing — SSRF enforced. Only allowlisted tools exposed; empty allowlist = no tools.
    Sandboxed,
}

impl McpTrustLevel {
    /// Returns a numeric restriction level where higher means more restricted.
    ///
    /// Used for "only demote, never promote automatically" comparisons.
    #[must_use]
    pub fn restriction_level(self) -> u8 {
        match self {
            Self::Trusted => 0,
            Self::Untrusted => 1,
            Self::Sandboxed => 2,
        }
    }
}

/// Rate limit configuration for a single MCP server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimit {
    /// Maximum number of tool calls allowed per minute across all tools on this server.
    pub max_calls_per_minute: u32,
}

/// Per-server MCP policy.
///
/// No policy present = allow all (backward compatible default).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpPolicy {
    /// Allowlist of tool names. `None` means all tools are allowed (subject to `denied_tools`).
    pub allowed_tools: Option<Vec<String>>,
    /// Denylist of tool names. Takes precedence over `allowed_tools`.
    pub denied_tools: Vec<String>,
    /// Optional rate limit for this server.
    pub rate_limit: Option<RateLimit>,
}

fn default_skill_allowlist() -> Vec<String> {
    vec!["*".into()]
}

/// Per-channel skill allowlist configuration.
///
/// Declares which skills are permitted on a given channel. The config is parsed and
/// `is_skill_allowed()` is available for callers to check membership. Runtime enforcement
/// (filtering skills before prompt assembly) is tracked in issue #2507 and not yet wired.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelSkillsConfig {
    /// Skill allowlist. `["*"]` = all skills allowed. `[]` = deny all.
    /// Supports exact names and `*` wildcard (e.g. `"web-*"` matches `"web-search"`).
    #[serde(default = "default_skill_allowlist")]
    pub allowed: Vec<String>,
}

impl Default for ChannelSkillsConfig {
    fn default() -> Self {
        Self {
            allowed: default_skill_allowlist(),
        }
    }
}

/// Returns `true` if the skill `name` matches any pattern in the allowlist.
///
/// Pattern rules: `"*"` matches any name; `"prefix-*"` matches names starting with `"prefix-"`;
/// exact strings match only themselves. Matching is case-sensitive.
#[must_use]
pub fn is_skill_allowed(name: &str, config: &ChannelSkillsConfig) -> bool {
    config.allowed.iter().any(|p| glob_match(p, name))
}

fn glob_match(pattern: &str, name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        if prefix.is_empty() {
            return true;
        }
        name.starts_with(prefix)
    } else {
        pattern == name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(patterns: &[&str]) -> ChannelSkillsConfig {
        ChannelSkillsConfig {
            allowed: patterns.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn test_default_output_schema_hint_bytes_is_1024() {
        assert_eq!(default_output_schema_hint_bytes(), 1024);
    }

    #[test]
    fn test_mcp_config_default_output_schema_hint_bytes_is_1024() {
        let cfg = McpConfig::default();
        assert_eq!(cfg.output_schema_hint_bytes, 1024);
    }

    #[test]
    fn wildcard_star_allows_any_skill() {
        let cfg = allow(&["*"]);
        assert!(is_skill_allowed("anything", &cfg));
        assert!(is_skill_allowed("web-search", &cfg));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        let cfg = allow(&[]);
        assert!(!is_skill_allowed("web-search", &cfg));
        assert!(!is_skill_allowed("shell", &cfg));
    }

    #[test]
    fn exact_match_allows_only_that_skill() {
        let cfg = allow(&["web-search"]);
        assert!(is_skill_allowed("web-search", &cfg));
        assert!(!is_skill_allowed("shell", &cfg));
        assert!(!is_skill_allowed("web-search-extra", &cfg));
    }

    #[test]
    fn prefix_wildcard_allows_matching_skills() {
        let cfg = allow(&["web-*"]);
        assert!(is_skill_allowed("web-search", &cfg));
        assert!(is_skill_allowed("web-fetch", &cfg));
        assert!(!is_skill_allowed("shell", &cfg));
        assert!(!is_skill_allowed("awesome-web-thing", &cfg));
    }

    #[test]
    fn multiple_patterns_or_logic() {
        let cfg = allow(&["shell", "web-*"]);
        assert!(is_skill_allowed("shell", &cfg));
        assert!(is_skill_allowed("web-search", &cfg));
        assert!(!is_skill_allowed("memory", &cfg));
    }

    #[test]
    fn default_config_allows_all() {
        let cfg = ChannelSkillsConfig::default();
        assert!(is_skill_allowed("any-skill", &cfg));
    }

    #[test]
    fn prefix_wildcard_does_not_match_empty_suffix() {
        let cfg = allow(&["web-*"]);
        // "web-" itself — prefix is "web-", remainder after stripping is "", which is the name
        // glob_match("web-*", "web-") → prefix="web-", name.starts_with("web-") is true, len > prefix
        // but name == "web-" means remainder is "", so starts_with returns true, let's verify:
        assert!(is_skill_allowed("web-", &cfg));
    }

    #[test]
    fn matching_is_case_sensitive() {
        let cfg = allow(&["Web-Search"]);
        assert!(!is_skill_allowed("web-search", &cfg));
        assert!(is_skill_allowed("Web-Search", &cfg));
    }
}

fn default_slack_port() -> u16 {
    3000
}

fn default_slack_webhook_host() -> String {
    "127.0.0.1".into()
}

fn default_a2a_host() -> String {
    "0.0.0.0".into()
}

fn default_a2a_port() -> u16 {
    8080
}

fn default_a2a_rate_limit() -> u32 {
    60
}

fn default_a2a_max_body() -> usize {
    1_048_576
}

fn default_drain_timeout_ms() -> u64 {
    30_000
}

fn default_max_dynamic_servers() -> usize {
    10
}

fn default_mcp_timeout() -> u64 {
    30
}

fn default_oauth_callback_port() -> u16 {
    18766
}

fn default_oauth_client_name() -> String {
    "Zeph".into()
}

/// Telegram channel configuration, nested under `[telegram]` in TOML.
///
/// When present, Zeph connects to Telegram as a bot using the provided token.
/// The token must be resolved from the vault at runtime via `ZEPH_TELEGRAM_TOKEN`.
///
/// # Example (TOML)
///
/// ```toml
/// [telegram]
/// allowed_users = ["myusername"]
/// ```
#[derive(Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    /// Bot token. Set to `None` and resolve from vault via `ZEPH_TELEGRAM_TOKEN`.
    pub token: Option<String>,
    /// Telegram usernames allowed to interact with the bot (empty = allow all).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Skill allowlist for this channel.
    #[serde(default)]
    pub skills: ChannelSkillsConfig,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("allowed_users", &self.allowed_users)
            .field("skills", &self.skills)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DiscordConfig {
    pub token: Option<String>,
    pub application_id: Option<String>,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_role_ids: Vec<String>,
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
    #[serde(default)]
    pub skills: ChannelSkillsConfig,
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("application_id", &self.application_id)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allowed_role_ids", &self.allowed_role_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .field("skills", &self.skills)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SlackConfig {
    pub bot_token: Option<String>,
    pub signing_secret: Option<String>,
    #[serde(default = "default_slack_webhook_host")]
    pub webhook_host: String,
    #[serde(default = "default_slack_port")]
    pub port: u16,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
    #[serde(default)]
    pub skills: ChannelSkillsConfig,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("bot_token", &self.bot_token.as_ref().map(|_| "[REDACTED]"))
            .field(
                "signing_secret",
                &self.signing_secret.as_ref().map(|_| "[REDACTED]"), // lgtm[rust/cleartext-logging]
            )
            .field("webhook_host", &self.webhook_host)
            .field("port", &self.port)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .field("skills", &self.skills)
            .finish()
    }
}

/// An IBCT signing key entry in the A2A server configuration.
///
/// Multiple entries allow key rotation: keep old keys until all tokens signed with them expire.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IbctKeyConfig {
    /// Unique key identifier. Must match the `key_id` field in issued IBCT tokens.
    pub key_id: String,
    /// Hex-encoded HMAC-SHA256 signing key.
    pub key_hex: String,
}

fn default_ibct_ttl() -> u64 {
    300
}

/// A2A server configuration, nested under `[a2a]` in TOML.
///
/// Controls the Agent-to-Agent HTTP server that exposes the agent via the A2A protocol.
/// The `AgentCard` served at `/.well-known/agent.json` is built from these settings combined
/// with runtime-detected capabilities (`images`, `audio`) and the opt-in `advertise_files` flag.
#[derive(Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic here
pub struct A2aServerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_a2a_host")]
    pub host: String,
    #[serde(default = "default_a2a_port")]
    pub port: u16,
    #[serde(default)]
    pub public_url: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_a2a_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_true")]
    pub require_tls: bool,
    #[serde(default = "default_true")]
    pub ssrf_protection: bool,
    #[serde(default = "default_a2a_max_body")]
    pub max_body_size: usize,
    #[serde(default = "default_drain_timeout_ms")]
    pub drain_timeout_ms: u64,
    /// When `true`, all requests are rejected with 401 if no `auth_token` is configured.
    /// Default `false` for backward compatibility — existing deployments without a token
    /// continue to operate. Set to `true` in production when authentication is mandatory.
    #[serde(default)]
    pub require_auth: bool,
    /// IBCT signing keys for per-task delegation scoping.
    ///
    /// When non-empty, all A2A task requests must include a valid `X-Zeph-IBCT` header
    /// signed with one of these keys. Multiple keys allow key rotation without downtime.
    #[serde(default)]
    pub ibct_keys: Vec<IbctKeyConfig>,
    /// Vault key name to resolve the primary IBCT signing key at startup (MF-3 fix).
    ///
    /// When set, the vault key is resolved at startup and used to construct an
    /// `IbctKey` with `key_id = "primary"`. Takes precedence over `ibct_keys[0]` if both
    /// are set.  Example: `"ZEPH_A2A_IBCT_KEY"`.
    #[serde(default)]
    pub ibct_signing_key_vault_ref: Option<String>,
    /// TTL (seconds) for issued IBCT tokens. Default: 300 (5 minutes).
    #[serde(default = "default_ibct_ttl")]
    pub ibct_ttl_secs: u64,
    /// Advertise non-media file attachment capability on the `AgentCard`.
    ///
    /// When `true`, the served `/.well-known/agent.json` sets `capabilities.files = true`,
    /// signalling to peer agents that this agent can receive `Part::File` entries that are
    /// not image or audio (e.g., documents, archives).
    ///
    /// Default `false` because generic file attachments have no built-in ingestion path in
    /// the current agent loop. Set to `true` only when the deployed agent has skills or MCP
    /// tools that can consume file parts; otherwise the card would advertise a capability
    /// the agent silently drops.
    ///
    /// Note: `images` and `audio` capability flags are auto-detected from the active LLM
    /// provider and STT configuration — no manual override is needed for those.
    #[serde(default)]
    pub advertise_files: bool,
}

impl std::fmt::Debug for A2aServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aServerConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("public_url", &self.public_url)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("rate_limit", &self.rate_limit)
            .field("require_tls", &self.require_tls)
            .field("ssrf_protection", &self.ssrf_protection)
            .field("max_body_size", &self.max_body_size)
            .field("drain_timeout_ms", &self.drain_timeout_ms)
            .field("require_auth", &self.require_auth)
            .field("ibct_keys_count", &self.ibct_keys.len())
            .field(
                "ibct_signing_key_vault_ref",
                &self.ibct_signing_key_vault_ref,
            )
            .field("ibct_ttl_secs", &self.ibct_ttl_secs)
            .field("advertise_files", &self.advertise_files)
            .finish()
    }
}

impl Default for A2aServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_a2a_host(),
            port: default_a2a_port(),
            public_url: String::new(),
            auth_token: None,
            rate_limit: default_a2a_rate_limit(),
            require_tls: true,
            ssrf_protection: true,
            max_body_size: default_a2a_max_body(),
            drain_timeout_ms: default_drain_timeout_ms(),
            require_auth: false,
            ibct_keys: Vec::new(),
            ibct_signing_key_vault_ref: None,
            ibct_ttl_secs: default_ibct_ttl(),
            advertise_files: false,
        }
    }
}

/// Dynamic MCP tool context pruning configuration (#2204).
///
/// When enabled, an LLM call evaluates which MCP tools are relevant to the current task
/// before sending tool schemas to the main LLM, reducing context usage and improving
/// tool selection accuracy for servers with many tools.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolPruningConfig {
    /// Enable dynamic tool pruning. Default: `false` (opt-in).
    pub enabled: bool,
    /// Maximum number of MCP tools to include after pruning.
    pub max_tools: usize,
    /// Provider name from `[[llm.providers]]` for the pruning LLM call.
    /// Should be a fast/cheap model. Empty string = use the default provider.
    pub pruning_provider: ProviderName,
    /// Minimum number of MCP tools below which pruning is skipped.
    pub min_tools_to_prune: usize,
    /// Tool names that are never pruned (always included in the result).
    pub always_include: Vec<String>,
}

impl Default for ToolPruningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tools: 15,
            pruning_provider: ProviderName::default(),
            min_tools_to_prune: 10,
            always_include: Vec::new(),
        }
    }
}

/// MCP tool discovery strategy (config-side representation).
///
/// Converted to `zeph_mcp::ToolDiscoveryStrategy` in `zeph-core` to avoid a
/// circular crate dependency (`zeph-config` → `zeph-mcp`).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolDiscoveryStrategyConfig {
    /// Embedding-based cosine similarity retrieval.  Fast, no LLM call per turn.
    Embedding,
    /// LLM-based pruning via `prune_tools_cached`.  Existing behavior.
    Llm,
    /// No filtering — all tools are passed through.  This is the default.
    #[default]
    None,
}

/// MCP tool discovery configuration (#2321).
///
/// Nested under `[mcp.tool_discovery]`.  When `strategy = "embedding"`, the
/// `mcp.pruning` section is ignored for this session — the embedding path
/// supersedes LLM pruning entirely.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolDiscoveryConfig {
    /// Discovery strategy.  Default: `none` (all tools, safe default).
    pub strategy: ToolDiscoveryStrategyConfig,
    /// Number of top-scoring tools to include per turn (embedding strategy only).
    pub top_k: usize,
    /// Minimum cosine similarity for a tool to be included (embedding strategy only).
    pub min_similarity: f32,
    /// Provider name from `[[llm.providers]]` for embedding computation.
    /// Should reference a fast/cheap embedding model.  Empty = use the agent's
    /// default embedding provider.
    pub embedding_provider: ProviderName,
    /// Tool names always included regardless of similarity score.
    pub always_include: Vec<String>,
    /// Minimum tool count below which discovery is skipped (all tools passed through).
    pub min_tools_to_filter: usize,
    /// When `true`, treat any embedding failure as a hard error instead of silently
    /// falling back to all tools.  Default: `false` (soft fallback).
    pub strict: bool,
}

impl Default for ToolDiscoveryConfig {
    fn default() -> Self {
        Self {
            strategy: ToolDiscoveryStrategyConfig::None,
            top_k: 10,
            min_similarity: 0.2,
            embedding_provider: ProviderName::default(),
            always_include: Vec::new(),
            min_tools_to_filter: 10,
            strict: false,
        }
    }
}

/// Trust calibration configuration, nested under `[mcp.trust_calibration]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct TrustCalibrationConfig {
    /// Enable trust calibration (default: false — opt-in).
    #[serde(default)]
    pub enabled: bool,
    /// Run pre-invocation probe on connect (Phase 1).
    #[serde(default = "default_true")]
    pub probe_on_connect: bool,
    /// Monitor invocations for trust score updates (Phase 2).
    #[serde(default = "default_true")]
    pub monitor_invocations: bool,
    /// Persist trust scores to `SQLite` (Phase 3).
    #[serde(default = "default_true")]
    pub persist_scores: bool,
    /// Per-day decay rate applied to trust scores above 0.5.
    #[serde(default = "default_decay_rate")]
    pub decay_rate_per_day: f64,
    /// Score penalty applied when injection is detected.
    #[serde(default = "default_injection_penalty")]
    pub injection_penalty: f64,
    /// Optional LLM provider for trust verification. Empty = disabled.
    #[serde(default)]
    pub verifier_provider: ProviderName,
}

fn default_decay_rate() -> f64 {
    0.01
}

fn default_injection_penalty() -> f64 {
    0.25
}

impl Default for TrustCalibrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_on_connect: true,
            monitor_invocations: true,
            persist_scores: true,
            decay_rate_per_day: default_decay_rate(),
            injection_penalty: default_injection_penalty(),
            verifier_provider: ProviderName::default(),
        }
    }
}

fn default_max_description_bytes() -> usize {
    2048
}

fn default_max_instructions_bytes() -> usize {
    2048
}

fn default_elicitation_timeout() -> u64 {
    120
}

fn default_elicitation_queue_capacity() -> usize {
    16
}

fn default_output_schema_hint_bytes() -> usize {
    1024
}

#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default = "default_max_dynamic_servers")]
    pub max_dynamic_servers: usize,
    /// Dynamic tool pruning for context optimization.
    #[serde(default)]
    pub pruning: ToolPruningConfig,
    /// Trust calibration settings (opt-in, disabled by default).
    #[serde(default)]
    pub trust_calibration: TrustCalibrationConfig,
    /// Embedding-based tool discovery (#2321).
    #[serde(default)]
    pub tool_discovery: ToolDiscoveryConfig,
    /// Maximum byte length for MCP tool descriptions. Truncated with "..." if exceeded. Default: 2048.
    #[serde(default = "default_max_description_bytes")]
    pub max_description_bytes: usize,
    /// Maximum byte length for MCP server instructions. Truncated with "..." if exceeded. Default: 2048.
    #[serde(default = "default_max_instructions_bytes")]
    pub max_instructions_bytes: usize,
    /// Enable MCP elicitation (servers can request user input mid-task).
    /// Default: false — all elicitation requests are auto-declined.
    /// Opt-in because it interrupts agent flow and could be abused by malicious servers.
    #[serde(default)]
    pub elicitation_enabled: bool,
    /// Timeout for user to respond to an elicitation request (seconds). Default: 120.
    #[serde(default = "default_elicitation_timeout")]
    pub elicitation_timeout: u64,
    /// Bounded channel capacity for elicitation events. Requests beyond this limit are
    /// auto-declined with a warning to prevent memory exhaustion from misbehaving servers.
    /// Default: 16.
    #[serde(default = "default_elicitation_queue_capacity")]
    pub elicitation_queue_capacity: usize,
    /// When true, warn the user before prompting for fields whose names match sensitive
    /// patterns (password, token, secret, key, credential, etc.). Default: true.
    #[serde(default = "default_true")]
    pub elicitation_warn_sensitive_fields: bool,
    /// Lock tool lists after initial connection for all servers.
    ///
    /// When `true`, `tools/list_changed` refresh events are rejected for servers that have
    /// completed their initial connection, preventing mid-session tool injection.
    /// Default: `false` (opt-in, backward compatible).
    #[serde(default)]
    pub lock_tool_list: bool,
    /// Default env isolation for all Stdio servers. Per-server `env_isolation` overrides this.
    ///
    /// When `true`, spawned processes only receive a minimal base env + their declared `env` map.
    /// Default: `false` (backward compatible).
    #[serde(default)]
    pub default_env_isolation: bool,
    /// When `true`, forward MCP tool output schemas as a hint appended to the tool description.
    ///
    /// Disabled by default to preserve Anthropic prompt-cache hit rates. Enabling this mutates
    /// tool descriptions, which changes the cached hash and causes a one-off cache miss after
    /// every MCP reconnect or server redeploy.
    ///
    /// See `output_schema_hint_bytes` for the budget controlling hint size.
    #[serde(default)]
    pub forward_output_schema: bool,
    /// Maximum bytes of the compact JSON appended to the tool description as the output schema
    /// hint when `forward_output_schema = true`. Default: 1024.
    ///
    /// If the serialized schema exceeds this budget, a stub message is used instead and a WARN
    /// is emitted once per session per tool.
    #[serde(default = "default_output_schema_hint_bytes")]
    pub output_schema_hint_bytes: usize,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            allowed_commands: Vec::new(),
            max_dynamic_servers: default_max_dynamic_servers(),
            pruning: ToolPruningConfig::default(),
            trust_calibration: TrustCalibrationConfig::default(),
            tool_discovery: ToolDiscoveryConfig::default(),
            max_description_bytes: default_max_description_bytes(),
            max_instructions_bytes: default_max_instructions_bytes(),
            elicitation_enabled: false,
            elicitation_timeout: default_elicitation_timeout(),
            elicitation_queue_capacity: default_elicitation_queue_capacity(),
            elicitation_warn_sensitive_fields: true,
            lock_tool_list: false,
            default_env_isolation: false,
            forward_output_schema: false,
            output_schema_hint_bytes: default_output_schema_hint_bytes(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub id: String,
    /// Stdio transport: command to spawn.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP transport: remote MCP server URL.
    pub url: Option<String>,
    #[serde(default = "default_mcp_timeout")]
    pub timeout: u64,
    /// Optional declarative policy for this server (allowlist, denylist, rate limit).
    #[serde(default)]
    pub policy: McpPolicy,
    /// Static HTTP headers for the transport (e.g. `Authorization: Bearer <token>`).
    /// Values support vault references: `${VAULT_KEY}`.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// OAuth 2.1 configuration for this server.
    #[serde(default)]
    pub oauth: Option<McpOAuthConfig>,
    /// Trust level for this server. Default: Untrusted.
    #[serde(default)]
    pub trust_level: McpTrustLevel,
    /// Tool allowlist. `None` means no override (inherit defaults).
    /// `Some(vec![])` is an explicit empty list (deny all for Untrusted/Sandboxed).
    /// `Some(vec!["a", "b"])` allows only listed tools.
    #[serde(default)]
    pub tool_allowlist: Option<Vec<String>>,
    /// Expected tool names for attestation. Supplements `tool_allowlist`.
    ///
    /// When non-empty: tools not in this list are filtered out (Untrusted/Sandboxed)
    /// or warned about (Trusted). Schema drift is logged when fingerprints change
    /// between connections.
    #[serde(default)]
    pub expected_tools: Vec<String>,
    /// Filesystem roots exposed to this MCP server via `roots/list`.
    /// Each entry is a `{uri, name?}` pair. URI must use `file://` scheme.
    /// When empty, the server receives an empty roots list.
    #[serde(default)]
    pub roots: Vec<McpRootEntry>,
    /// Per-tool security metadata overrides. Keys are tool names.
    /// When absent for a tool, metadata is inferred from the tool name via heuristics.
    #[serde(default)]
    pub tool_metadata: HashMap<String, ToolSecurityMeta>,
    /// Per-server elicitation override. `None` = inherit global `elicitation_enabled`.
    /// `Some(true)` = allow this server to elicit regardless of global setting.
    /// `Some(false)` = always decline for this server.
    #[serde(default)]
    pub elicitation_enabled: Option<bool>,
    /// Isolate the environment for this Stdio server.
    ///
    /// When `true` (or when `[mcp].default_env_isolation = true`), the spawned process
    /// only sees a minimal base env (`PATH`, `HOME`, etc.) plus this server's `env` map.
    /// Overrides `[mcp].default_env_isolation` when set explicitly.
    /// Default: `false` (backward compatible).
    #[serde(default)]
    pub env_isolation: Option<bool>,
}

/// A filesystem root exposed to an MCP server via `roots/list`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpRootEntry {
    /// URI of the root directory. Must use `file://` scheme.
    pub uri: String,
    /// Optional human-readable name for this root.
    #[serde(default)]
    pub name: Option<String>,
}

/// OAuth 2.1 configuration for an MCP server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpOAuthConfig {
    /// Enable OAuth 2.1 for this server.
    #[serde(default)]
    pub enabled: bool,
    /// Token storage backend.
    #[serde(default)]
    pub token_storage: OAuthTokenStorage,
    /// OAuth scopes to request. Empty = server default.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Port for the local callback server. `0` = auto-assign, `18766` = default fixed port.
    #[serde(default = "default_oauth_callback_port")]
    pub callback_port: u16,
    /// Client name sent during dynamic registration.
    #[serde(default = "default_oauth_client_name")]
    pub client_name: String,
}

impl Default for McpOAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_storage: OAuthTokenStorage::default(),
            scopes: Vec::new(),
            callback_port: default_oauth_callback_port(),
            client_name: default_oauth_client_name(),
        }
    }
}

/// Where OAuth tokens are stored.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthTokenStorage {
    /// Persisted in the age vault (default).
    #[default]
    Vault,
    /// In-memory only — tokens lost on restart.
    Memory,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_env: HashMap<&str, &str> = self
            .env
            .keys()
            .map(|k| (k.as_str(), "[REDACTED]"))
            .collect();
        // Redact header values to avoid leaking tokens in logs.
        let redacted_headers: HashMap<&str, &str> = self
            .headers
            .keys()
            .map(|k| (k.as_str(), "[REDACTED]"))
            .collect();
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &redacted_env)
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .field("policy", &self.policy)
            .field("headers", &redacted_headers)
            .field("oauth", &self.oauth)
            .field("trust_level", &self.trust_level)
            .field("tool_allowlist", &self.tool_allowlist)
            .field("expected_tools", &self.expected_tools)
            .field("roots", &self.roots)
            .field(
                "tool_metadata_keys",
                &self.tool_metadata.keys().collect::<Vec<_>>(),
            )
            .field("elicitation_enabled", &self.elicitation_enabled)
            .field("env_isolation", &self.env_isolation)
            .finish()
    }
}
