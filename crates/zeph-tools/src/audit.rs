// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structured JSONL audit logging for tool invocations.
//!
//! Every tool execution produces an [`AuditEntry`] that is serialized as a newline-delimited
//! JSON record and written to the configured destination (stdout or a file).
//!
//! # Configuration
//!
//! Audit logging is controlled by [`AuditConfig`]. When
//! `destination` is `"stdout"`, entries are emitted via `tracing::info!(target: "audit", ...)`.
//! Any other value is treated as a file path opened in append mode.
//!
//! # Security note
//!
//! Audit entries intentionally omit the raw cosine distance from anomaly detection
//! (`embedding_anomalous` is a boolean flag) to prevent threshold reverse-engineering.

use std::path::Path;

use zeph_common::ToolName;

use crate::config::AuditConfig;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

/// Outbound network call record emitted by HTTP-capable executors.
///
/// Serialized as a JSON Lines record onto the shared audit sink. Consumers
/// distinguish this record from [`AuditEntry`] by the presence of the `kind`
/// field (always `"egress"`).
///
/// # Example JSON output
///
/// ```json
/// {"timestamp":"1712345678","kind":"egress","correlation_id":"a1b2c3d4-...","tool":"fetch",
///  "url":"https://example.com","host":"example.com","method":"GET","status":200,
///  "duration_ms":120,"response_bytes":4096}
/// ```
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressEvent {
    /// Unix timestamp (seconds) when the request was issued.
    pub timestamp: String,
    /// Record-type discriminator — always `"egress"`. Consumers distinguish
    /// `EgressEvent` from `AuditEntry` by the presence of this field.
    pub kind: &'static str,
    /// Correlation id shared with the parent [`AuditEntry`] (`UUIDv4`, lowercased).
    pub correlation_id: String,
    /// Tool that issued the call (`"web_scrape"`, `"fetch"`, …).
    pub tool: ToolName,
    /// Destination URL (after SSRF/domain validation).
    pub url: String,
    /// Hostname, denormalized for TUI aggregation.
    pub host: String,
    /// HTTP method (`"GET"`, `"POST"`, …).
    pub method: String,
    /// HTTP response status. `None` when the request failed pre-response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Wall-clock duration from send to end-of-body, in milliseconds.
    pub duration_ms: u64,
    /// Bytes of response body received. Zero on pre-response failure or
    /// when `log_response_bytes = false`.
    pub response_bytes: usize,
    /// Whether the request was blocked before connection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
    /// Block reason: `"allowlist"` | `"blocklist"` | `"ssrf"` | `"scheme"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<&'static str>,
    /// Caller identity propagated from `ToolCall::caller_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_id: Option<String>,
    /// Redirect hop index (0 for the initial request). Distinguishes per-hop events
    /// sharing the same `correlation_id`.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub hop: u8,
}

