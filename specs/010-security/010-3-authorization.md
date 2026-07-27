---
aliases:
  - Authorization
  - Capability-Based Access Control
  - Shell Sandbox
  - SSRF Protection
  - Permission Policy
tags:
  - sdd
  - spec
  - security
  - contract
created: 2026-04-10
status: complete
related:
  - "[[010-security/spec]]"
  - "[[010-1-vault]]"
  - "[[010-2-injection-defense]]"
  - "[[010-4-audit]]"
  - "[[006-tools/spec]]"
  - "[[008-3-security]]"
---

# Spec: Authorization & Capability-Based Access Control

Permission policy enforcement, shell sandbox blocklist, SSRF protection, tool authorization.

## Overview

Zeph's authorization layer enforces what operations the agent is allowed to perform. It includes capability-based access control (what tools can run), shell sandbox restrictions (which commands are blocked), and SSRF protection (which URLs are reachable).

## Key Invariants

**Always:**
- All tool execution checked against `PolicyEnforcer` deny/allow rules before execution
- Shell commands checked against the blocklist unconditionally — **before** `PermissionPolicy` evaluation
- HTTP requests validated via `validate_url()` — private IP ranges blocked by default
- Authorization failures logged to audit trail with full context

**Never:**
- Bypass blocklist checks for "trusted" tools — blocklist is unconditional
- Allow shell execution without sandbox validation
- Make HTTP requests to private IP ranges without explicit allowlist

## Declarative Policy Compiler

`PolicyEnforcer` (`crates/zeph-tools/src/policy.rs`) evaluates TOML-based access-control rules with deny-first semantics:

```rust
pub struct PolicyEnforcer { /* ... */ }

#[non_exhaustive]
pub enum PolicyDecision {
    Allow { trace: String },   // Rule matched; execution allowed
    Deny { trace: String },    // Rule matched; execution denied
}

impl PolicyEnforcer {
    /// Evaluate policy rules against tool call context.
    ///
    /// Deny rules checked first. If a deny rule matches, returns `Deny`.
    /// Otherwise, checks allow rules. If no rule matches, uses `default_effect`.
    pub fn evaluate(
        &self,
        tool_name: &str,
        params: &serde_json::Map<String, serde_json::Value>,
        context: &PolicyContext,
    ) -> PolicyDecision { /* ... */ }
}

pub struct PolicyContext {
    pub trust_level: SkillTrustLevel,
    pub env: std::collections::HashMap<String, String>,
}
```

**Rule matching** (all conditions are AND'd):
- `effect`: `"allow"` or `"deny"`
- `tool`: glob pattern matching tool name (e.g., `"read_*"`, `"rm"`)
- `paths`: glob patterns matched against path-like parameters; rule fires if ANY matches
- `env`: environment variable names that must ALL be present for rule to apply
- `trust_level`: minimum required trust level (`Trusted` > `Neutral` > `Untrusted`)
- `args_match`: regex matched against individual string parameter values
- `capabilities`: named capabilities associated with this rule (for auditing/metadata)

**Semantics**: deny rules are evaluated first; matching deny → `Deny`. If no deny matches, evaluate allow rules; matching allow → `Allow`. If no rule matches, use `default_effect` (typically `Deny`).

**Config example**:
```toml
[[tools.policy]]
effect = "deny"
tool = "rm"                      # Glob pattern
# Blocks all rm invocations (handled specially by shell blocklist anyway)

[[tools.policy]]
effect = "allow"
tool = "read_file"
paths = ["/home/*/documents/*"]  # Glob patterns on paths
trust_level = "trusted"          # Only for trusted skill callers
```

## Shell Sandbox Blocklist

`ShellExecutor` enforces an unconditional blocklist before spawning shell commands. The blocklist runs **before** PermissionPolicy evaluation:

**Hardcoded blocklist** (`crates/zeph-tools/src/shell/mod.rs`):
```
sudo, mkfs, dd if=, curl, wget, nc, ncat, netcat, shutdown, reboot, halt
```

Any invocation containing one of these patterns (case-sensitive, as a substring or token) is blocked unconditionally.

**Special handling for `rm`**:
- `rm -rf` is **allowed** only when all three conditions are met:
  - Operating on relative paths (e.g., `rm -rf ./tempdir`)
  - NOT on `.git/worktrees`, root, or `$HOME`
  - NOT on absolute paths
- Example: `rm -rf /` is blocked; `rm -rf ./temp` is allowed (subject to policy checks)

**Limitations** (documented in code):
- Bypass via indirect invocation: `bash -c "rm ..."` — the `-c` argument is not scanned for blocked patterns
- Bypass via variable indirection: `cmd=rm; $cmd file` — the shell variable is not expanded for scanning
- The blocklist is a **first-pass defense**, not comprehensive; it mitigates common attacks but does not prevent all privilege escalation attempts

Blocklist validation is unconditional; all other shell commands then pass through `PolicyEnforcer` for fine-grained access control.

## SSRF Protection

`validate_url()` (`crates/zeph-tools/src/net.rs`) blocks requests to private IP ranges and loopback addresses:

```rust
/// Validate a URL for SSRF attacks.
///
/// Blocks all private IP ranges, localhost, link-local, and non-HTTPS schemes:
/// - IPv4: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 127.0.0.0/8, 0.0.0.0, 255.255.255.255
/// - IPv6: ::1, fc00::/7, fe80::/10
/// - Schemes: only `https://` allowed; `http://`, `file://`, `ftp://`, `data://`, `javascript://` blocked
pub fn validate_url(raw: &str) -> Result<Url, ToolError>;
```

**Redirect handling**: each redirect target is validated independently — a redirect chain where any hop points to a private IP fails at that hop.

**Applied to**:
- `WebScrapeExecutor` (`scrape` tool) — validates URLs before fetching; validates each redirect target
- Redirect chains: all hops must pass validation; a 302 to `http://localhost/` is caught and fails

**Allowlist**: no allowlist exists for SSRF validation; the deny-all private-IP policy is not configurable.

## Credential Environment Variable Scrubbing

`ShellExecutor` filters environment variables via a configurable `env_blocklist` before spawning subprocess commands:

```rust
pub struct ShellExecutor {
    env_blocklist: Vec<String>,  // Env var names to strip from subprocess (prefix match)
}
```

The blocklist is applied inline during command construction:
1. User provides extra env vars (e.g., API keys for a script)
2. ShellExecutor constructs subprocess env by filtering the parent's env + extra vars
3. Any env var matching a prefix in `env_blocklist` is stripped

**Default blocklist** (from config `[tools.shell].env_blocklist`):
- Typically includes: `ZEPH_*`, `AWS_*`, `GITHUB_*`, `OPENAI_*`, `ANTHROPIC_*`, etc.

Blocklist filtering is unconditional and applied at subprocess spawn time, not at config time. MCP stdio servers inherit the same env filtering policy.

## Integration Points

- [[008-mcp/spec]] — MCP tool allowlist + policy enforcement
- [[006-tools/spec]] — tool registry + authorization binding
- [[010-4-audit]] — authorization violations logged to audit trail
- Web executor — SSRF validation on all HTTP requests
- MCP OAuth — OAuth flows validated via `validate_oauth_metadata_urls()`

## See Also

- [[010-security/spec]] — Parent; shell blocklist and SSRF noted as unconditional
- [[010-2-injection-defense]] — guardrail filtering runs before tool dispatch
- [[008-3-security]] — MCP tool allowlist enforcement
