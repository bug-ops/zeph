# Security

Zeph implements defense-in-depth security for safe AI agent operations in production environments.

## Age Vault

Zeph can store secrets in an [age](https://age-encryption.org/)-encrypted vault file instead of environment variables. This is the recommended approach for production and shared environments.

### Setup

```bash
zeph vault init                        # generate keypair + empty vault
zeph vault set ZEPH_CLAUDE_API_KEY sk-ant-...
zeph vault set ZEPH_TELEGRAM_TOKEN 123456:ABC...
zeph vault list                        # show stored keys
zeph vault get ZEPH_CLAUDE_API_KEY     # retrieve a value
zeph vault rm ZEPH_CLAUDE_API_KEY      # remove a key
```

Enable the vault backend in config:

```toml
[vault]
backend = "age"
```

The vault file path defaults to `~/.zeph/vault.age`. The private key path defaults to `~/.zeph/key.txt`.

### Custom Secrets

Beyond built-in provider keys, you can store arbitrary secrets for skill authentication using the `ZEPH_SECRET_` prefix:

```bash
zeph vault set ZEPH_SECRET_GITHUB_TOKEN ghp_yourtokenhere
zeph vault set ZEPH_SECRET_STRIPE_KEY sk_live_...
```

Skills declare which secrets they require via `x-requires-secrets` in their frontmatter. Skills with unsatisfied secrets are excluded from the prompt automatically — they will not be matched or executed until the secret is available.

When a skill with `x-requires-secrets` is active, its secrets are injected as environment variables into shell commands it runs. The prefix is stripped and the name is uppercased:

| Vault key | Env var injected |
|-----------|-----------------|
| `ZEPH_SECRET_GITHUB_TOKEN` | `GITHUB_TOKEN` |
| `ZEPH_SECRET_STRIPE_KEY` | `STRIPE_KEY` |

Only the secrets declared by the currently active skill are injected — not all vault secrets.

See [Add Custom Skills — Secret-Gated Skills](../guides/custom-skills.md#secret-gated-skills) for how to declare requirements in a skill.

### Docker

Mount the vault and key files as read-only volumes:

```yaml
volumes:
  - ~/.zeph/vault.age:/home/zeph/.zeph/vault.age:ro
  - ~/.zeph/key.txt:/home/zeph/.zeph/key.txt:ro
```

## Shell Command Filtering

All shell commands from LLM responses pass through a security filter before execution. Shell command detection uses a tokenizer-based pipeline that splits input into tokens, handles wrapper commands (e.g., `env`, `nohup`, `timeout`), and applies word-boundary matching against blocked patterns. This replaces the prior substring-based approach for more accurate detection with fewer false positives. Commands matching blocked patterns are rejected with detailed error messages.

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

### Known Limitations

`find_blocked_command` operates on tokenized command text and cannot detect blocked commands embedded inside indirect execution constructs:

| Construct | Example | Why it bypasses |
|-----------|---------|-----------------|
| Process substitution | `diff <(sudo cat /etc/shadow) file` | `<(...)` content is passed to the shell, not parsed by the tokenizer |
| Here-strings | `bash <<< 'sudo rm -rf /'` | The payload string is opaque to the filter |
| `eval` / `bash -c` / `sh -c` | `eval 'sudo rm -rf /'` | String argument is not parsed |
| Variable expansion | `cmd=sudo; $cmd rm -rf /` | Variables are not resolved during tokenization |

**Mitigation:** The default `confirm_patterns` in `ShellConfig` include `<(`, `>(`, `<<<`, `eval `, `$(`, and `` ` `` — commands containing these constructs trigger a confirmation prompt before execution. For high-security deployments, complement this filter with OS-level sandboxing (Linux namespaces, seccomp, or similar).

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
- Default patterns: `rm`, `git push -f`, `git push --force`, `drop table`, `drop database`, `truncate`, `$(`, `` ` ``, `<(`, `>(`, `<<<`, `eval`
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

## Autonomy Levels

The `security.autonomy_level` setting controls the agent's tool access scope:

| Level | Tools Available | Confirmations |
|-------|----------------|---------------|
| `readonly` | `file_read`, `file_glob`, `file_grep`, `web_scrape` | N/A (write tools hidden) |
| `supervised` | All tools per permission policy | Yes, for destructive patterns |
| `full` | All tools | No confirmations |

Default is `supervised`. In `readonly` mode, write-capable tools are excluded from the LLM system prompt and rejected at execution time (defense-in-depth).

```toml
[security]
autonomy_level = "supervised"  # readonly, supervised, full
```

## Permission Policy

The `[tools.permissions]` config section provides fine-grained, pattern-based access control for each tool. Rules are evaluated in order (first match wins) using case-insensitive glob patterns against the tool input. See [Tool System — Permissions](../advanced/tools.md#permissions) for configuration details.

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

LLM responses are scanned for secret patterns using compiled regexes before display:

- Detected prefixes: `sk-`, `AKIA`, `ghp_`, `gho_`, `xoxb-`, `xoxp-`, `sk_live_`, `sk_test_`, `-----BEGIN`, `AIza` (Google API), `glpat-` (GitLab), `hf_` (HuggingFace), `npm_` (npm), `dckr_pat_` (Docker)
- Regex-based matching replaces detected secrets with `[REDACTED]`, preserving original whitespace formatting
- Enabled by default (`security.redact_secrets = true`), applied to both streaming and non-streaming responses

## Credential Scrubbing in Context

In addition to output redaction, Zeph scrubs credential patterns from conversation history **before** injecting it into the LLM context window. The `scrub_content()` function in the context builder detects the same secret prefixes and replaces them with `[REDACTED]`. This prevents credentials that appeared in past messages from leaking into future LLM prompts.

```toml
[memory]
redact_credentials = true  # default: true
```

This is independent of `security.redact_secrets` — output redaction sanitizes LLM *responses*, while credential scrubbing sanitizes LLM *inputs* from stored history.

## Config Validation

`Config::validate()` enforces upper bounds at startup to catch configuration errors early:

- `memory.history_limit` <= 10,000
- `memory.context_budget_tokens` <= 1,000,000 (when non-zero)
- `agent.max_tool_iterations` <= 100
- `a2a.rate_limit` > 0
- `gateway.rate_limit` > 0

The agent exits with an error message if any bound is violated.

## Timeout Policies

Configurable per-operation timeouts prevent hung connections:

```toml
[timeouts]
llm_seconds = 120       # LLM chat completion
embedding_seconds = 30  # Embedding generation
a2a_seconds = 30        # A2A remote calls
```

## A2A and Gateway Bearer Authentication

Both the A2A server and the HTTP gateway use bearer token authentication backed by constant-time comparison (`subtle::ConstantTimeEq`) to prevent timing side-channel attacks.

### A2A Server

Configure via `config.toml` or environment variable:

```toml
[a2a]
auth_token = "secret"  # or use vault: ZEPH_A2A_AUTH_TOKEN
```

The `/.well-known/agent-card.json` endpoint is intentionally public and bypasses auth to allow agent discovery.

If `auth_token` is `None` at startup, the server logs a `WARN`-level message:

```
WARN zeph_a2a: A2A server started without auth_token — endpoint is unauthenticated
```

### HTTP Gateway

Configure via `config.toml` or environment variable:

```toml
[gateway]
auth_token = "secret"  # or use vault: ZEPH_GATEWAY_TOKEN
```

The `GET /health` endpoint is intentionally public and bypasses auth.

If `auth_token` is `None` at startup, the server logs a `WARN`-level message:

```
WARN zeph_gateway: Gateway started without auth_token — endpoint is unauthenticated
```

**Recommendation:** Always set `auth_token` when binding to a non-loopback interface. Use the [Age Vault](security.md#age-vault) to store the token rather than embedding it in plain text in `config.toml`.

## SSRF Protection for Web Scraping

`WebScrapeExecutor` defends against Server-Side Request Forgery (SSRF) at every stage of a request, including multi-hop redirect chains.

### URL Validation

Before any network connection is made, `validate_url` checks:

- **HTTPS only:** HTTP, `file://`, `javascript:`, `data:`, and all other schemes are rejected with `ToolError::Blocked`.
- **Private hostnames:** The following hostname patterns are blocked regardless of DNS resolution:
  - `localhost` and `*.localhost` subdomains
  - `*.internal` TLD (cloud/Kubernetes internal DNS)
  - `*.local` TLD (mDNS/Bonjour)
  - IPv4 literals in RFC 1918 ranges (`10.x.x.x`, `172.16–31.x.x`, `192.168.x.x`)
  - IPv4 link-local (`169.254.x.x`), loopback (`127.x.x.x`), unspecified (`0.0.0.0`), and broadcast (`255.255.255.255`)
  - IPv6 loopback (`::1`), link-local (`fe80::/10`), unique-local (`fc00::/7`), and unspecified (`::`)
  - IPv4-mapped IPv6 addresses (`::ffff:x.x.x.x`) — the inner IPv4 is checked against all private ranges above

### DNS Rebinding Prevention

After URL validation, `resolve_and_validate` performs a DNS lookup and checks every returned IP address against the same private-range rules. The validated socket addresses are then pinned to the `reqwest` client via `resolve_to_addrs`, eliminating the TOCTOU window between DNS validation and the actual TCP connection.

If DNS resolves to a private IP, the request is rejected with:

```
ToolError::Blocked { command: "SSRF protection: private IP <ip> for host <host>" }
```

### Redirect Chain Defense

`WebScrapeExecutor` disables `reqwest`'s automatic redirect following (`redirect::Policy::none()`). Redirects are followed manually, up to a limit of **3 hops**. For every redirect:

1. The `Location` header value is extracted.
2. Relative URLs are resolved against the current request URL.
3. `validate_url` runs on the resolved target — blocking private hostnames and non-HTTPS schemes.
4. `resolve_and_validate` runs on the target — blocking DNS-based rebinding.
5. A new `reqwest` client is built, pinned to the validated addresses for the next hop.

This prevents the classic "open redirect to internal service" SSRF bypass: even if the initial URL passes validation, a redirect to `https://169.254.169.254/` (AWS metadata endpoint) or `https://10.0.0.1/` is blocked before the connection is made.

If more than 3 redirects occur, the request fails with `ToolError::Execution("too many redirects")`.

## A2A Network Security

- **TLS enforcement:** `a2a.require_tls = true` rejects HTTP endpoints (HTTPS only)
- **SSRF protection:** `a2a.ssrf_protection = true` blocks private IP ranges (RFC 1918, loopback, link-local) via DNS resolution
- **Payload limits:** `a2a.max_body_size` caps request body (default: 1 MiB)

**Safe execution model:**
- Commands parsed for blocked patterns, then sandbox-validated, then confirmation-checked
- Timeout enforcement (default: 30s, configurable)
- Full errors logged to system; user-facing messages pass through `sanitize_paths()` which replaces absolute filesystem paths (`/home/`, `/Users/`, `/root/`, `/tmp/`, `/var/`) with `[PATH]` to prevent information disclosure
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

## Secret Memory Hygiene

Zeph uses the [`zeroize`](https://crates.io/crates/zeroize) crate to ensure that secret material is erased from process memory as soon as it is no longer needed.

**`Secret` type:**

```rust
// Internal representation — wraps Zeroizing<String> instead of plain String
Secret(Zeroizing<String>)
```

`Zeroizing<T>` implements `Drop` to overwrite heap memory with zeros before deallocation, preventing secrets from lingering in freed pages.

**`AgeVaultProvider`:**

All decrypted values in the in-memory secrets map are stored as `BTreeMap<String, Zeroizing<String>>`. Using `BTreeMap` instead of `HashMap` ensures that secrets are serialized in deterministic key order when `vault.save()` re-encrypts the vault. This makes repeated save operations produce consistent JSON output, which is important for diffing and auditing encrypted vault changes. Key-file content and intermediate decrypt buffers are also wrapped in `Zeroizing` so they are cleared when the local binding is dropped.

**`Clone` intentionally removed:**

`Secret` no longer derives `Clone`. This is a deliberate trade-off: preventing accidental cloning reduces the number of live copies of a secret value in memory at any given time.

If you need to pass a secret to a function, accept `&Secret` or extract the inner `&str` directly rather than cloning.

## Code Security

Rust-native memory safety guarantees:

- **Workspace-level `unsafe` ban:** `unsafe_code = "deny"` is set in `[workspace.lints.rust]` in the root `Cargo.toml`, propagating the restriction to every crate in the workspace automatically. The single audited exception is an `#[allow(unsafe_code)]`-annotated block behind the `candle` feature flag for memory-mapped safetensors loading.
- **No panic in production:** `unwrap()` and `expect()` linted via clippy
- **Reduced attack surface:** Unused database backends (MySQL) and transitive dependencies (RSA) are excluded from the build
- **Secure dependencies:** All crates audited with `cargo-deny`
- **MSRV policy:** Rust 1.88+ (Edition 2024) for latest security patches

## Reporting Vulnerabilities

Do not open a public issue. Use [GitHub Security Advisories](https://github.com/bug-ops/zeph/security/advisories/new) to submit a private report.

Include: description, steps to reproduce, potential impact, suggested fix. Expect an initial response within 72 hours.
