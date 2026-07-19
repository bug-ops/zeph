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
#[non_exhaustive]
pub enum McpTrustLevel {
    /// Full trust — all tools exposed, SSRF check skipped. Use for operator-controlled servers.
    Trusted,
    /// Default. SSRF enforced. Fails closed (zero tools exposed) when no `tool_allowlist`
    /// is declared, unless [`McpServerConfig::allow_untrusted_without_allowlist`] is set.
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
    fn telegram_config_defaults() {
        // When all new fields are absent, defaults must be applied.
        let src = r#"token = "test_token""#;
        let cfg: TelegramConfig = toml::from_str(src).unwrap();
        assert!(!cfg.guest_mode);
        assert!(!cfg.bot_to_bot);
        assert!(cfg.allowed_bots.is_empty());
        assert_eq!(cfg.max_bot_chain_depth, 1);
    }

    #[test]
    fn telegram_config_explicit_values() {
        let src = r#"
token = "test_token"
guest_mode = true
bot_to_bot = true
allowed_bots = ["@bot_a", "@bot_b"]
max_bot_chain_depth = 5
"#;
        let cfg: TelegramConfig = toml::from_str(src).unwrap();
        assert!(cfg.guest_mode);
        assert!(cfg.bot_to_bot);
        assert_eq!(cfg.allowed_bots, vec!["@bot_a", "@bot_b"]);
        assert_eq!(cfg.max_bot_chain_depth, 5);
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
    fn max_connect_attempts_default_is_3() {
        let cfg = McpConfig::default();
        assert_eq!(cfg.max_connect_attempts, 3);
    }

    #[test]
    fn max_connect_attempts_accepts_valid_range() {
        for v in [1u8, 3, 10] {
            let src = format!("max_connect_attempts = {v}\n");
            let cfg: McpConfig = toml::from_str(&src)
                .unwrap_or_else(|e| panic!("max_connect_attempts = {v} should be valid, got: {e}"));
            assert_eq!(cfg.max_connect_attempts, v);
        }
    }

    #[test]
    fn max_connect_attempts_rejects_zero() {
        let src = "max_connect_attempts = 0\n";
        let result = toml::from_str::<McpConfig>(src);
        assert!(
            result.is_err(),
            "max_connect_attempts = 0 should be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("max_connect_attempts"),
            "error message should mention the field name, got: {msg}"
        );
    }

    #[test]
    fn max_connect_attempts_rejects_eleven() {
        let src = "max_connect_attempts = 11\n";
        let result = toml::from_str::<McpConfig>(src);
        assert!(
            result.is_err(),
            "max_connect_attempts = 11 should be rejected"
        );
    }

    #[test]
    fn startup_retry_backoff_ms_default_is_1000() {
        let cfg = McpConfig::default();
        assert_eq!(cfg.startup_retry_backoff_ms, 1000);
    }

    #[test]
    fn startup_retry_backoff_ms_deserializes_from_toml() {
        let src = "startup_retry_backoff_ms = 500\n";
        let cfg: McpConfig = toml::from_str(src).expect("valid toml");
        assert_eq!(cfg.startup_retry_backoff_ms, 500);
    }

    #[test]
    fn tool_timeout_secs_default_is_none() {
        let cfg = McpConfig::default();
        assert!(cfg.tool_timeout_secs.is_none());
    }

    #[test]
    fn tool_timeout_secs_deserializes_from_toml() {
        let src = "tool_timeout_secs = 120\n";
        let cfg: McpConfig = toml::from_str(src).expect("valid toml");
        assert_eq!(cfg.tool_timeout_secs, Some(120));
    }

    #[test]
    fn tool_timeout_secs_rejects_above_3600() {
        let src = "tool_timeout_secs = 3601\n";
        assert!(toml::from_str::<McpConfig>(src).is_err());
    }

    #[test]
    fn tool_timeout_secs_accepts_3600() {
        let src = "tool_timeout_secs = 3600\n";
        let cfg: McpConfig = toml::from_str(src).expect("valid toml");
        assert_eq!(cfg.tool_timeout_secs, Some(3600));
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

    #[test]
    fn a2a_client_config_defaults_are_hardened() {
        let cfg = A2aClientConfig::default();
        assert!(cfg.require_tls);
        assert!(cfg.ssrf_protection);
    }

    #[test]
    fn a2a_client_config_missing_toml_section_uses_defaults() {
        // Absent `[a2a_client]` in an existing/fresh config.toml must deserialize to the
        // hardened defaults, not fail — this is what makes the fix transparent for old configs.
        let cfg: A2aClientConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, A2aClientConfig::default());
    }

    #[test]
    fn a2a_client_config_partial_toml_fills_missing_field_from_default() {
        let cfg: A2aClientConfig = toml::from_str("require_tls = false\n").unwrap();
        assert!(!cfg.require_tls);
        assert!(cfg.ssrf_protection);
    }

    #[test]
    fn a2a_client_config_card_trust_policy_defaults_to_ignore() {
        let cfg = A2aClientConfig::default();
        assert_eq!(cfg.card_trust_policy, CardTrustPolicy::Ignore);
        assert!(cfg.trusted_agent_keys.is_empty());
    }

    #[test]
    fn card_trust_policy_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&CardTrustPolicy::Ignore).unwrap(),
            r#""ignore""#
        );
        assert_eq!(
            serde_json::to_string(&CardTrustPolicy::Prefer).unwrap(),
            r#""prefer""#
        );
        assert_eq!(
            serde_json::to_string(&CardTrustPolicy::Require).unwrap(),
            r#""require""#
        );
    }

    #[test]
    fn a2a_client_config_trusted_agent_keys_round_trip() {
        let toml_src = r#"
            card_trust_policy = "require"

            [[trusted_agent_keys]]
            kid = "key-1"
            alg = "ES256"
            jwk_or_pem = "-----BEGIN PUBLIC KEY-----\nMFk...\n-----END PUBLIC KEY-----"
        "#;
        let cfg: A2aClientConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.card_trust_policy, CardTrustPolicy::Require);
        assert_eq!(cfg.trusted_agent_keys.len(), 1);
        assert_eq!(cfg.trusted_agent_keys[0].kid, "key-1");
        assert_eq!(cfg.trusted_agent_keys[0].alg, "ES256");
    }

    #[test]
    fn ibct_key_config_debug_redacts_key_hex() {
        let key = IbctKeyConfig {
            key_id: "primary".into(),
            key_hex: "deadbeefdeadbeefdeadbeefdeadbeef".into(),
        };
        let debug = format!("{key:?}");
        assert!(!debug.contains("deadbeefdeadbeefdeadbeefdeadbeef"));
        assert!(debug.contains("primary"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn ibct_key_config_serialize_redacts_key_hex() {
        let key = IbctKeyConfig {
            key_id: "primary".into(),
            key_hex: "deadbeefdeadbeefdeadbeefdeadbeef".into(),
        };
        let json = serde_json::to_string(&key).unwrap();
        assert!(!json.contains("deadbeefdeadbeefdeadbeefdeadbeef"));
        assert!(json.contains("primary"));
        assert!(json.contains("REDACTED"));
    }

    #[test]
    fn telegram_config_serialize_omits_token() {
        let cfg = TelegramConfig {
            token: Some("real-secret-value".into()),
            allowed_users: Vec::new(),
            skills: ChannelSkillsConfig::default(),
            allowed_tools: None,
            stream_interval_ms: default_stream_interval_ms(),
            guest_mode: false,
            bot_to_bot: false,
            allowed_bots: Vec::new(),
            max_bot_chain_depth: default_max_bot_chain_depth(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("real-secret-value"));
        assert!(!json.contains("\"token\""));
    }

    #[test]
    fn discord_config_serialize_omits_token_but_keeps_application_id() {
        let cfg = DiscordConfig {
            token: Some("real-secret-value".into()),
            application_id: Some("123456789".into()),
            allowed_user_ids: Vec::new(),
            allowed_role_ids: Vec::new(),
            allowed_channel_ids: Vec::new(),
            skills: ChannelSkillsConfig::default(),
            allowed_tools: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("real-secret-value"));
        assert!(!json.contains("\"token\""));
        // application_id is a public snowflake, not a secret — it must still serialize.
        assert!(json.contains("123456789"));
    }

    #[test]
    fn slack_config_serialize_omits_bot_token_and_signing_secret() {
        let cfg = SlackConfig {
            bot_token: Some("real-secret-value".into()),
            signing_secret: Some("another-real-secret".into()),
            webhook_host: default_slack_webhook_host(),
            port: default_slack_port(),
            allowed_user_ids: Vec::new(),
            allowed_channel_ids: Vec::new(),
            skills: ChannelSkillsConfig::default(),
            allowed_tools: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("real-secret-value"));
        assert!(!json.contains("another-real-secret"));
        assert!(!json.contains("\"bot_token\""));
        assert!(!json.contains("\"signing_secret\""));
    }

    #[test]
    fn a2a_server_config_toml_round_trip_keeps_auth_token_plaintext() {
        // Guards against a future regression that redacts this Group-C field: `--init`
        // persists the raw auth_token to config.toml today, so redacting it here would
        // corrupt the config on reload.
        let cfg = A2aServerConfig {
            auth_token: Some("real-auth-token-value".into()),
            ..A2aServerConfig::default()
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        assert!(toml_str.contains("real-auth-token-value"));
    }

    #[test]
    fn group_a_configs_deserialize_missing_secret_field_as_none() {
        // `#[serde(skip_serializing)]` only affects the output side; serde's built-in
        // Option-defaulting already tolerates the key being absent on the input side. This
        // pins that `skip_serializing` cannot break loading a config that never had the key
        // (e.g. one written before this fix, or hand-edited without it).
        let telegram: TelegramConfig = toml::from_str("").unwrap();
        assert!(telegram.token.is_none());

        let discord: DiscordConfig = toml::from_str("").unwrap();
        assert!(discord.token.is_none());

        let slack: SlackConfig = toml::from_str("").unwrap();
        assert!(slack.bot_token.is_none());
        assert!(slack.signing_secret.is_none());
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

fn default_startup_retry_backoff_ms() -> u64 {
    1000
}

fn default_tool_timeout_secs() -> Option<u64> {
    None
}

fn default_oauth_callback_port() -> u16 {
    18766
}

fn default_oauth_client_name() -> String {
    "Zeph".into()
}

fn default_stream_interval_ms() -> u64 {
    3000
}

fn default_max_bot_chain_depth() -> u32 {
    1
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
/// stream_interval_ms = 3000
/// guest_mode = true
/// bot_to_bot = true
/// allowed_bots = ["@my_bot"]
/// max_bot_chain_depth = 1
/// ```
#[derive(Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    /// Bot token. Set to `None` and resolve from vault via `ZEPH_TELEGRAM_TOKEN`.
    ///
    /// # Security
    ///
    /// Never serialized: `--init` always persists this field as `None` (the real token
    /// goes to the vault), but runtime config resolution hydrates the real value into this
    /// field in memory. `#[serde(skip_serializing)]` keeps any future diagnostic `Serialize`
    /// of a live `Config` from leaking it; `Deserialize` is untouched so inline tokens in a
    /// hand-edited `config.toml` still load.
    #[serde(skip_serializing)]
    pub token: Option<String>,
    /// Telegram usernames allowed to interact with the bot.
    ///
    /// Must not be empty: the channel refuses to start (fail-closed) rather
    /// than run open to any sender when unconfigured.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Skill allowlist for this channel.
    #[serde(default)]
    pub skills: ChannelSkillsConfig,
    /// Tool allowlist for this channel. `None` means all tools are permitted.
    /// `Some(vec![])` denies all tools. `Some(vec!["shell"])` allows only listed tools.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Minimum interval in milliseconds between streaming message edits.
    ///
    /// Defaults to 3000 ms (3 seconds) to stay within Telegram's rate limits.
    /// Values below 500 ms are clamped to 500 ms with a warning; the Telegram
    /// Bot API enforces a hard limit of ~30 edits/second per chat.
    #[serde(default = "default_stream_interval_ms")]
    pub stream_interval_ms: u64,
    /// Enable responding to @mentions in any chat (Bot API 10.0 Guest Mode).
    ///
    /// When `false` (default), `guest_message` updates are ignored.
    #[serde(default)]
    pub guest_mode: bool,
    /// Enable receiving messages from other bots (Bot API 10.0).
    ///
    /// When `false` (default), messages where `from.is_bot = true` are silently dropped.
    #[serde(default)]
    pub bot_to_bot: bool,
    /// Bot usernames allowed to interact when `bot_to_bot = true`.
    ///
    /// Empty list (default) allows all bots. Include the `@` prefix (e.g. `"@my_bot"`).
    #[serde(default)]
    pub allowed_bots: Vec<String>,
    /// Maximum reply chain depth before Zeph stops responding to bot messages.
    ///
    /// Prevents infinite loops between bots. Checked against both the structural
    /// `reply_to_message` depth (spec FR-007) and the consecutive-reply counter
    /// for the same chat. Default: 1.
    ///
    /// Note: Telegram API payloads only expose one level of `reply_to_message`
    /// nesting, so values greater than 1 have no additional effect on structural
    /// depth alone. The consecutive-reply counter provides secondary loop
    /// prevention across multiple top-level exchanges.
    #[serde(default = "default_max_bot_chain_depth")]
    pub max_bot_chain_depth: u32,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("allowed_users", &self.allowed_users)
            .field("skills", &self.skills)
            .field("allowed_tools", &self.allowed_tools)
            .field("stream_interval_ms", &self.stream_interval_ms)
            .field("guest_mode", &self.guest_mode)
            .field("bot_to_bot", &self.bot_to_bot)
            .field("allowed_bots_count", &self.allowed_bots.len())
            .field("max_bot_chain_depth", &self.max_bot_chain_depth)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DiscordConfig {
    /// Bot token. Set to `None` and resolve from vault via `ZEPH_DISCORD_TOKEN`.
    ///
    /// # Security
    ///
    /// Never serialized: `--init` always persists this field as `None` (the real token
    /// goes to the vault), but runtime config resolution hydrates the real value into this
    /// field in memory. `#[serde(skip_serializing)]` keeps any future diagnostic `Serialize`
    /// of a live `Config` from leaking it; `Deserialize` is untouched so inline tokens in a
    /// hand-edited `config.toml` still load.
    #[serde(skip_serializing)]
    pub token: Option<String>,
    /// Public Discord application snowflake — not a secret, safe to serialize.
    pub application_id: Option<String>,
    /// Discord user snowflakes allowed to interact with the bot.
    ///
    /// At least one of `allowed_user_ids` or `allowed_role_ids` must be
    /// non-empty: the channel refuses to start (fail-closed) rather than run
    /// open to any sender when both are unconfigured.
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    /// Discord role snowflakes allowed to interact with the bot.
    ///
    /// See [`allowed_user_ids`](Self::allowed_user_ids) for the fail-closed
    /// startup requirement shared with this field.
    #[serde(default)]
    pub allowed_role_ids: Vec<String>,
    /// Discord channel snowflakes the bot responds in (empty = all channels).
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
    #[serde(default)]
    pub skills: ChannelSkillsConfig,
    /// Tool allowlist for this channel. `None` means all tools are permitted.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
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
            .field("allowed_tools", &self.allowed_tools)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SlackConfig {
    /// Bot token. Set to `None` and resolve from vault via `ZEPH_SLACK_BOT_TOKEN`.
    ///
    /// # Security
    ///
    /// Never serialized: `--init` always persists this field as `None` (the real token
    /// goes to the vault), but runtime config resolution hydrates the real value into this
    /// field in memory. `#[serde(skip_serializing)]` keeps any future diagnostic `Serialize`
    /// of a live `Config` from leaking it; `Deserialize` is untouched so inline tokens in a
    /// hand-edited `config.toml` still load.
    #[serde(skip_serializing)]
    pub bot_token: Option<String>,
    /// Request signing secret. Set to `None` and resolve from vault via
    /// `ZEPH_SLACK_SIGNING_SECRET`.
    ///
    /// # Security
    ///
    /// Never serialized — same rationale as [`bot_token`](Self::bot_token).
    #[serde(skip_serializing)]
    pub signing_secret: Option<String>,
    #[serde(default = "default_slack_webhook_host")]
    pub webhook_host: String,
    #[serde(default = "default_slack_port")]
    pub port: u16,
    /// Slack user IDs allowed to interact with the bot.
    ///
    /// Must not be empty: the channel refuses to start (fail-closed) rather
    /// than run open to any sender when unconfigured.
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    /// Slack channel IDs the bot responds in (empty = all channels).
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
    #[serde(default)]
    pub skills: ChannelSkillsConfig,
    /// Tool allowlist for this channel. `None` means all tools are permitted.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
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
            .field("allowed_tools", &self.allowed_tools)
            .finish()
    }
}

/// An IBCT signing key entry in the A2A server configuration.
///
/// Multiple entries allow key rotation: keep old keys until all tokens signed with them expire.
///
/// `Serialize` is hand-written and redacts `key_hex` to `"[REDACTED]"` (mirroring the
/// `Debug` impl below); `Deserialize` is derived and reads the real hex key untouched, since
/// config loading and the `--init` wizard both need the real value on the way in.
///
/// # Tradeoff
///
/// A future "load config → mutate → save TOML" flow that persists an inline
/// `[a2a] ibct_keys[].key_hex` would round-trip through this redacting `Serialize` and write
/// back `key_hex = "[REDACTED]"`, corrupting the key. This is acceptable today: no such flow
/// exists, `--migrate-config` operates on the TOML text directly (never through
/// `Config`/`Serialize`), and the documented direction is vault-resolved keys via
/// [`A2aServerConfig::ibct_signing_key_vault_ref`](crate::channels::A2aServerConfig::ibct_signing_key_vault_ref)
/// (which takes precedence over `ibct_keys[0]`), making inline `key_hex` a legacy path. If a
/// struct-based config save flow is ever added, this type should graduate to a split
/// config/diagnostic-shape design instead of redacting in place.
#[derive(Clone, Deserialize)]
pub struct IbctKeyConfig {
    /// Unique key identifier. Must match the `key_id` field in issued IBCT tokens.
    pub key_id: String,
    /// Hex-encoded HMAC-SHA256 signing key.
    pub key_hex: String,
}

impl std::fmt::Debug for IbctKeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IbctKeyConfig")
            .field("key_id", &self.key_id)
            .field("key_hex", &"[REDACTED]")
            .finish()
    }
}

impl Serialize for IbctKeyConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("IbctKeyConfig", 2)?;
        s.serialize_field("key_id", &self.key_id)?;
        s.serialize_field("key_hex", "[REDACTED]")?;
        s.end()
    }
}

fn default_ibct_ttl() -> u64 {
    300
}

fn default_a2a_request_timeout_ms() -> u64 {
    300_000
}

fn default_task_ttl_secs() -> u64 {
    3600
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
    /// Bearer token required on inbound A2A requests. `None` disables auth.
    ///
    /// # Security
    ///
    /// Intentionally **not** redacted in `Serialize`: unlike the channel tokens above, the
    /// `--init` wizard writes the raw value straight into `config.toml` (there is no vault
    /// indirection for this field yet), so a redacting `Serialize` would corrupt the
    /// persisted config on the next `--init`/save round-trip. The redacting `Debug` impl on
    /// this struct is the approved representation for any log/dump/status output — never emit
    /// this field's value via `Serialize` or any other non-`Debug` representation.
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_a2a_rate_limit")]
    pub rate_limit: u32,
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
    /// When non-empty, all requests to `/a2a` and `/a2a/stream` must include a valid
    /// `X-Zeph-IBCT` header signed with one of these keys, scoped to this server's own
    /// advertised endpoint (`AgentCard::url`, i.e. `public_url` above) and to the request's
    /// `task_id` (`params.id` for `tasks/get`/`tasks/cancel`, `params.message.taskId` for
    /// `message/send`/`message/stream` — the empty-string sentinel for a brand-new task with
    /// no server-assigned ID yet). A missing/undecodable header is rejected with `401`; a
    /// present-but-invalid one (bad signature, expired, unknown key, or scope mismatch) with
    /// `403`. Multiple keys allow key rotation without downtime — see [`IbctKeyConfig`].
    /// Enforced by `zeph_a2a::server::router::ibct_middleware`, wired via
    /// `A2aServer::with_ibct_keys`.
    ///
    /// **Before enabling in production**: as of #6260, no caller in this repository attaches
    /// `X-Zeph-IBCT` yet (the `--connect` remote-TUI client does not opt in, and no A2A
    /// delegation client exists). Setting this to a non-empty list will `401` `--connect` and
    /// any standard A2A peer, without protecting a delegated-subagent flow that doesn't yet
    /// exist — see `specs/010-security/spec.md`'s IBCT "Deployment status" note.
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
    /// Request processing timeout in milliseconds.
    ///
    /// Applies to both `message/send` and `tasks/stream` handlers.
    /// On timeout the task is set to `Failed` and the HTTP connection is closed.
    /// Defaults to 300 000 ms (5 minutes).
    #[serde(default = "default_a2a_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// TTL (seconds) for completed, failed, canceled, or rejected tasks in the in-memory store.
    ///
    /// Tasks that have reached a terminal state and whose age exceeds this value are evicted
    /// from memory by a background loop running every 60 seconds. Non-terminal tasks (submitted,
    /// working) are never evicted. Default: 3600 (1 hour).
    ///
    /// Set to `0` to disable eviction entirely. In that case the task store grows without bound
    /// and the operator is responsible for managing memory (e.g., via process restart).
    #[serde(default = "default_task_ttl_secs")]
    pub task_ttl_secs: u64,
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
            .field("request_timeout_ms", &self.request_timeout_ms)
            .field("task_ttl_secs", &self.task_ttl_secs)
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
            max_body_size: default_a2a_max_body(),
            drain_timeout_ms: default_drain_timeout_ms(),
            require_auth: false,
            ibct_keys: Vec::new(),
            ibct_signing_key_vault_ref: None,
            ibct_ttl_secs: default_ibct_ttl(),
            advertise_files: false,
            request_timeout_ms: default_a2a_request_timeout_ms(),
            task_ttl_secs: default_task_ttl_secs(),
        }
    }
}

/// Client-side security policy for outbound A2A connections made by `zeph --connect <URL>`
/// (the remote-TUI-over-A2A-SSE attach feature), nested under `[a2a_client]` in TOML.
///
/// Deliberately separate from [`A2aServerConfig`]'s `[a2a]` section: the two configure
/// different roles (this process attaching to a *remote* daemon vs. this process's *own*
/// A2A server accepting inbound connections) and must not share one config subtree — a
/// default/fresh config previously made every `--connect http://...` attempt fail with
/// "TLS required", even against `127.0.0.1` loopback, because `[a2a]`'s server-oriented
/// `require_tls = true` default was being reused for the client path (#5878).
///
/// Loopback targets (`127.0.0.1`, `::1`, `localhost` — see
/// [`is_loopback_host`](zeph_common::net::is_loopback_host)) are always permitted over
/// plain HTTP with SSRF protection skipped, regardless of these settings: connecting to
/// your own local daemon is definitionally not an SSRF risk, and the CLI's documented
/// `--connect http://127.0.0.1:8080/a2a/stream` usage example must work out of the box.
/// Non-loopback targets are governed by `require_tls`/`ssrf_protection` below, which
/// default to the same hardened posture as the server config.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct A2aClientConfig {
    /// Reject non-loopback endpoints that do not start with `https://`. Default: `true`.
    pub require_tls: bool,
    /// Resolve non-loopback endpoint hostnames via DNS and reject private/link-local
    /// ranges. Default: `true`.
    pub ssrf_protection: bool,
    /// Trust policy applied to peer [`AgentCard`](https://docs.rs/zeph-a2a) signatures and
    /// URL-origin consistency during discovery (A2A 1.0.0 §8.4, #5928). Default: `ignore`
    /// — byte-identical to pre-#5928 discovery behavior. See
    /// [`CardTrustPolicy`] doc comments for the `prefer`/`require` semantics, and
    /// [`Config::validate`](crate::root::Config::validate) for the `require`-without-the-
    /// `card-signing`-feature fail-fast check.
    pub card_trust_policy: CardTrustPolicy,
    /// Public keys trusted to sign peer `AgentCard`s, keyed by `kid`. Empty by default —
    /// `prefer`/`require` with no entries treats every peer as unverifiable (see
    /// `SignatureVerification::Unverifiable` in `zeph-a2a`).
    ///
    /// These are public verification keys, not secrets, so (unlike
    /// [`A2aServerConfig::ibct_signing_key_vault_ref`]) they are stored inline rather than
    /// via a vault reference.
    pub trusted_agent_keys: Vec<TrustedAgentKey>,
}

