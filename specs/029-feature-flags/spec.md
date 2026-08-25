---
aliases:
  - Feature Flags
  - Cargo Features
tags:
  - sdd
  - spec
  - build
  - features
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec#9. Feature Flag Contract]]"
  - "[[020-config-loading/spec]]"
---

# Spec: Feature Flag System

> Non-negotiable rules governing how Cargo feature flags are declared, named, and used in this workspace.
> Any change that violates these invariants requires an explicit architectural decision.
> This document supersedes all previous ad-hoc flag decisions.

## Sources

### Internal
| Area | File |
|---|---|
| Root feature definitions | `Cargo.toml` [features] |
| System invariants §9 | `.local/specs/001-system-invariants/spec.md` |
| Implementation plan | `.local/handoff/2565-architect.md` |

---

## 1. Context simplified the root feature set from 31 flags to 22 by removing nine flags that were pure behavioral markers with no real optional dependency: `guardrail`, `context-compression`, `compression-guidelines`, `policy-enforcer`, `lsp-context`, `experiments`, `bundled-skills`, `stt`, and `acp-unstable`. Those features are now always compiled in.

This spec captures the resulting design as a binding contract for all future flag decisions.

---

## 2. Decision Rule for Feature Flags

A feature flag is justified **only** when removing it would change the compiled binary in one of these ways:

1. An optional crate dependency (`dep:zeph-<name>`, `dep:axum`, etc.) would be unconditionally linked — increasing binary size or compilation time.
2. A platform-exclusive dependency (`candle/metal`, `cuda`, `dep:opentelemetry-otlp`) would be required on all platforms.
3. Two features are **mutually exclusive** at the type level and cannot coexist in the same binary (`sqlite` vs `postgres`).

A feature flag is **not** justified when:

- It gates code that always compiles cleanly without it (pure behavioral marker).
- The underlying dependency is already transitively present on all supported targets.
- The feature is controlled at runtime via config (`[section] enabled = true`).
- The only effect is enabling or disabling a config section or a code path.

**Corollary**: every surviving flag in §3 satisfies at least one criterion above. Any proposed new flag must satisfy at least one criterion before it is added.

---

## 3. Current Flag Inventory

### 3.1 Default Features

```toml
default = ["scheduler", "sqlite"]
```

| Flag | Justification |
|---|---|
| `scheduler` | Pulls in `dep:zeph-scheduler`, `dep:cron`, `dep:schemars`, `dep:chrono` |
| `sqlite` | Mutually exclusive with `postgres`; selects the SQLite backend in `zeph-db` |

> [!note]
> `profiling` and `sandbox` are optional flags (see §3.2), not part of `default` — verified against
> `Cargo.toml` `[features] default = [...]`. Both are included in the `full` bundle (§4) used by CI.

**Removed from default (consolidated as always-on per §3.3):**

| Former default flag | Reason for removal |
|---|---|
| `self-check` | Pure behavioral marker — no optional deps. Consolidated per §2 Decision Rule. |
| `env-vault` | Pure behavioral marker — no optional deps. Consolidated per §2 Decision Rule. |
| `task-metrics` | Pure behavioral marker — no optional deps. Consolidated per §2 Decision Rule. |

### 3.2 Individual Optional Flags

