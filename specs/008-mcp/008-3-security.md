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

`sanitize_tools()` (`crates/zeph-mcp/src/sanitize.rs`) mutates tool definitions in-place, scrubbing injection patterns:

```rust
pub struct SanitizeResult {
    /// Number of individual fields (description, schema strings) replaced with "[sanitized]"
    pub injection_count: usize,
    /// Names of tools that had at least one injected field
    pub flagged_tools: Vec<String>,
    /// (tool_name, pattern_name) pairs for audit and logging
    pub flagged_patterns: Vec<(String, String)>,
}

/// Sanitize tool definitions in-place: scrub injection patterns from descriptions/schemas.
/// 
/// Modifies `tools` directly; does not wrap or return them (sync, no Result).
pub fn sanitize_tools(
    tools: &mut [McpTool],
    server_id: &str,
    max_description_bytes: usize,
) -> SanitizeResult {
    // 1. Scan tool.name for injection patterns
    // 2. Scan tool.description; truncate/replace matched patterns
    // 3. Recursively scan tool.input_schema.properties[*].description
    // 4. Truncate schemas exceeding max_description_bytes (threat: decompression bomb)
    // 5. Record all flagged fields for audit
    // 6. Return metadata; tools are mutated in place
}
```

**Patterns scanned**:
- Prompt injection: "ignore this instruction", "pretend you are", "SYSTEM:", etc.
- SQL injection: `SELECT`, `DROP`, `; --`, etc.
- Shell injection: `$(...)`, `` `...` ``, `; rm -rf`, etc.

**Sanitization behavior**: matched patterns are replaced with `[sanitized]`. Tools with flagged descriptions remain callable; only the suspicious text is replaced. Trust score is updated based on `injection_count`.

## Pre-Connect Probing

Before loading tools via `tools/list`, `DefaultMcpProber::probe()` scans `resources/list` and `prompts/list` for pre-connection risk assessment:

```rust
pub struct ProbeResult {
    /// Trust score delta from probing (negative = risk detected)
    pub score_delta: f64,
    /// Probing summary (injection pattern count, resources/prompts found)
    pub summary: String,
    /// If true, server failed probing and is marked dangerous
    pub block: bool,
}

impl DefaultMcpProber {
    /// Scan resources and prompts for injection patterns; update trust scoring.
    /// 
    /// No Result wrapper — probing is advisory. Always returns ProbeResult.
    pub async fn probe(
        &self,
        server_id: &str,
        client: &McpClient,
    ) -> ProbeResult {
        // Fetches resources/list, prompts/list (if available)
        // Scans descriptions for injection patterns
        // Computes score delta; returns risk assessment
    }
}
```

**Probing semantics**: always runs before `tools/list`, even for trusted servers. Results are logged but do NOT block connection. Trust score is adjusted based on pattern counts detected.

## Trust Score System

`ServerTrustScore` (`crates/zeph-mcp/src/trust_score.rs`) tracks per-server cumulative risk with asymmetric decay:

```rust
pub struct ServerTrustScore {
    pub server_id: String,          // Unique server identifier
    pub score: f64,                 // [0.0, 1.0]; 0.5 = neutral (initial)
    pub success_count: u64,         // Successful tool executions
    pub failure_count: u64,         // Failed or injection-detected calls
    pub updated_at_secs: u64,       // Timestamp of last update
}

impl ServerTrustScore {
    /// Record a successful tool execution; boost score.
    pub fn record_success(&mut self);
    
    /// Record a failure or injection detection; penalize score.
    pub fn record_failure(&mut self);
}
```

**Score updates**:
- **Success**: `+0.02` per call (capped at 1.0)
- **Injection penalty**: `-0.25` per injection pattern detected
- **Failure penalty**: `-0.10` per call failure
- **Exponential decay**: scores **above** 0.5 decay toward 0.5 at ~0.01/day; **below or at** 0.5 stay put (asymmetric: no recovery from decay alone)

