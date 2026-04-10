---
aliases:
  - Complexity Triage
  - Complexity Routing
tags:
  - sdd
  - spec
  - routing
  - llm
created: 2026-03-23
status: approved
related:
  - "[[MOC-specs]]"
  - "[[024-multi-model-design/spec]]"
  - "[[022-config-simplification/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[009-orchestration/spec]]"
---

# Spec: Complexity Triage Routing

> **Status**: Approved (post-implementation)
> **Issue**: #2141
> **Date**: 2026-03-23
> **Crate**: `zeph-llm` (router), `zeph-config`, `zeph-core`

## 1. Overview

### Problem Statement

Routing all requests through a single high-capability model wastes cost and
latency on simple inputs (greetings, factual lookups, yes/no answers). The
orchestrator and cascade routers either require historical outcome feedback
(Thompson Sampling / EMA) or retry-based quality gating (cascade), neither of
which is suitable for proactive upfront cost reduction.

### Goal

Before each inference call, run a lightweight classification step against a
cheap/fast model to assign the request a `ComplexityTier` (simple, medium,
complex, expert), then delegate to the provider configured for that tier.

### Out of Scope

- Feedback loop from triage outcome (Thompson Sampling handles that separately).
- Per-provider `provider_override` — bypassed intentionally; triage constructs
  providers directly from `[[llm.providers]]` by entry name.
- Fallback cascade on misclassification (field reserved:
  `fallback_strategy`, not yet activated).
- TUI metrics panel integration for triage counters (tracked separately).

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `routing = "triage"` THE SYSTEM SHALL classify each request with a dedicated triage provider before dispatching to a tier provider | must |
| FR-002 | WHEN triage classification times out or returns an unparseable response THE SYSTEM SHALL fall back to the first (lowest) tier provider | must |
| FR-003 | WHEN the classified tier has no configured provider THE SYSTEM SHALL escalate to the next-higher available tier | must |
| FR-004 | WHEN no higher tier is available THE SYSTEM SHALL descend to the next-lower available tier | must |
| FR-005 | WHEN context tokens exceed 80% of the selected tier provider's context window THE SYSTEM SHALL escalate to the smallest provider whose window fits | must |
| FR-006 | WHEN `context_window()` returns `None` for the selected provider THE SYSTEM SHALL skip context-window escalation for that provider | must |
| FR-007 | WHEN `bypass_single_provider = true` and all configured tier entries resolve to the same config name THE SYSTEM SHALL return a single provider instead of wrapping in a TriageRouter | should |
| FR-008 | WHEN no tier providers are configured THE SYSTEM SHALL fall through to single-provider selection from the pool | must |
| FR-009 | THE SYSTEM SHALL record metrics for every triage call: tier distribution, timeout fallbacks, context escalations, and latency | must |
| FR-010 | WHEN `chat`, `chat_stream`, and `chat_with_tools` are called THE SYSTEM SHALL each independently perform triage (MF-2: no cross-delegation) | must |

---

## 3. Data Model

### `ComplexityTier` (enum, `zeph-llm`)

| Variant | Index | `as_str()` |
|---------|-------|------------|
| `Simple` | 0 | `"simple"` |
| `Medium` | 1 | `"medium"` |
| `Complex` | 2 | `"complex"` |
| `Expert` | 3 | `"expert"` |

Default: `Simple`. Serializes with `serde(rename_all = "lowercase")`.

### `TriageVerdict` (struct, deserialized from triage model output)

```
tier: ComplexityTier
reason: String
large_context: bool  (default false)
```

### `TriageMetrics` (struct, `Arc<TriageMetrics>` shared across clones)

All counters: `AtomicU64`, `Ordering::Relaxed`. Fields:

| Field | Meaning |
|-------|---------|
| `calls` | Total triage calls dispatched |
| `tier_simple/medium/complex/expert` | Per-tier classification count |
| `timeout_fallbacks` | Fallbacks due to timeout or parse failure |
| `escalations` | Context-window auto-escalations |
| `latency_us_total` | Sum of triage call latencies in microseconds |

`avg_latency_us()` returns `latency_us_total / calls` (0 if no calls).

### `TriageRouter` (struct, `zeph-llm`)

