---
aliases:
  - Runtime Thinking Controls NFR
  - Think Tokens Reasoning Effort NFR
  - NFR 3098
tags:
  - sdd
  - nfr
  - llm
  - core
created: 2026-07-10
status: approved
related:
  - "[[specs/070-runtime-thinking-controls/brd]]"
  - "[[specs/070-runtime-thinking-controls/srs]]"
  - "[[specs/070-runtime-thinking-controls/spec]]"
  - "[[specs/003-llm-providers/spec]]"
---

# NFR: Runtime Thinking Controls — `/think-tokens` and `/reasoning-effort` (GitHub #3098)

ISO/IEC 25010:2011 quality model.

---

## Performance Efficiency

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-PE-01 | Command dispatch and provider mutation add no observable latency to the turn boundary | < 100 µs; pure in-memory field writes, no I/O, no allocation beyond the returned `String` |
| NFR-PE-02 | No new heap-allocated shared state (`Arc`, `Rc`) is introduced solely to guard this mutation | `AnyProvider` remains an owned enum field on `Agent<C>`, not wrapped |

---

## Concurrency / Compatibility (Race-Free Turn-Boundary Mutation)

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-CC-01 | Mutating `self.provider` from a slash-command handler introduces no new lock, `RwLock`, `Mutex`, or `ArcSwap` | Code review confirms zero new synchronization primitives across the 13 touched files |
| NFR-CC-02 | The mutation is race-free because it occurs strictly between turns on the single-threaded `&mut self` turn loop, before `process_user_message` | Matches the existing `/provider` switch and `set_reasoning_effort` restore-path precedent (architect handoff, critic-confirmed) |
| NFR-CC-03 | Background tasks that hold a `self.provider.clone()` snapshot taken before a runtime mutation are NOT required to observe the mutation | Documented expected behavior (architect handoff, "Background-clone staleness") — not a defect; already-spawned clones keep their pre-mutation snapshot, consistent with existing `/provider`/`reasoning_effort` behavior |
| NFR-CC-04 | The added `&mut self` provider config setters do not violate spec-003's "provider methods are always `&self`" invariant for request/inference methods | `chat`, `chat_stream`, `chat_with_tools` remain `&self`; the invariant is clarified in `[[specs/003-llm-providers/spec]]` to scope explicitly to request/inference methods, exempting turn-boundary config setters (this spec adds the clarifying line; `set_reasoning_effort` already established the precedent before this invariant was written) |

---

## Reliability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-RE-01 | Out-of-range token-budget or malformed effort input never panics | All validation returns `Result`/`LlmError`, surfaced as a formatted error string at the `AgentAccess` boundary |
| NFR-RE-02 | Repeated enable/disable/enable cycles on Claude thinking are idempotent with respect to `max_tokens` | S1 property: `set_thinking(Some(_))` always recomputes from the immutable `base_max_tokens` baseline, never from the current (possibly floored) `max_tokens` — no ratchet |
| NFR-RE-03 | A `/provider` switch after a runtime override never leaves the new provider in an inconsistent or silently-stale state | S2: reset notice informs the user; the new provider always starts from its own configured defaults, never inherits stale override state |

---

## Maintainability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-MA-01 | `with_thinking` (construction-time builder) delegates to `set_thinking` (runtime setter) — no duplicated range/flooring validation logic | Single source of truth in `ClaudeProvider::set_thinking`; `with_thinking` becomes a thin wrapper |
| NFR-MA-02 | The persistence-restore path (`set_reasoning_effort`, OpenAI-only, string-based) and the new live-command path (`apply_reasoning_effort`, enum-based, all providers) remain two distinct methods, not merged | Deliberate S3 boundary — merging them would risk accidentally routing restore through the new fan-out; the minor code duplication in the OpenAI arm is the accepted cost |
| NFR-MA-03 | `ReasoningEffort` enum lives in `zeph-llm` (the crate that owns `AnyProvider`), not in `zeph-commands` or `zeph-core` | Lower layers own the domain type; the command layer parses user input into it and passes it down |
| NFR-MA-04 | All new `pub` items across the 13 touched files carry doc comments per the workspace rustdoc gate | `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-llm -p zeph-commands -p zeph-core` passes clean |

---

## Usability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-US-01 | No-arg invocation of either command always shows the current value, even when unset | Getter returns `None`-formatted as "not set" / provider default, never an empty or confusing string |
| NFR-US-02 | Unsupported-provider responses name the provider and the specific capability that is missing | e.g. "provider `ollama` does not support a thinking-token budget" — never a bare no-op |
| NFR-US-03 | Claude cross-override (Extended ↔ Adaptive) is stated explicitly in the confirmation message | User is never surprised that setting one silently cleared the other |
| NFR-US-04 | The `/provider` switch reset notice fires only when a runtime override was actually active | Avoids notice fatigue on every ordinary switch (see SRS FR-009's non-blocking over-warn caveat for the known conservative-but-acceptable exception) |

---

## Compatibility / Scope Boundary

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-CO-01 | Zero SQLite schema changes | No migration step required; `--migrate-config` is trivially N/A for this feature (S3) |
| NFR-CO-02 | Zero new `[[llm.providers]]` config fields | Startup defaults continue to resolve via existing `ProviderEntry` fields (`thinking`, `reasoning_effort`, `thinking_level`/`thinking_budget`) |
| NFR-CO-03 | `zeph-config` crate is not touched | All new state lives on the provider structs in `zeph-llm` and is session-scoped only |
