// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, ToolUseRequest};
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::agent::Agent;
use crate::agent::agent_tests::{MockChannel, create_test_registry};

struct NoOpExecutor;

impl ToolExecutor for NoOpExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: "bash".into(),
            description: "run shell command".into(),
            schema: schemars::Schema::default(),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        }]
    }

    async fn execute_tool_call(&self, _call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }
}

/// When a pre-execution verifier blocks a tool call and an audit logger is wired,
/// an `AuditEntry` with `AuditResult::Blocked` must be written.
#[tokio::test]
async fn pre_execution_block_writes_audit_entry() {
    use crate::config::{SecurityConfig, TimeoutConfig};
    use zeph_config::tools::{
        FirewallVerifierConfig, PreExecutionVerifierConfig, UrlGroundingVerifierConfig,
    };

    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");

    // Create audit logger pointing at the temp file.
    let audit_config = zeph_tools::AuditConfig {
        enabled: true,
        destination: audit_path.display().to_string(),
        ..Default::default()
    };
    let logger = Arc::new(
        zeph_tools::AuditLogger::from_config(&audit_config, false)
            .await
            .unwrap(),
    );

    // Provider returns one tool call with /etc/passwd as the file_path — triggers FirewallVerifier
    // (glob pattern matches the exact path value), then returns text to end the loop.
    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![ToolUseRequest {
                id: "call-block".to_owned(),
                name: "read_file".to_owned().into(),
                input: serde_json::json!({"file_path": "/etc/passwd"}),
            }],
            thinking_blocks: vec![],
        },
        ChatResponse::Text("done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec!["run it".to_string()]);
    let registry = create_test_registry();

    // Build SecurityConfig with firewall verifier enabled; disable all others to keep it simple.
    let security = SecurityConfig {
        pre_execution_verify: PreExecutionVerifierConfig {
            enabled: true,
            destructive_commands: zeph_tools::DestructiveVerifierConfig {
                enabled: false,
                ..Default::default()
            },
            injection_patterns: zeph_tools::InjectionVerifierConfig {
                enabled: false,
                ..Default::default()
            },
            url_grounding: UrlGroundingVerifierConfig {
                enabled: false,
                ..Default::default()
            },
            firewall: FirewallVerifierConfig {
                enabled: true,
                blocked_paths: Vec::new(),
                blocked_env_vars: Vec::new(),
                exempt_tools: Vec::new(),
            },
        },
        ..Default::default()
    };

    let mut agent = Agent::new(provider, channel, registry, None, 5, NoOpExecutor)
        .with_security(security, TimeoutConfig::default())
        .with_audit_logger(Arc::clone(&logger));

    agent.run().await.unwrap();

    // Give tokio::spawn a chance to flush the audit entry.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    drop(logger);

    let content = tokio::fs::read_to_string(&audit_path)
        .await
        .unwrap_or_default();
    assert!(
        !content.is_empty(),
        "audit log must contain at least one entry after pre-execution block"
    );
    assert!(
        content.contains("\"type\":\"blocked\""),
        "audit log must contain a blocked entry; got: {content}"
    );
    assert!(
        content.contains("pre_execution_block"),
        "audit log must contain error_category=pre_execution_block; got: {content}"
    );
    assert!(
        content.contains("read_file"),
        "audit log entry must reference the blocked tool id; got: {content}"
    );
    assert!(
        content.contains("\"error_domain\":\"security\""),
        "error_domain not found in audit entry"
    );
    assert!(
        content.contains("\"duration_ms\":0"),
        "duration_ms not found in audit entry"
    );
}
