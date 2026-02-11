use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use zeph_tools::ToolsConfig;

use crate::vault::{Secret, VaultProvider};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub skills: SkillsConfig,
    pub memory: MemoryConfig,
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub a2a: A2aServerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub vault: VaultConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(skip)]
    pub secrets: ResolvedSecrets,
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    pub cloud: Option<CloudLlmConfig>,
    pub candle: Option<CandleConfig>,
    pub orchestrator: Option<OrchestratorConfig>,
}

fn default_embedding_model() -> String {
    "qwen3-embedding".into()
}

#[derive(Debug, Deserialize)]
pub struct CloudLlmConfig {
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Deserialize)]
pub struct CandleConfig {
    #[serde(default = "default_candle_source")]
    pub source: String,
    #[serde(default)]
    pub local_path: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_chat_template")]
    pub chat_template: String,
    #[serde(default = "default_candle_device")]
    pub device: String,
    #[serde(default)]
    pub embedding_repo: Option<String>,
    #[serde(default)]
    pub generation: GenerationParams,
}

fn default_candle_source() -> String {
    "huggingface".into()
}

fn default_chat_template() -> String {
    "chatml".into()
}

fn default_candle_device() -> String {
    "cpu".into()
}

#[derive(Debug, Deserialize)]
pub struct GenerationParams {
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_seed")]
    pub seed: u64,
    #[serde(default = "default_repeat_penalty")]
    pub repeat_penalty: f32,
    #[serde(default = "default_repeat_last_n")]
    pub repeat_last_n: usize,
}

const MAX_TOKENS_CAP: usize = 32768;

impl GenerationParams {
    #[must_use]
    pub fn capped_max_tokens(&self) -> usize {
        self.max_tokens.min(MAX_TOKENS_CAP)
    }
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            temperature: default_temperature(),
            top_p: None,
            top_k: None,
            max_tokens: default_max_tokens(),
            seed: default_seed(),
            repeat_penalty: default_repeat_penalty(),
            repeat_last_n: default_repeat_last_n(),
        }
    }
}

fn default_temperature() -> f64 {
    0.7
}

fn default_max_tokens() -> usize {
    2048
}

fn default_seed() -> u64 {
    42
}

fn default_repeat_penalty() -> f32 {
    1.1
}

fn default_repeat_last_n() -> usize {
    64
}

#[derive(Debug, Deserialize)]
pub struct OrchestratorConfig {
    pub default: String,
    pub embed: String,
    #[serde(default)]
    pub providers: std::collections::HashMap<String, OrchestratorProviderConfig>,
    #[serde(default)]
    pub routes: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct OrchestratorProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SkillsConfig {
    pub paths: Vec<String>,
    #[serde(default = "default_max_active_skills")]
    pub max_active_skills: usize,
    #[serde(default)]
    pub learning: LearningConfig,
}

fn default_max_active_skills() -> usize {
    5
}

#[derive(Debug, Clone, Deserialize)]
pub struct LearningConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_activate: bool,
    #[serde(default = "default_min_failures")]
    pub min_failures: u32,
    #[serde(default = "default_improve_threshold")]
    pub improve_threshold: f64,
    #[serde(default = "default_rollback_threshold")]
    pub rollback_threshold: f64,
    #[serde(default = "default_min_evaluations")]
    pub min_evaluations: u32,
    #[serde(default = "default_max_versions")]
    pub max_versions: u32,
    #[serde(default = "default_cooldown_minutes")]
    pub cooldown_minutes: u64,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_activate: false,
            min_failures: default_min_failures(),
            improve_threshold: default_improve_threshold(),
            rollback_threshold: default_rollback_threshold(),
            min_evaluations: default_min_evaluations(),
            max_versions: default_max_versions(),
            cooldown_minutes: default_cooldown_minutes(),
        }
    }
}

fn default_min_failures() -> u32 {
    3
}
fn default_improve_threshold() -> f64 {
    0.7
}
fn default_rollback_threshold() -> f64 {
    0.5
}
fn default_min_evaluations() -> u32 {
    5
}
fn default_max_versions() -> u32 {
    10
}
fn default_cooldown_minutes() -> u64 {
    60
}

#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    pub sqlite_path: String,
    pub history_limit: u32,
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    #[serde(default)]
    pub semantic: SemanticConfig,
    #[serde(default = "default_summarization_threshold")]
    pub summarization_threshold: usize,
    #[serde(default = "default_context_budget_tokens")]
    pub context_budget_tokens: usize,
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".into()
}

fn default_summarization_threshold() -> usize {
    100
}

fn default_context_budget_tokens() -> usize {
    0
}

#[derive(Debug, Deserialize)]
pub struct SemanticConfig {
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_semantic_enabled(),
            recall_limit: default_recall_limit(),
        }
    }
}

fn default_semantic_enabled() -> bool {
    false
}

fn default_recall_limit() -> usize {
    5
}

#[derive(Clone, Deserialize)]
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

#[derive(Deserialize)]
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

fn default_true() -> bool {
    true
}

