use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use zeph_tools::ToolsConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub skills: SkillsConfig,
    pub memory: MemoryConfig,
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub tools: ToolsConfig,
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
pub struct SkillsConfig {
    pub paths: Vec<String>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub token: Option<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
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
        if let Ok(v) = std::env::var("ZEPH_TELEGRAM_TOKEN") {
            let tg = self.telegram.get_or_insert(TelegramConfig {
                token: None,
                allowed_users: Vec::new(),
            });
            tg.token = Some(v);
        }
        if let Ok(v) = std::env::var("ZEPH_TOOLS_TIMEOUT")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.tools.shell.timeout = secs;
        }
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
            },
            skills: SkillsConfig {
                paths: vec!["./skills".into()],
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
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const ENV_KEYS: [&str; 6] = [
        "ZEPH_LLM_PROVIDER",
        "ZEPH_LLM_BASE_URL",
        "ZEPH_LLM_MODEL",
        "ZEPH_LLM_EMBEDDING_MODEL",
        "ZEPH_SQLITE_PATH",
        "ZEPH_QDRANT_URL",
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
    fn env_overrides() {
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

    #[test]
    fn telegram_env_override() {
        let mut config = Config::default();
        assert!(config.telegram.is_none());

        unsafe { std::env::set_var("ZEPH_TELEGRAM_TOKEN", "env-token") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TELEGRAM_TOKEN") };

        let tg = config.telegram.unwrap();
        assert_eq!(tg.token.as_deref(), Some("env-token"));
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
    fn env_override_tools_timeout() {
        let mut config = Config::default();
        assert_eq!(config.tools.shell.timeout, 30);

        unsafe { std::env::set_var("ZEPH_TOOLS_TIMEOUT", "120") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_TIMEOUT") };

        assert_eq!(config.tools.shell.timeout, 120);
    }

    #[test]
    fn env_override_tools_timeout_invalid_ignored() {
        let mut config = Config::default();
        assert_eq!(config.tools.shell.timeout, 30);

        unsafe { std::env::set_var("ZEPH_TOOLS_TIMEOUT", "not-a-number") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_TOOLS_TIMEOUT") };

        assert_eq!(config.tools.shell.timeout, 30);
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
    fn config_env_override_context_budget_tokens() {
        let mut config = Config::default();
        assert_eq!(config.memory.context_budget_tokens, 0);

        unsafe { std::env::set_var("ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS", "8192") };
        config.apply_env_overrides();
        unsafe { std::env::remove_var("ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS") };

        assert_eq!(config.memory.context_budget_tokens, 8192);
    }
}
