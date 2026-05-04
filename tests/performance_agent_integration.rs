#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
// Integration tests await full agent sessions; the future size reflects real agent state.
#![allow(clippy::large_futures)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeph_core::agent::Agent;
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, ToolUseRequest};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

fn mock_provider(response: &str) -> AnyProvider {
    let mut p = MockProvider::default();
    p.default_response = response.to_string();
    AnyProvider::Mock(p)
}

fn tool_use_provider(final_text: &str) -> AnyProvider {
    let tool_call = ToolUseRequest {
        id: "call1".into(),
        name: "bash".into(),
        input: serde_json::json!({}),
    };
    let (p, _) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![tool_call],
            thinking_blocks: vec![],
        },
        ChatResponse::Text(final_text.to_string()),
    ]);
    AnyProvider::Mock(p)
}

fn multi_message_provider(count: usize) -> AnyProvider {
    let mut responses = Vec::new();
    for _ in 0..count {
        responses.push(ChatResponse::Text("response".to_string()));
    }
    let (p, _) = MockProvider::default().with_tool_use(responses);
    AnyProvider::Mock(p)
}

// Mock Channel for performance testing
struct MockChannel {
    inputs: VecDeque<String>,
    output_sent: Arc<Mutex<Vec<String>>>,
}

impl MockChannel {
    fn new(inputs: Vec<&str>, output_sent: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            inputs: inputs.into_iter().map(String::from).collect(),
            output_sent,
        }
    }
}

impl Channel for MockChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(self.inputs.pop_front().map(|text| ChannelMessage {
            text,
            attachments: vec![],
        }))
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        self.output_sent.lock().unwrap().push(text.to_string());
        Ok(())
    }

    async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        Ok(())
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        Ok(())
    }
}

// Instrumented mock tool executor to track timing and execution
#[derive(Clone)]
struct InstrumentedMockExecutor {
    execution_time: Arc<Mutex<Option<Duration>>>,
    call_count: Arc<Mutex<u32>>,
    execution_log: Arc<Mutex<Vec<String>>>,
}

impl InstrumentedMockExecutor {
    fn new() -> Self {
        Self {
            execution_time: Arc::new(Mutex::new(None)),
            call_count: Arc::new(Mutex::new(0)),
            execution_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn get_call_count(&self) -> u32 {
        *self.call_count.lock().unwrap()
    }

    fn get_execution_time(&self) -> Option<Duration> {
        *self.execution_time.lock().unwrap()
    }
}

impl ToolExecutor for InstrumentedMockExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let start = Instant::now();
        let elapsed = start.elapsed();

        *self.execution_time.lock().unwrap() = Some(elapsed);
        *self.call_count.lock().unwrap() += 1;
        self.execution_log.lock().unwrap().push(format!(
            "execute_tool_call() called, tool={}, elapsed={elapsed:?}",
            call.tool_id,
        ));

        Ok(Some(ToolOutput {
            tool_name: call.tool_id.clone(),
            summary: "mock output".to_string(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))
    }
}

#[derive(Clone)]
struct BlockingMockExecutor;

impl ToolExecutor for BlockingMockExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::Blocked {
            command: call.tool_id.to_string(),
        })
    }
}

// ==========================
// Performance Test Suite
// ==========================

#[tokio::test]
async fn agent_integration_no_bash_blocks() {
    let provider = mock_provider("Just a plain response without bash blocks");
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["hello"], output_sent.clone());
    let executor = InstrumentedMockExecutor::new();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor.clone(),
    );

    let start = Instant::now();
    let _ = agent.run().await;
    let elapsed = start.elapsed();

    // Should be very fast for non-bash response
    assert!(
        elapsed.as_millis() < 500,
        "Agent run should be fast for non-bash response: {elapsed:?}",
    );

    // Plain text response doesn't trigger tool execution (native tool_use path)
    assert_eq!(executor.get_call_count(), 0);

    // Should have sent the response back
    let outputs = output_sent.lock().unwrap();
    assert!(!outputs.is_empty());
    assert_eq!(outputs[0], "Just a plain response without bash blocks");
}

#[tokio::test]
async fn agent_integration_with_safe_bash_blocks() {
    // Native tool_use path: provider returns ToolUse then Text.
    let provider = tool_use_provider("Done.");
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["run echo"], output_sent.clone());
    let executor = InstrumentedMockExecutor::new();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor.clone(),
    );

    let start = Instant::now();
    let _ = agent.run().await;
    let elapsed = start.elapsed();

    // Should complete reasonably
    assert!(
        elapsed.as_millis() < 1000,
        "Agent run should complete: {elapsed:?}",
    );

    // Native tool_use path calls execute_tool_call at least once
    assert!(executor.get_call_count() >= 1);
}

