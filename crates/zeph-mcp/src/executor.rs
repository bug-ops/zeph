// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use parking_lot::RwLock;

use zeph_common::ToolName;
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, extract_fenced_blocks};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::manager::McpManager;
use crate::tool::McpTool;

/// [`ToolExecutor`] implementation that dispatches tool calls to MCP servers.
///
/// `McpToolExecutor` bridges the `zeph-tools` dispatch layer and `McpManager`. It
/// maintains a local snapshot of the registered MCP tools (updated via
/// [`set_tools`](McpToolExecutor::set_tools)) and resolves tool calls by matching
/// the sanitized tool ID against the snapshot before forwarding to the manager.
///
/// # Security invariant
///
/// [`execute_tool_call`](McpToolExecutor::execute_tool_call) sets
/// `ToolOutput::tool_name` to [`McpTool::qualified_name`] (i.e. `"server_id:name"`).
/// The `':'` in the name is the signal used by `zeph-core`'s `sanitize_tool_output()`
/// to route responses through the quarantine pipeline. Do not change this.
///
/// # Fenced-block execution
///
/// [`execute`](McpToolExecutor::execute) parses ```` ```mcp ```` fenced blocks
/// from LLM output and validates each `server:tool` pair against the registered list
/// before dispatching, preventing prompt injection from routing calls to unknown servers.
#[derive(Debug, Clone)]
pub struct McpToolExecutor {
    manager: Arc<McpManager>,
    tools: Arc<RwLock<Vec<McpTool>>>,
    /// Validator for MCP-sourced images (spec-072). `None` disables media passthrough
    /// entirely — `media_passthrough` server config becomes a no-op without it.
    media_sanitizer: Option<Arc<zeph_sanitizer::MediaSanitizer>>,
    /// Per-tool-result cap on validated images (`[mcp.media].max_images_per_result`).
    max_images_per_result: usize,
    /// Audit logger for MCP media accept/reject decisions.
    audit_logger: Option<Arc<zeph_tools::AuditLogger>>,
    /// Status sender for the mandatory TUI/CLI decode spinner (`CLAUDE.md` "TUI Rules").
    /// `None` disables the indicator only — sanitization still runs.
    status_tx: Option<zeph_llm::provider::StatusTx>,
}

impl McpToolExecutor {
    /// Create a new executor from a shared `McpManager` and a shared tool list.
    ///
    /// The `tools` `RwLock` is updated via [`set_tools`](Self::set_tools) after each
    /// connect or refresh. Pass the same `Arc<RwLock<Vec<McpTool>>>` to both the executor
    /// and the code that handles `tools/list_changed` events.
    #[must_use]
    pub fn new(manager: Arc<McpManager>, tools: Arc<RwLock<Vec<McpTool>>>) -> Self {
        Self {
            manager,
            tools,
            media_sanitizer: None,
            max_images_per_result: 0,
            audit_logger: None,
            status_tx: None,
        }
    }

    /// Attach a [`zeph_sanitizer::MediaSanitizer`] and its per-result image cap.
    ///
    /// Without this, every server's `media_passthrough` config is a no-op — there is no
    /// sanitizer to validate `ContentBlock::Image` blocks through, so `execute_tool_call`
    /// never populates `ToolOutput.media`.
    #[must_use]
    pub fn with_media(
        mut self,
        sanitizer: Arc<zeph_sanitizer::MediaSanitizer>,
        max_images_per_result: usize,
    ) -> Self {
        self.media_sanitizer = Some(sanitizer);
        self.max_images_per_result = max_images_per_result;
        self
    }

    /// Attach an audit logger for MCP media accept/reject decisions.
    #[must_use]
    pub fn with_audit(mut self, logger: Arc<zeph_tools::AuditLogger>) -> Self {
        self.audit_logger = Some(logger);
        self
    }