impl EgressEvent {
    /// Generate a new `UUIDv4` correlation id for use across a tool call's egress events.
    #[must_use]
    pub fn new_correlation_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// Async writer that appends [`AuditEntry`] records to a structured JSONL log.
///
/// Create via [`AuditLogger::from_config`] and share behind an `Arc`. Each executor
/// that should emit audit records accepts the logger via a builder method
/// (e.g. [`ShellExecutor::with_audit`](crate::ShellExecutor::with_audit)).
///
/// # Thread safety
///
/// File writes are serialized through an internal `tokio::sync::Mutex<File>`.
/// Multiple concurrent log calls are safe but may block briefly on the mutex.
#[derive(Debug)]
pub struct AuditLogger {
    destination: AuditDestination,
}

#[derive(Debug)]
enum AuditDestination {
    Stdout,
    File(tokio::sync::Mutex<tokio::fs::File>),
}

/// A single tool invocation record written to the audit log.
///
/// Serialized as a flat JSON object (newline-terminated). Optional fields are omitted
/// when `None` or `false` to keep entries compact.
///
/// # Example JSON output
///
/// ```json
/// {"timestamp":"1712345678","tool":"shell","command":"ls -la","result":{"type":"success"},
///  "duration_ms":12,"exit_code":0,"claim_source":"shell"}
/// ```
#[derive(serde::Serialize)]
#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
pub struct AuditEntry {
    /// Unix timestamp (seconds) when the tool invocation started.
    pub timestamp: String,
    /// Tool identifier (e.g. `"shell"`, `"web_scrape"`, `"fetch"`).
    pub tool: ToolName,
    /// Human-readable command or URL being invoked.
    pub command: String,
    /// Outcome of the invocation.
    pub result: AuditResult,
    /// Wall-clock duration from invocation start to completion, in milliseconds.
    pub duration_ms: u64,
    /// Fine-grained error category label from the taxonomy. `None` for successful executions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
    /// High-level error domain for recovery dispatch. `None` for successful executions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_domain: Option<String>,
    /// Invocation phase in which the error occurred per arXiv:2601.16280 taxonomy.
    /// `None` for successful executions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_phase: Option<String>,
    /// Provenance of the tool result. `None` for non-executor audit entries (e.g. policy checks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_source: Option<crate::executor::ClaimSource>,
    /// MCP server ID for tool calls routed through `McpToolExecutor`. `None` for native tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server_id: Option<String>,
    /// Tool output was flagged by regex injection detection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub injection_flagged: bool,
    /// Tool output was flagged as anomalous by the embedding guard.
    /// Raw cosine distance is NOT stored (prevents threshold reverse-engineering).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub embedding_anomalous: bool,
    /// Tool result crossed the MCP-to-ACP trust boundary (MCP tool result served to an ACP client).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cross_boundary_mcp_to_acp: bool,
    /// Decision recorded by the adversarial policy agent before execution.
    ///
    /// Values: `"allow"`, `"deny:<reason>"`, `"error:<message>"`.
    /// `None` when adversarial policy is disabled or not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adversarial_policy_decision: Option<String>,
    /// Process exit code for shell tool executions. `None` for non-shell tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether tool output was truncated before storage. Default false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Caller identity that initiated this tool call. `None` for system calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_id: Option<String>,
    /// Policy rule trace that matched this tool call. Populated from `PolicyDecision::trace`.
    /// `None` when policy is disabled or this entry is not from a policy check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_match: Option<String>,
    /// Correlation id shared with any associated [`EgressEvent`] emitted during this
    /// tool call. Generated at `execute_tool_call` entry. `None` for policy-only or
    /// rollback entries that do not map to a network-capable tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// VIGIL risk level when the pre-sanitizer gate flagged this tool output.
    /// `None` when VIGIL did not fire (output was clean or tool was exempt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vigil_risk: Option<VigilRiskLevel>,
    /// Name of the resolved execution environment (from `[[execution.environments]]`).
    /// `None` when no named environment was selected for this invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_env: Option<String>,
    /// Canonical absolute working directory actually used for this shell invocation.
    /// `None` for non-shell tools or legacy path without a resolved context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_cwd: Option<String>,
    /// Name of the capability scope active at `tool_definitions()` time (for scope-at-definition audit).
    /// `None` when `ScopedToolExecutor` is not in the chain or the scope is the identity (`general`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_at_definition: Option<String>,
    /// Name of the capability scope active at `execute_tool_call()` dispatch time.
    /// `None` when `ScopedToolExecutor` is not in the chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_at_dispatch: Option<String>,
}

/// Risk level assigned by the VIGIL pre-sanitizer gate to a flagged tool output.
///
/// Emitted in [`AuditEntry::vigil_risk`] when VIGIL fires.
/// Colocated with `AuditEntry` so the audit JSONL schema is self-contained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VigilRiskLevel {
    /// Reserved for future use: heuristic match below the primary threshold.
    Low,
    /// Single-pattern match in non-strict mode.
    Medium,
    /// ≥2 distinct pattern categories OR `strict_mode = true`.
    High,
}

