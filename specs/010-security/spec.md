# Spec: Security
## Sources
### External- **OWASP AI Agent Security Cheat Sheet** (2026): https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html
- **Prompt Injection Defenses** (Anthropic, 2025) — spotlighting, context sandboxing, dual-LLM (QuarantinedSummarizer): https://www.anthropic.com/research/prompt-injection-defenses
- **How Microsoft Defends Against Indirect Prompt Injection** (MSRC, 2025) — TrustLevel/ContentSource model: https://www.microsoft.com/en-us/msrc/blog/2025/07/how-microsoft-defends-against-indirect-prompt-injection-attacks
- **Indirect Prompt Injection Survey** (2025): https://arxiv.org/html/2506.08837v1
- **Log-To-Leak: Prompt Injection via MCP** (2025) — tool description sanitization at registration: https://openreview.net/forum?id=UVgbFuXPaO
- **Policy Compiler for Secure Agentic Systems** (Feb 2026) — PolicyEnforcer, PermissionPolicy design: https://arxiv.org/html/2602.16708v2
- **Llama Guard** (Meta AI, 2023) — GuardrailFilter classifier prompt, SAFE/UNSAFE prefix: https://arxiv.org/abs/2312.06674

### Internal| File | Contents |
|---|---|
| `crates/zeph-core/src/vault/` | `VaultProvider`, age/env backends |
| `crates/zeph-core/src/config/types/security.rs` | `SecurityConfig` |
| `crates/zeph-tools/src/filter/security.rs` | `SecurityPatterns`, 17 regex patterns |
| `crates/zeph-gateway/src/transport/auth.rs` | BLAKE3 + `ct_eq` bearer auth |
| `crates/zeph-acp/src/transport/auth.rs` | ACP bearer token auth |
| `crates/zeph-acp/src/fs.rs` | `resolve_resource_link`, SSRF/path checks |
| `crates/zeph-a2a/src/client.rs` | A2A SSRF protection, TLS enforcement |
| `crates/zeph-mcp/src/oauth.rs` | `validate_oauth_metadata_urls()` — SSRF on OAuth endpoints |
| `crates/zeph-core/src/bootstrap/oauth.rs` | `VaultCredentialStore` — OAuth token persistence |

---

Multiple crates — security is cross-cutting.

## Vault (Secret Management)
`crates/zeph-core/src/vault/` — backend abstraction for secrets:

| Backend | Activation |
|---|---|
| `age` | `vault.backend = "age"` (default, recommended) |
| `env` | `vault.backend = "env"` — reads `ZEPH_*` env vars |

- Secrets are resolved into `ResolvedSecrets` at startup — API keys never stored inline in TOML
- All secret values implement `Zeroize` — zeroed on drop
- Vault operations are the only place where secret plaintext exists in memory

## Bearer Token Auth (Gateway / ACP)
- BLAKE3 hash of the token + `ConstantTimeEq` (subtle crate) comparison
- No string comparison with `==` — always constant-time
- Token is never logged or included in error messages

## Shell Sandbox
- Blocklist check (`find_blocked_command()`) runs **unconditionally before** `PermissionPolicy`
- Blocked: process substitution `$(...)`, here-strings `<<<`, dangerous builtins (`rm -rf`, `mkfs`, etc.)
- Bypass attempts: passing blocked patterns as arguments is also caught

## Untrusted Content Isolation
`ContentSanitizer` pipeline (when guardrail feature enabled):

1. **ContentSanitizer**: strips/escapes injection patterns from external content
2. **Source boundaries**: wraps external content in `<!-- external: {source} -->` markers
3. **QuarantinedSummarizer**: uses Dual LLM approach — one LLM processes untrusted content, another summarizes into trusted context
4. **ExfiltrationGuard**: blocks markdown image URLs, suspicious tool URLs, unauthorized memory writes from untrusted content

## Policy Enforcer (feature: `policy-enforcer`)
- Configurable allow/deny rules for tool calls
- Rules evaluated before tool execution
- Violations logged to audit trail

## `unsafe_code = "deny"` Workspace-Wide
- No `unsafe` blocks anywhere — enforced by compiler
- No exceptions — new code requiring unsafe must use a safe wrapper crate

## SSRF Protection
- HTTP requests validate target URL: private IP ranges (`10.x`, `172.16-31.x`, `192.168.x`, loopback) are blocked
- Redirect chain is validated — each redirect target is also checked against blocklist
- Applied to: WebScrapeExecutor, any HTTP client in tool executors
- **MCP OAuth** (`validate_oauth_metadata_urls()`): all endpoints discovered from OAuth metadata (token_endpoint, authorization_endpoint, registration_endpoint, revocation_endpoint, jwks_uri) are validated through `validate_url_ssrf()` before use — prevents attacker-controlled MCP server from redirecting token exchange to internal services

