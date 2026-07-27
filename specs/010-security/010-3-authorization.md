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
- HTTP requests validated via `validate_url_ssrf()` — private IP ranges blocked by default
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
- `tools`: glob pattern over tool name
- `paths`: glob patterns over extracted path parameters (e.g., `file` argument)
- `env_required`: environment variables that must be present
- `trust_threshold`: minimum trust level required (`Trusted` > `Neutral` > `Untrusted`)
- `args_regex`: regex over individual string parameter values

**Semantics**: deny rules are evaluated first; matching deny → `Deny`. If no deny matches, evaluate allow rules; matching allow → `Allow`. If no rule matches, use `default_effect` (typically `Deny`).

Config:
```toml
[[tools.policy]]
effect = "deny"
tools = "rm *"                   # Glob pattern
reason = "Data destruction blocked"

[[tools.policy]]
effect = "allow"
tools = "read_file"
paths = ["/home/*/documents/*"]  # Glob patterns on paths
trust_threshold = "trusted"
```

## Shell Sandbox Blocklist

`ShellExecutor` enforces an unconditional blocklist of dangerous patterns before spawning shell commands:

```rust
pub struct ShellExecutor { /* ... */ }

impl ShellExecutor {
    /// Execute a shell command with blocklist checks.
    ///
    /// Blocklist validation runs unconditionally, before PermissionPolicy checks.
    pub async fn execute(&self, cmd: &str, ...) -> Result<String> {
        // 1. Check command against blocklist
        self.find_blocked_command(cmd)?;  // Panics if blocked pattern found
        
        // 2. Then evaluate PermissionPolicy (if configured)
        // 3. Scrub credential env vars from subprocess environment
        // 4. Spawn process
    }
}
```

**Blocked patterns** (`crates/zeph-tools/src/filter/security.rs`):
- Process substitution: `$(...)`, `` `...` ``
- Here-strings: `<<<`
- Destructive: `rm -rf`, `dd if=`, `mkfs`
- Fork bombs: `:(){ :|: }`
- Privilege escalation: `sudo`, `/etc/sudoers`, `su -`

Bypass attempts via arguments are also caught — passing a blocked pattern as an argument value is detected and blocked.

## SSRF Protection

`validate_url_ssrf()` (`crates/zeph-gateway/src/transport/ssrf.rs`, etc.) blocks requests to private IP ranges and loopback addresses:

```rust
/// Validate a URL for SSRF attacks.
///
/// Blocks: localhost, link-local (169.254.x), private ranges (10.x, 172.16-31.x, 192.168.x),
/// IPv6 loopback/private (::1, fc00::/7, fe80::/10).
pub fn validate_url_ssrf(url: &str) -> Result<(), SsrfError>;
```

**Redirect chains**: each redirect target in the chain is also validated — a redirect to a private IP is caught and fails.

**Applied to**:
- `WebScrapeExecutor` — all HTTP fetches
- `zeph-gateway` HTTP webhook receiver — incoming URLs validated
- `zeph-a2a` client — remote agent URLs validated
- MCP OAuth — OAuth metadata endpoint URLs validated via `validate_oauth_metadata_urls()`

Allowlist capability exists (via config `ssrf_allowlist`), but defaults to deny-all private IPs; allowlist is opt-in and explicit.

## Credential Environment Variable Scrubbing

`ShellExecutor` and MCP stdio server spawning scrub credential env vars from the subprocess environment:

```rust
impl ShellExecutor {
    async fn scrub_environment(&self, env: &mut HashMap<String, String>) {
        // Blocklist: AWS_*, GITHUB_*, ZEPH_*, OPENAI_API_KEY, etc.
        // Remove from subprocess environment to prevent getenv() leakage
    }
}
```

Blocklist is unconditional and cannot be disabled per-command. MCP stdio servers inherit the same scrubbed environment.

## Integration Points

- [[008-mcp/spec]] — MCP tool allowlist + policy enforcement
- [[006-tools/spec]] — tool registry + authorization binding
- [[010-4-audit]] — authorization violations logged to audit trail
- Web executor — SSRF validation on all HTTP requests
- Gateway — OAuth flows validated via `validate_oauth_metadata_urls()`

## See Also

- [[010-security/spec]] — Parent; shell blocklist and SSRF noted as unconditional
- [[010-2-injection-defense]] — guardrail filtering runs before tool dispatch
- [[008-3-security]] — MCP tool allowlist enforcement