fn default_a2a_max_body() -> usize {
    1_048_576
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

fn default_a2a_host() -> String {
    "0.0.0.0".into()
}

fn default_a2a_port() -> u16 {
    8080
}

fn default_a2a_rate_limit() -> u32 {
    60
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

fn default_llm_timeout() -> u64 {
    120
}

fn default_embedding_timeout() -> u64 {
    30
}

fn default_a2a_timeout() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TimeoutConfig {
    #[serde(default = "default_llm_timeout")]
    pub llm_seconds: u64,
    #[serde(default = "default_embedding_timeout")]
    pub embedding_seconds: u64,
    #[serde(default = "default_a2a_timeout")]
    pub a2a_seconds: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            llm_seconds: default_llm_timeout(),
            embedding_seconds: default_embedding_timeout(),
            a2a_seconds: default_a2a_timeout(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default = "default_max_dynamic_servers")]
    pub max_dynamic_servers: usize,
}

fn default_max_dynamic_servers() -> usize {
    10
}

#[derive(Clone, Deserialize)]
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
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted: HashMap<&str, &str> = self
            .env
            .keys()
            .map(|k| (k.as_str(), "[REDACTED]"))
            .collect();
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &redacted)
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn default_mcp_timeout() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
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

fn default_vault_backend() -> String {
    "env".into()
}

#[derive(Debug, Default)]
pub struct ResolvedSecrets {
    pub claude_api_key: Option<Secret>,
}

