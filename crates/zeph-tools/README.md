# zeph-tools

[![Crates.io](https://img.shields.io/crates/v/zeph-tools)](https://crates.io/crates/zeph-tools)
[![docs.rs](https://img.shields.io/docsrs/zeph-tools)](https://docs.rs/zeph-tools)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Tool executor trait with shell, web scrape, and composite executors for Zeph.

## Overview

Defines the `ToolExecutor` trait for sandboxed tool invocation and ships concrete executors for shell commands, file operations, and web scraping. The `CompositeExecutor` chains multiple backends with output filtering, permission checks, trust gating, anomaly detection, and audit logging.

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
| `overflow` | (removed — overflow storage migrated to SQLite in `zeph-memory`) |
| `config` | Per-tool TOML configuration; `OverflowConfig` for `[tools.overflow]` section (threshold, retention_days, max_overflow_bytes — note: `dir` field removed, overflow storage is now SQLite-backed); `AnomalyConfig` for `[tools.anomaly]` section (enabled, window_size, failure_threshold, auto_block) |

**Re-exports:** `CompositeExecutor`, `AuditLogger`, `AnomalyDetector`, `TrustLevel`

## Security

### SSRF Protection in `WebScrapeExecutor`

`WebScrapeExecutor` applies a layered SSRF defense:

1. **HTTPS-only** — non-HTTPS schemes (`http://`, `ftp://`, `file://`, `javascript:`, etc.) are blocked before any network activity.
2. **Pre-DNS host blocklist** — `localhost`, `*.localhost`, `*.internal`, `*.local`, and literal private/loopback IPs are rejected at URL parse time.
3. **Post-DNS IP validation** — all resolved socket addresses are checked against private, loopback, link-local, and unspecified ranges (IPv4 and IPv6, including IPv4-mapped IPv6).
4. **Pinned address client** — the validated IP set is pinned into the HTTP client via `resolve_to_addrs`, eliminating DNS TOCTOU rebinding attacks.
5. **Redirect chain defense** — automatic redirects are disabled; the executor manually follows up to 3 redirect hops. Each `Location` header (including relative URLs resolved against the current request URL) is passed through steps 1–4 before the next request is made.

> [!WARNING]
> Any redirect hop that resolves to a private or internal address causes the entire request to fail with `ToolError::Blocked`. This prevents open-redirect SSRF where a public server redirects to an internal endpoint.

## Shell sandbox

The `ShellExecutor` enforces two layers of protection:

1. **Blocklist** (`blocked_commands`) — tokenizer-based detection that normalizes escapes, splits on shell metacharacters, and matches through transparent prefixes (`env`, `command`, `exec`, etc.).
2. **Confirmation patterns** (`confirm_patterns`) — substring scan that triggers `ConfirmationRequired` before execution. Defaults include `$(`, `` ` ``, `<(`, `>(`, `<<<`, and `eval `.

> [!WARNING]
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

## Installation

```bash
cargo add zeph-tools
```

## License

MIT
