---
aliases:
  - Cache Keepalive Pings
tags:
  - sdd
  - spec
  - llm
  - prompt-cache
created: 2026-11-10
status: approved
related:
  - "[[constitution]]"
  - "[[MOC-specs]]"
  - "[[003-llm-providers/spec]]"
  - "[[039-background-task-supervisor/spec]]"
  - "[[082-per-message-usage-cost-tracking/spec]]"
  - "[[088-prompt-cache-prefix-stability/spec]]"
---

# Spec: Active Prompt-Cache Keepalive Pings

> [!info]
> While a session is idle, an opt-in per-agent service replays the last main request as a cheap, side-effect-free ping so the provider's prompt cache stays warm. Claude only in v1. Parity: pi v0.86.0, OpenCode v2.2.6, Aider `--cache-keepalive-pings`. GitHub #6911 (#3096, #2715).

## 1. Problem

Provider prompt caches expire quickly (Anthropic ephemeral: 5 minutes). After an idle gap the next real turn pays a full cache write. Zeph tracked warmth passively; reference agents refresh it with periodic cheap requests.

Keepalive is rarely worthwhile with an extended thinking budget: the gate prices the worst case of a ping that may regenerate its thinking (`budget + 1` output tokens), which usually exceeds the cache write it avoids, so the first ping of each idle period is refused and `/keepalive status` reports it as paused. It stays useful without a thinking budget, or with a small one.

The ping is priced, budgeted and attributed with the backend that is actually pinged (for a router: the `keepalive_provider` backend or the single prefix-caching one), never the backend that served the last request.

## 2. Out of Scope

Cache layout and breakpoints (spec 088); providers without a time-limited cache; extended-TTL selection; warming across restarts; non-Claude backends.

## 3. Functional Requirements

| ID | Requirement |
|----|-------------|
| FR-001 | With `[llm.cache_keepalive].enabled` and a prefix-caching provider, the service sends a minimal request reusing the cached prefix at `ttl - 30 s` after the last activity. |
| FR-002 | Disabled by default. |
| FR-003 | A real request (turn start and every main dispatch) resets the timer; an in-flight ping is cancelled and its result discarded. |
| FR-004 | Pings stop after `max_consecutive_pings` (default 6, validated to `1..=50` by `KeepalivePingCap`; a value outside the range fails config loading); real activity re-arms them. |
| FR-005 | `keepalive_provider` names a `[[llm.providers]]` entry; empty means the provider serving the requests (target rule in section 5). |
| FR-006 | The service runs under the agent's `TaskSupervisor` (`spawn_oneshot`, unique name `llm.cache_keepalive.<uuid>`); no direct `tokio::spawn`. |
| FR-007 | Ping tokens and cost are recorded in `CostTracker` and counted separately (`cache_keepalive_pings`, `cache_keepalive_cost_cents`), shown in `/keepalive status`, TUI `/cost` and the status bar. |
| FR-008 | While a ping is in flight the TUI shows the spinner `Warming prompt cache...`. |
| FR-009 | A failed ping is logged and backs off; it never surfaces as a turn error. Each ping is bounded by a 60 s timeout. |
| FR-010 | No ping while the cache is cold, the prefix changed, or the prefix is below the model's minimum cacheable size. |
| FR-011 | `[llm.cache_keepalive]` config, migration step 118, wizard step, `--cache-keepalive`, and `/keepalive on|off|status`. |

## 4. Non-Functional Requirements

