use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use zeph_core::agent::Agent;
use zeph_core::channel::{Channel, ChannelMessage};
use zeph_core::config::Config;
use zeph_llm::provider::{LlmProvider, Message};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::sqlite::SqliteStore;
use zeph_skills::loader::load_skill;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

// -- Mock LLM Provider --

#[derive(Clone)]
struct MockProvider {
    response: String,
}

impl MockProvider {
    fn new(response: &str) -> Self {
        Self {
            response: response.to_string(),
        }
    }
}

impl LlmProvider for MockProvider {
    async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
        Ok(self.response.clone())
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
    ) -> anyhow::Result<zeph_llm::provider::ChatStream> {
        let response = self.chat(messages).await?;
        Ok(Box::pin(tokio_stream::once(Ok(response))))
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![0.1, 0.2, 0.3])
    }

    fn supports_embeddings(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

// -- Mock Channel --

#[derive(Debug)]
struct MockChannel {
    inputs: VecDeque<String>,
    outputs: Arc<Mutex<Vec<String>>>,
}

impl MockChannel {
    fn new(inputs: Vec<&str>, outputs: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            inputs: inputs.into_iter().map(String::from).collect(),
            outputs,
        }
    }
}

impl Channel for MockChannel {
    async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
        Ok(self.inputs.pop_front().map(|text| ChannelMessage { text }))
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        self.outputs.lock().unwrap().push(text.to_string());
        Ok(())
    }

    async fn send_chunk(&mut self, _chunk: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

// -- Mock Tool Executor --

struct MockToolExecutor;

impl ToolExecutor for MockToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }
}

// -- Config tests --
// Combined into one test to avoid env var races between parallel test threads.

const ENV_KEYS: [&str; 5] = [
    "ZEPH_LLM_PROVIDER",
    "ZEPH_LLM_BASE_URL",
    "ZEPH_LLM_MODEL",
    "ZEPH_SQLITE_PATH",
    "ZEPH_TELEGRAM_TOKEN",
];

fn clear_env() {
    for key in ENV_KEYS {
        unsafe { std::env::remove_var(key) };
    }
}

#[test]
fn config_defaults_and_env_overrides() {
    clear_env();

    let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
    assert_eq!(config.llm.provider, "ollama");
    assert_eq!(config.llm.base_url, "http://localhost:11434");
    assert_eq!(config.llm.model, "mistral:7b");
    assert_eq!(config.agent.name, "Zeph");
    assert_eq!(config.memory.history_limit, 50);

    unsafe { std::env::set_var("ZEPH_LLM_MODEL", "test-model") };
    let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
    unsafe { std::env::remove_var("ZEPH_LLM_MODEL") };
    assert_eq!(config.llm.model, "test-model");
}

// -- Skills tests --

#[test]
fn skill_parse_valid() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("test-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    let path = skill_dir.join("SKILL.md");
    std::fs::write(
        &path,
        "---\nname: test-skill\ndescription: A test.\n---\n# Instructions\nDo stuff.",
    )
    .unwrap();

    let skill = load_skill(&path).unwrap();
    assert_eq!(skill.name(), "test-skill");
    assert_eq!(skill.description(), "A test.");
    assert!(skill.body.contains("Do stuff."));
}

#[test]
fn skill_parse_invalid_no_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SKILL.md");
    std::fs::write(&path, "no frontmatter here").unwrap();
    assert!(load_skill(&path).is_err());
}

#[test]
fn skill_registry_scans_temp_dir() {
    let dir = tempfile::tempdir().unwrap();

    let skill_dir = dir.path().join("alpha");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: alpha\ndescription: first\n---\nbody",
    )
    .unwrap();

    let skill_dir2 = dir.path().join("beta");
    std::fs::create_dir(&skill_dir2).unwrap();
    std::fs::write(
        skill_dir2.join("SKILL.md"),
        "---\nname: beta\ndescription: second\n---\nbody",
    )
    .unwrap();

    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    assert_eq!(registry.all_meta().len(), 2);
}

// -- Memory tests --

#[tokio::test]
async fn memory_save_load_roundtrip() {
    let store = SqliteStore::new(":memory:").await.unwrap();
    let cid = store.create_conversation().await.unwrap();

    store.save_message(cid, "user", "hello").await.unwrap();
    store.save_message(cid, "assistant", "world").await.unwrap();

    let history = store.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "hello");
    assert_eq!(history[1].content, "world");
}

#[tokio::test]
async fn memory_history_limit() {
    let store = SqliteStore::new(":memory:").await.unwrap();
    let cid = store.create_conversation().await.unwrap();

    for i in 0..20 {
        store
            .save_message(cid, "user", &format!("msg {i}"))
            .await
            .unwrap();
    }

    let history = store.load_history(cid, 5).await.unwrap();
    assert_eq!(history.len(), 5);
}

#[tokio::test]
async fn memory_conversation_isolation() {
    let store = SqliteStore::new(":memory:").await.unwrap();
    let cid1 = store.create_conversation().await.unwrap();
    let cid2 = store.create_conversation().await.unwrap();

    store.save_message(cid1, "user", "conv1").await.unwrap();
    store.save_message(cid2, "user", "conv2").await.unwrap();

    let h1 = store.load_history(cid1, 50).await.unwrap();
    let h2 = store.load_history(cid2, 50).await.unwrap();

    assert_eq!(h1.len(), 1);
    assert_eq!(h1[0].content, "conv1");
    assert_eq!(h2.len(), 1);
    assert_eq!(h2[0].content, "conv2");
}

// -- Agent end-to-end tests --

#[tokio::test]
async fn agent_roundtrip_mock() {
    let provider = MockProvider::new("mock response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["hello"], outputs.clone());
    let executor = MockToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0], "mock response");
}

#[tokio::test]
async fn agent_multiple_messages() {
    let provider = MockProvider::new("reply");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["first", "second", "third"], outputs.clone());
    let executor = MockToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 3);
    assert!(collected.iter().all(|o| o == "reply"));
}

#[tokio::test]
async fn agent_with_memory() {
    let provider = MockProvider::new("remembered");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["save this"], outputs.clone());
    let executor = MockToolExecutor;

    let memory = SemanticMemory::new(
        ":memory:",
        "http://invalid:6334", // Will fail gracefully, qdrant=None
        provider.clone(),
        "test-model",
    )
    .await
    .unwrap();

    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_memory(memory, cid, 50, 5, 100);
    agent.run().await.unwrap();
}

#[tokio::test]
async fn agent_shutdown_via_watch() {
    let provider = MockProvider::new("should not appear");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec![], outputs.clone());
    let executor = MockToolExecutor;

    let (tx, rx) = tokio::sync::watch::channel(false);

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_shutdown(rx);

    let _ = tx.send(true);

    agent.run().await.unwrap();
}
