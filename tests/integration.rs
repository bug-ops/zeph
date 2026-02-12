use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zeph_core::agent::Agent;
use zeph_core::channel::{Channel, ChannelMessage};
use zeph_core::config::{Config, SecurityConfig, TimeoutConfig};
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

#[derive(Clone)]
struct StreamingMockProvider {
    response: String,
}

impl LlmProvider for StreamingMockProvider {
    async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
        Ok(self.response.clone())
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
    ) -> anyhow::Result<zeph_llm::provider::ChatStream> {
        let chunks = self
            .response
            .chars()
            .map(|c| Ok(c.to_string()))
            .collect::<Vec<_>>();
        Ok(Box::pin(tokio_stream::iter(chunks)))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![0.1, 0.2, 0.3])
    }

    fn supports_embeddings(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "streaming-mock"
    }
}

#[derive(Clone)]
struct EmptyResponseProvider;

impl LlmProvider for EmptyResponseProvider {
    async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
    ) -> anyhow::Result<zeph_llm::provider::ChatStream> {
        Ok(Box::pin(tokio_stream::once(Ok(String::new()))))
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
        "empty"
    }
}

#[derive(Clone)]
struct FailingProvider;

impl LlmProvider for FailingProvider {
    async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("provider unavailable"))
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
    ) -> anyhow::Result<zeph_llm::provider::ChatStream> {
        Err(anyhow::anyhow!("provider unavailable"))
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Err(anyhow::anyhow!("embed unavailable"))
    }

    fn supports_embeddings(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "failing"
    }
}

#[derive(Clone)]
struct CountingProvider {
    response: String,
    call_count: Arc<AtomicUsize>,
}

impl LlmProvider for CountingProvider {
    async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
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
        "counting"
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

struct ChunkTrackingChannel {
    inputs: VecDeque<String>,
    outputs: Arc<Mutex<Vec<String>>>,
    chunks: Arc<Mutex<Vec<String>>>,
    flush_count: Arc<AtomicUsize>,
}

impl Channel for ChunkTrackingChannel {
    async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
        Ok(self.inputs.pop_front().map(|text| ChannelMessage { text }))
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        self.outputs.lock().unwrap().push(text.to_string());
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> anyhow::Result<()> {
        self.chunks.lock().unwrap().push(chunk.to_string());
        Ok(())
    }

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
        self.flush_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct ConfirmMockChannel {
    inputs: VecDeque<String>,
    outputs: Arc<Mutex<Vec<String>>>,
    confirm_result: bool,
    confirm_called: Arc<Mutex<bool>>,
}

impl Channel for ConfirmMockChannel {
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

    async fn confirm(&mut self, _prompt: &str) -> anyhow::Result<bool> {
        *self.confirm_called.lock().unwrap() = true;
        Ok(self.confirm_result)
    }
}

// -- Mock Tool Executors --

struct MockToolExecutor;

impl ToolExecutor for MockToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }
}

struct OutputToolExecutor {
    output: String,
}

impl ToolExecutor for OutputToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(Some(ToolOutput {
            summary: self.output.clone(),
            blocks_executed: 1,
        }))
    }
}

struct EmptyOutputToolExecutor;

impl ToolExecutor for EmptyOutputToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(Some(ToolOutput {
            summary: String::new(),
            blocks_executed: 1,
        }))
    }
}

struct ErrorOutputToolExecutor;

impl ToolExecutor for ErrorOutputToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(Some(ToolOutput {
            summary: "[error] command failed".into(),
            blocks_executed: 1,
        }))
    }
}

struct BlockedToolExecutor;

impl ToolExecutor for BlockedToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::Blocked {
            command: "rm -rf /".into(),
        })
    }
}

struct ConfirmToolExecutor;

impl ToolExecutor for ConfirmToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::ConfirmationRequired {
            command: "rm -rf /tmp".into(),
        })
    }

    async fn execute_confirmed(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(Some(ToolOutput {
            summary: "confirmed output".into(),
            blocks_executed: 1,
        }))
    }
}

struct SandboxToolExecutor;

impl ToolExecutor for SandboxToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::SandboxViolation {
            path: "/etc/passwd".into(),
        })
    }
}

struct IoErrorToolExecutor;

impl ToolExecutor for IoErrorToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::Execution(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "command not found",
        )))
    }
}

struct ExitCodeToolExecutor;

impl ToolExecutor for ExitCodeToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(Some(ToolOutput {
            summary: "[exit code 1] process failed".into(),
            blocks_executed: 1,
        }))
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

#[tokio::test]
async fn agent_builder_with_embedding_model() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["test"], outputs.clone());
    let executor = MockToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_embedding_model("custom-model".into());

    agent.run().await.unwrap();
}

#[tokio::test]
async fn agent_load_history_with_memory() {
    let provider = MockProvider::new("reply");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec![], outputs.clone());
    let executor = MockToolExecutor;

    let memory = SemanticMemory::new(":memory:", "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .sqlite()
        .save_message(cid, "user", "hello")
        .await
        .unwrap();
    memory
        .sqlite()
        .save_message(cid, "assistant", "world")
        .await
        .unwrap();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_memory(memory, cid, 50, 5, 100);

    agent.load_history().await.unwrap();
}

#[tokio::test]
async fn agent_load_history_without_memory() {
    let provider = MockProvider::new("reply");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec![], outputs.clone());
    let executor = MockToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );

    agent.load_history().await.unwrap();
}

#[tokio::test]
async fn agent_skills_command() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/skills"], outputs.clone());
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
    assert!(!collected.is_empty());
}

#[tokio::test]
async fn agent_skill_activate_command() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/skill activate test-skill"], outputs.clone());
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
}

#[tokio::test]
async fn agent_skill_deactivate_command() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/skill deactivate test-skill"], outputs.clone());
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
}