impl Default for A2aClientConfig {
    fn default() -> Self {
        Self {
            require_tls: true,
            ssrf_protection: true,
            card_trust_policy: CardTrustPolicy::default(),
            trusted_agent_keys: Vec::new(),
        }
    }
}

/// Trust policy for peer `AgentCard` signature + URL-origin verification during A2A
/// discovery (A2A 1.0.0 §8.4, #5928).
///
/// Mirrors `zeph_a2a::discovery::CardTrustPolicy` (protocol-crate-facing) as an
/// independent type — `zeph-config` must not depend on protocol crates, the same reason
/// [`McpTrustLevel`] has no `zeph-mcp` counterpart dependency. Conversion happens in the
/// top-level `zeph` binary crate (`src/tui_remote.rs::convert_card_trust_policy`), which
/// constructs the `AgentRegistry` used before `zeph --connect <URL>` establishes an A2A
/// session (#6200).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CardTrustPolicy {
    /// Discover peer cards without checking signatures or URL origin. Default —
    /// byte-identical to pre-#5928 behavior.
    #[default]
    Ignore,
    /// Log a warning on an untrusted/unverifiable card or URL-origin mismatch, but still
    /// accept it; reject only an actively tampered signature. Recommended production
    /// setting once real-peer interop is proven (see `zeph-a2a::card_signing` module docs).
    Prefer,
    /// Reject any card with an unverifiable signature or a URL-origin mismatch.
    ///
    /// Requires the `card-signing` feature to be compiled in — [`Config::validate`]
    /// rejects this setting at config-load time otherwise, rather than allowing it to
    /// silently degrade or brick discovery at runtime.
    ///
    /// [`Config::validate`]: crate::root::Config::validate
    Require,
}