## Input Validation
- All user input at system boundaries (CLI args, config values, tool inputs) is validated
- Null bytes, path traversal (`../`), and symlink escapes are caught at load time
- Instruction file loading: canonical path must stay within project root

## Key Invariants
- Secrets never flow through logging, error messages, or debug dumps (redaction applied)
- `ConstantTimeEq` is mandatory for all token/key comparisons — `==` is banned
- Blocklist check cannot be bypassed by `TrustLevel` or `PermissionPolicy`
- `ExfiltrationGuard` must run on all untrusted content before it can trigger memory writes or tool calls
- `unsafe_code = "deny"` must never be lifted — no exceptions

---

## IPI Defense: DeBERTa Soft-Signal, AlignSentinel, TurnCausalAnalyzer
> **Status**: Implemented. Closes #2193, #2208, #2335.

Three-layer indirect prompt injection (IPI) defense stack in `zeph-classifiers`:

### DeBERTa Soft-Signal
`CandleClassifier` (DeBERTa-based) produces a continuous injection probability score `[0.0, 1.0]` for each piece of external content. Scores above `soft_signal_threshold` are escalated to `AlignSentinel`; below are passed with a `DEBUG` note.

### AlignSentinel (3-Class)
`AlignSentinel` classifies content into three classes:
- `Clean` — safe content
- `Suspicious` — possible injection, warn but continue
- `Injection` — high-confidence injection, block

Hard threshold: scores above `hard_threshold` (default 0.85) are always classified as `Injection` regardless of AlignSentinel vote. Policy-blocked outputs are exempt from ML classification (skip ML on `policy_blocked` outputs).

### TurnCausalAnalyzer
`TurnCausalAnalyzer` checks for causal anomalies across turns: if a tool call in turn N produces a result that directly causes an unusual tool call in turn N+1 (based on semantic distance from expected call patterns), it is flagged as a potential injection-induced pivot.

### Config
```toml
[security.ipi]
enabled = false
soft_signal_threshold = 0.5
hard_threshold = 0.85
causal_analysis = false
```

### Key Invariants
- `policy_blocked` outputs must be skipped by ML classification — no double-processing
- Hard threshold bypass applies regardless of AlignSentinel vote — AlignSentinel is advisory when above hard threshold
- DeBERTa model uses Metal/CUDA device when available (`--features metal` on macOS)
- NEVER run IPI ML classifiers on agent-generated content — only on external/tool-sourced content

---

## PII NER Circuit Breaker
> **Status**: Implemented. Closes #2562, #2572.

`CandlePiiClassifier` detects PII (Personal Identifiable Information) in tool inputs and outputs using a candle-backed NER model. When PII is detected above the configured threshold, the content is blocked before being passed to the LLM or stored in memory.

### PII Allowlist
`pii_allowlist` in `[security.pii]` config: a list of regex patterns that exempt matched strings from PII blocking. Useful for known-safe identifiers that the NER model may misclassify.

### Input Truncation
PII NER input is truncated to `pii_max_input_chars` (default 4096) before model inference to prevent OOM on very large tool outputs during paginated reads.

### Config
```toml
[security.pii]
enabled = false
threshold = 0.85
pii_max_input_chars = 4096
pii_allowlist = []     # regex patterns exempt from PII blocking
```

### Key Invariants
- PII NER input must be truncated before model inference — never pass unbounded input
- `pii_allowlist` patterns are matched against detected PII entity strings, not full content
- `search_code` tool results must be reclassified as `ToolResult` for PII scanning (not `UserContent`)
- NEVER block agent-generated content on PII signal — only external/tool-sourced content

---

## Cross-Tool Injection Correlation and AgentRFC Protocol Audit
> **Status**: Implemented. Closes #2480, #2509, #2603.

### Cross-Tool Injection Correlation
`CrossToolCorrelator` tracks injection signals across multiple tool calls within a turn. If two or more tool outputs within the same turn produce injection signals above threshold, the entire turn is escalated to `InjectionConfirmed` regardless of per-tool individual scores.

Correlation is bounded to the current turn — signals do not carry across turn boundaries.

### AgentRFC Protocol Audit
`AgentRfcAuditor` validates that A2A and ACP protocol messages conform to the AgentRFC security model. Specifically:
- Validates that capability grants in protocol messages do not exceed the declared agent capability set
- Detects confused-deputy patterns where an agent's capability is invoked on behalf of a less-trusted principal