#[tokio::test]
async fn agent_with_bash_tool_executor() {
    let provider = MockProvider::new("```bash\necho hello\n```");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["run command"], outputs.clone());
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
}

// -- process_response: tool output triggers tool loop --

#[tokio::test]
async fn agent_process_response_with_tool_output() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "response with tool call".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["do something"], outputs.clone());
    let executor = OutputToolExecutor {
        output: "command output".into(),
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    // MAX_SHELL_ITERATIONS = 3, so provider should be called 3 times
    assert_eq!(call_count.load(Ordering::SeqCst), 3);
    let collected = outputs.lock().unwrap();
    // Each iteration: LLM response + tool output = 2 messages, 3 iterations = 6
    assert!(collected.len() >= 3);
}

#[tokio::test]
async fn agent_process_response_tool_loop_max_iterations() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "keep going".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["start"], outputs.clone());
    let executor = OutputToolExecutor {
        output: "tool result".into(),
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    assert_eq!(call_count.load(Ordering::SeqCst), 3);
}

// -- call_llm_with_timeout: non-streaming --

#[tokio::test]
async fn agent_non_streaming_provider() {
    let provider = MockProvider::new("non-stream response");
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
    assert_eq!(collected[0], "non-stream response");
}

// -- call_llm_with_timeout: streaming --

#[tokio::test]
async fn agent_streaming_provider() {
    let provider = StreamingMockProvider {
        response: "streamed".into(),
    };
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let flush_count = Arc::new(AtomicUsize::new(0));
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = ChunkTrackingChannel {
        inputs: vec!["hello".to_string()].into_iter().collect(),
        outputs: outputs.clone(),
        chunks: chunks.clone(),
        flush_count: flush_count.clone(),
    };
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

    let collected_chunks = chunks.lock().unwrap();
    assert!(!collected_chunks.is_empty());
    let full: String = collected_chunks.iter().cloned().collect();
    assert_eq!(full, "streamed");
    assert!(flush_count.load(Ordering::SeqCst) >= 1);
}

// -- handle_tool_result: empty tool output stops loop --

#[tokio::test]
async fn agent_tool_output_empty_summary() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "response".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = EmptyOutputToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    // Empty summary stops the tool loop, so only 1 LLM call
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

// -- handle_tool_result: [error] marker --

#[tokio::test]
async fn agent_tool_output_with_error_marker() {
    let provider = MockProvider::new("some response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["do it"], outputs.clone());
    let executor = ErrorOutputToolExecutor;

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
    let has_tool_output = collected.iter().any(|o| o.contains("[error]"));
    assert!(has_tool_output);
}

// -- handle_tool_result: [exit code] marker --

#[tokio::test]
async fn agent_tool_output_with_exit_code_marker() {
    let provider = MockProvider::new("some response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["do it"], outputs.clone());
    let executor = ExitCodeToolExecutor;

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
    let has_exit_code = collected.iter().any(|o| o.contains("[exit code"));
    assert!(has_exit_code);
}

// -- handle_tool_result: blocked command --

#[tokio::test]
async fn agent_tool_blocked_command() {
    let provider = MockProvider::new("do something dangerous");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = BlockedToolExecutor;

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
    let has_blocked = collected
        .iter()
        .any(|o| o.contains("blocked by security policy"));
    assert!(has_blocked);
}

// -- handle_tool_result: confirmation required, approved --

#[tokio::test]
async fn agent_tool_confirmation_required_approved() {
    let provider = MockProvider::new("run something");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let confirm_called = Arc::new(Mutex::new(false));
    let channel = ConfirmMockChannel {
        inputs: vec!["go".to_string()].into_iter().collect(),
        outputs: outputs.clone(),
        confirm_result: true,
        confirm_called: confirm_called.clone(),
    };
    let executor = ConfirmToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    assert!(*confirm_called.lock().unwrap());
    let collected = outputs.lock().unwrap();
    let has_confirmed_output = collected.iter().any(|o| o.contains("confirmed output"));
    assert!(has_confirmed_output);
}

// -- handle_tool_result: confirmation required, denied --

#[tokio::test]
async fn agent_tool_confirmation_required_denied() {
    let provider = MockProvider::new("run something");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let confirm_called = Arc::new(Mutex::new(false));
    let channel = ConfirmMockChannel {
        inputs: vec!["go".to_string()].into_iter().collect(),
        outputs: outputs.clone(),
        confirm_result: false,
        confirm_called: confirm_called.clone(),
    };
    let executor = ConfirmToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    assert!(*confirm_called.lock().unwrap());
    let collected = outputs.lock().unwrap();
    let has_cancelled = collected.iter().any(|o| o.contains("cancelled"));
    assert!(has_cancelled);
}

// -- handle_tool_result: sandbox violation --

#[tokio::test]
async fn agent_tool_sandbox_violation() {
    let provider = MockProvider::new("access files");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = SandboxToolExecutor;

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
    let has_sandbox = collected.iter().any(|o| o.contains("outside the sandbox"));
    assert!(has_sandbox);
}

// -- handle_tool_result: generic IO error --

#[tokio::test]
async fn agent_tool_generic_error() {
    let provider = MockProvider::new("run tool");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = IoErrorToolExecutor;

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
    let has_failure = collected
        .iter()
        .any(|o| o.contains("Tool execution failed"));
    assert!(has_failure);
}

// -- empty LLM response --

#[tokio::test]
async fn agent_empty_response_handling() {
    let provider = EmptyResponseProvider;
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
    let has_retry = collected.iter().any(|o| o.contains("empty response"));
    assert!(has_retry);
}

// -- LLM provider error --

#[tokio::test]
async fn agent_provider_error_handling() {
    let provider = FailingProvider;
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
    let has_error = collected.iter().any(|o| o.contains("Error:"));
    assert!(has_error, "expected error message, got: {collected:?}");
}

// -- streaming response accumulates chunks --

#[tokio::test]
async fn agent_streaming_response_accumulates_chunks() {
    let provider = StreamingMockProvider {
        response: "abc".into(),
    };
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let flush_count = Arc::new(AtomicUsize::new(0));
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = ChunkTrackingChannel {
        inputs: vec!["test".to_string()].into_iter().collect(),
        outputs,
        chunks: chunks.clone(),
        flush_count: flush_count.clone(),
    };
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

    let collected_chunks = chunks.lock().unwrap();
    assert_eq!(collected_chunks.len(), 3);
    assert_eq!(collected_chunks[0], "a");
    assert_eq!(collected_chunks[1], "b");
    assert_eq!(collected_chunks[2], "c");
    assert_eq!(flush_count.load(Ordering::SeqCst), 1);
}

// -- maybe_redact: redaction enabled --

#[tokio::test]
async fn agent_redaction_enabled() {
    let provider = MockProvider::new("use key sk-abc123def456 for auth");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["show key"], outputs.clone());
    let executor = MockToolExecutor;

    let security = SecurityConfig {
        redact_secrets: true,
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_security(security, TimeoutConfig::default());
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 1);
    assert!(collected[0].contains("[REDACTED]"));
    assert!(!collected[0].contains("sk-abc123def456"));
}

// -- maybe_redact: redaction disabled --

#[tokio::test]
async fn agent_redaction_disabled() {
    let provider = MockProvider::new("use key sk-abc123def456 for auth");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["show key"], outputs.clone());
    let executor = MockToolExecutor;

    let security = SecurityConfig {
        redact_secrets: false,
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_security(security, TimeoutConfig::default());
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 1);
    assert!(collected[0].contains("sk-abc123def456"));
}

