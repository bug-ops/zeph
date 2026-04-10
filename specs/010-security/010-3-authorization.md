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
  - "[[006-tools]]"
  - "[[008-3-security]]"
---

# Spec: Authorization & Capability-Based Access Control

Permission policy enforcement, shell sandbox blocklist, SSRF protection, tool authorization.

## Overview

Zeph's authorization layer enforces what operations the agent is allowed to perform. It includes capability-based access control (what tools can run), shell sandbox restrictions (which commands are blocked), and SSRF protection (which URLs are reachable).

## Key Invariants

**Always:**
- All tool execution requires authorization check against policy
- Shell commands checked against blocklist before execution
- HTTP requests checked for SSRF patterns (localhost, private ranges)
- Authorization failures logged with full context

**Never:**
- Bypass authorization checks for "trusted" tools
- Allow shell execution without sandbox validation
- Make HTTP requests to private IP ranges without explicit allow-list

## Capability-Based Access Control

Policies define which agents can execute which tools:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    ToolRead,           // read-only tools
    ToolWrite,          // file write, API modify
    ShellExecute,       // shell commands
    HttpRequest,        // HTTP/HTTPS requests
    NetworkListen,      // open ports
    ProcessManage,      // spawn/kill processes
}

pub struct AuthPolicy {
    // agent_id → capabilities
    grants: HashMap<String, HashSet<Capability>>,
    // capability → individual tool allows
    tool_overrides: HashMap<String, HashSet<String>>,
}

impl AuthPolicy {
    async fn check_authorization(
        &self,
        agent_id: &str,
        tool_name: &str,
        required_capability: Capability,
    ) -> Result<()> {
        // 1. Check if agent has capability
        let capabilities = self.grants
            .get(agent_id)
            .ok_or_else(|| anyhow!("Agent {} not in policy", agent_id))?;
        
        if !capabilities.contains(&required_capability) {
            return Err(anyhow!(
                "Agent {} lacks capability {:?}",
                agent_id,
                required_capability
            ));
        }
        
        // 2. Check tool-level override (e.g., "shell_execute" allowed but "rm -rf" blocked)
        if let Some(allowed_tools) = self.tool_overrides.get(tool_name) {
            if !allowed_tools.contains(agent_id) {
                return Err(anyhow!(
                    "Tool '{}' not in allow-list for agent {}",
                    tool_name,
                    agent_id
                ));
            }
        }
        
        Ok(())
    }
}
```

## Shell Sandbox

Blocklist of dangerous commands:

```rust
pub struct ShellSandbox {
    blocklist: Vec<ShellPattern>,
}

#[derive(Clone)]
pub struct ShellPattern {
    pattern: Regex,
    reason: &'static str,
}

impl ShellSandbox {
    fn new() -> Self {
        Self {
            blocklist: vec![
                // Destructive commands
                ShellPattern {
                    pattern: Regex::new(r"^\s*(rm|rmdir|dd)\s+-rf").unwrap(),
                    reason: "recursive deletion blocked",
                },
                ShellPattern {
                    pattern: Regex::new(r":(){ :|:|");").unwrap(),
                    reason: "fork bomb detected",
                },
                // Privilege escalation
                ShellPattern {
                    pattern: Regex::new(r"^sudo\s+|/etc/sudoers").unwrap(),
                    reason: "privilege escalation blocked",
                },
                // System modification
                ShellPattern {
                    pattern: Regex::new(r"^\s*(chmod|chown|passwd|usermod)").unwrap(),
                    reason: "system modification blocked",
                },
            ],
        }
    }
    
    fn validate_command(&self, cmd: &str) -> Result<()> {
        for pattern in &self.blocklist {
            if pattern.pattern.is_match(cmd) {
                return Err(anyhow!("{}", pattern.reason));
            }
        }
        Ok(())
    }
}
```

## SSRF Protection

Prevent requests to internal services:

```rust
pub struct SsrfValidator {
    blocked_ranges: Vec<IpNetwork>,
    allow_list: Vec<String>,  // explicit allowed domains
}

impl SsrfValidator {
    fn new() -> Self {
        Self {
            blocked_ranges: vec![
                // Private IPv4
                "127.0.0.0/8".parse().unwrap(),      // loopback
                "169.254.0.0/16".parse().unwrap(),   // link-local
                "10.0.0.0/8".parse().unwrap(),       // private
                "172.16.0.0/12".parse().unwrap(),    // private
                "192.168.0.0/16".parse().unwrap(),   // private
                // IPv6
                "::1/128".parse().unwrap(),           // loopback
                "fc00::/7".parse().unwrap(),          // private
                "fe80::/10".parse().unwrap(),         // link-local
            ],
            allow_list: vec![],
        }
    }
    
    async fn validate_url(&self, url: &str) -> Result<()> {
        let parsed = url::Url::parse(url)?;
        
        // 1. Check allow-list first
        if let Some(domain) = parsed.domain() {
            if self.allow_list.contains(&domain.to_string()) {
                return Ok(());
            }
        }
        
        // 2. Resolve hostname
        let addr = tokio::net::lookup_host(
            format!("{}:{}", parsed.host_str().ok_or("no host")?, 
                            parsed.port().unwrap_or(443))
        ).await?
            .next()
            .ok_or("hostname resolution failed")?;
        
        // 3. Check IP against blocked ranges
        for range in &self.blocked_ranges {
            if range.contains(addr.ip()) {
                return Err(anyhow!(
                    "SSRF blocked: {} resolves to private IP {}",
                    parsed.host_str().unwrap_or("?"),
                    addr.ip()
                ));
            }
        }
        
        Ok(())
    }
}
```

## Configuration

```toml
[security.authorization]
# Capability grants per agent
[[security.authorization.agents]]
id = "primary_agent"
capabilities = ["ToolRead", "ToolWrite", "HttpRequest"]

[[security.authorization.agents]]
id = "sandbox_agent"
capabilities = ["ToolRead"]

# Tool allow-lists (tool → allowed agents)
[security.authorization.tool_overrides]
shell_execute = ["primary_agent"]
file_delete = ["primary_agent"]
network_listen = []

# Shell sandbox
[security.sandbox]
enabled = true
blocklist = [
  "^\\s*(rm|rmdir)\\s+-rf",
  ":(){ :|:|;",
  "^sudo\\s+",
]

# SSRF protection
[security.ssrf]
enabled = true
blocked_ranges = ["127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
allow_list = ["api.example.com", "internal.trusted.service"]
```

## Integration Points

- [[006-tools]] — Tool execution calls authorization check
- [[008-3-security]] — OAP authorization for MCP tools
- [[010-4-audit]] — Authorization failures logged

## See Also

- [[010-security/spec]] — Parent
- [[006-tools]] — ToolExecutor enforces authorization
- [[008-3-security]] — OAP authorization for MCP
- [[010-4-audit]] — Audit trail of authorization checks
