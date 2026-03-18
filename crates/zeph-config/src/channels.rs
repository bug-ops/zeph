// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::defaults::default_true;

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

#[derive(Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub token: Option<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("allowed_users", &self.allowed_users)
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
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("application_id", &self.application_id)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allowed_role_ids", &self.allowed_role_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
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
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
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
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default = "default_max_dynamic_servers")]
    pub max_dynamic_servers: usize,
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
    pub policy: zeph_mcp::McpPolicy,
    /// Static HTTP headers for the transport (e.g. `Authorization: Bearer <token>`).
    /// Values support vault references: `${VAULT_KEY}`.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// OAuth 2.1 configuration for this server.
    #[serde(default)]
    pub oauth: Option<McpOAuthConfig>,
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
            .finish()
    }
}