| Flag | Dep(s) gated | Justification |
|---|---|---|
| `tui` | `dep:zeph-tui` | ratatui + crossterm; not needed in headless/server deployments |
| `candle` | `zeph-llm/candle`, `zeph-core/candle` | Pulls in candle-core/nn/transformers; heavy ML stack |
| `metal` | `candle` + Metal acceleration deps | macOS GPU only; compile error on non-Apple platforms |
| `cuda` | `candle` + CUDA deps | NVIDIA GPU only; compile error without CUDA toolkit |
| `classifiers` | `candle` + `zeph-llm/classifiers`, `zeph-sanitizer/classifiers` | Candle-backed ML classifiers; requires candle |
| `discord` | `zeph-channels/discord` | teloxide Discord adapter; optional messaging platform |
| `slack` | `zeph-channels/slack` | Slack adapter; optional messaging platform |
| `a2a` | `dep:zeph-a2a` | A2A protocol crate; not needed for pure local use |
| `acp` | `dep:zeph-acp` + unstable ACP features | Agent-Client Protocol; pulls in rmcp/WS transport |
| `acp-http` | `acp` + `dep:axum` | HTTP+SSE ACP transport; Axum is opt-in |
| `gateway` | `dep:zeph-gateway` | HTTP webhook ingestion; optional for inbound webhooks |
| `otel` | `dep:opentelemetry`, `dep:opentelemetry_sdk`, `dep:opentelemetry-otlp`, `dep:tracing-opentelemetry` | Heavy observability stack; not needed by default |
| `pdf` | `zeph-memory/pdf` | pdf-extract crate; large optional dep |
| `postgres` | `zeph-db/postgres`, `zeph-memory/postgres` | Mutually exclusive with `sqlite` |
| `sqlite` | `zeph-db/sqlite`, `zeph-memory/sqlite` | Mutually exclusive with `postgres` (also in default) |
| `session` | `dep:axum`, `dep:tokio-stream`, `zeph-common/http-middleware` | Session persistence + `zeph serve` mode (spec #068, new `zeph-session` crate); HTTP/SSE session API |
| `profiling` | `dep:tracing-chrome`, `dep:sysinfo` (+ per-crate `profiling` propagation) | Diagnostic tracing spans and system metrics; zero overhead when not actively tracing |
| `sandbox` | `zeph-tools/sandbox` (`dep:landlock`, `dep:seccompiler` on Linux; macOS Seatbelt compiles unconditionally) | Runtime-disabled by default (`tools.sandbox.enabled = false`) |
| `prometheus` | `gateway`, `dep:prometheus-client`, `zeph-gateway/prometheus` | OpenMetrics `/metrics` endpoint (spec #036); requires `gateway` |
| `gonka` | `zeph-llm/gonka`, `zeph-core/gonka` | gonka.ai inference provider (specs #051, #052) |
| `index` | `zeph-core/index` → `zeph-agent-context/index` (`dep:zeph-index`, +43 packages) | AST-based code indexing (spec #017); in the `desktop` bundle |
| `testing` | `zeph-llm/testing` (marker) | Test-double harness; exempt from §5.1 per the test-double clause. Not in `full` — see §4 |
| `bench` | `dep:zeph-bench` | Benchmark harness CLI (spec #034) |

> [!note]
> `prometheus`, `gonka`, `index`, `testing`, and `bench` were backfilled above
> (2026-07 reconciliation pass against `Cargo.toml`'s `[features]` block). `profiling-alloc` and
> `profiling-pyroscope` are still not individually documented here — both are thin variants of
> `profiling` (`profiling-alloc = ["profiling", "zeph-core/profiling-alloc"]`,
> `profiling-pyroscope = ["profiling", "otel", "dep:pprof"]`) and are omitted as low-risk; add rows
> for them if either gains independent justification beyond extending `profiling`.
>
> `deep-link`, `cocoon`, and `registry` were consolidated into always-on capabilities in the
> 2026-08 feature-flag audit — see §3.3.

### 3.3 Always-On Capabilities (No Flag)

These subsystems compile unconditionally. Before v0.18.0, they were behind optional feature flags
that gated only behavioral code with no distinct optional dependencies — pure compile-time markers.
As of v0.18.0, they were consolidated into always-on capabilities per the Decision Rule (§2):

| Subsystem | Former flag | Status |
|---|---|---|
| Content sanitization / guardrail | `guardrail` | Consolidated before v0.18.0 |
| Context compaction | `context-compression` | Consolidated before v0.18.0 |
| Compression guidelines | `compression-guidelines` | Consolidated before v0.18.0 |
| Policy enforcer | `policy-enforcer` | Consolidated before v0.18.0 |
| LSP context integration | `lsp-context` | Consolidated before v0.18.0 |
| Experiments subsystem | `experiments` | Consolidated v0.13.0–v0.18.0 |
| Bundled SKILL.md files | `bundled-skills` | Consolidated before v0.18.0 |
| Speech-to-text support | `stt` | Consolidated before v0.18.0 |
| ACP unstable capabilities | `acp-unstable` | Consolidated before v0.18.0 |
| MARCH self-check pipeline | `self-check` | Consolidated v0.20.x |
| Environment variable vault fallback | `env-vault` | Consolidated v0.20.x |
| Per-task CPU/wall-time metrics | `task-metrics` | Consolidated v0.20.x |
| `zeph://` deep-link URI dispatch | `deep-link` | Consolidated v0.22.x |
| Cocoon inference provider | `cocoon` | Consolidated v0.22.x |
| Skill/plugin registry marketplace | `registry` | Consolidated v0.22.x |
| Orchestration LLM planning | `zeph-orchestration/llm-planning` | Consolidated v0.22.x |
| Scheduler daemon mode | `zeph-scheduler/daemon` | Consolidated v0.22.x |
| ACP stabilised unstable handlers | 5 × `zeph-acp/unstable-*` | Consolidated v0.22.x |

**Why**: Each flag gated only behavioral code with no optional crate dependencies — they violated
the Decision Rule (§2). All these subsystems are active by default and cannot be disabled at build time;
runtime config (`enabled = true/false` in TOML) controls behavior where applicable.

---

## 4. Bundle Definitions

Bundles are the **only** mechanism for enabling groups of features. Do not instruct users to combine individual flags manually unless debugging.

| Bundle | Expands to | Target use case |
|---|---|---|
| `desktop` | `tui`, `session`, `index` | Local developer workstation with terminal UI |
| `ide` | `acp`, `acp-http` | IDE integration via Agent-Client Protocol |
| `server` | `gateway`, `a2a`, `otel`, `prometheus`, `session` | Headless server: webhook ingestion, A2A, telemetry, metrics |
| `chat` | `discord`, `slack` | Bot deployment on messaging platforms |
| `ml` | `candle`, `pdf` | On-device ML inference and PDF memory |
| `full` | `desktop`, `ide`, `server`, `chat`, `pdf`, `scheduler`, `classifiers`, `profiling`, `sandbox`, `gonka` | CI, pre-merge checks, complete feature matrix |

Bundle invariants:
- `full` must activate every flag that is safe to combine (excluding `metal`, `cuda`, `postgres` — platform/exclusive), and excluding dev-only harness features (`bench`, `testing`) — `release.yml` ships `full` to users, so benchmark harnesses and test doubles must not be in it. Each has its own CI leg (`bundle-check (bench)`; `testing` in the curated pre-merge string).
- `default` must remain minimal: only features that gate real optional deps AND have `Tested` coverage status. See §3.1 for the current list.
- The `full` configuration must be exercised by CI on every PR; this is satisfied by four independent legs — coverage (`ci.yml:507`), `bundle-check (full)`, `release-build-full`, and `ci-non-linux.yml:42` — while the hot `clippy`/`nextest` path runs a curated feature string. Moving the hot path to `full` would add candle's ~541 packages to every PR for no new coverage.
- `--all-features` is **not a supported build mode**: `sqlite` and `postgres` are mutually exclusive and `--all-features` triggers a `compile_error!`.

---

## 5. Key Invariants

1. **No pure-marker flags.** A flag that gates only behavioral code with no distinct optional dependency MUST NOT exist. Remove it and make the code unconditional. A marker flag whose only effect is to **withhold** test-double or test-harness code from non-test builds is exempt from this rule; §5.1 governs markers that gate shipped behaviour. This covers `zeph-llm/testing`, `zeph-memory/testing`, and the five `mock` flags (`zeph-vault`, `zeph-mcp`, `zeph-plugins`, `zeph-experiments`, `zeph-core`).

2. **Flags only for real optional deps or platform exclusives.** The gated content must be a crate or a transitive dependency that would otherwise link unconditionally.

3. **`default` is not empty.** The workspace default must include at minimum `scheduler` and `sqlite`. Additional features may be added if they satisfy both criteria: (a) gate real optional deps per §2 Decision Rule, and (b) have `Tested` coverage status with no open P0/P1 issues. Changing default features is a minor semver change documented in CHANGELOG.

4. **Bundles are immutable consumer surfaces.** A bundle name (`desktop`, `ide`, `server`, `chat`, `ml`, `full`) may not be removed. Its contents may only grow, not shrink (adding flags to a bundle is non-breaking; removing is breaking).

5. **Mutual exclusion must be enforced at compile time.** The `sqlite` and `postgres` flags activate a `compile_error!` in `zeph-db` when both are set. This guard must never be removed.

6. **The `full` configuration must be exercised by CI on every PR.** This is satisfied by four independent legs — coverage (`ci.yml:507`), `bundle-check (full)`, `release-build-full`, and `ci-non-linux.yml:42` — rather than requiring a literal `--features full` on the hot `clippy`/`nextest` path. Moving that path to `full` would add candle's ~541 packages to every PR for no new coverage.

7. **Flag names use kebab-case.** No underscores, no camelCase.

8. **Optional crate deps use `dep:` prefix.** Any crate that is optional must be declared as `dep:zeph-<name>` in the feature that enables it — never as an unconditional dep with an empty features list.

---

## 6. NEVER

- Add a feature flag whose sole effect is gating config-driven behavior (no optional dep, no platform gate).
- Add a feature flag for anything that compiles without error on all supported targets without it.
- Enable `sqlite` and `postgres` simultaneously (`--all-features` is explicitly unsupported).
- Remove or rename an existing bundle name without a major version bump and CHANGELOG entry.
- Leave a `#[cfg(feature = "...")]` gate for a flag that no longer exists in `Cargo.toml`.
- Ship a new crate as a mandatory dep when it could be an optional dep gated by an existing bundle flag.
- Use `--all-features` in CI, scripts, or documentation examples.

---

## 7. Adding a New Flag: Checklist

Before opening a PR that adds a new feature flag:

1. Confirm it gates at least one `dep:` crate or a platform-exclusive dependency.
2. Confirm the behavior is not configurable at runtime via `config.toml`.
3. Assign it to the appropriate bundle (`desktop`, `ide`, `server`, `chat`, `ml`) or justify a new bundle.
4. If it affects `full`, add it to `full` in the same PR.
5. Document it in this spec (§3.2) in the same PR.
6. Add a CHANGELOG entry under `[Unreleased]`.
7. Update `book/src/reference/feature-flags.md`.

---

## Agent Boundaries

### Always (without asking)
- Default features must satisfy §2 Decision Rule AND `Tested` coverage — verify both before adding
- Run the curated pre-merge feature string for `fmt`/`clippy`/`nextest`; ensure `full` stays covered by its four independent CI legs (§4, §5.6)
- Use `dep:` prefix for all optional crate dependencies
- Remove `#[cfg(feature = "...")]` gates for deleted flags

### Ask First
- Adding a new feature flag (must justify via §2 decision rule)
- Adding a flag to or removing one from a bundle
- Adding or removing a feature from `default` (must justify via §2 + coverage check)
- Renaming an existing flag

### Never
- Add flags for pure behavioral markers
- Use `--all-features` in CI or documentation
- Enable `sqlite` and `postgres` simultaneously
- Remove a bundle name
