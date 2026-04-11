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
#[allow(clippy::struct_excessive_bools)]
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
    /// # Errors
    ///
    /// Returns an error if a file destination cannot be opened.
    pub async fn from_config(config: &AuditConfig) -> Result<Self, std::io::Error> {
        let destination = if config.destination == "stdout" {
            AuditDestination::Stdout
        } else {
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(Path::new(&config.destination))
                .await?;
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
            caller_id: None,
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
            caller_id: None,
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
            caller_id: None,
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
            caller_id: None,
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
        let logger = AuditLogger::from_config(&config).await.unwrap();
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
            caller_id: None,
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
        let logger = AuditLogger::from_config(&config).await.unwrap();
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
            caller_id: None,
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
        let result = AuditLogger::from_config(&config).await;
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
            caller_id: None,
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
            caller_id: None,
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
        let logger = AuditLogger::from_config(&config).await.unwrap();

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
                caller_id: None,
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
            caller_id: None,
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
            caller_id: None,
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