impl Config {
    /// Load configuration from a TOML file with env var overrides.
    ///
    /// Falls back to sensible defaults when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            let content = std::fs::read_to_string(path).context("failed to read config file")?;
            toml::from_str::<Self>(&content).context("failed to parse config file")?
        } else {
            Self::default()
        };

        config.apply_env_overrides();
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_core();
        self.apply_env_overrides_security();
    }

    fn apply_env_overrides_core(&mut self) {
        if let Ok(v) = std::env::var("ZEPH_LLM_PROVIDER") {
            self.llm.provider = v;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_BASE_URL") {
            self.llm.base_url = v;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_MODEL") {
            self.llm.model = v;
        }
        if let Ok(v) = std::env::var("ZEPH_LLM_EMBEDDING_MODEL") {
            self.llm.embedding_model = v;
        }
        if let Ok(v) = std::env::var("ZEPH_SQLITE_PATH") {
            self.memory.sqlite_path = v;
        }
        if let Ok(v) = std::env::var("ZEPH_QDRANT_URL") {
            self.memory.qdrant_url = v;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SEMANTIC_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.memory.semantic.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_RECALL_LIMIT")
            && let Ok(limit) = v.parse::<usize>()
        {
            self.memory.semantic.recall_limit = limit;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_SUMMARIZATION_THRESHOLD")
            && let Ok(threshold) = v.parse::<usize>()
        {
            self.memory.summarization_threshold = threshold;
        }
        if let Ok(v) = std::env::var("ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS")
            && let Ok(tokens) = v.parse::<usize>()
        {
            self.memory.context_budget_tokens = tokens;
        }
        if let Ok(v) = std::env::var("ZEPH_SKILLS_MAX_ACTIVE")
            && let Ok(n) = v.parse::<usize>()
        {
            self.skills.max_active_skills = n;
        }
        if let Ok(v) = std::env::var("ZEPH_SKILLS_LEARNING_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.skills.learning.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_SKILLS_LEARNING_AUTO_ACTIVATE")
            && let Ok(auto_activate) = v.parse::<bool>()
        {
            self.skills.learning.auto_activate = auto_activate;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS") {
            self.tools.shell.allowed_commands = v
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_TIMEOUT")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.tools.shell.timeout = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SCRAPE_TIMEOUT")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.tools.scrape.timeout = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SCRAPE_MAX_BODY")
            && let Ok(bytes) = v.parse::<usize>()
        {
            self.tools.scrape.max_body_bytes = bytes;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.a2a.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_HOST") {
            self.a2a.host = v;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_PORT")
            && let Ok(port) = v.parse::<u16>()
        {
            self.a2a.port = port;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_PUBLIC_URL") {
            self.a2a.public_url = v;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_RATE_LIMIT")
            && let Ok(rate) = v.parse::<u32>()
        {
            self.a2a.rate_limit = rate;
        }
    }

    fn apply_env_overrides_security(&mut self) {
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SHELL_ALLOWED_PATHS") {
            self.tools.shell.allowed_paths = v
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_SHELL_ALLOW_NETWORK")
            && let Ok(allow) = v.parse::<bool>()
        {
            self.tools.shell.allow_network = allow;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_AUDIT_ENABLED")
            && let Ok(enabled) = v.parse::<bool>()
        {
            self.tools.audit.enabled = enabled;
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_AUDIT_DESTINATION") {
            self.tools.audit.destination = v;
        }
        if let Ok(v) = std::env::var("ZEPH_SECURITY_REDACT_SECRETS")
            && let Ok(redact) = v.parse::<bool>()
        {
            self.security.redact_secrets = redact;
        }
        if let Ok(v) = std::env::var("ZEPH_TIMEOUT_LLM")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.timeouts.llm_seconds = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TIMEOUT_EMBEDDING")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.timeouts.embedding_seconds = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_TIMEOUT_A2A")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.timeouts.a2a_seconds = secs;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_REQUIRE_TLS")
            && let Ok(require) = v.parse::<bool>()
        {
            self.a2a.require_tls = require;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_SSRF_PROTECTION")
            && let Ok(ssrf) = v.parse::<bool>()
        {
            self.a2a.ssrf_protection = ssrf;
        }
        if let Ok(v) = std::env::var("ZEPH_A2A_MAX_BODY_SIZE")
            && let Ok(size) = v.parse::<usize>()
        {
            self.a2a.max_body_size = size;
        }
    }

    /// Resolve sensitive configuration values through the vault.
    ///
    /// # Errors
    ///
    /// Returns an error if the vault backend fails.
    pub async fn resolve_secrets(&mut self, vault: &dyn VaultProvider) -> anyhow::Result<()> {
        if let Some(val) = vault.get_secret("ZEPH_CLAUDE_API_KEY").await? {
            self.secrets.claude_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_TELEGRAM_TOKEN").await? {
            let tg = self.telegram.get_or_insert(TelegramConfig {
                token: None,
                allowed_users: Vec::new(),
            });
            tg.token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_A2A_AUTH_TOKEN").await? {
            self.a2a.auth_token = Some(val);
        }
        Ok(())
    }

    fn default() -> Self {
        Self {
            agent: AgentConfig {
                name: "Zeph".into(),
            },
            llm: LlmConfig {
                provider: "ollama".into(),
                base_url: "http://localhost:11434".into(),
                model: "mistral:7b".into(),
                embedding_model: default_embedding_model(),
                cloud: None,
                candle: None,
                orchestrator: None,
            },
            skills: SkillsConfig {
                paths: vec!["./skills".into()],
                max_active_skills: default_max_active_skills(),
                learning: LearningConfig::default(),
            },
            memory: MemoryConfig {
                sqlite_path: "./data/zeph.db".into(),
                history_limit: 50,
                qdrant_url: default_qdrant_url(),
                semantic: SemanticConfig::default(),
                summarization_threshold: default_summarization_threshold(),
                context_budget_tokens: default_context_budget_tokens(),
            },
            telegram: None,
            tools: ToolsConfig::default(),
            a2a: A2aServerConfig::default(),
            mcp: McpConfig::default(),
            vault: VaultConfig::default(),
            security: SecurityConfig::default(),
            timeouts: TimeoutConfig::default(),
            secrets: ResolvedSecrets::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serial_test::serial;

    use super::*;

    const ENV_KEYS: [&str; 36] = [
        "ZEPH_LLM_PROVIDER",
        "ZEPH_LLM_BASE_URL",
        "ZEPH_LLM_MODEL",
        "ZEPH_LLM_EMBEDDING_MODEL",
        "ZEPH_CLAUDE_API_KEY",
        "ZEPH_SQLITE_PATH",
        "ZEPH_QDRANT_URL",
        "ZEPH_MEMORY_SUMMARIZATION_THRESHOLD",
        "ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS",
        "ZEPH_MEMORY_SEMANTIC_ENABLED",
        "ZEPH_MEMORY_RECALL_LIMIT",
        "ZEPH_SKILLS_MAX_ACTIVE",
        "ZEPH_TELEGRAM_TOKEN",
        "ZEPH_A2A_AUTH_TOKEN",
        "ZEPH_A2A_ENABLED",
        "ZEPH_A2A_HOST",
        "ZEPH_A2A_PORT",
        "ZEPH_A2A_PUBLIC_URL",
        "ZEPH_A2A_RATE_LIMIT",
        "ZEPH_A2A_REQUIRE_TLS",
        "ZEPH_A2A_SSRF_PROTECTION",
        "ZEPH_A2A_MAX_BODY_SIZE",
        "ZEPH_SECURITY_REDACT_SECRETS",
        "ZEPH_TIMEOUT_LLM",
        "ZEPH_TIMEOUT_EMBEDDING",
        "ZEPH_TIMEOUT_A2A",
        "ZEPH_TOOLS_TIMEOUT",
        "ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS",
        "ZEPH_TOOLS_SHELL_ALLOWED_PATHS",
        "ZEPH_TOOLS_SHELL_ALLOW_NETWORK",
        "ZEPH_TOOLS_SCRAPE_TIMEOUT",
        "ZEPH_TOOLS_SCRAPE_MAX_BODY",
        "ZEPH_TOOLS_AUDIT_ENABLED",
        "ZEPH_TOOLS_AUDIT_DESTINATION",
        "ZEPH_SKILLS_LEARNING_ENABLED",
        "ZEPH_SKILLS_LEARNING_AUTO_ACTIVATE",
    ];

    fn clear_env() {
        for key in ENV_KEYS {
            unsafe { std::env::remove_var(key) };
        }
    }

    #[test]
    fn defaults_when_file_missing() {
        let config = Config::default();
        assert_eq!(config.llm.provider, "ollama");
        assert_eq!(config.llm.base_url, "http://localhost:11434");
        assert_eq!(config.llm.model, "mistral:7b");
        assert_eq!(config.llm.embedding_model, "qwen3-embedding");
        assert_eq!(config.agent.name, "Zeph");
        assert_eq!(config.memory.history_limit, 50);
        assert_eq!(config.memory.qdrant_url, "http://localhost:6334");
        assert!(config.llm.cloud.is_none());
        assert!(config.telegram.is_none());
        assert!(config.tools.enabled);
        assert_eq!(config.tools.shell.timeout, 30);
        assert!(config.tools.shell.blocked_commands.is_empty());
    }

    #[test]
    fn parse_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "TestBot"

[llm]
provider = "ollama"
base_url = "http://custom:1234"
model = "llama3:8b"

[skills]
paths = ["./s"]

[memory]
sqlite_path = "./test.db"
history_limit = 10
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.agent.name, "TestBot");
        assert_eq!(config.llm.base_url, "http://custom:1234");
        assert_eq!(config.llm.model, "llama3:8b");
        assert_eq!(config.memory.history_limit, 10);
    }

    #[test]
    fn parse_toml_with_cloud() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "claude"
base_url = "http://localhost:11434"
model = "mistral:7b"

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 4096

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.llm.provider, "claude");
        let cloud = config.llm.cloud.unwrap();
        assert_eq!(cloud.model, "claude-sonnet-4-5-20250929");
        assert_eq!(cloud.max_tokens, 4096);
    }

    #[test]
    #[serial]
    fn env_overrides() {
        clear_env();
        let mut config = Config::default();
        assert_eq!(config.llm.model, "mistral:7b");

        unsafe { std::env::set_var("ZEPH_LLM_MODEL", "phi3:mini") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_LLM_MODEL") };

        assert_eq!(config.llm.model, "phi3:mini");
    }

    #[test]
    fn telegram_config_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tg.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[telegram]
token = "123:ABC"
allowed_users = ["alice", "bob"]
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        let tg = config.telegram.unwrap();
        assert_eq!(tg.token.as_deref(), Some("123:ABC"));
        assert_eq!(tg.allowed_users, vec!["alice", "bob"]);
    }

    #[tokio::test]
    async fn resolve_secrets_populates_telegram_token() {
        use crate::vault::MockVaultProvider;
        let vault = MockVaultProvider::new().with_secret("ZEPH_TELEGRAM_TOKEN", "vault-token");
        let mut config = Config::default();
        config.resolve_secrets(&vault).await.unwrap();
        let tg = config.telegram.unwrap();
        assert_eq!(tg.token.as_deref(), Some("vault-token"));
    }

    #[test]
    fn config_with_tools_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[tools]
enabled = true

[tools.shell]
timeout = 60
blocked_commands = ["custom-danger"]
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert!(config.tools.enabled);
        assert_eq!(config.tools.shell.timeout, 60);
        assert_eq!(config.tools.shell.blocked_commands, vec!["custom-danger"]);
    }

    #[test]
    fn config_without_tools_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_tools.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert!(config.tools.enabled);
        assert_eq!(config.tools.shell.timeout, 30);
        assert!(config.tools.shell.blocked_commands.is_empty());
    }

    #[test]
    #[serial]
    fn env_override_tools_timeout() {
        clear_env();
        let mut config = Config::default();
        assert_eq!(config.tools.shell.timeout, 30);

        unsafe { std::env::set_var("ZEPH_TOOLS_TIMEOUT", "120") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_TIMEOUT") };

        assert_eq!(config.tools.shell.timeout, 120);
    }

    #[test]
    #[serial]
    fn env_override_tools_timeout_invalid_ignored() {
        clear_env();
        let mut config = Config::default();
        assert_eq!(config.tools.shell.timeout, 30);

        unsafe { std::env::set_var("ZEPH_TOOLS_TIMEOUT", "not-a-number") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_TIMEOUT") };

        assert_eq!(config.tools.shell.timeout, 30);
    }

    #[test]
    #[serial]
    fn env_override_allowed_commands() {
        clear_env();
        let mut config = Config::default();
        assert!(config.tools.shell.allowed_commands.is_empty());

        unsafe { std::env::set_var("ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS", "curl, wget , ") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS") };

        assert_eq!(config.tools.shell.allowed_commands, vec!["curl", "wget"]);
    }

    #[test]
    fn config_default_embedding_model() {
        let config = Config::default();
        assert_eq!(config.llm.embedding_model, "qwen3-embedding");
    }

    #[test]
    fn config_parse_embedding_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embed.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"
embedding_model = "nomic-embed-text"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.llm.embedding_model, "nomic-embed-text");
    }

    #[test]
    #[serial]
    fn config_env_override_embedding_model() {
        let mut config = Config::default();
        assert_eq!(config.llm.embedding_model, "qwen3-embedding");

        unsafe { std::env::set_var("ZEPH_LLM_EMBEDDING_MODEL", "mxbai-embed-large") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_LLM_EMBEDDING_MODEL") };

        assert_eq!(config.llm.embedding_model, "mxbai-embed-large");
    }

    #[test]
    fn config_missing_embedding_model_uses_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_embed.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.llm.embedding_model, "qwen3-embedding");
    }

    #[test]
    fn config_default_qdrant_url() {
        let config = Config::default();
        assert_eq!(config.memory.qdrant_url, "http://localhost:6334");
    }

    #[test]
    fn config_parse_qdrant_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qdrant.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
qdrant_url = "http://qdrant:6334"
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.memory.qdrant_url, "http://qdrant:6334");
    }

    #[test]
    #[serial]
    fn config_env_override_qdrant_url() {
        let mut config = Config::default();
        assert_eq!(config.memory.qdrant_url, "http://localhost:6334");

        unsafe { std::env::set_var("ZEPH_QDRANT_URL", "http://remote:6334") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_QDRANT_URL") };

        assert_eq!(config.memory.qdrant_url, "http://remote:6334");
    }

    #[test]
    fn config_default_summarization_threshold() {
        let config = Config::default();
        assert_eq!(config.memory.summarization_threshold, 100);
    }

    #[test]
    fn config_parse_summarization_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sum.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
summarization_threshold = 200
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.memory.summarization_threshold, 200);
    }

    #[test]
    #[serial]
    fn config_env_override_summarization_threshold() {
        let mut config = Config::default();
        assert_eq!(config.memory.summarization_threshold, 100);

        unsafe { std::env::set_var("ZEPH_MEMORY_SUMMARIZATION_THRESHOLD", "150") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_MEMORY_SUMMARIZATION_THRESHOLD") };

        assert_eq!(config.memory.summarization_threshold, 150);
    }

    #[test]
    fn config_default_context_budget_tokens() {
        let config = Config::default();
        assert_eq!(config.memory.context_budget_tokens, 0);
    }

    #[test]
    fn config_parse_context_budget_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
context_budget_tokens = 4096
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.memory.context_budget_tokens, 4096);
    }

    #[test]
    #[serial]
    fn config_env_override_context_budget_tokens() {
        let mut config = Config::default();
        assert_eq!(config.memory.context_budget_tokens, 0);

        unsafe { std::env::set_var("ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS", "8192") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS") };

        assert_eq!(config.memory.context_budget_tokens, 8192);
    }

    #[test]
    fn learning_config_defaults() {
        let config = Config::default();
        let lc = &config.skills.learning;
        assert!(!lc.enabled);
        assert!(!lc.auto_activate);
        assert_eq!(lc.min_failures, 3);
        assert!((lc.improve_threshold - 0.7).abs() < f64::EPSILON);
        assert!((lc.rollback_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(lc.min_evaluations, 5);
        assert_eq!(lc.max_versions, 10);
        assert_eq!(lc.cooldown_minutes, 60);
    }

    #[test]
    fn parse_toml_with_learning_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("learn.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[skills.learning]
enabled = true
auto_activate = true
min_failures = 5
improve_threshold = 0.6
rollback_threshold = 0.4
min_evaluations = 10
max_versions = 20
cooldown_minutes = 120

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        let lc = &config.skills.learning;
        assert!(lc.enabled);
        assert!(lc.auto_activate);
        assert_eq!(lc.min_failures, 5);
        assert!((lc.improve_threshold - 0.6).abs() < f64::EPSILON);
        assert!((lc.rollback_threshold - 0.4).abs() < f64::EPSILON);
        assert_eq!(lc.min_evaluations, 10);
        assert_eq!(lc.max_versions, 20);
        assert_eq!(lc.cooldown_minutes, 120);
    }

    #[test]
    fn parse_toml_without_learning_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_learn.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert!(!config.skills.learning.enabled);
        assert_eq!(config.skills.learning.min_failures, 3);
    }

    #[test]
    #[serial]
    fn env_override_learning_enabled() {
        clear_env();
        let mut config = Config::default();
        assert!(!config.skills.learning.enabled);

        unsafe { std::env::set_var("ZEPH_SKILLS_LEARNING_ENABLED", "true") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_SKILLS_LEARNING_ENABLED") };

        assert!(config.skills.learning.enabled);
    }

    #[test]
    #[serial]
    fn env_override_learning_auto_activate() {
        clear_env();
        let mut config = Config::default();
        assert!(!config.skills.learning.auto_activate);

        unsafe { std::env::set_var("ZEPH_SKILLS_LEARNING_AUTO_ACTIVATE", "true") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_SKILLS_LEARNING_AUTO_ACTIVATE") };

        assert!(config.skills.learning.auto_activate);
    }

    #[test]
    #[serial]
    fn env_override_learning_invalid_ignored() {
        clear_env();
        let mut config = Config::default();
        assert!(!config.skills.learning.enabled);

        unsafe { std::env::set_var("ZEPH_SKILLS_LEARNING_ENABLED", "not-a-bool") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_SKILLS_LEARNING_ENABLED") };

        assert!(!config.skills.learning.enabled);
    }

    #[tokio::test]
    async fn resolve_secrets_populates_claude_api_key() {
        use crate::vault::MockVaultProvider;
        let vault = MockVaultProvider::new().with_secret("ZEPH_CLAUDE_API_KEY", "sk-test-123");
        let mut config = Config::default();
        config.resolve_secrets(&vault).await.unwrap();
        assert_eq!(
            config.secrets.claude_api_key.as_ref().unwrap().expose(),
            "sk-test-123"
        );
    }

    #[tokio::test]
    async fn resolve_secrets_populates_a2a_auth_token() {
        use crate::vault::MockVaultProvider;
        let vault = MockVaultProvider::new().with_secret("ZEPH_A2A_AUTH_TOKEN", "a2a-secret");
        let mut config = Config::default();
        config.resolve_secrets(&vault).await.unwrap();
        assert_eq!(config.a2a.auth_token.as_deref(), Some("a2a-secret"));
    }

    #[tokio::test]
    async fn resolve_secrets_empty_vault_leaves_defaults() {
        use crate::vault::MockVaultProvider;
        let vault = MockVaultProvider::new();
        let mut config = Config::default();
        config.resolve_secrets(&vault).await.unwrap();
        assert!(config.secrets.claude_api_key.is_none());
        assert!(config.telegram.is_none());
        assert!(config.a2a.auth_token.is_none());
    }

    #[tokio::test]
    async fn resolve_secrets_overrides_toml_values() {
        use crate::vault::MockVaultProvider;
        let vault = MockVaultProvider::new().with_secret("ZEPH_TELEGRAM_TOKEN", "vault-token");
        let mut config = Config::default();
        config.telegram = Some(TelegramConfig {
            token: Some("toml-token".into()),
            allowed_users: Vec::new(),
        });
        config.resolve_secrets(&vault).await.unwrap();
        let tg = config.telegram.unwrap();
        assert_eq!(tg.token.as_deref(), Some("vault-token"));
    }

    #[test]
    fn telegram_debug_redacts_token() {
        let tg = TelegramConfig {
            token: Some("secret-token".into()),
            allowed_users: vec!["alice".into()],
        };
        let debug = format!("{tg:?}");
        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn a2a_debug_redacts_auth_token() {
        let a2a = A2aServerConfig {
            auth_token: Some("secret-auth".into()),
            ..A2aServerConfig::default()
        };
        let debug = format!("{a2a:?}");
        assert!(!debug.contains("secret-auth"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn vault_config_default_backend() {
        let config = Config::default();
        assert_eq!(config.vault.backend, "env");
    }

    #[test]
    fn mcp_config_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-defaults.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Test"
[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "m"
[skills]
paths = ["./skills"]
[memory]
sqlite_path = ":memory:"
history_limit = 50
qdrant_url = "http://localhost:6334"
[mcp]
"#
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.mcp.servers.is_empty());
        assert!(config.mcp.allowed_commands.is_empty());
        assert_eq!(config.mcp.max_dynamic_servers, 10);
    }

    #[test]
    fn parse_toml_with_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[mcp]
allowed_commands = ["npx"]
max_dynamic_servers = 5

[[mcp.servers]]
id = "github"
command = "npx"
args = ["-y", "mcp-github"]
timeout = 60
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.mcp.allowed_commands, vec!["npx"]);
        assert_eq!(config.mcp.max_dynamic_servers, 5);
        assert_eq!(config.mcp.servers.len(), 1);
        assert_eq!(config.mcp.servers[0].id, "github");
        assert_eq!(config.mcp.servers[0].command.as_deref(), Some("npx"));
        assert_eq!(config.mcp.servers[0].timeout, 60);
    }

    #[test]
    fn parse_toml_mcp_http_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_http.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[[mcp.servers]]
id = "remote"
url = "http://remote-mcp:8080"
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.mcp.servers.len(), 1);
        assert_eq!(config.mcp.servers[0].id, "remote");
        assert_eq!(
            config.mcp.servers[0].url.as_deref(),
            Some("http://remote-mcp:8080")
        );
        assert!(config.mcp.servers[0].command.is_none());
        assert_eq!(config.mcp.servers[0].timeout, 30);
    }

    #[test]
    fn a2a_config_defaults() {
        let config = Config::default();
        assert!(!config.a2a.enabled);
        assert_eq!(config.a2a.host, "0.0.0.0");
        assert_eq!(config.a2a.port, 8080);
        assert!(config.a2a.public_url.is_empty());
        assert!(config.a2a.auth_token.is_none());
        assert_eq!(config.a2a.rate_limit, 60);
        assert!(config.a2a.require_tls);
        assert!(config.a2a.ssrf_protection);
        assert_eq!(config.a2a.max_body_size, 1_048_576);
    }

    #[test]
    fn parse_toml_with_a2a() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a2a.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[a2a]
enabled = true
host = "127.0.0.1"
port = 9090
public_url = "https://agent.example.com"
auth_token = "secret"
rate_limit = 120
require_tls = false
ssrf_protection = false
max_body_size = 2097152
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert!(config.a2a.enabled);
        assert_eq!(config.a2a.host, "127.0.0.1");
        assert_eq!(config.a2a.port, 9090);
        assert_eq!(config.a2a.public_url, "https://agent.example.com");
        assert_eq!(config.a2a.auth_token.as_deref(), Some("secret"));
        assert_eq!(config.a2a.rate_limit, 120);
        assert!(!config.a2a.require_tls);
        assert!(!config.a2a.ssrf_protection);
        assert_eq!(config.a2a.max_body_size, 2_097_152);
    }

    #[test]
    fn security_config_defaults() {
        let config = Config::default();
        assert!(config.security.redact_secrets);
    }

    #[test]
    fn parse_toml_with_security() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sec.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[security]
redact_secrets = false
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert!(!config.security.redact_secrets);
    }

    #[test]
    fn timeout_config_defaults() {
        let config = Config::default();
        assert_eq!(config.timeouts.llm_seconds, 120);
        assert_eq!(config.timeouts.embedding_seconds, 30);
        assert_eq!(config.timeouts.a2a_seconds, 30);
    }

    #[test]
    fn parse_toml_with_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timeouts.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[timeouts]
llm_seconds = 60
embedding_seconds = 15
a2a_seconds = 10
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.timeouts.llm_seconds, 60);
        assert_eq!(config.timeouts.embedding_seconds, 15);
        assert_eq!(config.timeouts.a2a_seconds, 10);
    }

    #[test]
    fn parse_toml_with_vault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[vault]
