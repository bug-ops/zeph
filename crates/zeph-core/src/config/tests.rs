use std::collections::HashMap;
use std::io::Write;

use serial_test::serial;

use super::*;

const ENV_KEYS: [&str; 47] = [
    "ZEPH_LLM_PROVIDER",
    "ZEPH_LLM_BASE_URL",
    "ZEPH_LLM_MODEL",
    "ZEPH_LLM_EMBEDDING_MODEL",
    "ZEPH_CLAUDE_API_KEY",
    "ZEPH_OPENAI_API_KEY",
    "ZEPH_SQLITE_PATH",
    "ZEPH_QDRANT_URL",
    "ZEPH_MEMORY_SUMMARIZATION_THRESHOLD",
    "ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS",
    "ZEPH_MEMORY_COMPACTION_THRESHOLD",
    "ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL",
    "ZEPH_MEMORY_PRUNE_PROTECT_TOKENS",
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
    "ZEPH_TOOLS_SUMMARIZE_OUTPUT",
    "ZEPH_MEMORY_AUTO_BUDGET",
    "ZEPH_INDEX_ENABLED",
    "ZEPH_INDEX_MAX_CHUNKS",
    "ZEPH_INDEX_SCORE_THRESHOLD",
    "ZEPH_INDEX_BUDGET_RATIO",
    "ZEPH_INDEX_REPO_MAP_TOKENS",
];

fn clear_env() {
    for key in ENV_KEYS {
        unsafe { std::env::remove_var(key) };
    }
}

#[test]
fn defaults_when_file_missing() {
    let config = Config::default();
    assert_eq!(config.llm.provider, super::ProviderKind::Ollama);
    assert_eq!(config.llm.base_url, "http://localhost:11434");
    assert_eq!(config.llm.model, "mistral:7b");
    assert_eq!(config.llm.embedding_model, "qwen3-embedding");
    assert_eq!(config.agent.name, "Zeph");
    assert_eq!(config.memory.history_limit, 50);
    assert_eq!(config.memory.qdrant_url, "http://localhost:6334");
    assert!(config.llm.cloud.is_none());
    assert!(config.llm.openai.is_none());
    assert!(config.telegram.is_none());
    assert!(config.tools.enabled);
    assert_eq!(config.tools.shell.timeout, 30);
    assert!(config.tools.shell.blocked_commands.is_empty());
}

#[test]
#[serial]
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
#[serial]
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
    assert_eq!(config.llm.provider, super::ProviderKind::Claude);
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
    assert_eq!(config.memory.summarization_threshold, 50);
}

#[test]
#[serial]
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
    assert_eq!(config.memory.summarization_threshold, 50);

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
#[serial]
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
    assert!(config.secrets.openai_api_key.is_none());
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
    assert!(config.memory.semantic.enabled);

    unsafe { std::env::set_var("ZEPH_MEMORY_SEMANTIC_ENABLED", "false") };
    config.apply_env_overrides();
    unsafe { std::env::remove_var("ZEPH_MEMORY_SEMANTIC_ENABLED") };

    assert!(!config.memory.semantic.enabled);
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
#[serial]
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
    assert_eq!(config.llm.provider, super::ProviderKind::Ollama);
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
    assert!(config.enabled);
    assert_eq!(config.recall_limit, 5);
}

#[test]
fn resolved_secrets_default() {
    let secrets = ResolvedSecrets::default();
    assert!(secrets.claude_api_key.is_none());
    assert!(secrets.openai_api_key.is_none());
}

#[test]
#[serial]
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
    assert_eq!(config.llm.provider, super::ProviderKind::Claude);
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

#[test]
#[serial]
fn parse_toml_with_openai() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("openai.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
        f,
        r#"
[agent]
name = "Zeph"

[llm]
provider = "openai"
base_url = "http://localhost:11434"
model = "mistral:7b"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 4096
embedding_model = "text-embedding-3-small"

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
    assert_eq!(config.llm.provider, super::ProviderKind::OpenAi);
    let openai = config.llm.openai.unwrap();
    assert_eq!(openai.base_url, "https://api.openai.com/v1");
    assert_eq!(openai.model, "gpt-4o");
    assert_eq!(openai.max_tokens, 4096);
    assert_eq!(
        openai.embedding_model.as_deref(),
        Some("text-embedding-3-small")
    );
}

#[test]
#[serial]
fn parse_toml_openai_without_embedding_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("openai_no_embed.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
        f,
        r#"
[agent]
name = "Zeph"

[llm]
provider = "openai"
base_url = "http://localhost:11434"
model = "mistral:7b"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
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
    let openai = config.llm.openai.unwrap();
    assert!(openai.embedding_model.is_none());
}