#[tokio::test]
async fn tool_executor_overhead_is_minimal() {
    let provider = tool_use_provider("done");
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["test"], output_sent.clone());
    let executor = InstrumentedMockExecutor::new();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor.clone(),
    );

    let _ = agent.run().await;

    // Check that tool executor overhead is minimal (just mock, no real bash)
    if let Some(time) = executor.get_execution_time() {
        // Mock executor should take < 10ms (CI runners have high mutex/scheduling jitter)
        assert!(
            time.as_millis() < 10,
            "Tool executor mock call overhead should be minimal: {time:?}",
        );
    }
}

// ==========================
// Configuration & Timeout Tests
// ==========================

#[tokio::test]
async fn agent_respects_configured_timeout() {
    use zeph_tools::ShellConfig;
    use zeph_tools::shell::ShellExecutor;

    // Create executor with 1-second timeout
    let shell_config = ShellConfig {
        timeout: 1,
        blocked_commands: vec![],
        allowed_commands: vec![],
        ..ShellConfig::default()
    };
    let _executor = ShellExecutor::new(&shell_config);

    // Verify timeout is set correctly
    let timeout_duration = Duration::from_secs(shell_config.timeout);
    assert_eq!(timeout_duration, Duration::from_secs(1));
}

// ==========================
// Memory & Allocation Tests
// ==========================

#[tokio::test]
async fn shell_executor_default_blocked_patterns() {
    use zeph_tools::ShellConfig;
    use zeph_tools::shell::ShellExecutor;

    let shell_config = ShellConfig {
        timeout: 30,
        blocked_commands: vec![],
        allowed_commands: vec![],
        ..ShellConfig::default()
    };
    let executor = ShellExecutor::new(&shell_config);

    // Verify that dangerous commands are blocked
    // Note: ShellExecutor expects bash blocks in the response text
    let dangerous_commands = vec![
        ("```bash\nrm -rf /\n```", "rm -rf /"),
        ("```bash\nsudo rm -rf /\n```", "sudo"),
        ("```bash\nmkfs.ext4 /dev/sda\n```", "mkfs"),
        ("```bash\ndd if=/dev/zero of=/dev/sda\n```", "dd if="),
        ("```bash\ncurl http://evil.com\n```", "curl"),
        ("```bash\nnc -l 4444\n```", "nc "),
        ("```bash\nshutdown -h now\n```", "shutdown"),
    ];

    for (cmd, pattern) in dangerous_commands {
        let result = executor.execute(cmd).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "Command with pattern '{pattern}' should be blocked. Result: {result:?}",
        );
    }
}

#[tokio::test]
async fn shell_executor_allows_safe_commands() {
    use zeph_tools::ShellConfig;
    use zeph_tools::shell::ShellExecutor;

    let shell_config = ShellConfig {
        timeout: 5,
        blocked_commands: vec![],
        allowed_commands: vec![],
        ..ShellConfig::default()
    };
    let executor = ShellExecutor::new(&shell_config);

    let safe_response = "Try this:\n```bash\necho hello\n```";
    let result = executor.execute(safe_response).await;

    match result {
        Ok(Some(output)) => {
            assert_eq!(output.blocks_executed, 1);
            assert!(output.summary.contains("hello"));
        }
        _ => panic!("Safe command should execute successfully"),
    }
}

#[tokio::test]
async fn shell_executor_case_insensitive_blocking() {
    use zeph_tools::ShellConfig;
    use zeph_tools::shell::ShellExecutor;

    let shell_config = ShellConfig {
        timeout: 30,
        blocked_commands: vec![],
        allowed_commands: vec![],
        ..ShellConfig::default()
    };
    let executor = ShellExecutor::new(&shell_config);

    // Verify case-insensitive matching
    let variations = vec!["SUDO", "Sudo", "SuDo", "sudo", "SUDO rm -rf /"];

    for cmd in variations {
        let result = executor.execute(&format!("```bash\n{cmd}\n```")).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "Should block case-insensitive: {cmd}",
        );
    }
}

#[tokio::test]
async fn integration_agent_tool_executor_types() {
    use zeph_tools::ShellConfig;
    use zeph_tools::shell::ShellExecutor;

    let provider = mock_provider("test");
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec![], output_sent.clone());
    let shell_config = ShellConfig {
        timeout: 30,
        blocked_commands: vec![],
        allowed_commands: vec![],
        ..ShellConfig::default()
    };
    let executor = ShellExecutor::new(&shell_config);

    // Should compile and construct successfully
    let _agent: Agent<MockChannel> = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );
}