/// Outcome of a tool invocation, serialized as a tagged JSON object.
///
/// The `type` field selects the variant; additional fields are present only for the
/// relevant variants.
///
/// # Serialization
///
/// ```json
/// {"type":"success"}
/// {"type":"blocked","reason":"sudo"}
/// {"type":"error","message":"exec failed"}
/// {"type":"timeout"}
/// {"type":"rollback","restored":3,"deleted":1}
/// ```
#[derive(serde::Serialize)]
#[serde(tag = "type")]
pub enum AuditResult {
    /// The tool executed successfully.
    #[serde(rename = "success")]
    Success,
    /// The tool invocation was blocked by policy before execution.
    #[serde(rename = "blocked")]
    Blocked {
        /// The matched blocklist pattern or policy rule that triggered the block.
        reason: String,
    },
    /// The tool attempted execution but failed with an error.
    #[serde(rename = "error")]
    Error {
        /// Human-readable error description.
        message: String,
    },
    /// The tool exceeded its configured timeout.
    #[serde(rename = "timeout")]
    Timeout,
    /// A transactional rollback was performed after a failed execution.
    #[serde(rename = "rollback")]
    Rollback {
        /// Number of files restored to their pre-execution snapshot.
        restored: usize,
        /// Number of newly-created files that were deleted during rollback.
        deleted: usize,
    },
}

impl AuditLogger {
    /// Create a new `AuditLogger` from config.
    ///
    /// When `tui_mode` is `true` and `config.destination` is `"stdout"`, the
    /// destination is redirected to a file (`audit.jsonl` in the current directory)
    /// to avoid corrupting the TUI output with raw JSON lines.
    ///
    /// # Errors
    ///
    /// Returns an error if a file destination cannot be opened.
    #[allow(clippy::unused_async)]
    pub async fn from_config(config: &AuditConfig, tui_mode: bool) -> Result<Self, std::io::Error> {
        let effective_dest = if tui_mode && config.destination == "stdout" {
            tracing::warn!("TUI mode: audit stdout redirected to file audit.jsonl");
            "audit.jsonl".to_owned()
        } else {
            config.destination.clone()
        };

        let destination = if effective_dest == "stdout" {
            AuditDestination::Stdout
        } else {
            let std_file = zeph_common::fs_secure::append_private(Path::new(&effective_dest))?;
            let file = tokio::fs::File::from_std(std_file);
            AuditDestination::File(tokio::sync::Mutex::new(file))
        };

        Ok(Self { destination })
    }

    /// Serialize `entry` to JSON and append it to the configured destination.
    ///
    /// Serialization errors are logged via `tracing::error!` and silently swallowed so
    /// that audit failures never interrupt tool execution.
    pub async fn log(&self, entry: &AuditEntry) {
        let json = match serde_json::to_string(entry) {
            Ok(j) => j,
            Err(err) => {
                tracing::error!("audit entry serialization failed: {err}");
                return;
            }
        };

        match &self.destination {
            AuditDestination::Stdout => {
                tracing::info!(target: "audit", "{json}");
            }
            AuditDestination::File(file) => {
                use tokio::io::AsyncWriteExt;
                let mut f = file.lock().await;
                let line = format!("{json}\n");
                if let Err(e) = f.write_all(line.as_bytes()).await {
                    tracing::error!("failed to write audit log: {e}");
                } else if let Err(e) = f.flush().await {
                    tracing::error!("failed to flush audit log: {e}");
                }
            }
        }
    }

    /// Serialize an [`EgressEvent`] onto the same JSONL destination as [`AuditEntry`].
    ///
    /// Ordering with respect to [`AuditLogger::log`] is preserved by the shared
    /// `tokio::sync::Mutex<File>` that serializes all writes on the same destination.
    ///
    /// Serialization errors are logged via `tracing::error!` and silently swallowed
    /// so that egress logging failures never interrupt tool execution.
    pub async fn log_egress(&self, event: &EgressEvent) {
        let json = match serde_json::to_string(event) {
            Ok(j) => j,
            Err(err) => {
                tracing::error!("egress event serialization failed: {err}");
                return;
            }
        };

        match &self.destination {
            AuditDestination::Stdout => {
                tracing::info!(target: "audit", "{json}");
            }
            AuditDestination::File(file) => {
                use tokio::io::AsyncWriteExt;
                let mut f = file.lock().await;
                let line = format!("{json}\n");
                if let Err(e) = f.write_all(line.as_bytes()).await {
                    tracing::error!("failed to write egress log: {e}");
                } else if let Err(e) = f.flush().await {
                    tracing::error!("failed to flush egress log: {e}");
                }
            }
        }
    }
}

