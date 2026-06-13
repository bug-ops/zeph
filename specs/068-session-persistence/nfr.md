---
aliases:
  - Session Persistence NFR
  - NFR 068
tags:
  - nfr
  - session
  - persistence
created: 2026-06-13
status: draft
related:
  - "[[068-session-persistence/spec]]"
  - "[[068-session-persistence/brd]]"
issues:
  - "#2807"
  - "#3102"
  - "#3074"
---

# NFR 068 — Session Persistence, Event Log Replay, and `zeph serve`

Non-functional requirements follow ISO/IEC 25010:2011 quality categories. All targets are measurable.

---

## 1. Performance

| ID | Requirement | Target | Measurement |
|----|-------------|--------|-------------|
| NFR-P1 | Event log append latency (p99, single event, no compaction) | < 5 ms | Perfetto span `session.log.append` |
| NFR-P2 | Resume latency for a session with ≤ 10,000 events | < 2 s wall time | `time zeph sessions resume <id>` |
| NFR-P3 | Resume latency for a session with ≤ 100,000 events | < 15 s wall time | `time zeph sessions resume <id>` |
| NFR-P4 | Fork latency for a session with ≤ 10,000 events | < 5 s wall time | `time zeph sessions fork <id>` |
| NFR-P5 | Projection reconcile latency after crash (≤ 100 missing events) | < 500 ms | Log from INV-SP-3 path |
| NFR-P6 | `zeph serve` request latency (POST /sessions/:id/prompt, time-to-first-token) | No regression vs. non-serve mode (< 200 ms overhead) | Trace span `serve.http.dispatch` |
| NFR-P7 | Memory overhead per idle session actor (no pending prompts) | < 1 MB RSS | `ps` / heap profiler |
| NFR-P8 | Concurrent sessions in `zeph serve` with no per-session degradation | ≥ 10 simultaneous active sessions | Load test: 10 concurrent `POST /prompt` requests to 10 different sessions |

---

## 2. Reliability

| ID | Requirement | Target | Measurement |
|----|-------------|--------|-------------|
| NFR-R1 | Message loss on clean process termination (SIGTERM) | 0 events lost | Start session, SIGTERM, resume; diff event counts |
| NFR-R2 | Message loss on crash (SIGKILL) after turn complete | 0 complete turns lost; ≤ 1 in-flight event lost (torn write) | Kill mid-turn; resume; verify last acked turn present |
| NFR-R3 | Torn-write recovery (INV-SP-2) success rate | 100% — no panic, no data loss beyond in-flight event | Fuzz: truncate last line at random byte; open session |
| NFR-R4 | Projection reconcile (INV-SP-3) closes the gap | 100% — projection matches log after reconcile | Write event; kill before projection write; resume; compare |
| NFR-R5 | Condensation non-overlap (INV-SP-4) violations per 1,000 condense operations | 0 | Unit + property-based tests on `last_condensed_seq` high-water mark |
| NFR-R6 | Migration 105 idempotency | Applying migration 105 twice produces no error and no schema change | Run migration; run again; check schema |
| NFR-R7 | ACP fork/resume integration tests pass after ACP handler delegation | 100% pass rate | `cargo nextest run -p zeph-acp` |
| NFR-R8 | `zeph serve` graceful shutdown: all active sessions flushed | 0 events buffered but un-flushed after SIGTERM + 30s timeout | SIGTERM serve; resume all active sessions; compare event counts |

---

## 3. Security