#[tokio::test]
async fn resolve_secrets_populates_openai_api_key() {
    use crate::vault::MockVaultProvider;
    let vault = MockVaultProvider::new().with_secret("ZEPH_OPENAI_API_KEY", "sk-openai-123");
    let mut config = Config::default();
    config.resolve_secrets(&vault).await.unwrap();
    assert_eq!(
        config.secrets.openai_api_key.as_ref().unwrap().expose(),
        "sk-openai-123"
    );
}

#[test]
#[serial]
fn parse_toml_openai_with_reasoning_effort() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("openai_reasoning.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
        f,
        r#"
[agent]
name = "Zeph"

[llm]
provider = "openai"
base_url = "http://localhost:11434"
model = "mistral:7b"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-5.2"
max_tokens = 4096
reasoning_effort = "high"

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
    let openai = config.llm.openai.unwrap();
    assert_eq!(openai.reasoning_effort.as_deref(), Some("high"));
}

#[test]
#[serial]
fn parse_toml_openai_without_reasoning_effort() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("openai_no_reasoning.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
        f,
        r#"
[agent]
name = "Zeph"

[llm]
provider = "openai"
base_url = "http://localhost:11434"
model = "mistral:7b"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-5.2"
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
    let openai = config.llm.openai.unwrap();
    assert!(openai.reasoning_effort.is_none());
}

#[test]
fn compaction_config_defaults() {
    let config = Config::default();
    assert!((config.memory.compaction_threshold - 0.80).abs() < f32::EPSILON);
    assert_eq!(config.memory.compaction_preserve_tail, 6);
}

#[test]
#[serial]
fn compaction_config_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compact.toml");
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
compaction_threshold = 0.90
compaction_preserve_tail = 6
"#
    )
    .unwrap();

    clear_env();

    let config = Config::load(&path).unwrap();
    assert!((config.memory.compaction_threshold - 0.90).abs() < f32::EPSILON);
    assert_eq!(config.memory.compaction_preserve_tail, 6);
}

#[test]
#[serial]
fn compaction_env_overrides() {
    clear_env();
    let mut config = Config::default();
    assert!((config.memory.compaction_threshold - 0.80).abs() < f32::EPSILON);
    assert_eq!(config.memory.compaction_preserve_tail, 6);

    unsafe { std::env::set_var("ZEPH_MEMORY_COMPACTION_THRESHOLD", "0.50") };
    unsafe { std::env::set_var("ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL", "8") };
    config.apply_env_overrides();
    unsafe { std::env::remove_var("ZEPH_MEMORY_COMPACTION_THRESHOLD") };
    unsafe { std::env::remove_var("ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL") };

    assert!((config.memory.compaction_threshold - 0.50).abs() < f32::EPSILON);
    assert_eq!(config.memory.compaction_preserve_tail, 8);
}

#[test]
fn tools_summarize_output_default_true() {
    let config = Config::default();
    assert!(config.tools.summarize_output);
}

#[test]
#[serial]
fn env_override_tools_summarize_output() {
    clear_env();
    let mut config = Config::default();
    assert!(config.tools.summarize_output);

    unsafe { std::env::set_var("ZEPH_TOOLS_SUMMARIZE_OUTPUT", "false") };
    config.apply_env_overrides();
    unsafe { std::env::remove_var("ZEPH_TOOLS_SUMMARIZE_OUTPUT") };

    assert!(!config.tools.summarize_output);
}

#[test]
#[serial]
fn auto_budget_default_true() {
    clear_env();
    let config = Config::default();
    assert!(config.memory.auto_budget);
}

#[test]
#[serial]
fn env_override_auto_budget() {
    clear_env();
    let mut config = Config::default();
    assert!(config.memory.auto_budget);

    unsafe { std::env::set_var("ZEPH_MEMORY_AUTO_BUDGET", "false") };
    config.apply_env_overrides();
    unsafe { std::env::remove_var("ZEPH_MEMORY_AUTO_BUDGET") };

    assert!(!config.memory.auto_budget);
}

#[test]
fn index_config_defaults() {
    let config = Config::default();
    assert!(!config.index.enabled);
    assert_eq!(config.index.max_chunks, 12);
    assert!((config.index.score_threshold - 0.25).abs() < f32::EPSILON);
    assert!((config.index.budget_ratio - 0.40).abs() < f32::EPSILON);
    assert_eq!(config.index.repo_map_tokens, 500);
}

#[test]
#[serial]
fn index_config_from_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.toml");
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

[index]
enabled = true
max_chunks = 20
score_threshold = 0.30
budget_ratio = 0.50
repo_map_tokens = 1000
"#
    )
    .unwrap();

    clear_env();

    let config = Config::load(&path).unwrap();
    assert!(config.index.enabled);
    assert_eq!(config.index.max_chunks, 20);
    assert!((config.index.score_threshold - 0.30).abs() < f32::EPSILON);
    assert!((config.index.budget_ratio - 0.50).abs() < f32::EPSILON);
    assert_eq!(config.index.repo_map_tokens, 1000);
}

