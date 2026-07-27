---
aliases:
  - MCP Security
  - Tool Sanitization
  - Trust Scoring
  - Data Flow Policy
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
  - "[[010-security/spec]]"
  - "[[010-3-authorization]]"
---

# Spec: MCP Security & Policy Enforcement

Tool sanitization, trust scoring, data-flow policy, elicitation gating, embedding anomaly detection.

## Overview

MCP servers are untrusted code running in subprocesses or remote HTTP services. Zeph enforces multiple layers of defense before tool calls reach the agent and after results return:

1. **Pre-connect**: Probing (`DefaultMcpProber`) scans `resources/list` and `prompts/list` for injection
2. **At registration**: Tool definitions sanitized; collision detection runs
3. **Input phase**: Schema validation + policy checks + elicitation probing (optional)
4. **Output phase**: Injection scanning + embedding anomaly detection + PII redaction
5. **Trust scoring**: Per-server risk track with decay

## Key Invariants

**Always:**
- All tool definitions sanitized (names, descriptions, input schema parameter descriptions) before registration
- Tool inputs validated against `input_schema` JSON schema before passing to server
- Tool outputs scanned for injection patterns before returning to agent
- `Untrusted` servers fail closed: zero tools exposed unless `tool_allowlist` is declared or `allow_untrusted_without_allowlist = true`
- Trust level enforcement: `Trusted` → all tools; `Untrusted` → allowlist only; `Sandboxed` → allowlist only, no elicitation
- Anomalous tool-call sequences flagged by `EmbeddingAnomalyGuard` post-execution

**Never:**
- Pass unsanitized user input to MCP tools — always validate against schema
- Trust server output without scanning — all responses pass through injection/PII detection
- Expose all tools for `Untrusted` servers without explicit operator opt-in
- Run elicitation on `Sandboxed` servers even if `elicitation_enabled = true`

## Tool Sanitization Pipeline

`sanitize_tools()` (`crates/zeph-mcp/src/sanitize.rs`) scrubs injection patterns from tool definitions at registration:

```rust
pub struct SanitizeResult {
    pub tools: Vec<McpTool>,                // Cleaned tool definitions
    pub injection_count: usize,             // Patterns found + removed
    pub flagged_parameters: Vec<(String, String)>,  // (path, pattern) tuples
}

pub fn sanitize_tools(
    tools: &[McpTool],
    config: &ContentIsolationConfig,
) -> SanitizeResult {
    // 1. Scan tool.name, tool.description for regex injection patterns
    // 2. Scan tool.input_schema.properties[*].description for injection patterns
    // 3. Replace matched patterns with [sanitized]
    // 4. Record flagged parameters for metadata
    // 5. Return cleaned tools + metadata
}
```

Patterns scanned:
- Prompt injection: "ignore this instruction", "pretend you are", "SYSTEM:", etc.
- SQL injection: `SELECT`, `DROP`, `; --`, etc.
- Shell injection: `$(...)`, `` `...` ``, `; rm -rf`, etc.

If a tool's description is flagged, it is sanitized but **not removed**. The tool remains callable; only the suspicious description text is replaced.

## Pre-Connect Probing

`DefaultMcpProber` (`crates/zeph-mcp/src/prober.rs`) scans resources and prompts before tools/list:

```rust
pub struct DefaultMcpProber { /* ... */ }

pub struct ProbeResult {
    pub injection_patterns_found: usize,
    pub resource_count: usize,
    pub prompt_count: usize,
}

impl DefaultMcpProber {
    /// Scan resources/list and prompts/list for injection patterns.
    pub async fn probe_server(&self, conn: &McpClient) -> Result<ProbeResult> {
        // Fetches resources/list, prompts/list (if available)
        // Scans descriptions for injection patterns
        // Updates server trust score
    }
}
```

Probing is non-blocking: results update the trust score but do NOT block tools/list fetch.

## Trust Score System

`ServerTrustScore` (`crates/zeph-mcp/src/trust_score.rs`) tracks per-server risk with exponential decay:

```rust
pub struct ServerTrustScore {
    pub current_score: f32,                 // [0.0, 1.0] (1.0 = fully trusted)
    pub last_decay_timestamp: i64,
    pub history: VecDeque<(f32, i64)>,     // (score, timestamp) snapshots
}

impl ServerTrustScore {
    /// Report an incident (e.g., injection pattern found); decrease score.
    pub fn report_incident(&mut self, severity: f32) {
        self.current_score *= (1.0 - severity);  // Multiplicative decay
    }
    
    /// Apply time-based recovery: score drifts back toward 1.0 slowly.
    pub fn apply_decay(&mut self, now: i64) {
        let elapsed_secs = (now - self.last_decay_timestamp) as f32;
        let recovery_rate = 0.001;  // per second
        self.current_score = (self.current_score + recovery_rate * elapsed_secs).min(1.0);
    }
}
```