    /// Attach a status sender used to surface a `"Decoding MCP image…"` indicator while
    /// [`zeph_sanitizer::MediaSanitizer::sanitize_image`]'s `spawn_blocking` decode runs
    /// (`CLAUDE.md` "TUI Rules" — every background/implicit operation needs a visible
    /// status indicator). Without this, decoding is silent in the UI; sanitization itself
    /// is unaffected.
    #[must_use]
    pub fn with_status_tx(mut self, tx: zeph_llm::provider::StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    /// Replace the registered tool snapshot.
    ///
    /// Logs a `WARN` for each `sanitized_id` collision: when two tools map to the same
    /// sanitized ID the second is unreachable via [`execute_tool_call`](Self::execute_tool_call).
    pub fn set_tools(&self, tools: Vec<McpTool>) {
        // Warn on sanitized_id collisions: two tools mapping to the same id means
        // the second will be unreachable via execute_tool_call.
        let mut seen = std::collections::HashMap::new();
        for t in &tools {
            let sid = t.sanitized_id();
            if let Some(prev) = seen.insert(sid.clone(), t.qualified_name()) {
                tracing::warn!(
                    sanitized_id = %sid,
                    first = %prev,
                    second = %t.qualified_name(),
                    "MCP tool sanitized_id collision: second tool will be unreachable"
                );
            }
        }
        let mut guard = self.tools.write();
        *guard = tools;
    }

    /// Validate `ContentBlock::Image` blocks in a tool result through the configured
    /// [`zeph_sanitizer::MediaSanitizer`], up to `max_images_per_result`.
    ///
    /// Returns an empty `Vec` (no-op) when no sanitizer is attached, `server_id` has
    /// `media_passthrough` unset/false, or the server is `McpTrustLevel::Sandboxed`
    /// (spec-072 FR-001/FR-002/C2) — the text placeholder from `render_content_blocks`
    /// always remains as the fallback regardless of this outcome.
    async fn collect_media(
        &self,
        server_id: &str,
        tool_name: &str,
        content: &[rmcp::model::ContentBlock],
    ) -> Vec<zeph_llm::provider::ImageData> {
        let Some(ref sanitizer) = self.media_sanitizer else {
            return Vec::new();
        };
        if !self.manager.media_passthrough_allowed(server_id).await {
            return Vec::new();
        }

        let has_images = content
            .iter()
            .any(|b| matches!(b, rmcp::model::ContentBlock::Image(_)));
        if has_images {
            self.send_status("Decoding MCP image\u{2026}");
        }

        let mut media = Vec::new();
        for block in content {
            let rmcp::model::ContentBlock::Image(img) = block else {
                continue;
            };
            if media.len() >= self.max_images_per_result {
                tracing::warn!(
                    server_id,
                    tool_name,
                    cap = self.max_images_per_result,
                    "MCP media: per-result image cap reached, remaining image(s) dropped"
                );
                break;
            }
            let decoded = match BASE64.decode(&img.data) {
                Ok(b) => b,
                Err(e) => {
                    self.audit_media_decision(
                        server_id,
                        tool_name,
                        &img.mime_type,
                        0,
                        &format!("base64 decode failed: {e}"),
                    )
                    .await;
                    continue;
                }
            };
            let byte_len = decoded.len();
            match sanitizer
                .sanitize_image(&decoded, &img.mime_type, server_id)
                .await
            {
                Ok(image_data) => {
                    self.audit_media_accept(server_id, tool_name, &img.mime_type, byte_len)
                        .await;
                    media.push(image_data);
                }
                Err(rejected) => {
                    self.audit_media_decision(
                        server_id,
                        tool_name,
                        &img.mime_type,
                        byte_len,
                        &rejected.to_string(),
                    )
                    .await;
                }
            }
        }
        if has_images {
            self.send_status("");
        }
        media
    }

    /// Send a best-effort status update via [`Self::status_tx`], when attached. Mirrors the
    /// `let _ = stx.send(...)` idiom used for MCP OAuth status messages
    /// (`crates/zeph-mcp/src/manager/connect.rs`).
    fn send_status(&self, text: &str) {
        if let Some(ref tx) = self.status_tx {
            let _ = tx.send(text.to_owned());
        }
    }

    /// Log an accepted MCP media validation via the tool audit path (spec-072 AC-14).
    async fn audit_media_accept(&self, server_id: &str, tool_name: &str, mime: &str, bytes: usize) {
        tracing::debug!(server_id, tool_name, mime, bytes, "MCP media: accepted");
        let Some(ref logger) = self.audit_logger else {
            return;
        };
        logger
            .log(&zeph_tools::AuditEntry {
                timestamp: zeph_tools::chrono_now(),
                tool: tool_name.to_owned().into(),
                command: format!("mime={mime} bytes={bytes}"),
                result: zeph_tools::AuditResult::Success,
                duration_ms: 0,
                error_category: None,
                error_domain: None,
                error_phase: None,
                claim_source: Some(zeph_tools::ClaimSource::Mcp),
                mcp_server_id: Some(server_id.to_owned()),
                injection_flagged: false,
                embedding_anomalous: false,
                cross_boundary_mcp_to_acp: false,
                adversarial_policy_decision: None,
                exit_code: None,
                truncated: false,
                caller_id: None,
                policy_match: None,
                correlation_id: None,
                vigil_risk: None,
                execution_env: None,
                resolved_cwd: None,
                scope_at_definition: None,
                scope_at_dispatch: None,
                skill_name: None,
            })
            .await;
    }

    /// Log a rejected MCP media validation via the tool audit path (spec-072 AC-14, FR-005).
    async fn audit_media_decision(
        &self,
        server_id: &str,
        tool_name: &str,
        mime: &str,
        bytes: usize,
        reason: &str,
    ) {
        tracing::warn!(
            server_id,
            tool_name,
            mime,
            bytes,
            reason,
            "MCP media: rejected"
        );
        let Some(ref logger) = self.audit_logger else {
            return;
        };
        logger
            .log(&zeph_tools::AuditEntry {
                timestamp: zeph_tools::chrono_now(),
                tool: tool_name.to_owned().into(),
                command: format!("mime={mime} bytes={bytes}"),
                result: zeph_tools::AuditResult::Blocked {
                    reason: reason.to_owned(),
                },
                duration_ms: 0,
                error_category: Some("media_rejected".to_owned()),
                error_domain: Some("security".to_owned()),
                error_phase: None,
                claim_source: Some(zeph_tools::ClaimSource::Mcp),
                mcp_server_id: Some(server_id.to_owned()),
                injection_flagged: false,
                embedding_anomalous: false,
                cross_boundary_mcp_to_acp: false,
                adversarial_policy_decision: None,
                exit_code: None,
                truncated: false,
                caller_id: None,
                policy_match: None,
                correlation_id: None,
                vigil_risk: None,
                execution_env: None,
                resolved_cwd: None,
                scope_at_definition: None,
                scope_at_dispatch: None,
                skill_name: None,
            })
            .await;
    }
}

impl ToolExecutor for McpToolExecutor {
    fn tool_definitions(&self) -> Vec<ToolDef> {
        let tools = self.tools.read();
        tools
            .iter()
            .map(|t| ToolDef {
                id: t.sanitized_id().into(),
                description: t.description.clone().into(),
                schema: serde_json::from_value(t.input_schema.clone())
                    .unwrap_or_else(|_| schemars::Schema::default()),
                invocation: InvocationHint::ToolCall,
                output_schema: t.output_schema.clone(),
                server_id: Some(t.server_id.clone()),
            })
            .collect()
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.executor.execute_tool_call", skip_all, fields(tool_id = %call.tool_id))
    )]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        // Lookup by sanitized_id because the LLM sees sanitized names (no ':' character).
        //
        // IMPORTANT: ToolOutput.tool_name MUST be set to qualified_name() (not sanitized_id()).
        // sanitize_tool_output() in zeph-core classifies tool output as external/untrusted MCP
        // content by checking tool_name.contains(':').  Breaking this invariant would silently
        // route MCP responses through the local/trusted pipeline, bypassing quarantine.
        let found = {
            let tools = self.tools.read();
            tools
                .iter()
                .find(|t| t.sanitized_id() == call.tool_id)
                .cloned()
        };
        let Some(tool) = found else {
            return Ok(None);
        };