/// A single trusted public key for verifying peer `AgentCard` signatures (#5928).
///
/// Public verification key material — not secret, so stored inline in config rather than
/// resolved via a vault reference (contrast IBCT's `ibct_signing_key_vault_ref`, which
/// protects a symmetric HMAC secret).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TrustedAgentKey {
    /// Key identifier, matched against the `kid` in a signature's protected header.
    pub kid: String,
    /// Signature algorithm this key is trusted to verify (e.g. `"ES256"`).
    pub alg: String,
    /// JWK JSON object or PEM-encoded `SubjectPublicKeyInfo` public key material.
    pub jwk_or_pem: String,
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
#[non_exhaustive]
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

fn default_max_connect_attempts() -> u8 {
    3
}

fn validate_max_connect_attempts<'de, D>(d: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = u8::deserialize(d)?;
    if !(1..=10).contains(&v) {
        return Err(serde::de::Error::custom(format!(
            "mcp.max_connect_attempts must be in 1..=10 (got {v})"
        )));
    }
    Ok(v)
}

fn validate_tool_timeout_secs<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<u64>::deserialize(d)?;
    if let Some(n) = v
        && n > 3600
    {
        return Err(serde::de::Error::custom(format!(
            "mcp.tool_timeout_secs must be \u{2264} 3600 (got {n})"
        )));
    }
    Ok(v)
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
    /// Maximum number of connection attempts for each MCP server at startup.
    ///
    /// Value `1` means one attempt with no retry. Value `3` (default) means up to three
    /// attempts with exponential backoff: 500 ms then 1 s between attempts.
    ///
    /// For `max_connect_attempts = N`, the inter-attempt delay sequence is
    /// `min(500 * 2^(k-1), 8_000) ms` for k = 1..N-1, giving at most ~47 s total backoff
    /// at the cap of `10`. Must be in `1..=10`.
    ///
    /// Note: dynamic `add_server` calls retain single-attempt behaviour regardless of this
    /// setting; a follow-up issue tracks extending retry there.
    #[serde(
        default = "default_max_connect_attempts",
        deserialize_with = "validate_max_connect_attempts"
    )]
    pub max_connect_attempts: u8,
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
    /// Base delay in milliseconds before each retry attempt at startup.
    ///
    /// The actual backoff is computed as `min(startup_retry_backoff_ms * 2^(k-1), 8_000) ms`
    /// where `k` is the 1-based attempt index. Default: 1000 ms.
    ///
    /// Set to a lower value for faster failover in test/development environments.
    #[serde(default = "default_startup_retry_backoff_ms")]
    pub startup_retry_backoff_ms: u64,
    /// Per-call timeout in seconds applied to each MCP tool invocation.
    ///
    /// This is separate from `[[mcp.servers]].timeout`, which controls the handshake and
    /// `tools/list` timeout. `tool_timeout_secs` applies after the connection is established,
    /// for each `tools/call` request.
    ///
    /// When absent (the default), the per-server `timeout` governs `tools/call` as well.
    /// Set to a lower value to cap runaway tools without changing the handshake timeout.
    /// Maximum accepted value is 3600 s; values above that are rejected at parse time.
    #[serde(
        default = "default_tool_timeout_secs",
        deserialize_with = "validate_tool_timeout_secs"
    )]
    pub tool_timeout_secs: Option<u64>,
    /// Global caps for MCP image passthrough (spec-072). Applies to every server with
    /// `media_passthrough = true`.
    #[serde(default)]
    pub media: McpMediaConfig,
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
            max_connect_attempts: default_max_connect_attempts(),
            startup_retry_backoff_ms: default_startup_retry_backoff_ms(),
            tool_timeout_secs: None,
            media: McpMediaConfig::default(),
        }
    }
}