### Key Invariants
- Cross-turn signal accumulation is NEVER performed — correlation is within-turn only
- `CrossToolCorrelator` state is cleared at the start of each user turn
- AgentRFC audit failures are logged as `WARN` and escalated to the security event log — they do not hard-block the turn by default

---

## MCP→ACP Confused-Deputy Boundary Enforcement
> **Status**: Implemented. Closes #2417, #2426, #2427, #2436.

When an MCP tool result triggers an ACP action (e.g., an MCP server result instructs the agent to perform an ACP capability call), a confused-deputy check validates that the MCP server's trust level is sufficient to authorize the requested ACP capability.

### Trust Level Mapping
| MCP Trust | Permitted ACP Capabilities |
|-----------|---------------------------|
| `trusted` | All declared ACP capabilities |
| `untrusted` | Read-only ACP capabilities only |
| `sandboxed` | No ACP capability invocation permitted |

### Key Invariants
- Sandboxed MCP servers MUST NOT trigger ACP capability calls — hard block
- Untrusted MCP servers may only trigger read-only ACP capabilities — write-path capabilities are blocked
- NEVER grant ACP capability based on MCP tool output content alone — trust level governs, not content
- Confused-deputy violations are recorded in the security audit log with full context

---

## SMCP Lifecycle and IBCT Tokens
> **Status**: Implemented. Closes #2533.

### SMCP Lifecycle
`SmcpLifecycle` manages the secure MCP server lifecycle: server startup, capability negotiation, and shutdown are audited. Each server's lifecycle transitions are logged to `mcp_lifecycle_events` table.

### IBCT: Invocation-Bound Capability Tokens
IBCT (Invocation-Bound Capability Tokens) are short-lived HMAC-SHA256 tokens bound to a specific tool invocation. They prevent capability reuse or replay across different invocations.

Token format: `HMAC-SHA256(key_id + ":" + invocation_id + ":" + capability_name + ":" + timestamp)`. Tokens are sent via `X-Zeph-IBCT` header on A2A calls (feature: `ibct`).

Key rotation: `key_id` field allows multiple active keys during rotation windows.

### Config
```toml
[a2a]
ibct_enabled = false       # feature: ibct
ibct_key_rotation_secs = 3600
```

### Key Invariants
- IBCT tokens are single-use — replay detected by `invocation_id` deduplication
- Token validity window is bounded — expired tokens are always rejected regardless of signature validity
- `key_id` rotation must maintain a grace window for in-flight requests during rotation
- NEVER use IBCT for MCP calls — IBCT applies to A2A calls only (see `014-a2a/spec.md`)

---

## MCP Tool Input Schema Injection Scan
> **Status**: Implemented. See also `008-mcp/spec.md`.

`sanitize_tools()` scans not only tool descriptions but also `input_schema` parameter descriptions for injection patterns. When an injection pattern is detected inside a tool parameter's `description` field, the parameter path and pattern name are recorded in `security_meta.flagged_parameters`.

### Behavior
- `flagged_parameters`: a list of `(property_path, pattern_name)` tuples populated for each `input_schema` property whose `description` matches an injection pattern
- The parameter description is sanitized (pattern replaced with `[sanitized]`) — the parameter itself is not removed
- `SanitizeResult.injection_count` increments for each flagged parameter description

### Key Invariants
- Input schema injection scan runs on every `sanitize_tools()` call — not only on suspicious servers
- Flagged parameter paths use dot notation (e.g., `properties.cmd`) for unambiguous identification
- NEVER remove a tool parameter on injection suspicion — sanitize the description and flag; the tool remains callable
- `security_meta.flagged_parameters` is set at registration time; subsequent calls do not re-scan unless the server re-registers

---

## Credential Env Var Scrubbing
> **Status**: Implemented. Closes #2449, #2446.

`ShellExecutor` scrubs a blocklist of credential environment variables from the subprocess environment before spawning. The blocklist covers common credential patterns: `AWS_*`, `GITHUB_TOKEN`, `ZEPH_*`, `OPENAI_API_KEY`, etc.

MCP server stdio env is also filtered: the blocklist is extended for MCP child processes to prevent credential leakage via `getenv()` in tool implementations.

### Key Invariants
- Blocklist is applied unconditionally for shell and MCP stdio subprocesses — no opt-out
- Audit logger silent-drop bug fixed: every audit write failure must be logged, not silently ignored
- `ZEPH_*` env vars must never appear in subprocess environments — they contain vault-resolved secrets