/// Log a per-tool risk summary at startup when `audit.tool_risk_summary = true`.
///
/// Each entry records tool name, privilege level (static mapping by tool id), and the
/// expected input sanitization method. This is a design-time inventory label —
/// NOT a runtime guarantee that sanitization is functioning correctly.
pub fn log_tool_risk_summary(tool_ids: &[&str]) {
    // Static privilege mapping: tool id prefix → (privilege level, expected sanitization).
    // "high" = can execute arbitrary OS commands; "medium" = network/filesystem access;
    // "low" = schema-validated parameters only.
    fn classify(id: &str) -> (&'static str, &'static str) {
        if id.starts_with("shell") || id == "bash" || id == "exec" {
            ("high", "env_blocklist + command_blocklist")
        } else if id.starts_with("web_scrape") || id == "fetch" || id.starts_with("scrape") {
            ("medium", "validate_url + SSRF + domain_policy")
        } else if id.starts_with("file_write")
            || id.starts_with("file_read")
            || id.starts_with("file")
        {
            ("medium", "path_sandbox")
        } else {
            ("low", "schema_only")
        }
    }

    for &id in tool_ids {
        let (privilege, sanitization) = classify(id);
        tracing::info!(
            tool = id,
            privilege_level = privilege,
            expected_sanitization = sanitization,
            "tool risk summary"
        );
    }
}