backend = "age"
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.vault.backend, "age");
    }

    #[test]
    #[serial]
    fn env_override_a2a_enabled() {
        clear_env();
        let mut config = Config::default();
        assert!(!config.a2a.enabled);

        unsafe { std::env::set_var("ZEPH_A2A_ENABLED", "true") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_A2A_ENABLED") };

        assert!(config.a2a.enabled);
    }

    #[test]
    #[serial]
    fn env_override_a2a_host_port() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_A2A_HOST", "127.0.0.1") };
        unsafe { std::env::set_var("ZEPH_A2A_PORT", "3000") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_A2A_HOST") };
        unsafe { std::env::remove_var("ZEPH_A2A_PORT") };

        assert_eq!(config.a2a.host, "127.0.0.1");
        assert_eq!(config.a2a.port, 3000);
    }

    #[test]
    #[serial]
    fn env_override_a2a_rate_limit() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_A2A_RATE_LIMIT", "200") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_A2A_RATE_LIMIT") };

        assert_eq!(config.a2a.rate_limit, 200);
    }

    #[test]
    #[serial]
    fn env_override_security_redact() {
        clear_env();
        let mut config = Config::default();
        assert!(config.security.redact_secrets);

        unsafe { std::env::set_var("ZEPH_SECURITY_REDACT_SECRETS", "false") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_SECURITY_REDACT_SECRETS") };

        assert!(!config.security.redact_secrets);
    }

    #[test]
    #[serial]
    fn env_override_timeout_llm() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_TIMEOUT_LLM", "300") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TIMEOUT_LLM") };

        assert_eq!(config.timeouts.llm_seconds, 300);
    }

    #[test]
    #[serial]
    fn env_override_timeout_embedding() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_TIMEOUT_EMBEDDING", "45") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TIMEOUT_EMBEDDING") };

        assert_eq!(config.timeouts.embedding_seconds, 45);
    }

    #[test]
    #[serial]
    fn env_override_timeout_a2a() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_TIMEOUT_A2A", "90") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TIMEOUT_A2A") };

        assert_eq!(config.timeouts.a2a_seconds, 90);
    }

    #[test]
    #[serial]
    fn env_override_a2a_require_tls() {
        clear_env();
        let mut config = Config::default();
        assert!(config.a2a.require_tls);

        unsafe { std::env::set_var("ZEPH_A2A_REQUIRE_TLS", "false") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_A2A_REQUIRE_TLS") };

        assert!(!config.a2a.require_tls);
    }

    #[test]
    #[serial]
    fn env_override_a2a_ssrf_protection() {
        clear_env();
        let mut config = Config::default();
        assert!(config.a2a.ssrf_protection);

        unsafe { std::env::set_var("ZEPH_A2A_SSRF_PROTECTION", "false") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_A2A_SSRF_PROTECTION") };

        assert!(!config.a2a.ssrf_protection);
    }

    #[test]
    #[serial]
    fn env_override_a2a_max_body_size() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_A2A_MAX_BODY_SIZE", "524288") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_A2A_MAX_BODY_SIZE") };

        assert_eq!(config.a2a.max_body_size, 524_288);
    }

    #[test]
    #[serial]
    fn env_override_scrape_timeout() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_TOOLS_SCRAPE_TIMEOUT", "60") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_SCRAPE_TIMEOUT") };

        assert_eq!(config.tools.scrape.timeout, 60);
    }

    #[test]
    #[serial]
    fn env_override_scrape_max_body() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_TOOLS_SCRAPE_MAX_BODY", "2097152") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_SCRAPE_MAX_BODY") };

        assert_eq!(config.tools.scrape.max_body_bytes, 2_097_152);
    }

    #[test]
    #[serial]
    fn env_override_shell_allowed_paths() {
        clear_env();
        let mut config = Config::default();
        assert!(config.tools.shell.allowed_paths.is_empty());

        unsafe { std::env::set_var("ZEPH_TOOLS_SHELL_ALLOWED_PATHS", "/tmp, /home") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_SHELL_ALLOWED_PATHS") };

        assert_eq!(config.tools.shell.allowed_paths, vec!["/tmp", "/home"]);
    }

    #[test]
    #[serial]
    fn env_override_shell_allow_network() {
        clear_env();
        let mut config = Config::default();
        assert!(config.tools.shell.allow_network);

        unsafe { std::env::set_var("ZEPH_TOOLS_SHELL_ALLOW_NETWORK", "false") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_SHELL_ALLOW_NETWORK") };

        assert!(!config.tools.shell.allow_network);
    }

    #[test]
    #[serial]
    fn env_override_audit_enabled() {
        clear_env();
        let mut config = Config::default();
        assert!(!config.tools.audit.enabled);

        unsafe { std::env::set_var("ZEPH_TOOLS_AUDIT_ENABLED", "true") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_AUDIT_ENABLED") };

        assert!(config.tools.audit.enabled);
    }

    #[test]
    #[serial]
    fn env_override_audit_destination() {
        clear_env();
        let mut config = Config::default();

        unsafe { std::env::set_var("ZEPH_TOOLS_AUDIT_DESTINATION", "/var/log/audit.log") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_AUDIT_DESTINATION") };

        assert_eq!(config.tools.audit.destination, "/var/log/audit.log");
    }

    #[test]
    #[serial]
    fn env_override_semantic_enabled() {
        clear_env();
        let mut config = Config::default();
        assert!(!config.memory.semantic.enabled);

        unsafe { std::env::set_var("ZEPH_MEMORY_SEMANTIC_ENABLED", "true") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_MEMORY_SEMANTIC_ENABLED") };

        assert!(config.memory.semantic.enabled);
    }

    #[test]
    #[serial]
    fn env_override_recall_limit() {
        clear_env();
        let mut config = Config::default();
        assert_eq!(config.memory.semantic.recall_limit, 5);

        unsafe { std::env::set_var("ZEPH_MEMORY_RECALL_LIMIT", "20") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_MEMORY_RECALL_LIMIT") };

        assert_eq!(config.memory.semantic.recall_limit, 20);
    }

    #[test]
    #[serial]
    fn env_override_skills_max_active() {
        clear_env();
        let mut config = Config::default();
        assert_eq!(config.skills.max_active_skills, 5);

        unsafe { std::env::set_var("ZEPH_SKILLS_MAX_ACTIVE", "10") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_SKILLS_MAX_ACTIVE") };

        assert_eq!(config.skills.max_active_skills, 10);
    }

    #[test]
    #[serial]
    fn env_override_a2a_public_url() {
        clear_env();
        let mut config = Config::default();
        assert!(config.a2a.public_url.is_empty());

        unsafe { std::env::set_var("ZEPH_A2A_PUBLIC_URL", "https://my-agent.dev") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_A2A_PUBLIC_URL") };

        assert_eq!(config.a2a.public_url, "https://my-agent.dev");
    }

    #[test]
    fn mcp_server_config_debug_redacts_env() {
        let mcp = McpServerConfig {
            id: "test".into(),
            command: Some("npx".into()),
            args: vec![],
            env: HashMap::from([("SECRET".into(), "super-secret".into())]),
            url: None,
            timeout: 30,
        };
        let debug = format!("{mcp:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn mcp_server_config_default_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_default_timeout.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50

[[mcp.servers]]
id = "test"
command = "cmd"
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.mcp.servers[0].timeout, 30);
    }

    #[test]
    fn config_load_nonexistent_file_uses_defaults() {
        let path = std::path::Path::new("/nonexistent/config.toml");
        let config = Config::load(path).unwrap();
        assert_eq!(config.agent.name, "Zeph");
        assert_eq!(config.llm.provider, "ollama");
    }

    #[test]
    fn generation_params_defaults() {
        let params = GenerationParams::default();
        assert!((params.temperature - 0.7).abs() < f64::EPSILON);
        assert!(params.top_p.is_none());
        assert!(params.top_k.is_none());
        assert_eq!(params.max_tokens, 2048);
        assert_eq!(params.seed, 42);
        assert!((params.repeat_penalty - 1.1).abs() < f32::EPSILON);
        assert_eq!(params.repeat_last_n, 64);
    }

    #[test]
    fn generation_params_capped_max_tokens() {
        let mut params = GenerationParams::default();
        params.max_tokens = 100_000;
        assert_eq!(params.capped_max_tokens(), 32_768);
    }

    #[test]
    fn generation_params_capped_below_cap() {
        let params = GenerationParams::default();
        assert_eq!(params.capped_max_tokens(), 2048);
    }

    #[test]
    fn semantic_config_defaults() {
        let config = SemanticConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.recall_limit, 5);
    }

    #[test]
    fn resolved_secrets_default() {
        let secrets = ResolvedSecrets::default();
        assert!(secrets.claude_api_key.is_none());
    }

    #[test]
    fn parse_toml_with_all_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[agent]
name = "FullBot"

[llm]
provider = "claude"
base_url = "http://localhost:11434"
model = "mistral:7b"
embedding_model = "nomic"

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 8192

[skills]
paths = ["./skills", "./extra-skills"]
max_active_skills = 3

[skills.learning]
enabled = true
min_failures = 5

[memory]
sqlite_path = "./data/test.db"
history_limit = 100
qdrant_url = "http://qdrant:6334"
summarization_threshold = 50
context_budget_tokens = 4096

[memory.semantic]
enabled = true
recall_limit = 10

[telegram]
token = "123:TOKEN"
allowed_users = ["admin"]

[tools]
enabled = true

[tools.shell]
timeout = 90
blocked_commands = ["rm"]
allowed_commands = ["curl"]
allowed_paths = ["/tmp"]
allow_network = false

[tools.scrape]
timeout = 30
max_body_bytes = 2097152

[tools.audit]
enabled = true
destination = "/var/log/zeph.log"

[a2a]
enabled = true
host = "127.0.0.1"
port = 9090
rate_limit = 100

[mcp]
max_dynamic_servers = 3

[vault]
backend = "age"

[security]
redact_secrets = false

[timeouts]
llm_seconds = 60
embedding_seconds = 10
a2a_seconds = 15
"#
        )
        .unwrap();

        clear_env();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.agent.name, "FullBot");
        assert_eq!(config.llm.provider, "claude");
        assert_eq!(config.llm.embedding_model, "nomic");
        assert!(config.llm.cloud.is_some());
        assert_eq!(config.skills.paths.len(), 2);
        assert_eq!(config.skills.max_active_skills, 3);
        assert!(config.skills.learning.enabled);
        assert_eq!(config.memory.history_limit, 100);
        assert!(config.memory.semantic.enabled);
        assert_eq!(config.memory.semantic.recall_limit, 10);
        assert!(config.telegram.is_some());
        assert!(!config.tools.shell.allow_network);
        assert!(config.tools.audit.enabled);
        assert!(config.a2a.enabled);
        assert_eq!(config.mcp.max_dynamic_servers, 3);
        assert_eq!(config.vault.backend, "age");
        assert!(!config.security.redact_secrets);
        assert_eq!(config.timeouts.llm_seconds, 60);
    }
}
