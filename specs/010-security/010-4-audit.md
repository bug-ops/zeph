---
aliases:
  - Audit Trail
  - Security Logging
  - AgentRFC Protocol Audit
  - Cross-Tool Injection Correlation
  - Env-Var Scrubbing
tags:
  - sdd
  - spec
  - security
  - infra
created: 2026-04-10
status: complete
related:
  - "[[010-security/spec]]"
  - "[[010-1-vault]]"
  - "[[010-2-injection-defense]]"
  - "[[010-3-authorization]]"
---

# Spec: Audit Trail & Security Logging

AgentRFC protocol audit, cross-tool injection correlation, environment variable scrubbing, compliance logging.

## Overview

Zeph maintains an immutable audit trail of all security-relevant events: tool invocations, authorization decisions, IPI detections, vault accesses. This log is used for compliance, incident investigation, and pattern detection (e.g., correlated injection attempts across multiple tools).

## Key Invariants

**Always:**
- All tool invocations logged with: tool name, input (sanitized), output (truncated), status, latency
- All authorization failures logged with: agent, tool, capability, policy check details
- All IPI detections logged with: confidence, source, content preview, user action
- All vault accesses logged with: key name (not value), action (get/set), success/failure
- Audit log persists to disk (SQLite or JSON lines format)

**Never:**
- Log secret values, API keys, or PII (always sanitize)
- Truncate or modify audit entries after creation (immutable log)
- Disable audit logging even in debug mode

## Audit Entry Schema

```rust
#[derive(Serialize, Deserialize, Debug)]
pub struct AuditEntry {
    id: String,                    // UUID v4
    timestamp: i64,                // unix epoch seconds
    agent_id: String,
    event_type: AuditEventType,
    resource: String,              // tool name, endpoint, etc.
    action: String,                // "invoke", "deny", "detect", "access"
    status: AuditStatus,           // "success", "denied", "error"
    details: serde_json::Value,    // event-specific details
    correlation_id: String,        // trace across related events
}

#[derive(Serialize, Deserialize, Debug)]
pub enum AuditEventType {
    ToolInvocation,
    AuthorizationCheck,
    IpiDetection,
    VaultAccess,
    ProtocolError,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum AuditStatus {
    #[serde(rename = "success")]
    Success,
    #[serde(rename = "denied")]
    Denied,
    #[serde(rename = "error")]
    Error,
}
```

## Tool Invocation Audit

Log every tool execution:

```rust
async fn audit_tool_invocation(
    logger: &AuditLogger,
    tool_name: &str,
    input: &Value,
    output: &Value,
    status: ExecutionStatus,
    latency_ms: u64,
) -> Result<()> {
    let entry = AuditEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: now(),
        agent_id: "primary".to_string(),
        event_type: AuditEventType::ToolInvocation,
        resource: tool_name.to_string(),
        action: "invoke".to_string(),
        status: match status {
            ExecutionStatus::Success => AuditStatus::Success,
            ExecutionStatus::Error => AuditStatus::Error,
        },
        details: json!({
            "input": sanitize_for_logging(input),
            "output_preview": truncate_output(output, 200),
            "latency_ms": latency_ms,
            "status_code": 200,  // if HTTP
        }),
        correlation_id: correlation_context::get_trace_id(),
    };
    
    logger.log(entry).await?;
    Ok(())
}

fn sanitize_for_logging(value: &Value) -> Value {
    // Redact keys known to contain secrets
    match value {
        Value::Object(map) => {
            let mut sanitized = map.clone();
            for key in ["api_key", "password", "token", "secret", "auth"] {
                if sanitized.contains_key(key) {
                    sanitized.insert(
                        key.to_string(),
                        Value::String("[REDACTED]".to_string()),
                    );
                }
            }
            Value::Object(sanitized)
        }
        _ => value.clone(),
    }
}
```

## Authorization Audit

Log all permission checks:

```rust
async fn audit_authorization(
    logger: &AuditLogger,
    agent_id: &str,
    tool_name: &str,
    capability: &str,
    allowed: bool,
) -> Result<()> {
    let entry = AuditEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: now(),
        agent_id: agent_id.to_string(),
        event_type: AuditEventType::AuthorizationCheck,
        resource: tool_name.to_string(),
        action: "check".to_string(),
        status: if allowed {
            AuditStatus::Success
        } else {
            AuditStatus::Denied
        },
        details: json!({
            "required_capability": capability,
            "decision": if allowed { "allow" } else { "deny" },
        }),
        correlation_id: correlation_context::get_trace_id(),
    };
    
    logger.log(entry).await?;
    Ok(())
}
```

## IPI Detection Audit

Log all injection attempts:

