use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub skills: SkillsConfig,
    pub memory: MemoryConfig,
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
}

#[derive(Debug, Deserialize)]
pub struct SkillsConfig {
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    pub sqlite_path: String,
    pub history_limit: usize,
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
            },
            skills: SkillsConfig {
                paths: vec!["./skills".into()],
            },
            memory: MemoryConfig {
                sqlite_path: "./data/zeph.db".into(),
                history_limit: 50,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn defaults_when_file_missing() {
        let config = Config::default();
        assert_eq!(config.llm.provider, "ollama");
        assert_eq!(config.llm.base_url, "http://localhost:11434");
        assert_eq!(config.llm.model, "mistral:7b");
        assert_eq!(config.agent.name, "Zeph");
        assert_eq!(config.memory.history_limit, 50);
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

        // Remove any ZEPH_ env vars that could interfere
        for key in ["ZEPH_LLM_PROVIDER", "ZEPH_LLM_BASE_URL", "ZEPH_LLM_MODEL"] {
            unsafe { std::env::remove_var(key) };
        }

        let config = Config::load(&path).unwrap();
        assert_eq!(config.agent.name, "TestBot");
        assert_eq!(config.llm.base_url, "http://custom:1234");
        assert_eq!(config.llm.model, "llama3:8b");
        assert_eq!(config.memory.history_limit, 10);
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
}
