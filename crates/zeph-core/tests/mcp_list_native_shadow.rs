// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::large_futures, clippy::unwrap_used, clippy::expect_used)]
#![recursion_limit = "256"]

//! Acceptance tests (tests-first) for #7134, #7118 and #7138: `/mcp list` driven through
//! `McpAccess::handle_mcp` with servers registered by `McpManager::register_connected_for_testing`.
//!
//! Requires the `zeph-mcp` `mock` feature as a dev-dependency of `zeph-core` (planned by #7138).

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
use zeph_tools::registry::{InvocationHint, ToolDef};

struct NullChannel;

impl Channel for NullChannel {
    #[allow(clippy::unused_async_trait_impl)]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(None)
    }
    #[allow(clippy::unused_async_trait_impl)]
    async fn send(&mut self, _text: &str) -> Result<(), ChannelError> {
        Ok(())
    }
    #[allow(clippy::unused_async_trait_impl)]
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

struct NativeExecutor(&'static str);
impl ToolExecutor for NativeExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }
    fn set_skill_env(&self, _env: Option<HashMap<String, String>>) {}
    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: self.0.into(),
            description: "native".into(),
            schema: serde_json::from_value(serde_json::json!({"type": "object"})).unwrap(),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        }]
    }
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

#[tokio::test]
async fn mcp_list_native_executor_added_after_with_mcp_hides_colliding_tool() {
    let tools = vec![tool("memory", "search"), tool("memory", "other")];
    let mgr = manager(&[("memory", tools.clone())]).await;
    let mut agent =
        with_mcp(base_agent(), tools, mgr).add_tool_executor(NativeExecutor("memory_search"));
    let out = agent.handle_mcp("list").await.unwrap();
    assert!(
        out.contains("native tool wins"),
        "late native executor not detected: {out}"
    );
    assert!(out.contains("- memory:search (id: memory_search)"), "{out}");
}

#[tokio::test]
async fn mcp_list_native_executor_added_before_with_mcp_hides_colliding_tool() {
    let tools = vec![tool("memory", "search")];
    let mgr = manager(&[("memory", tools.clone())]).await;
    let mut agent = with_mcp(
        base_agent().add_tool_executor(NativeExecutor("memory_search")),
        tools,
        mgr,
    );
    let out = agent.handle_mcp("list").await.unwrap();
    assert!(out.contains("- memory:search (id: memory_search)"), "{out}");
}

#[tokio::test]
async fn mcp_list_without_native_collision_has_no_hidden_section() {
    let tools = vec![tool("memory", "search")];
    let mgr = manager(&[("memory", tools.clone())]).await;
    let mut agent =
        with_mcp(base_agent(), tools, mgr).add_tool_executor(NativeExecutor("unrelated"));
    let out = agent.handle_mcp("list").await.unwrap();
    assert!(!out.contains("native tool wins"), "{out}");
}

#[tokio::test]
async fn mcp_list_servers_in_configured_order_with_shadowed_section() {
    let servers = [
        ("a.b", vec![tool("a.b", "c")]),
        ("a", vec![tool("a", "b_c")]),
    ];
    let all: Vec<McpTool> = servers.iter().flat_map(|(_, t)| t.clone()).collect();
    let mgr = manager(&servers).await;
    let mut agent = with_mcp(base_agent(), all, mgr);
    let out = agent.handle_mcp("list").await.unwrap();
    let first = out.find("- a.b (").expect("a.b listed");
    let second = out.find("- a (").expect("a listed");
    assert!(
        first < second,
        "configured order must win over alphabetical: {out}"
    );
    assert!(out.contains("Shadowed tools"), "{out}");
    assert!(
        out.contains("a:b_c is shadowed by a.b:c (id: a_b_c)"),
        "{out}"
    );
}