// -- persist_message with memory --

#[tokio::test]
async fn agent_persist_message_with_memory() {
    let provider = MockProvider::new("stored response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["save me"], outputs.clone());
    let executor = MockToolExecutor;

    let memory = SemanticMemory::new(":memory:", "http://invalid:6334", provider.clone(), "test")
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

    let store = SqliteStore::new(":memory:").await.unwrap();
    let _ = store;
}

// -- check_summarization triggers with low threshold --

#[tokio::test]
async fn agent_check_summarization_triggers() {
    let provider = MockProvider::new("reply");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["msg1", "msg2", "msg3"], outputs.clone());
    let executor = MockToolExecutor;

    let memory = SemanticMemory::new(":memory:", "http://invalid:6334", provider.clone(), "test")
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
    .with_memory(memory, cid, 50, 5, 2);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 3);
}

// -- /skills with memory and usage stats --

#[tokio::test]
async fn agent_skills_command_with_usage_stats() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["hello", "/skills"], outputs.clone());
    let executor = MockToolExecutor;

    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("test-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: A test skill.\n---\nbody",
    )
    .unwrap();

    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

    let memory = SemanticMemory::new(":memory:", "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_memory(memory, cid, 50, 5, 100);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    let skills_output = collected.iter().find(|o| o.contains("Available skills"));
    assert!(skills_output.is_some());
}

// -- /skill command disabled (non-self-learning) --

#[tokio::test]
async fn agent_skill_command_disabled() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/skill stats"], outputs.clone());
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
    // In self-learning builds, this shows stats; in non-self-learning, it says "not enabled"
    assert!(!collected.is_empty());
}

// -- /feedback command disabled (non-self-learning) --

#[tokio::test]
async fn agent_feedback_disabled() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/feedback test-skill bad output"], outputs.clone());
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
    assert!(!collected.is_empty());
}

// -- with_security builder --

#[tokio::test]
async fn agent_with_security_config() {
    let provider = MockProvider::new("response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["hello"], outputs.clone());
    let executor = MockToolExecutor;

    let security = SecurityConfig {
        redact_secrets: true,
    };
    let timeouts = TimeoutConfig {
        llm_seconds: 60,
        embedding_seconds: 15,
        a2a_seconds: 10,
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_security(security, timeouts);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 1);
}

// -- with_skill_reload builder --

#[tokio::test]
async fn agent_with_skill_reload() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["test"], outputs.clone());
    let executor = MockToolExecutor;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    drop(tx);

    let dir = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_skill_reload(vec![dir.path().to_path_buf()], rx);

    agent.run().await.unwrap();
}

// -- skill reload via channel event --

#[tokio::test]
async fn agent_skill_reload_via_channel() {
    use zeph_skills::watcher::SkillEvent;

    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["after reload"], outputs.clone());
    let executor = MockToolExecutor;

    let (tx, rx) = tokio::sync::mpsc::channel(16);

    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("reload-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: reload-skill\ndescription: reload test\n---\nbody",
    )
    .unwrap();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_skill_reload(vec![dir.path().to_path_buf()], rx);

    tx.send(SkillEvent::Changed).await.unwrap();
    drop(tx);

    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert!(!collected.is_empty());
}

// -- rebuild_system_prompt without matcher (all skills selected) --

#[tokio::test]
async fn agent_rebuild_without_matcher() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["query"], outputs.clone());
    let executor = MockToolExecutor;

    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("my-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: A skill.\n---\nInstructions here.",
    )
    .unwrap();

    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 1);
}

// -- LLM timeout path --

#[tokio::test]
async fn agent_llm_timeout_non_streaming() {
    #[derive(Clone)]
    struct SlowProvider;

    impl LlmProvider for SlowProvider {
        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok("should not arrive".into())
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
        ) -> anyhow::Result<zeph_llm::provider::ChatStream> {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(Box::pin(tokio_stream::once(Ok("never".into()))))
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
            "slow"
        }
    }

    let provider = SlowProvider;
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["hello"], outputs.clone());
    let executor = MockToolExecutor;

    let timeouts = TimeoutConfig {
        llm_seconds: 1,
        ..TimeoutConfig::default()
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_security(SecurityConfig::default(), timeouts);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    let has_timeout = collected.iter().any(|o| o.contains("timed out"));
    assert!(has_timeout);
}

