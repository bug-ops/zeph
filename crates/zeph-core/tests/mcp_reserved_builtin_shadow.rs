// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    reason = "The test exercises the concrete async path without boxing its future.; Fixture construction failures should fail the test immediately."
)]
#![recursion_limit = "256"]

//! Acceptance tests (tests-first) for #7142: MCP tools whose sanitized id equals a reserved
//! agent built-in name (`ask_user`, `start_focus`, `compress_context`, ...) are hidden even while
//! the built-in itself is disabled, regardless of builder order.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use zeph_commands::McpAccess;
use zeph_core::Agent;
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_mcp::policy::PolicyEnforcer;
use zeph_mcp::tool::ToolSecurityMeta;
use zeph_mcp::{McpManager, McpTool, McpTransport, McpTrustLevel, ServerEntry};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

struct NullChannel;

impl Channel for NullChannel {
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "recv implements the trait-required async signature while completing synchronously"
    )]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(None)
    }
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "send implements the trait-required async signature while completing synchronously"
    )]
    async fn send(&mut self, _text: &str) -> Result<(), ChannelError> {
        Ok(())
    }
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "send_chunk implements the trait-required async signature while completing synchronously"
    )]
    async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
        Ok(())
    }
    fn flush_chunks(&mut self) -> impl Future<Output = Result<(), ChannelError>> + Send {
        std::future::ready(Ok(()))
    }
}

struct NoopExecutor;
impl ToolExecutor for NoopExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }
    fn set_skill_env(&self, _env: Option<HashMap<String, String>>) {}
    zeph_tools::tool_executor_no_inner_defaults!();
}

fn tool(server: &str, name: &str) -> McpTool {
    McpTool {
        server_id: server.into(),
        name: name.to_owned(),
        description: String::new(),
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: None,
        security_meta: ToolSecurityMeta::default(),
    }
}

fn entry(id: &str) -> ServerEntry {
    ServerEntry {
        id: id.into(),
        transport: McpTransport::Stdio {
            command: "nonexistent-mcp-binary".into(),
            args: Vec::new(),
            env: HashMap::new(),
        },
        timeout: Duration::from_secs(5),
        trust_level: McpTrustLevel::Untrusted,
        tool_allowlist: None,
        allow_untrusted_without_allowlist: false,
        expected_tools: Vec::new(),
        roots: Vec::new(),
        tool_metadata: HashMap::new(),
        elicitation_enabled: false,
        elicitation_timeout_secs: 120,
        env_isolation: false,
        media_passthrough: false,
    }
}

async fn manager(servers: &[(&str, Vec<McpTool>)]) -> Arc<McpManager> {
    let mgr = Arc::new(McpManager::new(
        servers.iter().map(|(id, _)| entry(id)).collect(),
        vec![],
        PolicyEnforcer::new(vec![]),
    ));
    for (id, tools) in servers.iter().rev() {
        mgr.register_connected_for_testing(id, tools.clone()).await;
    }
    mgr
}

fn base_agent() -> Agent<NullChannel> {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    Agent::new(
        AnyProvider::Mock(MockProvider::default()),
        NullChannel,
        registry,
        None,
        5,
        NoopExecutor,
    )
}

fn with_mcp(
    agent: Agent<NullChannel>,
    tools: Vec<McpTool>,
    mgr: Arc<McpManager>,
) -> Agent<NullChannel> {
    agent.with_mcp(
        tools,
        None,
        Some(mgr),
        &zeph_core::config::McpConfig::default(),
    )
}

const RESERVED: [(&str, &str, &str); 6] = [
    ("compress", "context", "compress_context"),
    ("request", "compaction", "request_compaction"),
    ("start", "focus", "start_focus"),
    ("complete", "focus", "complete_focus"),
    ("skill", "state_patch", "skill_state_patch"),
    ("ask", "user", "ask_user"),
];

#[tokio::test]
async fn reserved_builtin_named_mcp_tool_is_hidden_with_builtin_disabled() {
    for (server, name, id) in RESERVED {
        let tools = vec![tool(server, name), tool(server, "benign")];
        let mgr = manager(&[(server, tools.clone())]).await;
        let (writer, reader) = zeph_mcp::McpToolList::new(tools.clone());
        let ids = writer.ids_handle();
        let _agent = with_mcp(base_agent(), tools, mgr).with_mcp_tool_list(writer);
        let names: Vec<_> = reader
            .snapshot()
            .tools
            .iter()
            .map(McpTool::qualified_name)
            .collect();
        assert_eq!(names, [format!("{server}:benign")], "{id} must be hidden");
        assert!(!ids.contains(id), "{id} must not be an MCP id");
    }
}

#[tokio::test]
async fn mcp_list_reports_reserved_builtin_collision_in_hidden_section() {
    let tools = vec![tool("ask", "user")];
    let mgr = manager(&[("ask", tools.clone())]).await;
    let mut agent = with_mcp(base_agent(), tools, mgr);
    let out = agent.handle_mcp("list").await.unwrap();
    assert!(out.contains("reserved built-in"), "{out}");
    assert!(out.contains("- ask:user (id: ask_user)"), "{out}");
}
