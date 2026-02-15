# Security

Zeph implements defense-in-depth security for safe AI agent operations in production environments.

## Shell Command Filtering

All shell commands from LLM responses pass through a security filter before execution. Commands matching blocked patterns are rejected with detailed error messages.

**12 blocked patterns by default:**

| Pattern | Risk Category | Examples |
|---------|---------------|----------|
| `rm -rf /`, `rm -rf /*` | Filesystem destruction | Prevents accidental system wipe |
| `sudo`, `su` | Privilege escalation | Blocks unauthorized root access |
| `mkfs`, `fdisk` | Filesystem operations | Prevents disk formatting |
| `dd if=`, `dd of=` | Low-level disk I/O | Blocks dangerous write operations |
| `curl \| bash`, `wget \| sh` | Arbitrary code execution | Prevents remote code injection |
| `nc`, `ncat`, `netcat` | Network backdoors | Blocks reverse shell attempts |
| `shutdown`, `reboot`, `halt` | System control | Prevents service disruption |

**Configuration:**
```toml
[tools.shell]
timeout = 30
blocked_commands = ["custom_pattern"]  # Additional patterns (additive to defaults)
allowed_paths = ["/home/user/workspace"]  # Restrict filesystem access
allow_network = true  # false blocks curl/wget/nc
confirm_patterns = ["rm ", "git push -f"]  # Destructive command patterns
```

Custom blocked patterns are **additive** — you cannot weaken default security. Matching is case-insensitive.

## Shell Sandbox

Commands are validated against a configurable filesystem allowlist before execution:

- `allowed_paths = []` (default) restricts access to the working directory only
- Paths are canonicalized to prevent traversal attacks (`../../etc/passwd`)
- Relative paths containing `..` segments are rejected before canonicalization as an additional defense layer
- `allow_network = false` blocks network tools (`curl`, `wget`, `nc`, `ncat`, `netcat`)

## Destructive Command Confirmation

Commands matching `confirm_patterns` trigger an interactive confirmation before execution:

- **CLI:** `y/N` prompt on stdin
- **Telegram:** inline keyboard with Confirm/Cancel buttons
- Default patterns: `rm`, `git push -f`, `git push --force`, `drop table`, `drop database`, `truncate`
- Configurable via `tools.shell.confirm_patterns` in TOML

## File Executor Sandbox

`FileExecutor` enforces the same `allowed_paths` sandbox as the shell executor for all file operations (`read`, `write`, `edit`, `glob`, `grep`).

**Path validation:**
- All paths are resolved to absolute form and canonicalized before access
- Non-existing paths (e.g., for `write`) use ancestor-walk canonicalization: the resolver walks up the path tree to the nearest existing ancestor, canonicalizes it, then re-appends the remaining segments. This prevents symlink and `..` traversal on paths that do not yet exist on disk
- If the resolved path does not fall under any entry in `allowed_paths`, the operation is rejected with a `SandboxViolation` error

**Glob and grep enforcement:**
- `glob` results are post-filtered: matched paths outside the sandbox are silently excluded
- `grep` validates the search root directory before scanning begins

**Configuration** is shared with the shell sandbox:
```toml
[tools.shell]
allowed_paths = ["/home/user/workspace"]  # Empty = cwd only
```

## Permission Policy

The `[tools.permissions]` config section provides fine-grained, pattern-based access control for each tool. Rules are evaluated in order (first match wins) using case-insensitive glob patterns against the tool input. See [Tool System — Permissions](guide/tools.md#permissions) for configuration details.

Key security properties:
- Tools with all-deny rules are excluded from the LLM system prompt, preventing the model from attempting to use them
- Legacy `blocked_commands` and `confirm_patterns` are auto-migrated to equivalent permission rules when `[tools.permissions]` is absent
- Default action when no rule matches is `Ask` (confirmation required)

## Audit Logging

Structured JSON audit log for all tool executions:

```toml
[tools.audit]
enabled = true
destination = "./data/audit.jsonl"  # or "stdout"
```

Each entry includes timestamp, tool name, command, result (success/blocked/error/timeout), and duration in milliseconds.

## Secret Redaction

LLM responses are scanned for common secret patterns before display:

- Detected patterns: `sk-`, `AKIA`, `ghp_`, `gho_`, `xoxb-`, `xoxp-`, `sk_live_`, `sk_test_`, `-----BEGIN`, `AIza` (Google API), `glpat-` (GitLab)
- Secrets replaced with `[REDACTED]` preserving original whitespace formatting
- Enabled by default (`security.redact_secrets = true`), applied to both streaming and non-streaming responses

## Timeout Policies

Configurable per-operation timeouts prevent hung connections:

```toml
[timeouts]
llm_seconds = 120       # LLM chat completion
embedding_seconds = 30  # Embedding generation
a2a_seconds = 30        # A2A remote calls
```

## A2A Network Security

- **TLS enforcement:** `a2a.require_tls = true` rejects HTTP endpoints (HTTPS only)
- **SSRF protection:** `a2a.ssrf_protection = true` blocks private IP ranges (RFC 1918, loopback, link-local) via DNS resolution
- **Payload limits:** `a2a.max_body_size` caps request body (default: 1 MiB)

**Safe execution model:**
- Commands parsed for blocked patterns, then sandbox-validated, then confirmation-checked
- Timeout enforcement (default: 30s, configurable)
- Full errors logged to system, sanitized messages shown to users
- Audit trail for all tool executions (when enabled)

## Container Security

| Security Layer | Implementation | Status |
|----------------|----------------|--------|
| **Base image** | Oracle Linux 9 Slim | Production-hardened |
| **Vulnerability scanning** | Trivy in CI/CD | **0 HIGH/CRITICAL CVEs** |
| **User privileges** | Non-root `zeph` user (UID 1000) | Enforced |
| **Attack surface** | Minimal package installation | Distroless-style |

**Continuous security:**
- Every release scanned with [Trivy](https://trivy.dev/) before publishing
- Automated Dependabot PRs for dependency updates
- `cargo-deny` checks in CI for license/vulnerability compliance

## Code Security

Rust-native memory safety guarantees:

- **Minimal `unsafe`:** One audited `unsafe` block behind `candle` feature flag (memory-mapped safetensors loading). Core crates enforce `#![deny(unsafe_code)]`
- **No panic in production:** `unwrap()` and `expect()` linted via clippy
- **Reduced attack surface:** Unused database backends (MySQL) and transitive dependencies (RSA) are excluded from the build
- **Secure dependencies:** All crates audited with `cargo-deny`
- **MSRV policy:** Rust 1.88+ (Edition 2024) for latest security patches

## Reporting Vulnerabilities

Do not open a public issue. Use [GitHub Security Advisories](https://github.com/bug-ops/zeph/security/advisories/new) to submit a private report.

Include: description, steps to reproduce, potential impact, suggested fix. Expect an initial response within 72 hours.