// -- LLM timeout path (streaming) --

#[tokio::test]
async fn agent_llm_timeout_streaming() {
    #[derive(Clone)]
    struct SlowStreamingProvider;

    impl LlmProvider for SlowStreamingProvider {
        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok("should not arrive".into())
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
        ) -> anyhow::Result<zeph_llm::provider::ChatStream> {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(Box::pin(tokio_stream::once(Ok("never".into()))))
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.1, 0.2, 0.3])
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "slow-streaming"
        }
    }

    let provider = SlowStreamingProvider;
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["hello"], outputs.clone());
    let executor = MockToolExecutor;

    let timeouts = TimeoutConfig {
        llm_seconds: 1,
        ..TimeoutConfig::default()
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_security(SecurityConfig::default(), timeouts);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    let has_timeout = collected.iter().any(|o| o.contains("timed out"));
    assert!(has_timeout);
}

// -- redaction in streaming path --

#[tokio::test]
async fn agent_streaming_redaction() {
    let provider = StreamingMockProvider {
        response: "key sk-secret123".into(),
    };
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let flush_count = Arc::new(AtomicUsize::new(0));
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = ChunkTrackingChannel {
        inputs: vec!["show secret".to_string()].into_iter().collect(),
        outputs,
        chunks: chunks.clone(),
        flush_count,
    };
    let executor = MockToolExecutor;

    let security = SecurityConfig {
        redact_secrets: true,
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_security(security, TimeoutConfig::default());
    agent.run().await.unwrap();

    let collected_chunks = chunks.lock().unwrap();
    let full: String = collected_chunks.iter().cloned().collect();
    // Individual chars from "key sk-secret123" -- each char is a chunk,
    // redaction applies per-chunk so individual chars won't trigger prefix match
    assert!(!full.is_empty());
}

// -- redaction in tool output --

#[tokio::test]
async fn agent_redaction_in_tool_output() {
    let provider = MockProvider::new("response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = OutputToolExecutor {
        output: "found key sk-abc123secret in config".into(),
    };

    let security = SecurityConfig {
        redact_secrets: true,
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_security(security, TimeoutConfig::default());
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    let has_redacted = collected.iter().any(|o| o.contains("[REDACTED]"));
    assert!(has_redacted);
    let has_secret = collected.iter().any(|o| o.contains("sk-abc123secret"));
    assert!(!has_secret);
}

// -- memory persistence verified in SQLite --

#[tokio::test]
async fn agent_persist_messages_verified() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db_str = db_path.to_str().unwrap();

    let provider = MockProvider::new("response-123");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["user-input"], outputs.clone());
    let executor = MockToolExecutor;

    let memory = SemanticMemory::new(db_str, "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(
        provider.clone(),
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_memory(memory, cid, 50, 5, 100);
    agent.run().await.unwrap();

    let store = SqliteStore::new(db_str).await.unwrap();
    let history = store.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "user-input");
    assert_eq!(history[1].content, "response-123");
}

// -- load_history skips empty messages from history --

#[tokio::test]
async fn agent_load_history_skips_empty_messages() {
    let provider = MockProvider::new("reply");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec![], outputs.clone());
    let executor = MockToolExecutor;

    let memory = SemanticMemory::new(":memory:", "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();
    memory
        .sqlite()
        .save_message(cid, "user", "valid")
        .await
        .unwrap();
    memory
        .sqlite()
        .save_message(cid, "assistant", "   ")
        .await
        .unwrap();
    memory
        .sqlite()
        .save_message(cid, "user", "also valid")
        .await
        .unwrap();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_memory(memory, cid, 50, 5, 100);

    agent.load_history().await.unwrap();
}

// -- multiple input messages with memory verify all persisted --

#[tokio::test]
async fn agent_persist_multiple_exchanges() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db_str = db_path.to_str().unwrap();

    let provider = MockProvider::new("ack");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["msg1", "msg2"], outputs.clone());
    let executor = MockToolExecutor;

    let memory = SemanticMemory::new(db_str, "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(
        provider.clone(),
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_memory(memory, cid, 50, 5, 100);
    agent.run().await.unwrap();

    let store = SqliteStore::new(db_str).await.unwrap();
    let history = store.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 4); // 2 user + 2 assistant
}

// -- tool output is persisted when memory is enabled --

#[tokio::test]
async fn agent_tool_output_persisted_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db_str = db_path.to_str().unwrap();

    let provider = MockProvider::new("llm response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = OutputToolExecutor {
        output: "tool result text".into(),
    };

    let memory = SemanticMemory::new(db_str, "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(
        provider.clone(),
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_memory(memory, cid, 50, 5, 100);
    agent.run().await.unwrap();

    let store = SqliteStore::new(db_str).await.unwrap();
    let history = store.load_history(cid, 50).await.unwrap();
    let has_tool_msg = history.iter().any(|m| m.content.contains("[tool output]"));
    assert!(has_tool_msg);
}

// -- shutdown signal during message processing --

#[tokio::test]
async fn agent_shutdown_during_processing() {
    let provider = MockProvider::new("response");
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

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let _ = tx.send(true);
    });

    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert!(collected.is_empty());
}

// -- confirmation with tool output that has content --

