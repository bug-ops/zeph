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

## 1. Context

PR #2565 simplified the root feature set from 31 flags to 22 by removing nine flags that were pure behavioral markers with no real optional dependency: `guardrail`, `context-compression`, `compression-guidelines`, `policy-enforcer`, `lsp-context`, `experiments`, `bundled-skills`, `stt`, and `acp-unstable`. Those features are now always compiled in.

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

### 3.3 Always-On Capabilities (No Flag)

These subsystems compile unconditionally. They were previously behind flags that have been removed:

| Subsystem | Former flag | Why inlined |
|---|---|---|
| Content sanitization / guardrail | `guardrail` | Pure marker; deps already unconditional |
| Context compaction | `context-compression` | Pure marker; ~60 source gates removed |
| Compression guidelines | `compression-guidelines` | Pure marker; ~20 source gates removed |
| Policy enforcer | `policy-enforcer` | Pure marker; ~20 source gates removed |
| LSP context integration | `lsp-context` | Pure marker; ~25 source gates removed |
| Experiments subsystem | `experiments` | Pure marker; ~35 source gates removed |
| Bundled SKILL.md files | `bundled-skills` | `include_dir` is always a dep of zeph-skills |
| Speech-to-text support | `stt` | `reqwest/multipart` is a minor feature, not an opt-in crate |
| ACP unstable capabilities | `acp-unstable` | zeph-acp enables all unstable features in its own defaults; `acp` alone is sufficient |

---

## 4. Bundle Definitions

Bundles are the **only** mechanism for enabling groups of features. Do not instruct users to combine individual flags manually unless debugging.

| Bundle | Expands to | Target use case |
|---|---|---|
| `desktop` | `tui` | Local developer workstation with terminal UI |
| `ide` | `acp`, `acp-http` | IDE integration via Agent-Client Protocol |
| `server` | `gateway`, `a2a`, `otel` | Headless server: webhook ingestion, A2A, telemetry |
| `chat` | `discord`, `slack` | Bot deployment on messaging platforms |
| `ml` | `candle`, `pdf` | On-device ML inference and PDF memory |
| `full` | `desktop`, `ide`, `server`, `chat`, `pdf`, `scheduler`, `classifiers` | CI, pre-merge checks, complete feature matrix |

Bundle invariants:
- `full` must activate every flag that is safe to combine (excluding `metal`, `cuda`, `postgres` — platform/exclusive).
- `default` must remain minimal: only `scheduler` and `sqlite`.
- CI MUST run with `--features full` for lint and tests. Partial-feature builds do not count as pre-merge validation.
- `--all-features` is **not a supported build mode**: `sqlite` and `postgres` are mutually exclusive and `--all-features` triggers a `compile_error!`.

---

## 5. Key Invariants

1. **No pure-marker flags.** A flag that gates only behavioral code with no distinct optional dependency MUST NOT exist. Remove it and make the code unconditional.

2. **Flags only for real optional deps or platform exclusives.** The gated content must be a crate or a transitive dependency that would otherwise link unconditionally.

3. **`default = []` is forbidden.** The workspace default must remain `["scheduler", "sqlite"]`. Changing this is a breaking config change.

4. **Bundles are immutable consumer surfaces.** A bundle name (`desktop`, `ide`, `server`, `chat`, `ml`, `full`) may not be removed. Its contents may only grow, not shrink (adding flags to a bundle is non-breaking; removing is breaking).

5. **Mutual exclusion must be enforced at compile time.** The `sqlite` and `postgres` flags activate a `compile_error!` in `zeph-db` when both are set. This guard must never be removed.

6. **`--features full` is the CI gate.** Pre-merge checks (`fmt`, `clippy`, `nextest`) run with `--features full`. This must match what CI runs exactly.

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
- Keep `default = ["scheduler", "sqlite"]`
- Run `--features full` for all pre-merge checks
- Use `dep:` prefix for all optional crate dependencies
- Remove `#[cfg(feature = "...")]` gates for deleted flags

### Ask First
- Adding a new feature flag (must justify via §2 decision rule)
- Adding a flag to or removing one from a bundle
- Changing `default` features
- Renaming an existing flag

### Never
- Add flags for pure behavioral markers
- Use `--all-features` in CI or documentation
- Enable `sqlite` and `postgres` simultaneously
- Remove a bundle name
