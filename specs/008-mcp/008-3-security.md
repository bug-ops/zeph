---
aliases:
  - MCP Security
  - MCP Elicitation
  - OAP Authorization
  - SMCP Secure MCP
tags:
  - sdd
  - spec
  - mcp
  - protocol
  - security
  - contract
created: 2026-04-10
status: complete
related:
  - "[[008-mcp/spec]]"
  - "[[008-1-lifecycle]]"
  - "[[008-2-discovery]]"
  - "[[010-security]]"
  - "[[010-3-authorization]]"
---

# Spec: MCP Security & OAP Authorization

Elicitation phases, injection detection, OAP (Org-Aware Permission) authorization, SMCP secure protocol.

## Overview

MCP servers are untrusted code running in subprocesses. Zeph enforces multiple layers of defense: input sanitization, output injection detection, and capability-based authorization via OAP.

## Key Invariants

**Always:**
- All tool inputs sanitized before passing to server (schema validation, injection checks)
- All tool outputs scanned for injection patterns before returning to agent
- Server responses that fail validation are rejected with logging
- OAP authorization policies applied: which tools can be called by which agents

**Never:**
- Pass unsanitized user input to MCP tools
- Trust server output without injection detection (DeBERTa + regex)
- Allow cross-agent tool access without explicit capability delegation

## Elicitation Phases

Tool invocation has three security phases:

```
1. INPUT PHASE
   ├─ Schema validation (input_schema)
   ├─ Injection detection (SQL, shell, prompt injection)
   └─ Rate limiting check

2. EXECUTION PHASE
   ├─ RPC call to MCP server
   ├─ Timeout enforcement
   └─ Error handling

3. OUTPUT PHASE
   ├─ Response format validation
   ├─ Injection detection (DeBERTa on output)
   └─ PII detection (NER + redaction)
```

Code:

```rust
async fn invoke_tool_secure(
    &self,
    tool_name: &str,
    input: Value,
    caller_context: &AgentContext,
) -> Result<Value> {
    // PHASE 1: Input validation
    let tool = self.get_tool(tool_name)?;
    
    // Schema validation
    jsonschema::validate(&input, &tool.input_schema)
        .map_err(|e| anyhow!("Schema validation failed: {}", e))?;
    
    // Injection detection
    if self.detect_injection(&input)? {
        return Err(anyhow!("Injection detected in tool input"));
    }
    
    // Rate limiting
    self.check_rate_limit(tool_name, caller_context)?;
    
    // PHASE 2: Execution
    let result = self.server.call_tool(
        tool_name,
        input,
        Duration::from_secs(30),  // timeout
    ).await?;
    
    // PHASE 3: Output validation
    
    // Format check
    if !result.is_object() && !result.is_string() {
        return Err(anyhow!("Unexpected tool output format"));
    }
    
    // Injection detection on output
    if self.detect_output_injection(&result)? {
        log::warn!("Injection detected in tool output from {}", tool_name);
        return Err(anyhow!("Tool output validation failed"));
    }
    
    // PII redaction
    let redacted = self.redact_pii(&result)?;
    
    Ok(redacted)
}
```

## Injection Detection

Multi-layer detection using DeBERTa + regex patterns:

```rust
pub struct InjectionDetector {
    deberta: Arc<DeBERTaClassifier>,  // "is this injection?" binary classifier
    regex_patterns: Vec<Regex>,       // SQL, shell, prompt injection patterns
}

impl InjectionDetector {
    fn detect_injection(&self, value: &Value) -> Result<bool> {
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Object(o) => serde_json::to_string(o)?,
            _ => return Ok(false),
        };
        
        // 1. Regex check (fast)
        for pattern in &self.regex_patterns {
            if pattern.is_match(&text) {
                log::warn!("Regex injection pattern matched");
                return Ok(true);
            }
        }
        
        // 2. DeBERTa check (slow, only if suspicious)
        if text.len() > 100 && text.contains("SELECT") || text.contains("$(") {
            let score = self.deberta.classify(&text).await?;
            if score > 0.8 {
                return Ok(true);
            }
        }
        
        Ok(false)
    }
}
```

## OAP Authorization

Capability-based access control:

```rust
pub struct OAPPolicy {
    // agent_id → allowed tool names
    permissions: HashMap<String, HashSet<String>>,
    // tool_id → required capability
    capabilities: HashMap<String, Capability>,
}

impl OAPPolicy {
    fn check_authorization(
        &self,
        agent_id: &str,
        tool_name: &str,
    ) -> Result<()> {
        // 1. Check if agent has tool permission
        let allowed = self.permissions
            .get(agent_id)
            .ok_or_else(|| anyhow!("Agent {} not in ACL", agent_id))?;
        
        if !allowed.contains(tool_name) {
            return Err(anyhow!(
                "Agent {} not authorized for tool '{}'",
                agent_id,
                tool_name
            ));
        }
        
        // 2. Check capability delegation
        let required = self.capabilities
            .get(tool_name)
            .copied()
            .unwrap_or(Capability::PublicRead);
        
        if !self.has_capability(agent_id, required) {
            return Err(anyhow!(
                "Agent {} lacks capability {:?} for tool '{}'",
                agent_id,
                required,
                tool_name
            ));
        }
        
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Capability {
    PublicRead,
    MutableState,
    Execute,
    Privileged,
}
```

## SMCP (Secure MCP)

Optional enhanced security mode for critical tools:

```rust
pub struct SmcpEnvelope {
    // HMAC-signed request/response
    request_id: String,
    nonce: [u8; 32],
    signature: Vec<u8>,
    timestamp: i64,
}

impl SmcpEnvelope {
    fn sign_request(
        request: &ToolRequest,
        shared_secret: &[u8],
    ) -> Result<SmcpEnvelope> {
        let mut hasher = HmacSha256::new_from_slice(shared_secret)?;
        let payload = serde_json::to_vec(request)?;
        hasher.update(&payload);
        
        let signature = hasher.finalize().into_bytes().to_vec();
        
        Ok(SmcpEnvelope {
            request_id: uuid::Uuid::new_v4().to_string(),
            nonce: rand::random(),
            signature,
            timestamp: now(),
        })
    }
}
```

## Configuration

```toml
[mcp.security]
injection_detection_enabled = true
deberta_enabled = false         # expensive; enable for high-risk tools
output_validation_enabled = true
oap_authorization_enabled = true
rate_limit_per_minute = 60

# Injection patterns (regex)
injection_patterns = [
    "(?i)(DROP|DELETE|UPDATE|INSERT).*FROM",  # SQL
    "\\$\\(.*\\)|`.*`",                       # Shell
    "prompt\\s*injection|jailbreak|ignore",   # Prompt injection
]

# Tool capabilities
[[mcp.security.tool_capabilities]]
tool = "shell_execute"
capability = "Execute"
rate_limit_per_minute = 10

[[mcp.security.tool_capabilities]]
tool = "list_files"
capability = "PublicRead"
```

## Integration Points

- [[008-1-lifecycle]] — Security checks during subprocess spawning
- [[008-2-discovery]] — Tool descriptions scanned for injection before caching
- [[010-security]] — Parent security spec; refers to injection defense
- [[010-3-authorization]] — OAP policy enforcement
- [[025-classifiers]] — DeBERTa-backed injection detection

## See Also

- [[008-mcp/spec]] — Parent
- [[008-1-lifecycle]] — Server lifecycle where security starts
- [[008-2-discovery]] — Tool discovery with injection scanning
- [[010-security]] — Cross-cutting security constraints
- [[010-3-authorization]] — OAP authorization policies
