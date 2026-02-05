use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub skills: SkillsConfig,
    pub memory: MemoryConfig,
    pub telegram: Option<TelegramConfig>,
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
    pub cloud: Option<CloudLlmConfig>,
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
            let content =
                std::fs::read_to_string(path).context("failed to read config file")?;
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
        if let Ok(v) = std::env::var("ZEPH_SQLITE_PATH") {
            self.memory.sqlite_path = v;
        }
        if let Ok(v) = std::env::var("ZEPH_TELEGRAM_TOKEN") {
            let tg = self.telegram.get_or_insert(TelegramConfig {
                token: None,
                allowed_users: Vec::new(),
            });
            tg.token = Some(v);
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
                cloud: None,
            },
            skills: SkillsConfig {
                paths: vec!["./skills".into()],
            },
            memory: MemoryConfig {
                sqlite_path: "./data/zeph.db".into(),
                history_limit: 50,
            },
            telegram: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const LLM_ENV_KEYS: [&str; 4] = [
        "ZEPH_LLM_PROVIDER",
        "ZEPH_LLM_BASE_URL",
        "ZEPH_LLM_MODEL",
        "ZEPH_SQLITE_PATH",
    ];

    fn clear_llm_env() {
        for key in LLM_ENV_KEYS {
            unsafe { std::env::remove_var(key) };
        }
    }

    #[test]
    fn defaults_when_file_missing() {
        let config = Config::default();
        assert_eq!(config.llm.provider, "ollama");
        assert_eq!(config.llm.base_url, "http://localhost:11434");
        assert_eq!(config.llm.model, "mistral:7b");
        assert_eq!(config.agent.name, "Zeph");
        assert_eq!(config.memory.history_limit, 50);
        assert!(config.llm.cloud.is_none());
        assert!(config.telegram.is_none());
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

        clear_llm_env();

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

        clear_llm_env();

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

        clear_llm_env();

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
}