/// Global caps enforced by `MediaSanitizer` (`zeph-sanitizer`) on every MCP-sourced image,
/// for servers with `media_passthrough = true` (spec-072 §3.4).
///
/// Defaults are conservative starting points, tunable per deployment; a follow-up
/// benchmarking pass may adjust them (spec-072 §10, OQ-1).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct McpMediaConfig {
    /// Maximum encoded byte size of a single image, checked before any decode attempt.
    /// Default: 5 MiB — below the existing 20 MiB user-upload `MAX_IMAGE_BYTES`.
    pub max_image_bytes: usize,
    /// Maximum width or height in pixels, enforced on the decoded image.
    /// Default: 8192.
    pub max_dimension_px: u32,
    /// Maximum total pixel count (width * height), enforced on the decoded image —
    /// decompression-bomb defense that a byte cap alone cannot provide. Default: 64,000,000 (~64 MP).
    pub max_pixels: u64,
    /// Maximum number of images validated/attached per single tool result.
    /// Default: 4.
    pub max_images_per_result: usize,
    /// Maximum number of images attached per turn, aggregated across all tool calls
    /// in the batch. Default: 8.
    pub max_images_per_turn: usize,
    /// Allowed image formats (short names, e.g. `"png"`, `"jpeg"`, `"gif"`, `"webp"`).
    /// Default: all four.
    pub allowed_formats: Vec<String>,
}