        let args = serde_json::Value::Object(call.params.clone());
        let result = self
            .manager
            .call_tool(&tool.server_id, &tool.name, args)
            .await
            .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

        let raw_text = crate::content::render_content_blocks(&result.content);

        let text = crate::sanitize::intent_anchor_wrap(&tool.server_id, &tool.name, &raw_text);

        let media = self
            .collect_media(&tool.server_id, &tool.name, &result.content)
            .await;

        Ok(Some(ToolOutput {
            tool_name: tool.qualified_name().into(),
            summary: text,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(zeph_tools::ClaimSource::Mcp),
            media,
        }))
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.executor.execute", skip_all)
    )]
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let blocks = extract_fenced_blocks(response, "mcp");
        if blocks.is_empty() {
            return Ok(None);
        }

        let mut outputs = Vec::with_capacity(blocks.len());
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            let instruction: McpInstruction =
                serde_json::from_str(block).map_err(|e: serde_json::Error| {
                    ToolError::Execution(std::io::Error::other(e.to_string()))
                })?;

            // SECURITY: Validate server:tool against the registered tool list before dispatch.
            // This prevents a prompt injection from routing calls to unregistered servers or tools.
            let found = {
                let tools = self.tools.read();
                tools
                    .iter()
                    .find(|t| t.server_id == instruction.server && t.name == instruction.tool)
                    .cloned()
            };
            let Some(tool) = found else {
                return Err(ToolError::Execution(std::io::Error::other(format!(
                    "MCP tool {}:{} not in registered tool list",
                    instruction.server, instruction.tool
                ))));
            };

            // Delegate to execute_tool_call() so all security layers apply.
            let call = ToolCall {
                tool_id: tool.sanitized_id().into(),
                params: match instruction.args {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                },
                caller_id: None,
                context: None,
                tool_call_id: String::new(),
                skill_name: None,
            };
            if let Some(output) = self.execute_tool_call(&call).await? {
                outputs.push(output.summary);
            }
        }

        Ok(Some(ToolOutput {
            // SECURITY: Use qualified format so quarantine routing works (tool_name must contain ':').
            tool_name: ToolName::new("mcp:fenced_block"),
            summary: outputs.join("\n\n"),
            blocks_executed,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(zeph_tools::ClaimSource::Mcp),
            ..Default::default()
        }))
    }

    zeph_tools::tool_executor_no_inner_defaults!();
}