Score factors:
- **Probing failures**: -0.2 (injection patterns in resources/prompts)
- **Sanitization hits**: -0.1 (injection patterns in tool descriptions)
- **Embedding anomalies**: -0.15 (unexpected tool-call sequence)
- **Time decay**: +0.001 per second (gradual recovery)

## Data-Flow Policy Enforcement

`check_data_flow()` restricts sensitive tools based on trust level:

```rust
pub enum DataSensitivity {
    Public,          // Available to all trust levels
    Internal,        // `Untrusted` can use only with allowlist
    Confidential,    // `Sandboxed` cannot use
}

pub struct DataFlowViolation {
    pub tool: String,
    pub server: String,
    pub trust_level: McpTrustLevel,
    pub reason: String,
}

pub fn check_data_flow(
    tool: &McpTool,
    server_trust: McpTrustLevel,
    tool_metadata: &ToolSecurityMeta,
) -> Result<(), DataFlowViolation> {
    match (server_trust, tool_metadata.sensitivity) {
        (McpTrustLevel::Sandboxed, DataSensitivity::Confidential) => {
            return Err(DataFlowViolation { /* ... */ });
        }
        (McpTrustLevel::Untrusted, DataSensitivity::Confidential)
            if !in_allowlist => {
            return Err(DataFlowViolation { /* ... */ });
        }
        _ => Ok(()),
    }
}
```

## Output Validation: Injection Scanning

All tool responses pass through `ContentSanitizer` (see [[010-2-injection-defense]]) for injection detection:

```rust
impl McpToolExecutor {
    async fn execute_tool(&self, tool: &McpTool, args: Value) -> Result<Value> {
        // ... execute tool ...
        let raw_response = /* result from server */;
        
        // Sanitize output
        let source = ContentSource::new(ContentSourceKind::McpToolResult)
            .with_trust_level(ContentTrustLevel::LocalUntrusted);
        
        let sanitized = self.sanitizer.sanitize(
            &raw_response.to_string(),
            source,
        );
        
        // Check injection flags
        if !sanitized.injection_flags.is_empty() {
            tracing::warn!(
                "Injection patterns in tool output: {:?}",
                sanitized.injection_flags
            );
        }
        
        Ok(sanitized.body)
    }
}
```

## Output Validation: Embedding Anomaly Detection

`EmbeddingAnomalyGuard` (`crates/zeph-mcp/src/embedding_guard.rs`) detects unexpected tool-call sequences:

```rust
pub struct EmbeddingAnomalyGuard { /* ... */ }

pub enum EmbeddingGuardEvent {
    Benign,
    Suspicious { score: f32, reason: String },
    Anomalous { score: f32 },
}

impl EmbeddingAnomalyGuard {
    /// Check if this tool call is anomalous given prior results.
    pub async fn analyze_call(
        &self,
        prior_result: &str,
        requested_tool: &str,
        tool_description: &str,
    ) -> Result<EmbeddingGuardEvent> {
        // Embed prior result and tool description
        // Compute cosine similarity
        // If similarity is unexpectedly low, flag as anomalous
        // Update trust score
    }
}
```

Example: If a file-read tool's output (e.g., PDF contents) triggers a request for `delete_account` tool, the embedding distance between the result and that tool is large → flagged as anomalous.

## Allowlist & Fail-Closed Semantics

Trust level behavior:

| Trust Level | Tool Allowlist Provided | Behavior |
|---|---|---|
| `Trusted` | No | Expose all tools |
| `Trusted` | Yes | Expose allowlisted tools only |
| `Untrusted` | No | **Expose zero tools** (fail closed) unless `allow_untrusted_without_allowlist = true` |
| `Untrusted` | Yes | Expose allowlisted tools only |
| `Sandboxed` | No | Expose zero tools |
| `Sandboxed` | Yes | Expose allowlisted tools only; disable elicitation |

Default: `allow_untrusted_without_allowlist = false` (secure default). An operator must explicitly opt in to expose tools from an untrusted server without an allowlist.

## Elicitation Gating

When `elicitation_enabled = true`, Zeph probes tool parameters for hidden capabilities:

- `Sandboxed` servers: elicitation disabled regardless of config
- `Untrusted` servers: elicitation allowed unless allowlist says otherwise
- `Trusted` servers: elicitation allowed

See [[008-4-elicitation]] for elicitation details.

## Attestation

When `expected_tools` list is provided, compare actual tools against expected list at startup (see [[008-2-discovery]]). Schema mismatches logged but do NOT block usage.

## Integration Points

- [[008-1-lifecycle]] — Sanitization runs after tools/list fetch
- [[008-2-discovery]] — Collision detection after sanitization
- [[008-4-elicitation]] — Parameter probing (optional)
- [[010-2-injection-defense]] — Output scanning via ContentSanitizer
- [[010-3-authorization]] — Policy enforcement

## See Also

- [[008-mcp/spec]] — Parent
- [[010-security/spec]] — Security architecture
- [[008-4-elicitation]] — Optional tool parameter discovery
