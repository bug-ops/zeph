// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::config::AuditConfig;

#[derive(Debug)]
pub struct AuditLogger {
    destination: AuditDestination,
}

#[derive(Debug)]
enum AuditDestination {
    Stdout,
    File(tokio::sync::Mutex<tokio::fs::File>),
}

#[derive(serde::Serialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub tool: String,
    pub command: String,
    pub result: AuditResult,
    pub duration_ms: u64,
    /// Fine-grained error category label from the taxonomy. `None` for successful executions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
    /// High-level error domain for recovery dispatch. `None` for successful executions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_domain: Option<String>,
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
}

#[derive(serde::Serialize)]
#[serde(tag = "type")]
pub enum AuditResult {
    #[serde(rename = "success")]
    Success,
    #[serde(rename = "blocked")]
    Blocked { reason: String },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "timeout")]
    Timeout,
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

    pub async fn log(&self, entry: &AuditEntry) {
        let Ok(json) = serde_json::to_string(entry) else {
            return;
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

pub(crate) fn chrono_now() -> String {
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
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
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
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
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
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
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
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"timeout\""));
    }

    #[tokio::test]
    async fn audit_logger_stdout() {
        let config = AuditConfig {
            enabled: true,
            destination: "stdout".into(),
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
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
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
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
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
        };
        let result = AuditLogger::from_config(&config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn audit_logger_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let config = AuditConfig {
            enabled: true,
            destination: path.display().to_string(),
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
                claim_source: None,
                mcp_server_id: None,
                injection_flagged: false,
                embedding_anomalous: false,
            };
            logger.log(&entry).await;
        }

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content.lines().count(), 5);
    }
}
