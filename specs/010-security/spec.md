# Spec: Security

## Sources

### External
- **OWASP AI Agent Security Cheat Sheet** (2026): https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html
- **Prompt Injection Defenses** (Anthropic, 2025) — spotlighting, context sandboxing, dual-LLM (QuarantinedSummarizer): https://www.anthropic.com/research/prompt-injection-defenses
- **How Microsoft Defends Against Indirect Prompt Injection** (MSRC, 2025) — TrustLevel/ContentSource model: https://www.microsoft.com/en-us/msrc/blog/2025/07/how-microsoft-defends-against-indirect-prompt-injection-attacks
- **Indirect Prompt Injection Survey** (2025): https://arxiv.org/html/2506.08837v1
- **Log-To-Leak: Prompt Injection via MCP** (2025) — tool description sanitization at registration: https://openreview.net/forum?id=UVgbFuXPaO
- **Policy Compiler for Secure Agentic Systems** (Feb 2026) — PolicyEnforcer, PermissionPolicy design: https://arxiv.org/html/2602.16708v2
- **Llama Guard** (Meta AI, 2023) — GuardrailFilter classifier prompt, SAFE/UNSAFE prefix: https://arxiv.org/abs/2312.06674

### Internal
| File | Contents |
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