#[test]
#[serial]
fn index_config_missing_uses_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no_index.toml");
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
    assert!(!config.index.enabled);
    assert_eq!(config.index.max_chunks, 12);
}

#[test]
#[serial]
fn index_config_env_overrides() {
    clear_env();
    let mut config = Config::default();
    assert!(!config.index.enabled);
    assert_eq!(config.index.max_chunks, 12);

    unsafe { std::env::set_var("ZEPH_INDEX_ENABLED", "true") };
    unsafe { std::env::set_var("ZEPH_INDEX_MAX_CHUNKS", "24") };
    unsafe { std::env::set_var("ZEPH_INDEX_SCORE_THRESHOLD", "0.35") };
    unsafe { std::env::set_var("ZEPH_INDEX_BUDGET_RATIO", "0.60") };
    unsafe { std::env::set_var("ZEPH_INDEX_REPO_MAP_TOKENS", "750") };
    config.apply_env_overrides();
    unsafe { std::env::remove_var("ZEPH_INDEX_ENABLED") };
    unsafe { std::env::remove_var("ZEPH_INDEX_MAX_CHUNKS") };
    unsafe { std::env::remove_var("ZEPH_INDEX_SCORE_THRESHOLD") };
    unsafe { std::env::remove_var("ZEPH_INDEX_BUDGET_RATIO") };
    unsafe { std::env::remove_var("ZEPH_INDEX_REPO_MAP_TOKENS") };

    assert!(config.index.enabled);
    assert_eq!(config.index.max_chunks, 24);
    assert!((config.index.score_threshold - 0.35).abs() < f32::EPSILON);
    assert!((config.index.budget_ratio - 0.60).abs() < f32::EPSILON);
    assert_eq!(config.index.repo_map_tokens, 750);
}

#[test]
#[serial]
fn index_config_env_overrides_clamped() {
    clear_env();
    let mut config = Config::default();

    unsafe { std::env::set_var("ZEPH_INDEX_SCORE_THRESHOLD", "-0.5") };
    unsafe { std::env::set_var("ZEPH_INDEX_BUDGET_RATIO", "2.0") };
    config.apply_env_overrides();
    unsafe { std::env::remove_var("ZEPH_INDEX_SCORE_THRESHOLD") };
    unsafe { std::env::remove_var("ZEPH_INDEX_BUDGET_RATIO") };

    assert!((config.index.score_threshold - 0.0).abs() < f32::EPSILON);
    assert!((config.index.budget_ratio - 1.0).abs() < f32::EPSILON);
}

#[test]
#[serial]
fn index_config_env_override_invalid_ignored() {
    clear_env();
    let mut config = Config::default();

    unsafe { std::env::set_var("ZEPH_INDEX_ENABLED", "not-a-bool") };
    unsafe { std::env::set_var("ZEPH_INDEX_MAX_CHUNKS", "abc") };
    config.apply_env_overrides();
    unsafe { std::env::remove_var("ZEPH_INDEX_ENABLED") };
    unsafe { std::env::remove_var("ZEPH_INDEX_MAX_CHUNKS") };

    assert!(!config.index.enabled);
    assert_eq!(config.index.max_chunks, 12);
}

#[test]
fn security_config_default_autonomy_supervised() {
    let config = Config::default();
    assert_eq!(config.security.autonomy_level, AutonomyLevel::Supervised);
}

#[test]
fn discord_config_defaults() {
    let config = Config::default();
    assert!(config.discord.is_none());
}

#[test]
#[serial]
fn parse_toml_with_autonomy_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("autonomy_readonly.toml");
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
autonomy_level = "readonly"
"#
    )
    .unwrap();

    clear_env();

    let config = Config::load(&path).unwrap();
    assert_eq!(config.security.autonomy_level, AutonomyLevel::ReadOnly);
}

#[test]
#[serial]
fn discord_config_from_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("discord.toml");
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

[discord]
token = "discord-bot-token"
application_id = "12345"
allowed_user_ids = ["u1", "u2"]
allowed_role_ids = ["admin"]
allowed_channel_ids = ["ch1"]
"#
    )
    .unwrap();

    clear_env();

    let config = Config::load(&path).unwrap();
    let dc = config.discord.unwrap();
    assert_eq!(dc.token.as_deref(), Some("discord-bot-token"));
    assert_eq!(dc.application_id.as_deref(), Some("12345"));
    assert_eq!(dc.allowed_user_ids, vec!["u1", "u2"]);
    assert_eq!(dc.allowed_role_ids, vec!["admin"]);
    assert_eq!(dc.allowed_channel_ids, vec!["ch1"]);
}