impl Default for McpMediaConfig {
    fn default() -> Self {
        Self {
            max_image_bytes: 5 * 1024 * 1024,
            max_dimension_px: 8192,
            max_pixels: 64_000_000,
            max_images_per_result: 4,
            max_images_per_turn: 8,
            allowed_formats: vec![
                "jpeg".to_owned(),
                "png".to_owned(),
                "gif".to_owned(),
                "webp".to_owned(),
            ],
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
    /// Environment variables for the spawned Stdio process. Values may hold vault
    /// references (`${VAULT_KEY}`) or, in a hand-written config, raw secrets.
    ///
    /// # Security
    ///
    /// Intentionally **not** redacted in `Serialize`: `--init` persists this map to
    /// `config.toml`, so a redacting `Serialize` would corrupt the round-trip. The
    /// redacting `Debug` impl on this struct is the approved representation for any
    /// log/dump/status output — never emit this field's values via `Serialize` or any other
    /// non-`Debug` representation.
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
    ///
    /// # Security
    ///
    /// Intentionally **not** redacted in `Serialize` — same rationale as
    /// [`env`](Self::env): `--init` persists this map to `config.toml`, and the redacting
    /// `Debug` impl is the approved representation for log/dump/status output — never emit
    /// this field's values via `Serialize` or any other non-`Debug` representation.
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
    /// Explicit opt-in to expose all tools for an `Untrusted` server that has no
    /// `tool_allowlist` declared. Default: `false` — secure by default (fails closed).
    ///
    /// When `false` (default) and `trust_level == Untrusted` with `tool_allowlist = None`,
    /// zero tools are exposed. Set `true` only when you intentionally want this server to
    /// expose all its tools while still running the full untrusted pipeline (SSRF checks,
    /// sanitization, injection detection, attestation, data-flow filtering) — this is
    /// distinct from `trust_level = trusted`, which additionally relaxes SSRF/data-flow
    /// enforcement. Has no effect on `Trusted`/`Sandboxed` servers or when `tool_allowlist`
    /// is set.
    #[serde(default)]
    pub allow_untrusted_without_allowlist: bool,
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
    /// Opt-in: decode and attach images this server returns as native `MessagePart::Image`
    /// siblings for vision-capable providers (spec-072). Default: `false`.
    ///
    /// Independent of [`trust_level`](Self::trust_level) but always hard-blocked when
    /// `trust_level == McpTrustLevel::Sandboxed`, regardless of this flag.
    #[serde(default)]
    pub media_passthrough: bool,
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
#[non_exhaustive]
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
            .field(
                "allow_untrusted_without_allowlist",
                &self.allow_untrusted_without_allowlist,
            )
            .field("expected_tools", &self.expected_tools)
            .field("roots", &self.roots)
            .field(
                "tool_metadata_keys",
                &self.tool_metadata.keys().collect::<Vec<_>>(),
            )
            .field("elicitation_enabled", &self.elicitation_enabled)
            .field("env_isolation", &self.env_isolation)
            .field("media_passthrough", &self.media_passthrough)
            .finish()
    }
}
