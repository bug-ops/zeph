# zeph-tools

[![Crates.io](https://img.shields.io/crates/v/zeph-tools)](https://crates.io/crates/zeph-tools)
[![docs.rs](https://img.shields.io/docsrs/zeph-tools)](https://docs.rs/zeph-tools)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

Tool executor trait with shell, web scrape, and composite executors for Zeph.

## Overview

Defines the `ToolExecutor` trait for sandboxed tool invocation and ships concrete executors for shell commands, file operations, and web scraping. The `CompositeExecutor` chains multiple backends with output filtering, permission checks, trust gating, anomaly detection, audit logging, and TAFC (Think-Augmented Function Calling) for reasoning-enhanced tool selection.

## Key modules

| Module | Description |
|--------|-------------|
| `executor` | `ToolExecutor` trait, `ToolOutput`, `ToolCall`; `DynExecutor` newtype wrapping `Arc<dyn ErasedToolExecutor>` for object-safe executor composition |
| `shell` | Shell command executor with tokenizer-based command detection, escape normalization, and transparent wrapper skipping; receives skill-scoped env vars injected by the agent for active skills that declare `x-requires-secrets`. Default `confirm_patterns` cover process substitution (`<(`, `>(`), here-strings (`<<<`), and `eval` |
| `file` | File operation executor |
| `scrape` | Web scraping executor with SSRF protection: HTTPS-only, pre-DNS host blocklist, post-DNS private IP validation, pinned address client, and redirect chain defense (up to 3 hops each re-validated before following) |
| `composite` | `CompositeExecutor` — chains executors with middleware |
| `filter` | Output filtering pipeline — unified declarative TOML engine with 9 strategy types (`strip_noise`, `truncate`, `keep_matching`, `strip_annotated`, `test_summary`, `group_by_rule`, `git_status`, `git_diff`, `dedup`) and 19 embedded built-in rules; user-configurable via `filters.toml` |
| `permissions` | Permission checks for tool invocation |
| `audit` | `AuditLogger` — tool execution audit trail |
| `registry` | Tool registry and discovery |
| `trust_level` | `TrustLevel` enum — four-tier trust model (`Trusted`, `Verified`, `Quarantined`, `Blocked`) with severity ordering and `min_trust` helper |
| `trust_gate` | Trust-based tool access control |
| `anomaly` | `AnomalyDetector` — sliding-window failure rate detection; integrated into the agent tool execution pipeline — records every tool outcome, emits `Severity::Critical` when the failure rate exceeds `failure_threshold` in the last `window_size` executions, and auto-blocks the tool via the trust system |
| `schema_filter` | `ToolSchemaFilter` — dynamic tool schema filtering via embedding similarity; selects top-K relevant tools per query. `ToolDependencyGraph` — dependency graph with `requirements_met()` gate preventing tool execution until prerequisites are completed; `DependencyExclusion` marks tools excluded by unmet deps |
| `cache` | `ToolResultCache` — in-memory LRU cache for deterministic tool results with TTL expiry; `CacheKey` hashes tool name + args; `is_cacheable()` whitelist for safe-to-cache tools |
| `tool_filter` | `ToolFilter<E>` — executor wrapper that suppresses specified tools from the LLM tool set |
| `overflow` | (removed — overflow storage migrated to SQLite in `zeph-memory`) |
| `shell::transaction` | Transactional shell executor — snapshot/rollback filesystem state around shell commands; captures pre-execution state and reverts on failure or user request |
| `adversarial_policy` | Adversarial policy agent — pre-execution LLM validation that evaluates tool calls for safety before dispatch |
| `adversarial_gate` | `AdversarialPolicyGateExecutor` — executor wrapper that routes tool calls through the adversarial policy agent before execution |
| `policy_gate` | Policy-based tool access control gate |
| `error_taxonomy` | Tool invocation phase taxonomy — classifies errors by execution phase for structured diagnostics |
| `config` | Per-tool TOML configuration; `OverflowConfig` for `[tools.overflow]` section (threshold, retention_days, max_overflow_bytes — note: `dir` field removed, overflow storage is now SQLite-backed); `AnomalyConfig` for `[tools.anomaly]` section (enabled, window_size, failure_threshold, auto_block); `TafcConfig` for `[tools.tafc]` section; `ResultCacheConfig` for `[tools.result_cache]`; `DependencyConfig` + `ToolDependency` for `[tools.dependencies]`; `FileConfig` for `[tools.file]` section (deny_read/allow_read glob lists); `AuthorizationConfig` for `[tools.authorization]` (OAP declarative authorization rules); `max_tool_calls_per_session: Option<u32>` on `ToolsConfig` |

**Re-exports:** `CompositeExecutor`, `AuditLogger`, `AnomalyDetector`, `TrustLevel`, `ToolResultCache`, `CacheKey`, `ToolSchemaFilter`, `ToolDependencyGraph`, `ToolFilter`

## Structured shell output

`execute_bash` captures stdout and stderr as separate streams. Results are returned in a `ShellOutputEnvelope { stdout, stderr, exit_code, truncated }` stored in `ToolOutput.raw_response`. `AuditEntry` gains two new fields: `exit_code: Option<i32>` and `truncated: bool`, so audit logs record whether the process succeeded and whether its output was cut off.

## Per-path file read sandbox

`[tools.file]` in `config.toml` configures a glob-based read sandbox for the file executor. Paths are canonicalized and symlink-safe before matching.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `deny_read` | `Vec<String>` | `[]` | Glob patterns that always deny reads, evaluated first |
| `allow_read` | `Vec<String>` | `[]` | Glob patterns that allow reads after deny check (empty = allow all) |

Deny takes precedence over allow (deny-then-allow evaluation). A path matching a deny glob is blocked even if it also matches an allow glob.

```toml
[tools.file]
deny_read  = ["**/.env", "**/secrets/**"]
allow_read = ["/home/user/projects/**"]
```

## Security

`claim_source` is now propagated into `AdversarialPolicyGateExecutor` audit entries, so audit logs record which claim triggered the gate decision. `extract_paths` detects relative path tokens (e.g. `src/main.rs`) in addition to absolute paths.

## Security

### SSRF Protection in `WebScrapeExecutor`

`WebScrapeExecutor` applies a layered SSRF defense:

1. **HTTPS-only** — non-HTTPS schemes (`http://`, `ftp://`, `file://`, `javascript:`, etc.) are blocked before any network activity.
2. **Pre-DNS host blocklist** — `localhost`, `*.localhost`, `*.internal`, `*.local`, and literal private/loopback IPs are rejected at URL parse time.
3. **Post-DNS IP validation** — all resolved socket addresses are checked against private, loopback, link-local, and unspecified ranges (IPv4 and IPv6, including IPv4-mapped IPv6).
4. **Pinned address client** — the validated IP set is pinned into the HTTP client via `resolve_to_addrs`, eliminating DNS TOCTOU rebinding attacks.
5. **Redirect chain defense** — automatic redirects are disabled; the executor manually follows up to 3 redirect hops. Each `Location` header (including relative URLs resolved against the current request URL) is passed through steps 1–4 before the next request is made.

**Warning:**
> Any redirect hop that resolves to a private or internal address causes the entire request to fail with `ToolError::Blocked`. This prevents open-redirect SSRF where a public server redirects to an internal endpoint.

## Shell sandbox

The `ShellExecutor` enforces two layers of protection:

1. **Blocklist** (`blocked_commands`) — tokenizer-based detection that normalizes escapes, splits on shell metacharacters, and matches through transparent prefixes (`env`, `command`, `exec`, etc.).
2. **Confirmation patterns** (`confirm_patterns`) — substring scan that triggers `ConfirmationRequired` before execution. Defaults include `$(`, `` ` ``, `<(`, `>(`, `<<<`, and `eval `.

**Warning:**
> `find_blocked_command` does **not** detect commands hidden inside `eval`/`bash -c` string arguments or variable expansion (`$cmd`). Backtick substitution (`` `cmd` ``), `$(cmd)`, and process substitution (`<(...)` / `>(...)`) are now detected by the blocklist tokenizer; they are also covered by `confirm_patterns` as a second layer. For high-security deployments, complement this filter with OS-level sandboxing.

## Installation

## Anomaly detection configuration

`AnomalyDetector` is enabled by default when `tools.anomaly.enabled = true`. Configure via `[tools.anomaly]` in `config.toml`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Activate anomaly detection in the tool execution pipeline |
| `window_size` | usize | `20` | Rolling window of last N tool executions to evaluate |
| `failure_threshold` | f64 | `0.7` | Fraction of failures in the window to trigger a Critical alert |
| `auto_block` | bool | `true` | Automatically block a tool via trust system on Critical alert |

```toml
[tools.anomaly]
enabled = true
window_size = 20
failure_threshold = 0.7
auto_block = true
```

## TAFC (Think-Augmented Function Calling)

TAFC injects a reasoning step before tool selection, allowing the LLM to evaluate which tools are appropriate for the current task. Configure via `[tools.tafc]` in `config.toml`.

## Dynamic tool schema filtering

`ToolSchemaFilter` uses embedding similarity to select only the top-K most relevant tools for each query, reducing the tool catalog size in the LLM context. Tools marked as `always_on` bypass filtering and are always included.

## Tool result cache

`ToolResultCache` caches results of deterministic tools (those on the `is_cacheable()` whitelist) in memory with configurable TTL. Cache keys are computed by hashing tool name and arguments. The `/status` command reports cache hit/miss rates and tool filter state.

## Tool dependency graph

`ToolDependencyGraph` enforces execution ordering: a tool with declared `requires` dependencies cannot execute until all prerequisites have completed. Unmet dependencies produce a `DependencyExclusion` that gates the tool from the LLM tool set until requirements are satisfied. Configure via `[tools.dependencies]`.

## Tool call quota

Limit the total number of tool call attempts per agent session:

```toml
[tools]
max_tool_calls_per_session = 100   # Option<u32>; omit or set null for unlimited (default)
```

Only the first attempt counts — retries of a failed call do not consume quota. When the quota is exhausted the executor returns a `quota_blocked` error.

## OAP authorization

`[tools.authorization]` provides a declarative capability-based authorization layer evaluated after `[tools.policy]` rules (first-match-wins). Disabled by default.

```toml
[tools.authorization]
enabled = true

[[tools.authorization.rules]]
action = "allow"
tools  = ["read_file", "list_directory"]

[[tools.authorization.rules]]
action = "deny"
tools  = ["shell"]
```

Rules are merged into `PolicyEnforcer` at startup. `[tools.policy]` rules always take precedence — use `policy` for safety-critical deny rules and `authorization` for capability grants.

## Caller identity

`ToolCall::caller_id: Option<String>` carries the originating agent or sub-agent identifier. Set automatically by the orchestrator for sub-agent dispatches; `None` for the primary agent. Recorded in audit log entries.

## Features

| Feature | Description |
|---------|-------------|
| `policy-enforcer` | Enables `PolicyEnforcerConfig` and policy-based tool access control |

## Installation

```bash
cargo add zeph-tools
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