#[test]
fn discord_debug_redacts_token() {
    let dc = DiscordConfig {
        token: Some("secret-discord-token".into()),
        application_id: Some("app123".into()),
        allowed_user_ids: vec![],
        allowed_role_ids: vec![],
        allowed_channel_ids: vec![],
    };
    let debug = format!("{dc:?}");
    assert!(!debug.contains("secret-discord-token"));
    assert!(debug.contains("[REDACTED]"));
    assert!(debug.contains("app123"));
}

#[test]
#[serial]
fn parse_toml_with_autonomy_full() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("autonomy_full.toml");
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
autonomy_level = "full"
"#
    )
    .unwrap();

    clear_env();

    let config = Config::load(&path).unwrap();
    assert_eq!(config.security.autonomy_level, AutonomyLevel::Full);
}

#[test]
#[serial]
fn discord_config_empty_allowlists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("discord_empty.toml");
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

[discord]
token = "tok"
"#
    )
    .unwrap();

    clear_env();

    let config = Config::load(&path).unwrap();
    let dc = config.discord.unwrap();
    assert!(dc.allowed_user_ids.is_empty());
    assert!(dc.allowed_role_ids.is_empty());
    assert!(dc.allowed_channel_ids.is_empty());
}

#[test]
fn slack_config_defaults() {
    let config = Config::default();
    assert!(config.slack.is_none());
}

#[test]
#[serial]
fn slack_config_from_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slack.toml");
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

[slack]
bot_token = "xoxb-slack-token"
signing_secret = "slack-sign-secret"
port = 4000
allowed_user_ids = ["U1"]
allowed_channel_ids = ["C1"]
"#
    )
    .unwrap();

    clear_env();

    let config = Config::load(&path).unwrap();
    let sl = config.slack.unwrap();
    assert_eq!(sl.bot_token.as_deref(), Some("xoxb-slack-token"));
    assert_eq!(sl.signing_secret.as_deref(), Some("slack-sign-secret"));
    assert_eq!(sl.port, 4000);
    assert_eq!(sl.allowed_user_ids, vec!["U1"]);
    assert_eq!(sl.allowed_channel_ids, vec!["C1"]);
}

#[test]
fn slack_config_default_port() {
    let sl = SlackConfig {
        bot_token: None,
        signing_secret: None,
        webhook_host: "127.0.0.1".into(),
        port: 3000,
        allowed_user_ids: vec![],
        allowed_channel_ids: vec![],
    };
    assert_eq!(sl.port, 3000);
}

#[test]
fn slack_debug_redacts_tokens() {
    let sl = SlackConfig {
        bot_token: Some("xoxb-secret".into()),
        signing_secret: Some("sign-secret".into()),
        webhook_host: "127.0.0.1".into(),
        port: 3000,
        allowed_user_ids: vec![],
        allowed_channel_ids: vec![],
    };
    let debug = format!("{sl:?}");
    assert!(!debug.contains("xoxb-secret"));
    assert!(!debug.contains("sign-secret"));
    assert!(debug.contains("[REDACTED]"));
    assert!(debug.contains("3000"));
}

#[tokio::test]
async fn resolve_secrets_populates_discord_token() {
    use crate::vault::MockVaultProvider;
    let vault = MockVaultProvider::new().with_secret("ZEPH_DISCORD_TOKEN", "dc-vault-token");
    let mut config = Config::default();
    config.resolve_secrets(&vault).await.unwrap();
    let dc = config.discord.unwrap();
    assert_eq!(dc.token.as_deref(), Some("dc-vault-token"));
}

#[tokio::test]
async fn resolve_secrets_populates_slack_tokens() {
    use crate::vault::MockVaultProvider;
    let vault = MockVaultProvider::new()
        .with_secret("ZEPH_SLACK_BOT_TOKEN", "xoxb-vault")
        .with_secret("ZEPH_SLACK_SIGNING_SECRET", "sign-vault");
    let mut config = Config::default();
    config.resolve_secrets(&vault).await.unwrap();
    let sl = config.slack.unwrap();
    assert_eq!(sl.bot_token.as_deref(), Some("xoxb-vault"));
    assert_eq!(sl.signing_secret.as_deref(), Some("sign-vault"));
}

#[test]
fn stt_config_defaults() {
    let toml_str = r#"
[llm.stt]
"#;
    let stt: SttConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(stt.provider, "whisper");
    assert_eq!(stt.model, "whisper-1");
}

#[test]
fn stt_config_custom_values() {
    let toml_str = r#"
provider = "custom"
model = "whisper-large-v3"
"#;
    let stt: SttConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(stt.provider, "custom");
    assert_eq!(stt.model, "whisper-large-v3");
}

#[test]
fn llm_config_stt_none_by_default() {
    let config = Config::default();
    assert!(config.llm.stt.is_none());
}