/// Returns the current Unix timestamp as a decimal string.
///
/// Used to populate [`AuditEntry::timestamp`]. Returns `"0"` if the system clock
/// is before the Unix epoch (which should never happen in practice).
#[must_use]
pub fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_entry_serialization() {
        let entry = AuditEntry {
            timestamp: "1234567890".into(),
            tool: "shell".into(),
            command: "echo hello".into(),
            result: AuditResult::Success,
            duration_ms: 42,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"success\""));
        assert!(json.contains("\"tool\":\"shell\""));
        assert!(json.contains("\"duration_ms\":42"));
    }

    #[test]
    fn audit_result_blocked_serialization() {
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "sudo rm".into(),
            result: AuditResult::Blocked {
                reason: "blocked command: sudo".into(),
            },
            duration_ms: 0,
            error_category: Some("policy_blocked".to_owned()),
            error_domain: Some("action".to_owned()),
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"blocked\""));
        assert!(json.contains("\"reason\""));
    }

    #[test]
    fn audit_result_error_serialization() {
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "bad".into(),
            result: AuditResult::Error {
                message: "exec failed".into(),
            },
            duration_ms: 0,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"error\""));
    }

    #[test]
    fn audit_result_timeout_serialization() {
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "sleep 999".into(),
            result: AuditResult::Timeout,
            duration_ms: 30000,
            error_category: Some("timeout".to_owned()),
            error_domain: Some("system".to_owned()),
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"timeout\""));
    }

    #[tokio::test]
    async fn audit_logger_stdout() {
        let config = AuditConfig {
            enabled: true,
            destination: "stdout".into(),
            ..Default::default()
        };
        let logger = AuditLogger::from_config(&config, false).await.unwrap();
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "echo test".into(),
            result: AuditResult::Success,
            duration_ms: 1,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        logger.log(&entry).await;
    }

    #[tokio::test]
    async fn audit_logger_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let config = AuditConfig {
            enabled: true,
            destination: path.display().to_string(),
            ..Default::default()
        };
        let logger = AuditLogger::from_config(&config, false).await.unwrap();
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "echo test".into(),
            result: AuditResult::Success,
            duration_ms: 1,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        logger.log(&entry).await;

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("\"tool\":\"shell\""));
    }

    #[tokio::test]
    async fn audit_logger_file_write_error_logged() {
        let config = AuditConfig {
            enabled: true,
            destination: "/nonexistent/dir/audit.log".into(),
            ..Default::default()
        };
        let result = AuditLogger::from_config(&config, false).await;
        assert!(result.is_err());
    }

    #[test]
    fn claim_source_serde_roundtrip() {
        use crate::executor::ClaimSource;
        let cases = [
            (ClaimSource::Shell, "\"shell\""),
            (ClaimSource::FileSystem, "\"file_system\""),
            (ClaimSource::WebScrape, "\"web_scrape\""),
            (ClaimSource::Mcp, "\"mcp\""),
            (ClaimSource::A2a, "\"a2a\""),
            (ClaimSource::CodeSearch, "\"code_search\""),
            (ClaimSource::Diagnostics, "\"diagnostics\""),
            (ClaimSource::Memory, "\"memory\""),
        ];
        for (variant, expected_json) in cases {
            let serialized = serde_json::to_string(&variant).unwrap();
            assert_eq!(serialized, expected_json, "serialize {variant:?}");
            let deserialized: ClaimSource = serde_json::from_str(&serialized).unwrap();
            assert_eq!(deserialized, variant, "deserialize {variant:?}");
        }
    }

    #[test]
    fn audit_entry_claim_source_none_omitted() {
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "echo".into(),
            result: AuditResult::Success,
            duration_ms: 1,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("claim_source"),
            "claim_source must be omitted when None: {json}"
        );
    }

    #[test]
    fn audit_entry_claim_source_some_present() {
        use crate::executor::ClaimSource;
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "echo".into(),
            result: AuditResult::Success,
            duration_ms: 1,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: Some(ClaimSource::Shell),
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains("\"claim_source\":\"shell\""),
            "expected claim_source=shell in JSON: {json}"
        );
    }

    #[tokio::test]
    async fn audit_logger_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let config = AuditConfig {
            enabled: true,
            destination: path.display().to_string(),
            ..Default::default()
        };
        let logger = AuditLogger::from_config(&config, false).await.unwrap();

        for i in 0..5 {
            let entry = AuditEntry {
                timestamp: i.to_string(),
                tool: "shell".into(),
                command: format!("cmd{i}"),
                result: AuditResult::Success,
                duration_ms: i,
                error_category: None,
                error_domain: None,
                error_phase: None,
                claim_source: None,
                mcp_server_id: None,
                injection_flagged: false,
                embedding_anomalous: false,
                cross_boundary_mcp_to_acp: false,
                adversarial_policy_decision: None,
                exit_code: None,
                truncated: false,
                policy_match: None,
                correlation_id: None,
                caller_id: None,
                vigil_risk: None,
                execution_env: None,
                resolved_cwd: None,
                scope_at_definition: None,
                scope_at_dispatch: None,
            };
            logger.log(&entry).await;
        }

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content.lines().count(), 5);
    }

    #[test]
    fn audit_entry_exit_code_serialized() {
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "shell".into(),
            command: "echo hi".into(),
            result: AuditResult::Success,
            duration_ms: 5,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: Some(0),
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains("\"exit_code\":0"),
            "exit_code must be serialized: {json}"
        );
    }

    #[test]
    fn audit_entry_exit_code_none_omitted() {
        let entry = AuditEntry {
            timestamp: "0".into(),
            tool: "file".into(),
            command: "read /tmp/x".into(),
            result: AuditResult::Success,
            duration_ms: 1,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            policy_match: None,
            correlation_id: None,
            caller_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("exit_code"),
            "exit_code None must be omitted: {json}"
        );
    }

    #[test]
    fn log_tool_risk_summary_does_not_panic() {
        log_tool_risk_summary(&[
            "shell",
            "bash",
            "exec",
            "web_scrape",
            "fetch",
            "scrape_page",
            "file_write",
            "file_read",
            "file_delete",
            "memory_search",
            "unknown_tool",
        ]);
    }

    #[test]
    fn log_tool_risk_summary_empty_input_does_not_panic() {
        log_tool_risk_summary(&[]);
    }
}