```rust
async fn audit_ipi_detection(
    logger: &AuditLogger,
    source: &str,                // "web_fetch", "mcp_output", etc.
    confidence: f32,
    pattern_matched: &str,
    user_action: &str,           // "approved", "rejected", "blocked"
) -> Result<()> {
    let entry = AuditEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: now(),
        agent_id: "security".to_string(),
        event_type: AuditEventType::IpiDetection,
        resource: source.to_string(),
        action: "detect".to_string(),
        status: match user_action {
            "approved" => AuditStatus::Success,
            _ => AuditStatus::Denied,
        },
        details: json!({
            "confidence": format!("{:.2}%", confidence * 100.0),
            "pattern": pattern_matched,
            "user_action": user_action,
        }),
        correlation_id: correlation_context::get_trace_id(),
    };
    
    logger.log(entry).await?;
    Ok(())
}
```

## Cross-Tool Injection Correlation

Detect patterns across multiple tool invocations:

```rust
pub struct InjectionCorrelator {
    recent_detections: Arc<Mutex<VecDeque<IpiDetection>>>,
    correlation_window: Duration,
}

impl InjectionCorrelator {
    async fn check_correlation(
        &self,
        new_detection: &IpiDetection,
    ) -> Result<Option<InjectionCluster>> {
        let mut detections = self.recent_detections.lock().await;
        
        // Remove stale detections
        let cutoff = now() - self.correlation_window.as_secs();
        while !detections.is_empty() && detections.front().unwrap().timestamp < cutoff {
            detections.pop_front();
        }
        
        // Check for patterns
        let same_pattern_count = detections
            .iter()
            .filter(|d| d.pattern_matched == new_detection.pattern_matched)
            .count();
        
        if same_pattern_count >= 3 {
            // Same injection pattern detected 3+ times recently
            return Ok(Some(InjectionCluster {
                pattern: new_detection.pattern_matched.clone(),
                count: same_pattern_count + 1,
                sources: detections
                    .iter()
                    .map(|d| d.source.clone())
                    .collect(),
            }));
        }
        
        detections.push_back(new_detection.clone());
        Ok(None)
    }
}
```

## Environment Variable Scrubbing

Remove secrets from subprocess environment:

```rust
fn scrub_environment(env: &HashMap<String, String>) -> HashMap<String, String> {
    let secret_prefixes = vec![
        "ZEPH_",
        "OPENAI_API",
        "ANTHROPIC_API",
        "AWS_",
        "GCP_",
        "GITHUB_TOKEN",
        "SLACK_",
        "TELEGRAM_",
        "DATABASE_PASSWORD",
    ];
    
    let mut scrubbed = HashMap::new();
    
    for (key, value) in env {
        let is_secret = secret_prefixes
            .iter()
            .any(|prefix| key.to_uppercase().starts_with(prefix));
        
        if is_secret {
            log::debug!("Scrubbing env var: {}", key);
            // Don't include in subprocess environment
            continue;
        }
        
        scrubbed.insert(key.clone(), value.clone());
    }
    
    scrubbed
}
```

## Audit Log Storage

Immutable persistence:

```rust
pub struct AuditLogger {
    db: Arc<sqlx::SqlitePool>,  // immutable log table
}

impl AuditLogger {
    async fn log(&self, entry: AuditEntry) -> Result<()> {
        sqlx::query(
            "INSERT INTO audit_log (
                id, timestamp, agent_id, event_type, resource,
                action, status, details, correlation_id
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&entry.id)
        .bind(entry.timestamp)
        .bind(&entry.agent_id)
        .bind(format!("{:?}", entry.event_type))
        .bind(&entry.resource)
        .bind(&entry.action)
        .bind(format!("{:?}", entry.status))
        .bind(serde_json::to_string(&entry.details)?)
        .bind(&entry.correlation_id)
        .execute(&*self.db)
        .await?;
        
        Ok(())
    }
    
    async fn query_by_correlation(
        &self,
        correlation_id: &str,
    ) -> Result<Vec<AuditEntry>> {
        sqlx::query_as::<_, AuditEntry>(
            "SELECT * FROM audit_log WHERE correlation_id = ? ORDER BY timestamp"
        )
        .bind(correlation_id)
        .fetch_all(&*self.db)
        .await
        .context("audit query failed")
    }
}
```

## Configuration

```toml
[security.audit]
enabled = true
log_level = "info"              # "debug", "info", "warn"
backend = "sqlite"              # or "jsonl"
path = ".local/audit.db"

# Retention
retention_days = 90             # after which entries can be archived
archive_path = ".local/audit.archive.jsonl"

# Sampling (reduce volume in production)
sample_rate = 1.0               # log all events; set < 1.0 to sample
```

## Integration Points

- [[006-tools]] — Tool execution audited here
- [[010-1-vault]] — Vault accesses audited
- [[010-2-injection-defense]] — IPI detections audited
- [[010-3-authorization]] — Authorization checks audited

## See Also

- [[010-security/spec]] — Parent
- [[010-2-injection-defense]] — IPI detection events
- [[010-3-authorization]] — Authorization check events