| Field | Type | Notes |
|-------|------|-------|
| `triage_provider` | `AnyProvider` | Cheap model used for classification only |
| `tier_providers` | `Vec<(ComplexityTier, AnyProvider)>` | Ordered simple→expert |
| `default_index` | `usize` | Always 0 (first/cheapest tier) |
| `triage_timeout` | `Duration` | From `triage_timeout_secs` |
| `metrics` | `Arc<TriageMetrics>` | Shared across Clone copies |
| `last_provider_idx` | `Arc<AtomicUsize>` | Shared across Clone copies; sentinel `usize::MAX` = no call yet |

### `ComplexityRoutingConfig` (struct, `zeph-config`)

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `triage_provider` | `Option<String>` | `None` | Entry name in `[[llm.providers]]`; falls back to first pool entry |
| `bypass_single_provider` | `bool` | `true` | Skip triage when all tiers reference the same config entry name |
| `tiers` | `TierMapping` | all `None` | Per-tier provider name mapping |
| `max_triage_tokens` | `u32` | `50` | Max output tokens for the classification call |
| `triage_timeout_secs` | `u64` | `5` | Timeout for the classification call |
| `fallback_strategy` | `Option<String>` | `None` | Reserved for future use; currently unused |

### `TierMapping` (struct, `zeph-config`)

```toml
[llm.complexity_routing.tiers]
simple  = "<provider-name>"   # optional
medium  = "<provider-name>"   # optional
complex = "<provider-name>"   # optional
expert  = "<provider-name>"   # optional
```

All fields are `Option<String>`. Unset tiers are skipped at bootstrap; the
router escalates/descends to the nearest available tier.

### `LlmRoutingStrategy` (enum variant addition)

```rust
LlmRoutingStrategy::Triage
```

Serializes as `"triage"` (lowercase). Activates `build_triage_provider()` in
bootstrap.

---

## 4. Triage Prompt and Response Parsing

The triage prompt is built from the last user message (truncated to 400 chars)
plus aggregate context stats (message count, estimated token count). Target
input size: ~120 tokens to keep classification cost minimal.

Expected response format:
```json
{"tier":"simple|medium|complex|expert","reason":"...","large_context":false}
```

Parse strategy (three-level fallback):
1. Direct `serde_json::from_str::<TriageVerdict>`.
2. Extract first `{...}` fragment and retry parse.
3. Substring scan for `"tier"` key followed by a recognized tier string.

If all three fail, returns `None` and increments `timeout_fallbacks`.

---

## 5. Context Window Escalation (D6)

Threshold: `context_tokens > window * 4/5` (strictly greater than 80%).

- When the provider for the classified tier has `context_window() = None`,
  escalation is skipped entirely (MF-3).
- When the provider's window is too small, `tier_providers` (ordered
  smallest→largest context window) is scanned linearly; first fit wins.
- No escalation is possible when only one provider exists or none has a larger
  window — the original index is kept without error.

---

## 6. Integration Points

### Config (`config.toml`)

```toml
[llm]
routing = "triage"

[llm.complexity_routing]
triage_provider = "fast"        # optional; defaults to first pool entry
bypass_single_provider = true   # optional; default true
triage_timeout_secs = 5         # optional; default 5
max_triage_tokens = 50          # optional; default 50

[llm.complexity_routing.tiers]
simple  = "fast"
medium  = "medium"
complex = "sonnet"
expert  = "opus"

[[llm.providers]]
provider_type = "ollama"
name = "fast"
model = "qwen3:1.7b"

[[llm.providers]]
...
```

Provider names in `[llm.complexity_routing.tiers]` and `triage_provider` must
match `effective_name()` of entries in `[[llm.providers]]`.

### Bootstrap (`zeph-core/src/bootstrap/provider.rs`)

`build_triage_provider()` is called from `create_provider_from_pool()` when
`config.llm.routing == LlmRoutingStrategy::Triage`. It:

1. Resolves `triage_provider` by name (or defaults to first pool entry).
2. Iterates `tier_config` array in tier order (simple → expert), resolving each
   by `create_named_provider()`. Skips missing entries with `tracing::warn!`.
3. Applies bypass check: if `bypass_single_provider = true` and all
   `tier_config_names` are equal, returns a single provider from the pool.
4. Constructs `TriageRouter::new(...)` and wraps it in `AnyProvider::Triage`.

Bypass detection compares **config entry names**, not `provider.name()`, to
correctly distinguish two pool entries that use the same provider type
(e.g., two Claude entries for Haiku and Opus).

### `AnyProvider` enum (`zeph-llm/src/any.rs`)

`AnyProvider::Triage(Box<TriageRouter>)` is a first-class variant. The
`match_provider!` macro must include it exhaustively. `set_status_tx` propagates
the sender to all tier providers via `TriageRouter::set_status_tx`.