| ID | Requirement | Target | Measurement |
|----|-------------|--------|-------------|
| NFR-S1 | Session event log file permissions on creation | `0o600` (owner read/write only) | `stat events.jsonl \| grep 0600` |
| NFR-S2 | Session blob directory permissions on creation | `0o700` (owner only) | `stat blobs/ \| grep 0700` |
| NFR-S3 | `[serve] auth_token` stored only in vault, never in config on disk | No plaintext token in `config.toml` after `--init` | Search `config.toml` for token literal |
| NFR-S4 | `serve.http` bearer auth: constant-time comparison (BLAKE3 + `subtle::ConstantTimeEq`) | Timing attack resistance | Code review; no short-circuit comparison |
| NFR-S5 | HTTP endpoint `/health` unauthenticated; all others require valid bearer token | 401 returned for missing/invalid token on all non-health endpoints | Integration test: 8 endpoints × 2 auth states |
| NFR-S6 | Session export (`sessions export`) does not include secrets injected by the vault | No `ZEPH_*` key values in exported JSONL | Grep exported file for vault key patterns |

---

## 4. Maintainability

| ID | Requirement | Target | Measurement |
|----|-------------|--------|-------------|
| NFR-M1 | `zeph-session` crate has no dependency on `zeph-llm`, `zeph-memory`, or `zeph-core` (dependency-free data layer) | No path in `Cargo.toml` from `zeph-session` to those crates | `cargo tree -p zeph-session` |
| NFR-M2 | `SessionEvent` enum variants covered by replay fold | 100% — every variant has a match arm in `ReplayEngine::replay` | Exhaustive match in Rust (compile-time) |
| NFR-M3 | All `pub` types and functions in `zeph-session` have rustdoc | 0 `rustdoc::missing_docs` warnings with `RUSTDOCFLAGS=--deny missing_docs` | `cargo doc -p zeph-session` |
| NFR-M4 | Spec-068 acceptance criteria (AC-1 through AC-13) have corresponding tests | 100% covered | Test names traceable to AC IDs in test file comments |

---

## 5. Compatibility

| ID | Requirement | Target | Measurement |
|----|-------------|--------|-------------|
| NFR-C1 | Existing ACP sessions (pre-migration-105) resumable without manual migration step | `zeph sessions resume <old_acp_id>` works for legacy sessions | Test on pre-105 SQLite fixture |
| NFR-C2 | `events.jsonl` format is stable once published (no breaking changes without a version field) | Schema version field included from day 1; parsers reject unknown versions gracefully | Unit test: parse v0 envelope, unknown version |
| NFR-C3 | `SessionsCommand` backward compatibility: `list`, `delete`, `resume --print` behave as before | Existing CLI scripts using these commands produce identical output | Golden-file tests for `list` and `delete` |
| NFR-C4 | PostgreSQL and SQLite produce identical behavior for migration 105 | Same rows created, same constraints enforced, same index names | Dual-dialect integration test fixture |

---

## 6. Usability

| ID | Requirement | Target | Measurement |
|----|-------------|--------|-------------|
| NFR-U1 | TUI shows a spinner for any replay/condensation operation lasting > 100 ms | Spinner visible within 100 ms of operation start | Manual observation + TUI event-log test |
| NFR-U2 | `zeph sessions list` output includes: session ID, title (derived from first user message), status, event count, last updated | All 5 fields present in output | `zeph sessions list` output parsing test |
| NFR-U3 | Error message when resuming a non-existent session ID is actionable | Message includes: "Session <id> not found. Run `zeph sessions list` to see available sessions." | `zeph sessions resume nonexistent` output assertion |

---

## 7. Portability

| ID | Requirement | Target | Measurement |
|----|-------------|--------|-------------|
| NFR-Pt1 | `events.jsonl` files are valid JSON on every line; parseable with any JSON library | No binary or non-UTF-8 content in log files | Round-trip with `jq` on exported file |
| NFR-Pt2 | `zeph serve` HTTP API conforms to standard HTTP/1.1 + SSE (W3C spec) | Responses parseable by `curl`, browser EventSource, and generic SSE client | Integration tests with `reqwest` + `eventsource-client` |
| NFR-Pt3 | Session export (`sessions export`) produces a self-contained JSONL file importable on a different machine | `sessions import <file>` succeeds on a clean install | Cross-machine import test (fixture-based) |