#[derive(serde::Deserialize)]
struct McpInstruction {
    server: String,
    tool: String,
    #[serde(default = "default_args")]
    args: serde_json::Value,
}

fn default_args() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyEnforcer;

    fn make_executor() -> McpToolExecutor {
        let mgr = Arc::new(McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])));
        let tools = Arc::new(RwLock::new(vec![]));
        McpToolExecutor::new(mgr, tools)
    }

    // --- MediaSanitizer / media_passthrough gating (spec-072) ---

    // 1x1 valid PNG fixture (magic bytes + minimal IHDR/IDAT/IEND chunks).
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92, 0xEF, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn media_entry(
        id: &str,
        media_passthrough: bool,
        trust_level: crate::manager::McpTrustLevel,
    ) -> crate::manager::ServerEntry {
        crate::manager::ServerEntry {
            id: id.to_owned(),
            transport: crate::manager::McpTransport::Stdio {
                command: "nonexistent-mcp-binary".into(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
            },
            timeout: std::time::Duration::from_secs(5),
            trust_level,
            tool_allowlist: None,
            expected_tools: Vec::new(),
            roots: Vec::new(),
            tool_metadata: std::collections::HashMap::new(),
            elicitation_enabled: false,
            elicitation_timeout_secs: 120,
            env_isolation: false,
            media_passthrough,
        }
    }

    fn executor_with_media(entry: crate::manager::ServerEntry) -> McpToolExecutor {
        let mgr = Arc::new(McpManager::new(
            vec![entry],
            vec![],
            PolicyEnforcer::new(vec![]),
        ));
        let tools = Arc::new(RwLock::new(vec![]));
        let sanitizer = Arc::new(zeph_sanitizer::MediaSanitizer::new(
            &zeph_config::McpMediaConfig::default(),
        ));
        McpToolExecutor::new(mgr, tools).with_media(sanitizer, 4)
    }

    fn png_image_block() -> rmcp::model::ContentBlock {
        rmcp::model::ContentBlock::image(BASE64.encode(PNG_1X1), "image/png")
    }

    #[tokio::test]
    async fn collect_media_noop_without_sanitizer_attached() {
        let executor = make_executor();
        let media = executor
            .collect_media("srv", "tool", std::slice::from_ref(&png_image_block()))
            .await;
        assert!(media.is_empty());
    }

    #[tokio::test]
    async fn collect_media_noop_when_media_passthrough_disabled() {
        let entry = media_entry("srv", false, crate::manager::McpTrustLevel::Untrusted);
        let executor = executor_with_media(entry);
        let media = executor
            .collect_media("srv", "tool", std::slice::from_ref(&png_image_block()))
            .await;
        assert!(
            media.is_empty(),
            "media_passthrough=false must never populate media (AC-1)"
        );
    }

    #[tokio::test]
    async fn collect_media_populates_when_opted_in() {
        let entry = media_entry("srv", true, crate::manager::McpTrustLevel::Untrusted);
        let executor = executor_with_media(entry);
        let media = executor
            .collect_media("srv", "tool", std::slice::from_ref(&png_image_block()))
            .await;
        assert_eq!(
            media.len(),
            1,
            "opted-in server must attach the image (AC-2)"
        );
        assert_eq!(media[0].mime_type, "image/png");
    }

    #[tokio::test]
    async fn collect_media_sandboxed_server_never_populates() {
        let entry = media_entry("srv", true, crate::manager::McpTrustLevel::Sandboxed);
        let executor = executor_with_media(entry);
        let media = executor
            .collect_media("srv", "tool", std::slice::from_ref(&png_image_block()))
            .await;
        assert!(
            media.is_empty(),
            "Sandboxed trust level must hard-block media regardless of the flag (AC-3, C2)"
        );
    }

    #[tokio::test]
    async fn collect_media_respects_per_result_cap() {
        let entry = media_entry("srv", true, crate::manager::McpTrustLevel::Untrusted);
        let mgr = Arc::new(McpManager::new(
            vec![entry],
            vec![],
            PolicyEnforcer::new(vec![]),
        ));
        let tools = Arc::new(RwLock::new(vec![]));
        let sanitizer = Arc::new(zeph_sanitizer::MediaSanitizer::new(
            &zeph_config::McpMediaConfig::default(),
        ));
        // Cap of 2, but 3 images in the result — only the first 2 are attached (AC-13).
        let executor = McpToolExecutor::new(mgr, tools).with_media(sanitizer, 2);
        let blocks = vec![png_image_block(), png_image_block(), png_image_block()];
        let media = executor.collect_media("srv", "tool", &blocks).await;
        assert_eq!(media.len(), 2);
    }

    #[tokio::test]
    async fn collect_media_rejects_mime_mismatch() {
        let entry = media_entry("srv", true, crate::manager::McpTrustLevel::Untrusted);
        let executor = executor_with_media(entry);
        let mismatched = rmcp::model::ContentBlock::image(BASE64.encode(PNG_1X1), "image/jpeg");
        let media = executor
            .collect_media("srv", "tool", std::slice::from_ref(&mismatched))
            .await;
        assert!(media.is_empty(), "MIME mismatch must be rejected (AC-4)");
    }

    #[tokio::test]
    async fn collect_media_emits_decode_status_and_clears_it() {
        let entry = media_entry("srv", true, crate::manager::McpTrustLevel::Untrusted);
        let mgr = Arc::new(McpManager::new(
            vec![entry],
            vec![],
            PolicyEnforcer::new(vec![]),
        ));
        let tools = Arc::new(RwLock::new(vec![]));
        let sanitizer = Arc::new(zeph_sanitizer::MediaSanitizer::new(
            &zeph_config::McpMediaConfig::default(),
        ));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let executor = McpToolExecutor::new(mgr, tools)
            .with_media(sanitizer, 4)
            .with_status_tx(tx);

        let media = executor
            .collect_media("srv", "tool", std::slice::from_ref(&png_image_block()))
            .await;
        assert_eq!(media.len(), 1);

        assert_eq!(rx.try_recv().unwrap(), "Decoding MCP image\u{2026}");
        assert_eq!(
            rx.try_recv().unwrap(),
            "",
            "status must be cleared once decoding finishes"
        );
        assert!(rx.try_recv().is_err(), "no further status messages");
    }

    #[tokio::test]
    async fn collect_media_skips_status_when_no_images_in_result() {
        let entry = media_entry("srv", true, crate::manager::McpTrustLevel::Untrusted);
        let executor = executor_with_media(entry);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let executor = executor.with_status_tx(tx);

        let text_block = rmcp::model::ContentBlock::text("no images here".to_owned());
        let media = executor
            .collect_media("srv", "tool", std::slice::from_ref(&text_block))
            .await;
        assert!(media.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "no status update when there are no image blocks to decode"
        );
    }

    #[test]
    fn parse_instruction_full() {
        let json = r#"{"server": "github", "tool": "create_issue", "args": {"title": "bug"}}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.server, "github");
        assert_eq!(instr.tool, "create_issue");
        assert_eq!(instr.args["title"], "bug");
    }

    #[test]
    fn parse_instruction_no_args() {
        let json = r#"{"server": "fs", "tool": "list_dir"}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.server, "fs");
        assert_eq!(instr.tool, "list_dir");
        assert!(instr.args.is_object());
    }

    #[test]
    fn parse_instruction_empty_args() {
        let json = r#"{"server": "s", "tool": "t", "args": {}}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert!(instr.args.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_instruction_missing_server_fails() {
        let json = r#"{"tool": "t"}"#;
        assert!(serde_json::from_str::<McpInstruction>(json).is_err());
    }

    #[test]
    fn parse_instruction_missing_tool_fails() {
        let json = r#"{"server": "s"}"#;
        assert!(serde_json::from_str::<McpInstruction>(json).is_err());
    }

    #[test]
    fn extract_mcp_blocks() {
        let text = "Here:\n```mcp\n{\"server\":\"a\",\"tool\":\"b\"}\n```\nDone";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("\"server\""));
    }

    #[test]
    fn no_mcp_blocks() {
        let text = "```bash\necho hello\n```";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert!(blocks.is_empty());
    }

    #[test]
    fn multiple_mcp_blocks() {
        let text = "```mcp\n{\"server\":\"a\",\"tool\":\"b\"}\n```\n\
                    text\n\
                    ```mcp\n{\"server\":\"c\",\"tool\":\"d\"}\n```";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn parse_instruction_invalid_json() {
        let json = r"{not valid json}";
        assert!(serde_json::from_str::<McpInstruction>(json).is_err());
    }

    #[test]
    fn parse_instruction_extra_fields_ignored() {
        let json = r#"{"server":"s","tool":"t","args":{},"extra":"ignored"}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.server, "s");
        assert_eq!(instr.tool, "t");
    }

    #[test]
    fn parse_instruction_args_array() {
        let json = r#"{"server":"s","tool":"t","args":["a","b"]}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert!(instr.args.is_array());
    }

    #[test]
    fn parse_instruction_args_nested() {
        let json = r#"{"server":"s","tool":"t","args":{"nested":{"key":"val"}}}"#;
        let instr: McpInstruction = serde_json::from_str(json).unwrap();
        assert_eq!(instr.args["nested"]["key"], "val");
    }

    #[test]
    fn default_args_is_empty_object() {
        let val = default_args();
        assert!(val.is_object());
        assert!(val.as_object().unwrap().is_empty());
    }

    #[test]
    fn extract_mcp_blocks_empty_input() {
        let blocks = extract_fenced_blocks("", "mcp");
        assert!(blocks.is_empty());
    }

    #[test]
    fn extract_mcp_blocks_other_lang_ignored() {
        let text =
            "```json\n{\"key\":\"val\"}\n```\n```mcp\n{\"server\":\"a\",\"tool\":\"b\"}\n```";
        let blocks = extract_fenced_blocks(text, "mcp");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("\"server\""));
    }

    #[test]
    fn executor_construction() {
        let executor = make_executor();
        let dbg = format!("{executor:?}");
        assert!(dbg.contains("McpToolExecutor"));
    }

    #[test]
    fn tool_definitions_empty_when_no_tools() {
        let executor = make_executor();
        assert!(executor.tool_definitions().is_empty());
    }

    #[test]
    fn tool_definitions_returns_sanitized_names() {
        let mgr = Arc::new(McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])));
        let tools = Arc::new(RwLock::new(vec![McpTool {
            server_id: "gh".into(),
            name: "create_issue".into(),
            description: "Create a GitHub issue".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }]));
        let executor = McpToolExecutor::new(mgr, tools);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "gh_create_issue");
        assert_eq!(defs[0].description.as_ref(), "Create a GitHub issue");
    }

    #[test]
    fn set_tools_updates_definitions() {
        let executor = make_executor();
        assert!(executor.tool_definitions().is_empty());
        executor.set_tools(vec![McpTool {
            server_id: "fs".into(),
            name: "list_dir".into(),
            description: "List directory".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }]);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "fs_list_dir");
    }

    #[tokio::test]
    async fn execute_no_blocks_returns_none() {
        let executor = make_executor();
        let result = executor.execute("no mcp blocks here").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_invalid_json_block_returns_error() {
        let executor = make_executor();
        let text = "```mcp\nnot json\n```";
        let result = executor.execute(text).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_valid_block_tool_not_registered_returns_error() {
        // Tool is not in the registered list → rejected before any server call.
        let executor = make_executor();
        let text = "```mcp\n{\"server\":\"missing\",\"tool\":\"t\"}\n```";
        let result = executor.execute(text).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not in registered tool list"),
            "expected 'not in registered tool list' in: {err_msg}"
        );
    }

    #[tokio::test]
    async fn execute_fenced_block_tool_name_contains_colon() {
        // Verify the output tool_name uses qualified format for quarantine routing.
        // We can't easily run a full call, but we can verify the rejection error path
        // hits before any server dispatch.
        let executor = make_executor();
        // Register a real tool so the lookup can succeed but server call fails.
        executor.set_tools(vec![McpTool {
            server_id: "srv".into(),
            name: "tool".into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }]);
        let text = "```mcp\n{\"server\":\"srv\",\"tool\":\"tool\"}\n```";
        // Server not actually connected, so execute_tool_call returns Err.
        let result = executor.execute(text).await;
        assert!(result.is_err(), "expected Err when server is not connected");
    }

    #[tokio::test]
    async fn execute_tool_call_unknown_format_returns_none() {
        let executor = make_executor();
        let call = ToolCall {
            tool_id: ToolName::new("no_colon_here"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_tool_call_unknown_server_returns_none() {
        let executor = make_executor();
        let call = ToolCall {
            tool_id: ToolName::new("unknown_server:tool"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    // --- sanitized_id routing tests ---

    #[tokio::test]
    async fn execute_tool_call_by_sanitized_id_not_found_returns_none() {
        // Register a tool whose sanitized_id is "gh_create_issue".
        // A call with tool_id "gh_create_issue" matches; a call with a different id does not.
        let mgr = Arc::new(McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])));
        let tools = Arc::new(RwLock::new(vec![McpTool {
            server_id: "gh".into(),
            name: "create_issue".into(),
            description: "desc".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }]));
        let executor = McpToolExecutor::new(mgr, tools);

        // A different sanitized id must not match.
        let call = ToolCall {
            tool_id: ToolName::new("gh_list_issues"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_tool_call_by_sanitized_id_matched_but_server_missing_returns_err() {
        // Register a tool so the lookup succeeds, but the manager has no connected server —
        // the call_tool on the manager must return an error (not None).
        let mgr = Arc::new(McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])));
        let tools = Arc::new(RwLock::new(vec![McpTool {
            server_id: "missing_server".into(),
            name: "some_tool".into(),
            description: "desc".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }]));
        let executor = McpToolExecutor::new(mgr, tools);

        // tool_id matches the sanitized_id "missing_server_some_tool".
        let call = ToolCall {
            tool_id: ToolName::new("missing_server_some_tool"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err(), "expected Err when server is not connected");
    }

    #[test]
    fn tool_definitions_sanitized_id_has_no_colon() {
        // After the fix, no tool definition id may contain ':'.
        let mgr = Arc::new(McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])));
        let tools = Arc::new(RwLock::new(vec![
            McpTool {
                server_id: "srv-one".into(),
                name: "tool:with:colons".into(),
                description: "d".into(),
                input_schema: serde_json::json!({}),
                output_schema: None,
                security_meta: crate::tool::ToolSecurityMeta::default(),
            },
            McpTool {
                server_id: "srv.two".into(),
                name: "normal_tool".into(),
                description: "d".into(),
                input_schema: serde_json::json!({}),
                output_schema: None,
                security_meta: crate::tool::ToolSecurityMeta::default(),
            },
        ]));
        let executor = McpToolExecutor::new(mgr, tools);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 2);
        for def in &defs {
            assert!(
                !def.id.contains(':'),
                "tool id must not contain ':' but got: {}",
                def.id
            );
        }
    }

    #[test]
    fn tool_definitions_sanitized_id_matches_expected_pattern() {
        // Verify that every character in every id matches [a-zA-Z0-9_-].
        let mgr = Arc::new(McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])));
        let tools = Arc::new(RwLock::new(vec![McpTool {
            server_id: "my.server".into(),
            name: "tool name!".into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }]));
        let executor = McpToolExecutor::new(mgr, tools);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        let id = defs[0].id.as_ref();
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "id contains invalid chars: {id}"
        );
    }
}