### `LlmProvider` implementation notes

- `context_window()` returns the **maximum** across all tier providers.
- `supports_streaming()`, `supports_vision()`, `supports_tool_use()` use `any()`.
- `supports_embeddings()` and `supports_structured_output()` return `false`.
- `embed()` returns `LlmError::EmbedUnsupported`.
- `debug_request_json()` delegates to the first tier provider (best effort; tier
  is unknown statically).
- `last_usage()` and `last_cache_usage()` delegate to the last-used tier
  provider via `last_provider_idx`; return `None` before any call completes.

### `Box::pin` usage

All three `LlmProvider` async methods (`chat`, `chat_stream`, `chat_with_tools`)
use `Box::pin(async move { ... })`. This is required to break the type-cycle
introduced by `AnyProvider::Triage` containing a `TriageRouter` that itself
holds `AnyProvider` values — the compiler cannot infer `impl Future` sizes
without pinning.

---

## 7. Key Invariants

| ID | Invariant |
|----|-----------|
| INV-01 | Triage timeout always falls back to `default_index = 0`; it never propagates errors upward |
| INV-02 | `context_window() = None` on a provider means skip context escalation for that provider (do not treat as zero) |
| INV-03 | All triage metrics use `AtomicU64` with `Ordering::Relaxed`; no locks in any hot path |
| INV-04 | `AnyProvider::Triage` must be handled in every `match` arm / macro expansion in `any.rs` — exhaustiveness is a compile-time guarantee |
| INV-05 | `chat`, `chat_stream`, and `chat_with_tools` each call `classify()` independently; none delegates to another (MF-2) |
| INV-06 | `last_provider_idx` sentinel `usize::MAX` means no call completed; `last_usage()` returns `None` in that case |
| INV-07 | `tier_providers` must be non-empty at `TriageRouter::new()` (assertion panic at bootstrap, not at call time) |
| INV-08 | Bypass detection uses config entry names, not runtime `provider.name()` |
| INV-09 | `LlmRoutingStrategy::Triage` requires `[llm.complexity_routing]` to be present; its absence is a hard bootstrap error |

---

## 8. Edge Cases and Error Handling

| Scenario | Behavior |
|----------|----------|
| Triage model times out | Fallback to first tier; `timeout_fallbacks` incremented; `tracing::warn!` emitted |
| Triage model returns garbage | Same as timeout; parse falls through all three strategies, returns `None` |
| Classified tier has no matching provider | Escalate to next-higher tier; if none, descend to next-lower; if none, `default_index` |
| All tiers reference the same provider name | When `bypass_single_provider = true`, triage router is skipped entirely |
| No tiers configured in mapping | Falls through to `build_single_provider_from_pool`; `tracing::warn!` emitted |
| `complexity_routing` section absent with `routing = "triage"` | `BootstrapError::Provider` — hard startup failure |
| Provider listed in tiers not in pool | Skipped with `tracing::warn!`; if all tiers fail, fallback to single provider |
| Single provider, context exceeds 80% | No escalation possible; original index kept |
| Provider with `context_window() = None`, large context | Escalation skipped; original index kept (MF-3) |

---

## 9. Agent Boundaries

### Always (without asking)
- Run `cargo nextest run --workspace --features full --lib --bins` after changes
- Add unit tests for new tier-selection or parsing paths
- Follow `AtomicU64` / `Ordering::Relaxed` pattern for new metrics fields

### Ask First
- Adding new `ComplexityTier` variants (affects serialization and all match arms)
- Activating cascade fallback feature
- Exposing triage metrics in the TUI panel

### Never
- Use blocking I/O or locks inside `classify()` or `maybe_escalate_for_context()`
- Delegate `chat` to `chat_with_tools` or vice-versa (violates MF-2 / INV-05)
- Treat `context_window() = None` as zero (violates INV-02)

---

## 10. References

- Issue: #2141
- `crates/zeph-llm/src/router/triage.rs` — core router implementation
- `crates/zeph-config/src/providers.rs` — `ComplexityRoutingConfig`, `TierMapping`, `LlmRoutingStrategy::Triage`
- `crates/zeph-core/src/bootstrap/provider.rs` — `build_triage_provider()`
- `crates/zeph-llm/src/any.rs` — `AnyProvider::Triage` variant
- `003-llm-providers/spec.md` — parent spec for the LlmProvider trait and AnyProvider
- `022-config-simplification/spec.md` — unified `ProviderEntry` format that triage routing depends on