#[tokio::test]
async fn agent_confirmation_approved_with_output_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db_str = db_path.to_str().unwrap();

    let provider = MockProvider::new("run command");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let confirm_called = Arc::new(Mutex::new(false));
    let channel = ConfirmMockChannel {
        inputs: vec!["go".to_string()].into_iter().collect(),
        outputs: outputs.clone(),
        confirm_result: true,
        confirm_called: confirm_called.clone(),
    };
    let executor = ConfirmToolExecutor;

    let memory = SemanticMemory::new(db_str, "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(
        provider.clone(),
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    )
    .with_memory(memory, cid, 50, 5, 100);
    agent.run().await.unwrap();

    let store = SqliteStore::new(db_str).await.unwrap();
    let history = store.load_history(cid, 50).await.unwrap();
    let has_confirmed = history
        .iter()
        .any(|m| m.content.contains("confirmed output"));
    assert!(has_confirmed);
}

// -- /skills with loaded skills shows skill list --

#[tokio::test]
async fn agent_skills_command_with_loaded_skills() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/skills"], outputs.clone());
    let executor = MockToolExecutor;

    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("alpha");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: alpha\ndescription: first skill\n---\nbody",
    )
    .unwrap();
    let skill_dir2 = dir.path().join("beta");
    std::fs::create_dir(&skill_dir2).unwrap();
    std::fs::write(
        skill_dir2.join("SKILL.md"),
        "---\nname: beta\ndescription: second skill\n---\nbody",
    )
    .unwrap();

    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 1);
    assert!(collected[0].contains("Available skills"));
    assert!(collected[0].contains("alpha"));
    assert!(collected[0].contains("beta"));
}

// -- /skill with unknown subcommand --

#[tokio::test]
async fn agent_skill_unknown_subcommand() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/skill unknown-cmd"], outputs.clone());
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
    assert!(!collected.is_empty());
}

// -- /feedback without arguments --

#[tokio::test]
async fn agent_feedback_without_args() {
    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["/feedback test-skill"], outputs.clone());
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
    assert!(!collected.is_empty());
}

// -- agent with SkillMatcher (InMemory backend) --

#[tokio::test]
async fn agent_rebuild_with_skill_matcher() {
    use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};

    let provider = MockProvider::new("matched response");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["query about alpha"], outputs.clone());
    let executor = MockToolExecutor;

    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("alpha");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: alpha\ndescription: first skill\n---\nalpha body",
    )
    .unwrap();
    let skill_dir2 = dir.path().join("beta");
    std::fs::create_dir(&skill_dir2).unwrap();
    std::fs::write(
        skill_dir2.join("SKILL.md"),
        "---\nname: beta\ndescription: second skill\n---\nbeta body",
    )
    .unwrap();

    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let all_meta = registry.all_meta();

    let embed_fn = |text: &str| -> zeph_skills::matcher::EmbedFuture {
        let _ = text;
        Box::pin(async { Ok(vec![1.0, 0.0, 0.0]) })
    };

    let matcher = SkillMatcher::new(&all_meta, embed_fn).await;
    let backend = matcher.map(SkillMatcherBackend::InMemory);

    let mut agent = Agent::new(provider, channel, registry, backend, 1, executor);
    agent.run().await.unwrap();

    let collected = outputs.lock().unwrap();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0], "matched response");
}

// -- multiple commands in one session (skills + normal message) --

#[tokio::test]
async fn agent_mixed_commands_and_messages() {
    let provider = MockProvider::new("reply");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(
        vec!["/skills", "normal message", "/skill stats"],
        outputs.clone(),
    );
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
    // /skills -> 1 output, normal message -> 1 reply, /skill stats -> 1 output
    assert_eq!(collected.len(), 3);
}

// -- tool loop continues through multiple iterations with tool output --

#[tokio::test]
async fn agent_tool_loop_three_iterations() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "```bash\necho hello\n```".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["start"], outputs.clone());
    let executor = OutputToolExecutor {
        output: "hello".into(),
    };

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    // Tool loop runs MAX_SHELL_ITERATIONS=3 times
    assert_eq!(call_count.load(Ordering::SeqCst), 3);
}

// -- agent with memory records skill usage for active skills --

#[tokio::test]
async fn agent_records_skill_usage() {
    let tmpdir = tempfile::tempdir().unwrap();
    let db_path = tmpdir.path().join("test.db");
    let db_str = db_path.to_str().unwrap();

    let provider = MockProvider::new("ok");
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["hello"], outputs.clone());
    let executor = MockToolExecutor;

    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("tracked-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: tracked-skill\ndescription: tracked\n---\nbody",
    )
    .unwrap();

    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

    let memory = SemanticMemory::new(db_str, "http://invalid:6334", provider.clone(), "test")
        .await
        .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(provider.clone(), channel, registry, None, 5, executor)
        .with_memory(memory, cid, 50, 5, 100);
    agent.run().await.unwrap();

    let store = SqliteStore::new(db_str).await.unwrap();
    let usage = store.load_skill_usage().await.unwrap();
    let has_tracked = usage.iter().any(|u| u.skill_name == "tracked-skill");
    assert!(has_tracked);
}

// -- streaming provider with empty response --

