# zeph-tools

[![Crates.io](https://img.shields.io/crates/v/zeph-tools)](https://crates.io/crates/zeph-tools)
[![docs.rs](https://img.shields.io/docsrs/zeph-tools)](https://docs.rs/zeph-tools)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

Tool executor trait with shell, web scrape, and composite executors for Zeph.

## Overview

Defines the `ToolExecutor` trait for sandboxed tool invocation and ships concrete executors for shell commands, file operations, and web scraping. The `CompositeExecutor` chains multiple backends with output filtering, permission checks, trust gating, anomaly detection, audit logging, egress network logging, and TAFC (Think-Augmented Function Calling) for reasoning-enhanced tool selection. Supports OS-level isolation via macOS Seatbelt and Linux Landlock when the `sandbox` feature is enabled.

## Key modules

| Module | Description |
|--------|-------------|
| `executor` | `ToolExecutor` trait, `ToolOutput`, `ToolCall`; `DynExecutor` newtype wrapping `Arc<dyn ErasedToolExecutor>` for object-safe executor composition |
| `shell` | Shell command executor with tokenizer-based command detection, escape normalization, and transparent wrapper skipping; receives skill-scoped env vars injected by the agent for active skills that declare `x-requires-secrets`. Default `confirm_patterns` cover process substitution (`<(`, `>(`), here-strings (`<<<`), and `eval` |
| `file` | File operation executor |
| `scrape` | Web scraping executor with SSRF protection: HTTPS-only, pre-DNS host blocklist, post-DNS private IP validation, pinned address client, and redirect chain defense (up to 3 hops each re-validated before following) |
| `search` | `WebSearchExecutor` — the `web_search` tool: issues a natural-language query to an external search provider (Brave) and returns ranked `title`/`url`/`snippet` results without requiring a pre-known URL. Reuses `scrape`'s SSRF validation and IPI filtering; result URLs are never auto-fetched. Disabled by default, gated by `[tools.search]` |
| `composite` | `CompositeExecutor` — chains executors with middleware |
| `filter` | Output filtering pipeline — unified declarative TOML engine with 9 strategy types (`strip_noise`, `truncate`, `keep_matching`, `strip_annotated`, `test_summary`, `group_by_rule`, `git_status`, `git_diff`, `dedup`) and 25 embedded built-in rules; user-configurable via a `filters.toml` placed next to `config.toml` |
| `permissions` | Permission checks for tool invocation |
| `audit` | `AuditLogger` — tool execution audit trail; `EgressEvent` with per-hop emission for outbound network requests and JSONL egress records |
| `registry` | Tool registry and discovery |
| `trust_level` | Re-exports `zeph_common::SkillTrustLevel` — four-tier trust model (`Trusted`, `Verified`, `Quarantined`, `Blocked`; `Quarantined` is the `Default`) |
| `risk_chain` | `RiskChainAccumulator` — cross-turn attack-chain detection. Records each invocation's `RiskTag`s and returns a `RiskChainVerdict` when a sequence forms a known dangerous chain. Window configurable via `[tools.shell] risk_chain_window_turns` (falls back to `DEFAULT_CROSS_TURN_WINDOW_TURNS` = 3) |
| `scope` | `ScopedToolExecutor` / `ToolScope` — task-type capability scoping over fully-qualified tool ids; built via `build_scoped_executor` from `[security.capability_scopes]` |
| `trust_gate` | Trust-based tool access control |
| `anomaly` | `AnomalyDetector` — sliding-window error-rate detection over the last `window_size` tool outcomes. Emits `AnomalySeverity::Warning` at `error_threshold` and `AnomalySeverity::Critical` at `critical_threshold`; blocked executions count as errors. Reporting only — blocking is the trust/policy layer's job |
| `schema_filter` | `ToolSchemaFilter` — dynamic tool schema filtering via embedding similarity; selects top-K relevant tools per query. `ToolDependencyGraph` — dependency graph with `requirements_met()` gate preventing tool execution until prerequisites are completed; `DependencyExclusion` marks tools excluded by unmet deps |
| `cache` | `ToolResultCache` — in-memory LRU cache for deterministic tool results with TTL expiry; `CacheKey` hashes tool name + args; `is_cacheable()` whitelist for safe-to-cache tools |
| `tool_filter` | `ToolFilter<E>` — executor wrapper that suppresses specified tools from the LLM tool set |
| `executor_delegate` | Forwarding macros (`tool_executor_forward!`, `tool_executor_no_inner_defaults!`, and their `erased_*` counterparts) that implement the required `ToolExecutor`/`ErasedToolExecutor` methods for wrapper and leaf executors, respectively |
| `shell::transaction` | Transactional shell executor — snapshot/rollback filesystem state around shell commands; captures pre-execution state and reverts on failure or user request |
| `adversarial_policy` | Adversarial policy agent — pre-execution LLM validation that evaluates tool calls for safety before dispatch |
| `adversarial_gate` | `AdversarialPolicyGateExecutor` — executor wrapper that routes tool calls through the adversarial policy agent before execution |
| `policy_gate` | Policy-based tool access control gate |
| `error_taxonomy` | Tool invocation phase taxonomy — classifies errors by execution phase for structured diagnostics |
| `config` | Per-tool TOML configuration (types live in `zeph-config`, re-exported here). `OverflowConfig` for `[tools.overflow]` (`threshold`, `retention_days`, `max_overflow_bytes`, `max_per_call_override`; overflow storage is SQLite-backed in `zeph-memory`); `AnomalyConfig` for `[tools.anomaly]` (`enabled`, `window_size`, `error_threshold`, `critical_threshold`, `reasoning_model_warning`); `TafcConfig` for `[tools.tafc]`; `ResultCacheConfig` for `[tools.result_cache]` (`enabled`, `ttl_secs`); `DependencyConfig` + `ToolDependency` for `[tools.dependencies]` (`enabled`, `boost_per_dep`, `max_total_boost`, `rules`); `FileConfig` for `[tools.file]` (`deny_read`/`allow_read` glob lists); `AuthorizationConfig` for `[tools.authorization]` (OAP declarative rules); `SpeculativeConfig` for `[tools.speculative]`; `SearchConfig` for `[tools.search]`; `max_tool_calls_per_session: Option<u32>` on `ToolsConfig` |

**Re-exports:** `CompositeExecutor`, `AuditLogger`, `AnomalyDetector`, `SkillTrustLevel`, `ToolResultCache`, `CacheKey`, `ToolSchemaFilter`, `ToolDependencyGraph`, `ToolFilter`, `RiskChainAccumulator`, `ScopedToolExecutor`, `ExecutionContext`

## Structured shell output

`execute_bash` captures stdout and stderr as separate streams. Results are returned in a `ShellOutputEnvelope { stdout, stderr, exit_code, truncated }` stored in `ToolOutput.raw_response`. `AuditEntry` gains two new fields: `exit_code: Option<i32>` and `truncated: bool`, so audit logs record whether the process succeeded and whether its output was cut off.

## Per-path file read sandbox

`[tools.file]` in `config.toml` configures a glob-based read sandbox for the file executor. Paths are canonicalized and symlink-safe before matching.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `deny_read` | `Vec<String>` | `[]` | Glob patterns denied for reading. Empty = the sandbox is inactive and every path inside `allowed_paths` is readable |
| `allow_read` | `Vec<String>` | `[]` | Exception list — glob patterns re-allowed after a `deny_read` match |

`allow_read` is an **exception list layered on top of `deny_read`**, not an independent
allowlist: a read is rejected only when the canonical path matches `deny_read` **and** does not
match `allow_read`. With `deny_read` empty the sandbox never fires, whatever `allow_read` says.

```toml
[tools.file]
deny_read  = ["**/*.env", "**/secrets/**"]
allow_read = ["**/public.env"]   # carve-out from the deny above
```

## Security

`claim_source` is now propagated into `AdversarialPolicyGateExecutor` audit entries, so audit logs record which claim triggered the gate decision. `extract_paths` detects relative path tokens (e.g. `src/main.rs`) in addition to absolute paths.

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

## WebSearchExecutor — `web_search` tool

Disabled by default. Requires a Brave Search API key stored in the age vault, then enabled via `[tools.search]`:

```bash
zeph vault set ZEPH_WEB_SEARCH_API_KEY <your-api-key>
```

```toml
[tools.search]
enabled      = true
backend      = "brave"   # currently the only supported provider
max_results  = 10
```

The pinned `reqwest::Client` used for the search endpoint is cached and reused across calls
keyed by the resolved (sorted, deduplicated) address set, avoiding a fresh TCP+TLS handshake
per query while preserving SSRF address-pinning on every resolution change.

## Anomaly detection configuration

`AnomalyDetector` is enabled by default. Configure via `[tools.anomaly]` in `config.toml`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Activate anomaly detection in the tool execution pipeline |
| `window_size` | usize | `10` | Rolling window of last N tool executions to evaluate |
| `error_threshold` | f64 | `0.5` | Error-rate fraction in the window that raises a WARN |
| `critical_threshold` | f64 | `0.8` | Error-rate fraction in the window that raises a CRIT |
| `reasoning_model_warning` | bool | `true` | Emit a WARN when a reasoning model produces a quality failure |

```toml
[tools.anomaly]
enabled                 = true
window_size             = 10
error_threshold         = 0.5
critical_threshold      = 0.8
reasoning_model_warning = true
```

## Cross-turn risk chains

`RiskChainAccumulator` records the `RiskTag`s of each shell invocation and returns a
`RiskChainVerdict` when a sequence of calls forms a known dangerous chain — an attack split
across several turns that no single call would trip on its own. `advance_turn` is called at each
turn boundary and prunes any recorded call older than the configured window.

The window is configurable via `[tools.shell] risk_chain_window_turns`:

```toml
[tools.shell]
risk_chain_window_turns = 3   # Option<u64>; omit to use DEFAULT_CROSS_TURN_WINDOW_TURNS (3)
```

A wider window catches slower-paced chains at the cost of holding more history and a higher
false-positive rate; the default of `3` is deliberately narrow. `--migrate-config` adds the key
to `[tools.shell]` as a commented-out default for existing configs.

## Capability scoping (`ScopedToolExecutor`)

`ScopedToolExecutor` wraps any `ToolExecutor` and filters both `tool_definitions()` (the tool
list surfaced to the LLM) and `execute_tool_call()` (the dispatch path) down to an
operator-declared allow-list of **fully-qualified tool ids** — not filesystem paths or network
hosts. It sits outermost in the wrapper chain, so an out-of-scope call short-circuits before
policy evaluation:

```text
ScopedToolExecutor → PolicyGateExecutor → TrustGateExecutor → CompositeExecutor → …
```

Tool ids carry a namespace prefix before scope resolution: `builtin:`, `skill:<name>/`,
`mcp:<server_id>/`, `acp:<peer>/`, `a2a:<peer>/`. Built-in executors register unqualified ids
(`"bash"`, `"read"`) that are normalised to `builtin:<id>` at the scope boundary.

Named scopes are keyed by task type and configured under `[security.capability_scopes]`
(`CapabilityScopesConfig`), then compiled by `build_scoped_executor`:

```toml
[security.capability_scopes]
default_scope = "general"
strict        = false

[security.capability_scopes.general]
patterns = ["*"]

[security.capability_scopes.research]
patterns = ["builtin:fetch", "builtin:web_scrape", "builtin:search_*", "builtin:read"]

[security.capability_scopes.code_edit]
patterns = ["builtin:read", "builtin:edit", "builtin:write", "builtin:shell", "builtin:glob"]
```

Pattern strictness differs per namespace: a zero-match `builtin:`/`skill:` glob is a fatal
`ScopeError::DeadPattern`, while `mcp:`/`acp:`/`a2a:` globs are provisional
(`ScopeWarning::ProvisionalDeadPattern`) and re-resolved when tools register dynamically. A glob
matching the entire registry without an explicit `general` opt-in is `ScopeError::AccidentallyFull`.

> [!NOTE]
> The session-wide trajectory risk level computed outside this crate (spec-050) reaches the tool
> layer as `policy_gate::TrajectoryRiskSlot` — a shared `u8` (`0` Calm … `3` Critical). At `3`,
> `check_policy` downgrades an `Allow` decision to `Deny`. `zeph-tools` only reads this slot; the
> analysis itself lives in `zeph-memory`/`zeph-core`.

## Per-call ExecutionContext

`ExecutionContext` is attached to a `ToolCall` to override the **working directory and
environment variables** for that specific call. When absent, `ShellExecutor` uses the process CWD
and inherited process environment.

```rust
use zeph_tools::ExecutionContext;

let ctx = ExecutionContext::new()
    .with_name("repo")
    .with_cwd("/workspace/myproject")
    .with_env("CARGO_TARGET_DIR", "/tmp/cargo-target");
```

`name` matches an entry in the `[[execution.environments]]` config table, from which unspecified
fields are looked up. Resolution precedence, highest first: the call's own `context`, the named
registry entry, skill env (env only), the `default_env` registry entry, then the process CWD /
inherited env minus the blocklist.

> [!IMPORTANT]
> Contexts built through the public API are **untrusted**: their env overrides are re-filtered
> through the executor's `env_blocklist` after every merge step, so an LLM-controlled caller
> cannot reintroduce a blocked variable. Only `ExecutionContext::trusted_from_parts` (crate-internal,
> used for operator-authored TOML) produces a trusted context that bypasses that final filter.

## TAFC (Think-Augmented Function Calling)

TAFC injects a reasoning step before tool selection, allowing the LLM to evaluate which tools are appropriate for the current task. Configure via `[tools.tafc]` in `config.toml`.

## Speculative tool dispatch

Speculative dispatch pre-runs read-only tool calls while the LLM generates its response and reuses the cached result when the model issues the same call — eliminating the round-trip latency for deterministic read operations. Non-deterministic or state-mutating tools are excluded from speculation via the `requires_confirmation` policy gate. Configure via `SpeculativeConfig` / `SpeculationMode` under `[tools.speculative]`.

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
effect = "allow"
tool   = "read_file"

[[tools.authorization.rules]]
effect = "allow"
tool   = "list_*"          # `tool` is a glob over the tool id

[[tools.authorization.rules]]
effect = "deny"
tool   = "shell"
```

Each rule takes a single `tool` glob plus an `effect` of `"allow"` or `"deny"`; optional
`paths`, `env`, `trust_level`, `args_match`, and `capabilities` narrow when it fires.

Rules are appended to `PolicyEnforcer` after the `[tools.policy]` rules at startup, so
`[tools.policy]` always takes precedence — use `policy` for safety-critical deny rules and
`authorization` for capability grants.

## Caller identity

`ToolCall::caller_id: Option<String>` carries the originating agent or sub-agent identifier. Set automatically by the orchestrator for sub-agent dispatches; `None` for the primary agent. Recorded in audit log entries.

## ToolExecutor trait contract

> [!WARNING]
> `requires_confirmation`, `execute_tool_call_confirmed`, `checkpoint_undo`/`checkpoint_redo`/
> `checkpoint_list`, and `is_tool_speculatable` (and their `_erased` counterparts on
> `ErasedToolExecutor`) have **no default implementation**. A wrapper or leaf executor that
> omits one of these no longer silently inherits a permissive fallback — it fails to compile.
> Leaf executors with no wrapped inner should implement them via
> [`tool_executor_no_inner_defaults!`](https://docs.rs/zeph-tools); wrappers that forward to an
> inner executor should use [`tool_executor_forward!`](https://docs.rs/zeph-tools). This closed
> a recurring defect class where a decorator's `impl` block quietly fell back to an overly
> permissive trait default instead of forwarding to its inner executor.

## Multimodal tool output (media passthrough)

`ToolOutput.media: Vec<zeph_llm::ImageData>` carries validated image data across the tool
boundary (e.g. MCP `ContentBlock::Image` passthrough, spec-072), introducing a `zeph-tools` →
`zeph-llm` dependency edge. `ImageData` has a redacting `Debug` impl so raw image bytes never
reach logs or debug dumps.

`media` is populated by `zeph-mcp`'s `McpToolExecutor` when the server has
`media_passthrough = true` and a `zeph_sanitizer::MediaSanitizer` is attached (`[mcp.media]`).
Every other executor leaves it empty, and the rendered text placeholder always remains as a
fallback regardless of whether an image is attached.

## Features

| Feature | Description |
|---------|-------------|
| `sqlite` | SQLite backend for `zeph-db`/`zeph-sanitizer` (enabled by `default`) |
| `postgres` | PostgreSQL backend for `zeph-db`/`zeph-sanitizer` |
| `sandbox` | Gates Linux-only Landlock + seccomp BPF deps; the macOS Seatbelt backend compiles unconditionally |
| `profiling` | Emits `tracing` instrumentation spans around executor hot paths |

## Installation

```bash
cargo add zeph-tools
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
