---
aliases:
  - Per-Message Usage/Cost Tracking
  - Per-Message TTFT and Throughput
tags:
  - sdd
  - spec
  - observability
  - cost-tracking
  - parity-gap
created: 2026-07-20
status: draft
related:
  - "[[constitution]]"
  - "[[048-slm-cost-metrics/spec]]"
---

# Feature: Per-Message Usage and Cost Tracking

> [!info] Metadata
> **Author**: rust-researcher (CI cycle 1431)
> **Branch**: `feat/issue-6549/per-message-usage-cost-tracking`
> **Finding type**: parity gap / enhancement, P3 (single reference project so far)
> **Issue**: GitHub #6549

## 1. Overview

### Problem Statement

Zeph's cost/usage tracking is aggregate-only. `CostTracker` (`crates/zeph-core/src/cost.rs:160,44-51,198-209`) folds every `record_usage()` call directly into a `CostState { spent_cents, providers: HashMap<String, ProviderUsage> }` keyed by **provider name**, reset at the UTC day boundary — never keyed by message or turn id, and never persisted per-message. The `messages` table (`crates/zeph-db/migrations/sqlite/001_init.sql:6-12`) has no token/cost/latency columns; later migrations (`014_message_metadata.sql`, `070_message_category.sql`, `073_message_visibility_enum.sql`) add visibility/category flags but nothing usage-related. `UsageTracker` (`crates/zeph-llm/src/usage.rs:14`) and `LlmProvider::last_usage()`/`last_cache_usage()` (`crates/zeph-llm/src/provider.rs:919-927`) hold only the *most recent* call's token tuple in a `Mutex`, overwritten every call. TTFT/throughput exist only as ephemeral TUI state (`StreamRate` in `crates/zeph-tui/src/delights.rs:29-90`, feeding the status bar at `crates/zeph-tui/src/widgets/status.rs:490-509`) showing the *last completed turn's* numbers only — never written to DB, never retrievable historically per message.

Goose (reference agent, `aaif-goose/goose`, v1.42.0/v1.43.0, July 2026) ships "per-message usage stats UI (tokens, cost, TTFT, tok/s)" plus "per-message usage/cost tracking with derived session totals" — each individual message carries its own usage record, and session totals are *derived by summing* those records rather than being the only thing tracked. This was flagged as an open comparison item in a prior cycle (CI-1394, 2026-07-17) and left as "not confirmed present or absent" for zeph; code verification this cycle (CI-1431, 2026-07-20) confirms zeph has no equivalent.

### Goal

Every assistant message/turn persists its own usage record (input/output/cache tokens, cost in cents, TTFT, tokens/sec) queryable independently of the running session/daily aggregate. **Every** LLM call that feeds `CostTracker` — the conversational turn loop AND background/orchestration calls (planner, aggregator, ensemble members, scheduled/A2A tasks) — also emits a durable usage row, so the sum of the current UTC day's rows reconciles with `CostTracker.current_spend()`. Background/orchestration rows carry no `message_id` (there is no persisted conversational `Message` for them); conversational rows link to their `messages.id`.

### Out of Scope

- Changing `CostTracker`'s daily-budget enforcement semantics (spec `048-slm-cost-metrics`) — this spec adds a finer-grained record alongside it, not a replacement.
- A TUI per-message usage display — a natural follow-up once the data exists, but the data model is the blocking gap; UI is a separate, smaller PR.
- Cross-session aggregation or historical analytics dashboards beyond simple per-message/per-session queries.

## 2. User Stories

### US-001: Operator diagnosing a cost or latency spike
AS A Zeph operator investigating why a session's cost or latency jumped
I WANT to see which individual message(s) drove the increase (tokens, cost, TTFT, tok/s per message)
SO THAT I can pinpoint the expensive or slow turn instead of only seeing a daily/session aggregate