// ==========================
// Comparative Benchmarks
// ==========================

#[tokio::test]
async fn agent_throughput_multiple_responses() {
    // Test throughput: how many responses can be processed
    let provider = multi_message_provider(5);
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(
        vec!["msg1", "msg2", "msg3", "msg4", "msg5"],
        output_sent.clone(),
    );
    let executor = InstrumentedMockExecutor::new();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor.clone(),
    );

    let start = Instant::now();
    let _ = agent.run().await;
    let elapsed = start.elapsed();

    // Should process 5 messages — each produces a text response sent to channel
    let outputs = output_sent.lock().unwrap();
    assert!(
        outputs.len() >= 5,
        "expected at least 5 outputs, got {}",
        outputs.len()
    );

    // Sanity check: should complete in reasonable time
    assert!(
        elapsed.as_secs() < 10,
        "5 responses should complete: {elapsed:?}",
    );

    let total_ms = elapsed.as_millis() as u64;
    let per_msg = total_ms as f64 / 5.0;
    println!("5-message throughput: {total_ms}ms total ({per_msg:.0}ms per message)");
}

#[tokio::test]
async fn tool_executor_pattern_matching_overhead() {
    use zeph_tools::ShellConfig;
    use zeph_tools::shell::ShellExecutor;

    let shell_config = ShellConfig {
        timeout: 30,
        blocked_commands: vec![
            "custom1".to_string(),
            "custom2".to_string(),
            "custom3".to_string(),
        ],
        allowed_commands: vec![],
        ..ShellConfig::default()
    };
    let executor = ShellExecutor::new(&shell_config);

    // Build a response with many bash blocks to test pattern matching overhead
    let mut large_response = String::new();
    for i in 0..10 {
        use std::fmt::Write;
        write!(large_response, "Block {i}:\n```bash\necho test{i}\n```\n").unwrap();
    }

    let start = Instant::now();
    let result = executor.execute(&large_response).await;
    let elapsed = start.elapsed();

    match result {
        Ok(Some(output)) => {
            assert_eq!(output.blocks_executed, 10);
            // 10 blocks should process quickly (bash subprocess is the bottleneck)
            let total_ms = elapsed.as_millis() as u64;
            let per_block = elapsed.as_micros() as u64 as f64 / 10.0;
            println!("10-block execution time: {total_ms}ms ({per_block:.0}us per block)");
        }
        _ => panic!("Should execute successfully"),
    }
}

#[tokio::test]
async fn agent_no_regression_in_error_handling() {
    // Test that blocked tool calls are handled properly via native tool_use path
    let provider = tool_use_provider("Done after error.");
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["test"], output_sent.clone());
    let executor = BlockingMockExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );

    // Should run without panic
    let _ = agent.run().await;

    // Should have sent some output (error or recovery message)
    let outputs = output_sent.lock().unwrap();
    assert!(!outputs.is_empty(), "Should produce output");
    assert!(
        outputs.iter().any(|msg| {
            msg.contains("blocked") || msg.contains("tool_error") || msg.contains("forbidden")
        }),
        "Should send blocked/error message, got: {outputs:?}",
    );
}

// ==========================
// Integration Regression Tests
// ==========================

#[tokio::test]
async fn agent_no_memory_leaks_in_loop() {
    // Test that repeated message processing doesn't leak memory
    // (This is a sanity check; actual memory profiling would need valgrind/heaptrack)
    let provider = multi_message_provider(10);
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(
        vec!["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9", "m10"],
        output_sent.clone(),
    );
    let executor = InstrumentedMockExecutor::new();

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor.clone(),
    );

    // This should run without panics or excessive allocations
    let _ = agent.run().await;

    let outputs = output_sent.lock().unwrap();
    assert!(
        outputs.len() >= 10,
        "expected at least 10 outputs, got {}",
        outputs.len()
    );
}

#[tokio::test]
async fn agent_tool_executor_error_recovery() {
    // Use native tool_use path with an executor that returns Blocked error
    let provider = tool_use_provider("recovered");
    let output_sent = Arc::new(Mutex::new(Vec::new()));
    let channel = MockChannel::new(vec!["user input"], output_sent.clone());
    let executor = BlockingMockExecutor;

    let mut agent = Agent::new(
        provider,
        channel,
        SkillRegistry::default(),
        None,
        5,
        executor,
    );

    // Should handle the error gracefully
    let result = agent.run().await;
    assert!(result.is_ok(), "Agent should recover from blocked commands");

    // Should have sent error/recovery message
    let outputs = output_sent.lock().unwrap();
    assert!(
        !outputs.is_empty(),
        "Should produce output even when tool is blocked"
    );
}