**Semantics**: score below 0.5 indicates the server is untrusted and requires explicit `tool_allowlist` to expose tools. An attacker cannot regain trust by going quiet (decay doesn't raise scores below neutral).

## Data-Flow Policy Enforcement

`check_data_flow()` restricts sensitive tools based on trust level — sensitivity levels are ordered `None < Low < Medium < High`:

```rust
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum DataFlowViolation {
    #[error(
        "tool '{tool_name}' (sensitivity={sensitivity:?}) on server '{server_id}' \
         (trust={trust:?}) violates data-flow policy"
    )]
    SensitivityTrustMismatch {
        server_id: String,
        tool_name: ToolName,
        sensitivity: DataSensitivity,
        trust: McpTrustLevel,
    },
}

/// Check data-flow policy: tool sensitivity must not exceed server trust.
/// 
/// Sensitivity is read from `tool.security_meta.data_sensitivity`.
pub fn check_data_flow(
    tool: &McpTool,
    server_trust: McpTrustLevel,
) -> Result<(), DataFlowViolation> {
    let sensitivity = tool.security_meta.data_sensitivity;
    
    // Examples of violations (errors):
    // - High-sensitivity tool on Sandboxed server → block
    // - High-sensitivity tool on Untrusted server → block
    
    // Allowed (no error):
    // - Any tool on Trusted server
    // - Medium-sensitivity tool on Untrusted/Sandboxed (Sandboxed logs WARN)
    // - Low-sensitivity tool on Untrusted/Sandboxed
}
```

**Policy enforcement**: high-sensitivity tools (e.g., credential management, user deletion) require `Trusted` servers only; medium-sensitivity requires `Trusted` or `Untrusted` (Sandboxed is allowed with warning); low/none are available everywhere.

## Output Validation: Injection Scanning

Tool output sanitization is a cross-cutting concern at the agent loop layer (`crates/zeph-core/src/agent/tool_execution/sanitize.rs`), not per-executor. All tool outputs — MCP, web scrape, memory retrieval, and native — flow through a unified sanitization pipeline:

```rust
/// Sanitize tool output body before inserting into LLM message history.
pub(super) async fn sanitize_tool_output(
    &mut self,
    body: &str,
    tool_name: &str,
) -> (
    String,                                    // Sanitized body
    bool,                                      // Injection detected flag
    ContentSourceKind,                         // Source classification
    zeph_sanitizer::ContentTrustLevel,        // Trust level
) {
    // Classify output source: MCP response, web scrape, memory retrieval, or generic tool result
    let source = build_tool_output_source(tool_name);
    
    // MCP responses are identified by tool_name containing ':' (server:tool format)
    // and tagged as ContentSourceKind::McpResponse
    
    // Injection detection via regex spotlighting and optional ML classifiers
    let sanitized = self.sanitizer.sanitize(&body, source);
    
    (sanitized.body, !sanitized.injection_flags.is_empty(), source.kind, source.trust_level)
}

/// Route tool outputs to their correct source classification.
fn build_tool_output_source(tool_name: &str) -> ContentSource {
    if tool_name.contains(':') || tool_name == "mcp" {
        // MCP responses identified by server:tool naming convention
        ContentSource::new(ContentSourceKind::McpResponse).with_identifier(tool_name)
    } else if tool_name.starts_with("web") {
        ContentSource::new(ContentSourceKind::WebScrape).with_identifier(tool_name)
    } else if tool_name == "memory_search" {
        ContentSource::new(ContentSourceKind::MemoryRetrieval).with_identifier(tool_name)
    } else {
        ContentSource::new(ContentSourceKind::ToolResult).with_identifier(tool_name)
    }
}
```

**Flow**: `McpToolExecutor::execute_tool()` returns raw output → agent loop classifies via `build_tool_output_source()` → sanitization applies injection detection and trust wrapping via `sanitize_tool_output()` → clean body enters LLM context.

## Output Validation: Embedding Anomaly Detection

`EmbeddingAnomalyGuard` (`crates/zeph-mcp/src/embedding_guard.rs`) detects anomalous per-(server,tool) output patterns asynchronously via centroid drift:

```rust
pub struct EmbeddingGuardEvent {
    pub server_id: String,
    pub tool_name: ToolName,
    pub result: EmbeddingGuardResult,
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum EmbeddingGuardResult {
    /// Output is within the expected distribution for this (server,tool) pair
    Normal { distance: f64 },
    /// Output is anomalous — possible injection or unexpected content
    Anomalous { distance: f64, threshold: f64 },
    /// Cold-start: insufficient clean samples; regex fallback used instead
    RegexFallback { injection_detected: bool },
}

pub struct EmbeddingAnomalyGuard { /* ... */ }

impl EmbeddingAnomalyGuard {
    /// Asynchronous fire-and-forget check of tool output for anomalies.
    /// 
    /// Results are sent to a channel; no return value.
    pub fn check_async(
        &self,
        server_id: &str,
        tool_name: ToolName,
        tool_output: &str,
    ) {
        // Background task: embed output
        // Compute distance from running centroid of this (server,tool)'s historical outputs
        // Send EmbeddingGuardEvent to the result channel
    }
}
```

**Semantics**: the guard tracks per-(server,tool) centroid of **that tool's own** historical outputs, not cross-tool pattern matching. Anomalies (outputs far from the tool's historical pattern) trigger trust-score penalties.

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