**Acceptance criteria:**
- A queryable per-message record exists with: input tokens, output tokens, cache read/write tokens, cost (cents), TTFT (ms), tokens/sec, provider name, model name.
- The record persists across process restarts (DB-backed, not in-memory `Mutex` state).
- `SUM(cost_cents)` over usage rows created during the current UTC day equals `CostTracker.current_spend()` (live current-day reconciliation). Rows are permanent and message-keyed; `CostTracker` resets at the UTC boundary and is provider-keyed, so reconciliation is defined for the current day only, never historically. Caveat: `usage_records.message_id`/`conversation_id` cascade-delete (`ON DELETE CASCADE`) when their parent `messages`/`conversations` row is purged (e.g. same-day conversation purge via the overflow/summary sweep) — the corresponding rows vanish from the `SUM`, but `CostTracker`'s in-memory daily aggregate is not decremented, so the reconciliation invariant can transiently desync in that edge case. Accepted for this MVP (low-sensitivity telemetry, not an audit trail).
- `ttft_ms` is populated on every LLM call that traverses an HTTP round-trip (Claude/OpenAI/Ollama/Gemini/Gonka/Cocoon, via `AnyProvider`/`MaskedProvider`/`TriageRouter` delegating to the active inner provider): the true time-to-first-token when the call streams — captured today only on the one production streaming path, speculative decoding, at `SpeculativeStreamDrainer::drive`'s stream-consumption point in `zeph-core` — otherwise a **TTFB (time-to-first-byte) proxy** measured from request-send to arrival of the first HTTP response byte. It is distinct from `latency_ms` (full round-trip). `ttft_ms` is `NULL` for the in-process Candle backend (no network path). It is also `NULL` for calls routed through `RouterProvider` (the ensemble/fallback router, `AnyProvider::Router`) — `RouterProvider` has a separate, pre-existing gap where it never propagates `last_usage`/`last_cache_usage`/`last_ttft_ms` from its active member provider at all (verified: it hardcodes `last_cache_usage` to `None` and has no `last_usage` override either); this is unrelated to this feature and not fixed here — tracked as a follow-up.

## 3. Key Invariants

- Per-message usage recording MUST NOT block the agent turn loop or hot path — write asynchronously via the existing persistence pipeline (`zeph-agent-persistence`), consistent with the project's non-blocking contract (CLAUDE.md "Async & Background Tasks").
- MUST NOT duplicate `CostTracker`'s existing daily-budget enforcement logic — per-message tracking is additive telemetry, not a second budget-enforcement path.
- MUST NOT introduce a new `tokio::spawn()` call site — route any async persistence through the existing awaited persistence path or a `TaskSupervisor`-named service per the project's supervisor mandate.
- Every production call site that feeds `CostTracker::record_usage` MUST also emit exactly one `usage_records` row computed from the same token/cost values (single pricing source of truth via `CostTracker::price_of`). Adding a new `record_usage` site without a paired ledger row is a reconciliation regression.

## 4. Evidence

- `crates/zeph-core/src/cost.rs:44-51,160,198-209,273` — `CostTracker`/`CostState`/`ProviderUsage`, provider-keyed daily aggregate only.
- `crates/zeph-db/migrations/sqlite/001_init.sql:6-12` — `messages` table schema, no usage columns.
- `crates/zeph-llm/src/usage.rs:14`, `crates/zeph-llm/src/provider.rs:919-927` — `UsageTracker`/`last_usage()`, transient last-call-only state.
- `crates/zeph-tui/src/delights.rs:29-90`, `crates/zeph-tui/src/widgets/status.rs:490-509` — `StreamRate`, ephemeral last-turn TTFT/throughput, TUI-only, not persisted.
- Reference: Goose `aaif-goose/goose` v1.42.0/v1.43.0 changelog (July 2026), "per-message usage stats UI (tokens, cost, TTFT, tok/s)" + "per-message usage/cost tracking with derived session totals".

## 5. Resolved Decisions

- **RESOLVED (was: new table vs. columns)**: A dedicated `usage_records` table (own autoincrement PK; nullable `message_id` FK → `messages.id ON DELETE CASCADE`; nullable `conversation_id`; `source` discriminator). A new table avoids widening the hot `messages` row, and the nullable `message_id` accommodates background/orchestration rows that have no conversational message. A partial-unique index on `message_id` (WHERE NOT NULL) keeps conversational rows 1:1.
- **RESOLVED (was: TTFT capture point)**: Non-streaming path records a TTFB proxy in `zeph-llm`'s per-provider `UsageTracker`, measured per-attempt inside `retry::send_with_retry` (or the equivalent per-attempt retry loop for gonka) so a retried call's value is never inflated by the backoff sleep between attempts, and exposed via `LlmProvider::last_ttft_ms()` (delegated through `AnyProvider`/`MaskedProvider`/`TriageRouter` to whichever concrete provider handled the call). The one production streaming path (speculative decoding) records true TTFT separately, at its own stream-consumption point in `zeph-core` (`agent::speculative::stream_drainer::SpeculativeStreamDrainer::drive`, elapsed time to the first SSE event), which `Agent::build_usage_record` prefers over the provider-level TTFB proxy when present. Candle (in-process) leaves it `NULL`.
- **RESOLVED (was: reconciliation scope)**: Every `CostTracker`-feeding site (turn loop, `plan.rs` planner + aggregator, `scheduler_loop.rs` ensemble members) emits a `usage_records` row using the same computed cost, so current-day `SUM` reconciles with the aggregate. See §3 invariants.