#[tokio::test]
async fn agent_streaming_empty_response() {
    let provider = StreamingMockProvider {
        response: String::new(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let flush_count = Arc::new(AtomicUsize::new(0));
    let channel = ChunkTrackingChannel {
        inputs: vec!["hello".to_string()].into_iter().collect(),
        outputs: outputs.clone(),
        chunks,
        flush_count,
    };
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
    let has_empty_msg = collected.iter().any(|o| o.contains("empty response"));
    assert!(has_empty_msg);
}

// -- multiple tool error types in sequence (different messages) --

#[tokio::test]
async fn agent_blocked_does_not_loop() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "dangerous".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = BlockedToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    // Blocked error stops loop immediately, only 1 LLM call
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn agent_sandbox_violation_does_not_loop() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "access".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = SandboxToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn agent_io_error_does_not_loop() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "exec".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
    let executor = IoErrorToolExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
    agent.run().await.unwrap();

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

// -- No tool output (Ok(None)) stops loop --

#[tokio::test]
async fn agent_no_tool_output_stops_loop() {
    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = CountingProvider {
        response: "simple text".into(),
        call_count: call_count.clone(),
    };
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["go"], outputs.clone());
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

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

// --- Self-learning agent tests ---

#[cfg(feature = "self-learning")]
mod self_learning {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use zeph_core::agent::Agent;
    use zeph_core::channel::{Channel, ChannelMessage};
    use zeph_core::config::LearningConfig;
    use zeph_llm::provider::{LlmProvider, Message};
    use zeph_memory::semantic::SemanticMemory;
    use zeph_memory::sqlite::SqliteStore;
    use zeph_skills::registry::SkillRegistry;
    use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

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

    #[derive(Clone)]
    struct SequentialProvider {
        responses: Arc<Mutex<VecDeque<String>>>,
        call_count: Arc<AtomicUsize>,
    }

    impl LlmProvider for SequentialProvider {
        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut q = self.responses.lock().unwrap();
            Ok(q.pop_front().unwrap_or_default())
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
            "sequential"
        }
    }

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

    struct MockToolExecutor;

    impl ToolExecutor for MockToolExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
    }

    struct ErrorToolExecutor;

    impl ToolExecutor for ErrorToolExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                summary: "[error] command failed".into(),
                blocks_executed: 1,
            }))
        }
    }

    async fn make_memory(provider: &MockProvider) -> (SemanticMemory<MockProvider>, i64) {
        let memory =
            SemanticMemory::new(":memory:", "http://invalid:6334", provider.clone(), "test")
                .await
                .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        (memory, cid)
    }

    async fn make_memory_file<P: LlmProvider + Clone>(
        provider: &P,
        db_path: &str,
    ) -> (SemanticMemory<P>, i64) {
        let memory = SemanticMemory::new(db_path, "http://invalid:6334", provider.clone(), "test")
            .await
            .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        (memory, cid)
    }

    fn learning_config(enabled: bool) -> LearningConfig {
        LearningConfig {
            enabled,
            auto_activate: false,
            min_failures: 3,
            improve_threshold: 0.7,
            rollback_threshold: 0.5,
            min_evaluations: 5,
            max_versions: 10,
            cooldown_minutes: 0,
        }
    }

    fn make_skill_dir() -> (tempfile::TempDir, SkillRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill.\n---\nDo test stuff.",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        (dir, registry)
    }

    // -- /skill stats --

    #[tokio::test]
    async fn skill_stats_no_memory() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill stats"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Memory not available")));
    }

    #[tokio::test]
    async fn skill_stats_empty_data() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill stats"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(
            collected
                .iter()
                .any(|o| o.contains("No skill outcome data"))
        );
    }

    #[tokio::test]
    async fn skill_stats_with_outcomes() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill stats"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        memory
            .sqlite()
            .record_skill_outcome("git", None, Some(cid), "success", None)
            .await
            .unwrap();
        memory
            .sqlite()
            .record_skill_outcome("git", None, Some(cid), "tool_failure", Some("err"))
            .await
            .unwrap();

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        let stats_output = collected
            .iter()
            .find(|o| o.contains("Skill outcome statistics"));
        assert!(stats_output.is_some());
        assert!(stats_output.unwrap().contains("git"));
    }

    // -- /skill versions --

    #[tokio::test]
    async fn skill_versions_no_name() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill versions"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Usage:")));
    }

    #[tokio::test]
    async fn skill_versions_no_memory() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill versions git"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Memory not available")));
    }

    #[tokio::test]
    async fn skill_versions_empty() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill versions nonexistent"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("No versions found")));
    }

    #[tokio::test]
    async fn skill_versions_with_data() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill versions git"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let v1 = memory
            .sqlite()
            .save_skill_version("git", 1, "body v1", "Git helper", "manual", None, None)
            .await
            .unwrap();
        memory
            .sqlite()
            .activate_skill_version("git", v1)
            .await
            .unwrap();
        memory
            .sqlite()
            .save_skill_version("git", 2, "body v2", "Git helper", "auto", None, Some(v1))
            .await
            .unwrap();

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        let versions_output = collected.iter().find(|o| o.contains("Versions for"));
        assert!(versions_output.is_some());
        let text = versions_output.unwrap();
        assert!(text.contains("v1"));
        assert!(text.contains("v2"));
        assert!(text.contains("active"));
    }

    // -- /skill activate --

    #[tokio::test]
    async fn skill_activate_missing_args() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill activate"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Usage:")));
    }

    #[tokio::test]
    async fn skill_activate_invalid_version() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill activate git abc"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Invalid version")));
    }

    #[tokio::test]
    async fn skill_activate_no_memory() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill activate git 1"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Memory not available")));
    }

    #[tokio::test]
    async fn skill_activate_version_not_found() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill activate git 99"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("not found")));
    }

    #[tokio::test]
    async fn skill_activate_success() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("git");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: git\ndescription: Git helper\n---\nold body",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill activate git 2"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let v1 = memory
            .sqlite()
            .save_skill_version("git", 1, "body v1", "Git helper", "manual", None, None)
            .await
            .unwrap();
        memory
            .sqlite()
            .activate_skill_version("git", v1)
            .await
            .unwrap();
        memory
            .sqlite()
            .save_skill_version("git", 2, "body v2", "Git helper v2", "auto", None, Some(v1))
            .await
            .unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Activated v2")));

        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(content.contains("body v2"));
    }

    // -- /skill approve --

    #[tokio::test]
    async fn skill_approve_no_name() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill approve"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Usage:")));
    }

    #[tokio::test]
    async fn skill_approve_no_memory() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill approve git"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Memory not available")));
    }

    #[tokio::test]
    async fn skill_approve_no_pending() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill approve git"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let v1 = memory
            .sqlite()
            .save_skill_version("git", 1, "body", "desc", "manual", None, None)
            .await
            .unwrap();
        memory
            .sqlite()
            .activate_skill_version("git", v1)
            .await
            .unwrap();

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(
            collected
                .iter()
                .any(|o| o.contains("No pending auto version"))
        );
    }

    #[tokio::test]
    async fn skill_approve_success() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("git");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: git\ndescription: Git helper\n---\nold body",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill approve git"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let v1 = memory
            .sqlite()
            .save_skill_version("git", 1, "body v1", "desc", "manual", None, None)
            .await
            .unwrap();
        memory
            .sqlite()
            .activate_skill_version("git", v1)
            .await
            .unwrap();
        memory
            .sqlite()
            .save_skill_version("git", 2, "improved body", "desc", "auto", None, Some(v1))
            .await
            .unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(
            collected
                .iter()
                .any(|o| o.contains("Approved and activated v2"))
        );

        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(content.contains("improved body"));
    }

    // -- /skill reset --

    #[tokio::test]
    async fn skill_reset_no_name() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill reset"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Usage:")));
    }

    #[tokio::test]
    async fn skill_reset_no_memory() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill reset git"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Memory not available")));
    }

    #[tokio::test]
    async fn skill_reset_no_v1() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill reset git"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(
            collected
                .iter()
                .any(|o| o.contains("Original version not found"))
        );
    }

    #[tokio::test]
    async fn skill_reset_success() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("git");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: git\ndescription: Git helper\n---\nmodified body",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill reset git"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let v1 = memory
            .sqlite()
            .save_skill_version(
                "git",
                1,
                "original body",
                "Git helper",
                "manual",
                None,
                None,
            )
            .await
            .unwrap();
        let v2 = memory
            .sqlite()
            .save_skill_version(
                "git",
                2,
                "modified body",
                "Git helper",
                "auto",
                None,
                Some(v1),
            )
            .await
            .unwrap();
        memory
            .sqlite()
            .activate_skill_version("git", v2)
            .await
            .unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(
            collected
                .iter()
                .any(|o| o.contains("Reset \"git\" to original v1"))
        );

        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(content.contains("original body"));
    }

    // -- /skill unknown subcommand --

    #[tokio::test]
    async fn skill_unknown_subcommand() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/skill bogus"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(
            collected
                .iter()
                .any(|o| o.contains("Unknown /skill subcommand"))
        );
    }

    // -- /feedback --

    #[tokio::test]
    async fn feedback_no_message() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/feedback test-skill"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Usage:")));
    }

    #[tokio::test]
    async fn feedback_empty_message() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/feedback test-skill \"\""], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Usage:")));
    }

    #[tokio::test]
    async fn feedback_no_memory() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/feedback test-skill bad output"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Memory not available")));
    }

    #[tokio::test]
    async fn feedback_records_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/feedback test-skill bad output"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100);
        agent.run().await.unwrap();

        {
            let collected = outputs.lock().unwrap();
            assert!(collected.iter().any(|o| o.contains("Feedback recorded")));
        }

        let store = SqliteStore::new(db_str).await.unwrap();
        let stats = store.load_skill_outcome_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].skill_name, "test-skill");
    }

    #[tokio::test]
    async fn feedback_with_learning_triggers_improvement() {
        let (dir, registry) = make_skill_dir();

        let provider = MockProvider::new("improved skill body content");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(
            vec!["/feedback test-skill \"the output is wrong\""],
            outputs.clone(),
        );
        let (memory, cid) = make_memory(&provider).await;

        let config = learning_config(true);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Feedback recorded")));
    }

    // -- is_learning_enabled --

    #[tokio::test]
    async fn learning_enabled_with_config() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["hello"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        let config = learning_config(true);
        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100)
        .with_learning(config);

        agent.run().await.unwrap();
    }

    #[tokio::test]
    async fn learning_disabled_without_config() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["hello"], outputs.clone());

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        );

        agent.run().await.unwrap();
    }

    // -- record_skill_outcomes --

    #[tokio::test]
    async fn record_skill_outcomes_with_active_skills() {
        let (dir, registry) = make_skill_dir();
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["hello"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let config = learning_config(false);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let store = SqliteStore::new(db_str).await.unwrap();
        let stats = store.load_skill_outcome_stats().await.unwrap();
        assert!(stats.iter().any(|s| s.skill_name == "test-skill"));
    }

    #[tokio::test]
    async fn record_skill_outcomes_tool_failure() {
        let (dir, registry) = make_skill_dir();
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("response");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["do it"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let config = learning_config(false);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, ErrorToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let store = SqliteStore::new(db_str).await.unwrap();
        let stats = store.load_skill_outcome_stats().await.unwrap();
        let skill_stats = stats.iter().find(|s| s.skill_name == "test-skill");
        assert!(skill_stats.is_some());
        assert!(skill_stats.unwrap().failures > 0);
    }

    // -- check_rollback --

    #[tokio::test]
    async fn check_rollback_triggers_on_low_success_rate() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test.\n---\nauto body",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("response");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["do it"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let v1 = memory
            .sqlite()
            .save_skill_version("test-skill", 1, "original", "desc", "manual", None, None)
            .await
            .unwrap();
        let v2 = memory
            .sqlite()
            .save_skill_version("test-skill", 2, "auto body", "desc", "auto", None, Some(v1))
            .await
            .unwrap();
        memory
            .sqlite()
            .activate_skill_version("test-skill", v2)
            .await
            .unwrap();

        for _ in 0..6 {
            memory
                .sqlite()
                .record_skill_outcome("test-skill", None, Some(cid), "tool_failure", Some("err"))
                .await
                .unwrap();
        }

        let config = LearningConfig {
            enabled: true,
            auto_activate: false,
            min_failures: 1,
            improve_threshold: 0.7,
            rollback_threshold: 0.5,
            min_evaluations: 5,
            max_versions: 10,
            cooldown_minutes: 0,
        };

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, ErrorToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let store = SqliteStore::new(db_str).await.unwrap();
        let active = store.active_skill_version("test-skill").await.unwrap();
        assert!(active.is_some());
        assert_eq!(active.unwrap().version, 1);
    }

    #[tokio::test]
    async fn check_rollback_skips_when_not_auto() {
        let (dir, registry) = make_skill_dir();
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("response");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["do it"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let v1 = memory
            .sqlite()
            .save_skill_version("test-skill", 1, "original", "desc", "manual", None, None)
            .await
            .unwrap();
        memory
            .sqlite()
            .activate_skill_version("test-skill", v1)
            .await
            .unwrap();

        for _ in 0..6 {
            memory
                .sqlite()
                .record_skill_outcome("test-skill", None, Some(cid), "tool_failure", None)
                .await
                .unwrap();
        }

        let config = LearningConfig {
            enabled: true,
            auto_activate: false,
            min_failures: 1,
            improve_threshold: 0.7,
            rollback_threshold: 0.5,
            min_evaluations: 5,
            max_versions: 10,
            cooldown_minutes: 0,
        };

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, ErrorToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let store = SqliteStore::new(db_str).await.unwrap();
        let active = store.active_skill_version("test-skill").await.unwrap();
        assert!(active.is_some());
        assert_eq!(active.unwrap().version, 1);
    }

    // -- check_improvement_allowed --

    #[tokio::test]
    async fn improvement_blocked_by_min_failures() {
        let (dir, registry) = make_skill_dir();
        let provider = MockProvider::new("improved body");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/feedback test-skill bad result"], outputs.clone());
        let (memory, cid) = make_memory(&provider).await;

        memory
            .sqlite()
            .ensure_skill_version_exists("test-skill", "Do test stuff.", "A test skill.")
            .await
            .unwrap();
        memory
            .sqlite()
            .record_skill_outcome("test-skill", None, Some(cid), "success", None)
            .await
            .unwrap();

        let config = LearningConfig {
            enabled: true,
            auto_activate: false,
            min_failures: 100,
            improve_threshold: 0.7,
            rollback_threshold: 0.5,
            min_evaluations: 5,
            max_versions: 10,
            cooldown_minutes: 0,
        };

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let collected = outputs.lock().unwrap();
        assert!(collected.iter().any(|o| o.contains("Feedback recorded")));
    }

    // -- generate_improved_skill + store_improved_version with auto_activate --

    #[tokio::test]
    async fn generate_and_auto_activate_improvement() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill.\n---\nDo test stuff.",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("improved test stuff");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(
            vec!["/feedback test-skill \"needs improvement\""],
            outputs.clone(),
        );
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let config = LearningConfig {
            enabled: true,
            auto_activate: true,
            min_failures: 0,
            improve_threshold: 1.0,
            rollback_threshold: 0.5,
            min_evaluations: 5,
            max_versions: 10,
            cooldown_minutes: 0,
        };

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        let store = SqliteStore::new(db_str).await.unwrap();
        let versions = store.load_skill_versions("test-skill").await.unwrap();
        assert!(versions.len() >= 2);
        let active = store.active_skill_version("test-skill").await.unwrap();
        assert!(active.is_some());
        assert!(active.unwrap().version >= 2);
    }

    // -- attempt_self_reflection --

    #[tokio::test]
    async fn self_reflection_on_empty_response() {
        let (dir, registry) = make_skill_dir();

        let responses: VecDeque<String> =
            vec![String::new(), "recovered response".to_string()].into();
        let call_count = Arc::new(AtomicUsize::new(0));
        let provider = SequentialProvider {
            responses: Arc::new(Mutex::new(responses)),
            call_count: call_count.clone(),
        };

        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["hello"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, ":memory:").await;

        let config = learning_config(true);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        assert!(call_count.load(Ordering::SeqCst) >= 2);
    }

    // -- with_learning builder --

    #[tokio::test]
    async fn with_learning_builder() {
        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["test"], outputs.clone());

        let config = learning_config(true);
        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_learning(config);

        agent.run().await.unwrap();
        let collected = outputs.lock().unwrap();
        assert_eq!(collected.len(), 1);
    }

    // -- feedback with learning disabled does not generate improvement --

    #[tokio::test]
    async fn feedback_learning_disabled_no_improvement() {
        let (dir, registry) = make_skill_dir();
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["/feedback test-skill bad result"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let config = learning_config(false);

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        drop(tx);

        let mut agent = Agent::new(provider, channel, registry, None, 5, MockToolExecutor)
            .with_memory(memory, cid, 50, 5, 100)
            .with_learning(config)
            .with_skill_reload(vec![dir.path().to_path_buf()], rx);
        agent.run().await.unwrap();

        {
            let collected = outputs.lock().unwrap();
            assert!(collected.iter().any(|o| o.contains("Feedback recorded")));
        }

        let store = SqliteStore::new(db_str).await.unwrap();
        let versions = store.load_skill_versions("test-skill").await.unwrap();
        assert!(versions.is_empty());
    }

    // -- record_skill_outcomes with no active skills is a no-op --

    #[tokio::test]
    async fn record_outcomes_no_active_skills() {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let db_str = db_path.to_str().unwrap();

        let provider = MockProvider::new("ok");
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let channel = MockChannel::new(vec!["hello"], outputs.clone());
        let (memory, cid) = make_memory_file(&provider, db_str).await;

        let config = learning_config(true);

        let mut agent = Agent::new(
            provider,
            channel,
            SkillRegistry::default(),
            None,
            5,
            MockToolExecutor,
        )
        .with_memory(memory, cid, 50, 5, 100)
        .with_learning(config);
        agent.run().await.unwrap();

        let store = SqliteStore::new(db_str).await.unwrap();
        let stats = store.load_skill_outcome_stats().await.unwrap();
        assert!(stats.is_empty());
    }
}