- NFR-001 Pings never block input, turn or render loops.
- NFR-002 Cost: a gate before each ping compares the worst-case ping cost (prefix cache read plus `keepalive_output_cap` output tokens, priced at the captured speed) with the cache write it avoids; the daily budget is respected. A ping that cannot be priced (no cost tracker, cost tracking off, or a model without pricing) fails closed with `Stopped(Unpriced)` when its output cap exceeds one token (a thinking budget is set); a one-token ping stays bounded by the ping cap alone (#7129).
- NFR-005 Memory: a capture copies only the messages added since the previous capture (#7128, see section 5).
- NFR-003 Security: a ping adds no content, tools or secrets beyond the captured prefix; masking is applied exactly as for the main request.
- NFR-004 Observability: span `llm.cache.keepalive`; cache read tokens per ping.

## 5. Design

- `LlmProvider::keepalive_ping(&KeepaliveRequest) -> KeepaliveOutcome` (default `Unsupported`), forwarded by `AnyProvider`, `LlmProviderDyn`, masking (which masks the replayed messages) and the router; Claude overrides it. The request is a non-streaming call with the main request's messages, tools and `max_tokens` floor; it writes nothing to provider usage, the prefix guard or the status channel.
- `KeepaliveRequest { provider, messages, tools, target }` is captured by core after every successful main-loop response, tool-use responses included (a turn that waits longer than the TTL after a tool-use response, for a long tool run or an unanswered `ask_user` question, must not go cold; the capture runs only while the service is armed and unchanged tools share one allocation); the provider is a clone carrying the thinking, effort and model state the request was sent with. Side calls on clones and subagents never touch it. `/clear` and a provider switch drop it.
- **History mirror (#7128)**: `Shared` keeps the captured history as `HistoryChunks`, a list of immutable `Arc<[Message]>` chunks. `capture` compares the incoming history with the mirror using `wire_eq` (role, content, parts, and metadata `visibility`, `origin`, `request_policy`) outside the lock: when the mirror is a prefix, only the new tail is copied as one more chunk (`Appended`); otherwise (rewrite, truncation, compaction, first capture) the history is copied once (`Rebuilt`); above 64 chunks they are merged (`Coalesced`, one full copy). The service materializes the single `Arc<[Message]>` a ping needs in `begin_ping`, after releasing the lock, so the agent never waits on a full copy. The capture point is unchanged (after the response, before compaction); span `core.keepalive.capture` (debug).
- **Target rule**: a single provider must equal `keepalive_provider` (or it is empty) else `Stopped(ProviderMismatch)`; a router pings the backend `keepalive_provider` names, else its single prefix-caching backend, else `Unsupported` with one warning.
- **State** (`KeepaliveState` in `MetricsSnapshot`): `Disabled`, `Idle`, `InFlight`, `Stopped(StopReason)`. `CapReached` and `BudgetExhausted` re-arm on real activity; `NoCacheHit` (a ping read no cache), `PrefixRewritten` (a ping wrote more than 512 cache tokens: something uncaptured changed the prefix), `Unpriced` (the ping cannot be priced and may generate more than one token), `ProviderMismatch` and `UserOff` are cleared only by `/keepalive on` or a provider switch.
- The service lives until its guard drops (agent drop aborts it); it is spawned once per agent on the first armed turn start. Startup-only config: not hot-reloaded.

## 6. Key Invariants

1. Disabled by default; zero pings unless enabled.
2. A ping never changes provider state visible to a real request.
3. Spend is bounded by the ping cap (`1..=50`), the cost gate (fail closed when a multi-token ping cannot be priced) and the daily budget.
4. At most one ping task per agent, named uniquely so concurrent sessions never abort each other.
5. A failed or discarded ping never reaches a turn.

## 7. NEVER

- Spawn the service outside the supervisor; block a hot path; hardcode a model.
- Ping with content that was not part of a captured main request.
- Treat a ping as a provider request in cost accounting.

## 8. Acceptance Criteria

- [x] Default config sends zero pings; enabled sends them only within idle bounds (paused-time tests).
- [x] Ping cost visible in `CostTracker` and the TUI; the `tokio::spawn` scan count is unchanged.
- [ ] Live Claude session shows a cache read on the turn after an idle gap longer than the TTL (live gate).

## 9. See Also

- [[003-llm-providers/spec]], [[039-background-task-supervisor/spec]], [[082-per-message-usage-cost-tracking/spec]], [[088-prompt-cache-prefix-stability/spec]]
