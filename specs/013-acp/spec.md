---
aliases:
  - ACP
  - Agent Client Protocol
tags:
  - sdd
  - spec
  - protocol
  - acp
created: 2026-04-08
updated: 2026-07-27
status: approved
related:
  - "[[MOC-specs]]"
  - "[[014-a2a/spec]]"
---

# Spec: ACP (Agent Client Protocol)

> [!info]
> ACP transports, session management, permissions, fork/resume,
> capability advertisement, agent-client-protocol 2.0.0 / schema =1.5.0 — see Addendum
> (migration completed, bumped in `Cargo.toml`).

## Spec Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-04-08 | sdd | Initial spec (SDK 0.11.1 / schema 0.12.0) |
| 1.1 | 2026-05-19 | sdd | Updated to SDK 0.12.1 / schema 0.13.2; added Providers API, Elicitation, MCP-over-ACP, Session Usage, Session Delete migration, v2 tracking, breaking changes resolution |
| 1.2 | 2026-05-29 | sdd | Mark Providers API, Elicitation protocol, Session Usage, and session/delete as implemented; update SDK to 0.12.1; wire IDE-provided MCP servers into do_new_session; add blocking-await timeout note |
| 1.3 | 2026-06-06 | sdd | ACP 0.14.0 protocol bump: bumped core 0.12.1→0.14.0, schema pinned =0.13.6; removed session/set_model RPC (model switching preserved via set_config_option); removed inbound message-id echo feature; renamed provider ext-method types to singular; stabilized delete/logout/resume/add-dirs feature flags; renamed session-usage upstream gate; added elicitation core passthrough; documented MessageId newtype change |
| 1.4 | 2026-06-30 | developer | ACP 1.0.1 schema-path migration: bumped core 0.14.0→1.0.1, schema pinned =1.1.0; mechanical `schema::X` → `schema::v1::X` reorg (root re-export removed upstream), `ProtocolVersion`/`MaybeUndefined`/`IntoOption`/`IntoMaybeUndefined` stay flat; removed root re-exports `cookbook`/`handler`/`jsonrpcmsg`/six message enums; deleted 5 long-dead `#[cfg(any())]` test modules (153 unverifiable sites); no handler/transport/builder logic changed; `unstable_cancel_request` and `model_config` evaluated and deferred to follow-up issues |
| 1.5 | 2026-07-01 | developer | Adopt `model_config` option category (#5361): new `config_id="temperature"` `session/set_config_option`, presets precise/balanced/creative, `[acp.model_config]` config section, CLI + wizard integration. Wire `unstable_cancel_request` (#5362): new `unstable-cancel-request` Cargo feature (not in `default`), `$/cancel_request` bridged onto `cancel_signal: Arc<Notify>` via `Responder::cancellation()` in `session/prompt`, plus a low-level tracing-only `CancelRequestNotification` handler in the `Agent.builder()` chain. Added test coverage for the 8 previously-untested handler files (#5367). |
| 1.6 | 2026-07-01 | developer | Review fix pass on #5361/#5362: (S-C1) `default_temperature_preset` is now primed into the effective `provider_override` at session creation (`do_new_session`/`do_load_session`/`do_fork_session`/`do_resume_session`), not just advertised in the IDE dropdown; (S-C2) the `$/cancel_request` watcher select is now `biased` with prompt completion checked first, and `drain_agent_events` drains a stale `cancel_signal` permit before its main loop, so a cancellation resolving at/after prompt completion can no longer leak into the next, unrelated prompt (also hardens the pre-existing `session/cancel` no-active-prompt race — `cancel_before_prompt_returns_cancelled` renamed to `cancel_before_prompt_is_a_no_op` to reflect the corrected semantics). Documented fork/resume reset behavior for `temperature_preset` (#5373 tracking issue filed); filed #5374 for the untested `resume_session` store-backed path. |
| 1.7 | 2026-07-01 | developer | Fixed #5379: `zeph acp model-config show` now loads the resolved config and marks the active `[acp.model_config].default_temperature_preset` in its output, instead of only printing the static preset table. Resolved #5373: `session/fork` and `session/resume` now inherit `model`/`temperature_preset`/`thinking_enabled`/`auto_approve_level` from the source session (live in-memory state, falling back to a persisted close-time snapshot, falling back to configured defaults) instead of always resetting to defaults — new `acp_sessions` columns via migration `105_acp_session_config`; see "Fork/Resume Config Inheritance" below. |
| 1.8 | 2026-07-17 | developer | Renovate `rust-minor-patch` bundle bumped core `1.0.1`→`1.2.0`, schema `=1.1.0`→`=1.4.0`. Core `1.1.0` stabilized `$/cancel_request` unconditionally and dropped its `unstable_cancel_request` forward — `unstable-cancel-request` is now a local-only opt-in gate (Cargo feature unchanged, but no longer maps to an upstream feature); `unstable-boolean-config` tombstoned the same way after schema `1.1.0` made `SessionConfigOptionValue::Boolean` unconditional. Schema `1.4.0` renamed `SetProviderRequest`/`DisableProviderRequest`/`ProviderInfo`'s `id: String` field to `provider_id: ProviderId` (`Arc<str>` newtype) — updated all `providers.rs` call sites and tests. No handler/transport/builder-chain logic changed otherwise. |
| 1.9 | 2026-07-21 | developer | Mechanical decomposition (#6624): `ZephAcpAgentState`'s god-object `agent/mod.rs` (4424 lines, six unrelated responsibilities) split into eight sibling files under `crates/zeph-acp/src/agent/`. No public API or behavior change. See "Implementation Structure" below. |
| 1.10 | 2026-07-23 | sdd | ACP v2 migration plan (spec update only, no code changes yet): documented bump `agent-client-protocol` 1.2.0 → 2.0.0, schema pin left as an open item resolved via `cargo tree` at implementation time (between `=1.5.0`/`=1.6.0`, both confirmed wire-V1-safe); added the crate-major-vs-wire-protocol-version invariant (`ProtocolVersion::LATEST == V1 == 1`, compile-enforced when `unstable_protocol_v2` is off); added "Breaking Changes Resolution (SDK 1.2.0 → 2.0.0)" table; reconciled the ~15 present-tense sections the 2026-07 audit below flagged stale at `1.0.1`/`=1.1.0` directly to the `2.0.0` target state; added Implementation Gap Tracker entry I23. |
| 1.11 | 2026-07-27 | developer | ACP 2.0.0 migration completed (#6655): bumped core `1.2.0`→`2.0.0`, schema resolved and pinned `=1.4.0`→`=1.5.0` (`cargo tree` confirms this is what `agent-client-protocol 2.0.0` requires). Pre-merge wire gate passed: `ProtocolVersion::LATEST.as_u16() == 1` under schema `1.5.0` with `unstable_protocol_v2` off; added a compile-time `const _: () = assert!(...)` regression guard in `crates/zeph-acp/src/lib.rs`. All three named compiler-verify points (`ActiveSession::connection()` return type, `Option<&T>` accessor changes, `Dispatch<Req, Notif>` new bound) compiled clean with zero source changes required. Full CI suite green (fmt, clippy, 15071 nextest tests, rustdoc gate, workspace doc-tests). Live round-trip verified via `zeph acp run-agent` (self-hosted client↔agent over the real SDK, real Ollama completion, `stop_reason=EndTurn`). Also resolves #6633 — the ~15 `1.0.1`/`=1.1.0` references remaining in the body are historical narrative and were verified correct as-is, not current-state staleness. **Several claims in this entry and the body it wrote were later found inaccurate by pre-review (critic/tester/security) — see v1.12.** |
| 1.12 | 2026-07-27 | developer | Pre-review correction pass on #6655 (v1.11), triggered by critic verdict "significant" (S1-S3) plus tester and security findings — no new migration work, all corrections to v1.11's own claims and evidence gaps: **(1)** `crates/zeph-acp/README.md:12` still said SDK v1.0 — fixed to v2.0. **(2)** v1.11 claimed `/agent.json` and `/.well-known/acp.json` "share the same `discovery_handler`" and both carry `protocol_version` — **false**; they are two distinct handlers (`discovery_handler` vs `agent_json_handler`, `transport/router.rs`), and only `/.well-known/acp.json` carries `protocol_version` at all — corrected throughout "Protocol Version" and the Addendum, including a pre-existing (not #6655-introduced) instance of the same error in the historical 0.14.0→1.0.1 note. **(3)** v1.11's "220-test integration suite" conflated the whole-crate test count with `tests/integration.rs` (33 tests) — reworded. **(4)** The literal `cargo nextest run -p zeph-acp`/`--all-features` commands v1.11 cited do not actually run under `default = ["sqlite"]"`/hit the `zeph-db` sqlite+postgres `compile_error!` guard, so the fork/providers/cancel tests and the 6 per-feature standalone builds were unverified as literally documented — re-ran with the correct `--no-default-features --features "sqlite,..."` command (plus `CARGO_BUILD_WARNINGS=warn` to bypass a pre-existing, unrelated zeph-core dead-code false-positive) and confirmed all pass; documented the caveat. **(5)** The `Dropping a Responder...` and `Ordered-response-callback barrier...` table rows both had incorrect supporting reasoning (verified false by security against vendored SDK source) — conclusions unchanged, reasoning corrected; the latter surfaced a **pre-existing HIGH-severity permission-gate deadlock** (`handle_prompt` holds the serial dispatch loop across the whole turn, so `block_task`'s permission reply can never route back) — identical in 1.2.0, not introduced by this migration, tracked separately, not fixed here. **(6)** Added a real regression test for the 2.0.0 JSON-RPC batch-transport addition (`post_batch_body_dispatches_all_entries_and_returns_all_responses`) — previously asserted "for free" with no test evidence. **(7)** Added a runtime test (`protocol_version_latest_is_hardcoded_wire_v1`) that hardcodes the literal `1` independently of the `LATEST` symbol, since the const-assert and `discovery_returns_expected_json_fields` both derive from that same live symbol and would stay green even if the invariant were silently weakened. **(8)** Reworded the `lib.rs` guard's comment and assert message: it is a tautology for the version pinned today, valuable only against a *future* schema-pin redefinition of `LATEST`. **(9)** Added Breaking Changes Resolution rows for the `agent-client-protocol-schema` `=1.4.0`→`=1.5.0` pin delta (audited: no impact) and for `TypeNotification` (not removed, de-generified — corrected from an earlier "Removed" misclassification), `MatchDispatch::from_handled` (removed), `SessionBuilder::with_mcp_server` (now cfg-gated) — all zero-usage in Zeph. **(10)** `cargo deny check advisories` independently re-run — **superseded, see v1.13 item (4): this command and its cited results were wrong.** **(11)** I20's Gap Tracker row said "Implemented (this PR)" three rows above I23's "(#6655)" — ambiguous which PR; disambiguated to its actual 2026-06-30/v1.4 origin. |
| 1.13 | 2026-07-27 | developer | Second correction pass, triggered by (a) team-lead asking for a definitive answer instead of v1.12's "file a follow-up" hedge on `/agent.json` `protocol_version`, and (b) reviewer catching that v1.12 fixed only the "Protocol Version" section's own claims, missing sibling copies of the same `/agent.json`-vs-`/.well-known/acp.json` confusion elsewhere in the file. **(1)** Fetched the upstream ACP Registry RFD (`agentclientprotocol.com/rfds/acp-agent-registry.md`) directly: required manifest fields are exactly `id`/`name`/`version`/`description`/`distribution` — **no `protocol_version` field exists in that schema at all**. `agent_json_handler` (added #2431, predates this migration by 4 months) already implements exactly this set — the omission is correct as designed, not a gap; the original migration step's "assert protocol_version on both endpoints" requirement was itself based on a false premise. Noted, not fixed (out of scope): the RFD's optional fields (`repository`/`authors`/`license`/`icon`) are unimplemented, and its `binary` distribution type wants Windows listed alongside Darwin/Linux. **(2)** Reviewer found the "#### /agent.json Endpoint" subsection (Capability Negotiation) had been mislabeled since before this PR — its example JSON (`protocol`, `protocol_version`, `transports`, `authentication`) is `discovery_handler`'s actual response shape, not `agent_json_handler`'s; retitled to "#### /.well-known/acp.json Endpoint" and added a new, separate, accurately-shaped "#### /agent.json Endpoint" subsection. **(3)** Fixed 3 more instances of the same `/agent.json` mislabeling v1.12 missed: the "#### Key Invariants" subsection under Capability Negotiation (2 bullets), a *separate* top-level "## Key Invariants" section (1 bullet, easy to miss — same wording, different section), and a historical "Version Upgrade Note (0.12.1 → 0.14.0)" bullet (`/agent.json` `protocol` field → corrected to `/.well-known/acp.json`, matching the parallel fix already applied to the adjacent 0.14.0→1.0.1 note in v1.12). **(4)** Reviewer caught that v1.12 item (10) and the Addendum's step 5 cited bare `cargo deny check advisories` reporting "3 long-standing accepted advisories (RUSTSEC-2026-0173, RUSTSEC-2025-0134, RUSTSEC-2024-0370)" — **wrong invocation and wrong result**. The project's actual CI command is `cargo deny --config .github/deny.toml check` (`.github/workflows/security.yml:25`); running that (reproduced independently) gives **`advisories ok`, zero failures** — `.github/deny.toml`'s ignore list covers 4 IDs (RUSTSEC-2025-0134, RUSTSEC-2024-0436, RUSTSEC-2026-0173, RUSTSEC-2026-0192), not the 3 cited, and RUSTSEC-2024-0370 was never in that list at all (likely misread from advisory-text historical context, not a live finding). Bare `cargo deny check advisories` without `--config` does show 2 unignored hits, but that is not the command this repo's CI runs. Bottom-line conclusion unchanged (no new advisory from the bump) — only the command/count/IDs cited as evidence were wrong. |
| 1.14 | 2026-08-16 | sdd | Spec-drift reconciliation pass (v0.22.3→HEAD audit): four post-migration bugfixes landed after v1.13 but never updated this spec. **(1)** #6660 fixed the exact permission-gate deadlock v1.12 item (5) had documented as "pre-existing... tracked separately, not fixed here" — `handle_prompt` (`crates/zeph-acp/src/agent/handlers/prompt.rs`) now spawns the whole turn via `cx.spawn` instead of awaiting it inline inside the SDK's `on_receive_request` callback, freeing the serial dispatch loop to route the IDE's `session/request_permission` reply back while the turn runs; the response is sent from inside the spawned task. **(2)** #6665 introduced `PromptChannelGuard` (`turn.rs`): `entry.output_rx` is now owned by this guard for the whole `do_prompt` call and restored in `Drop`, covering every exit path (previously only the success path after `drain_agent_events` restored it, so a `input_tx.send` failure or task-abort mid-drain permanently wedged the session with "prompt already in progress"). `drain_agent_events` borrows the receiver rather than consuming it. **(3)** #6672 hardened the guard against a reload/resume race: `SessionEntry` gained a monotonic `generation: u64` field (`SESSION_ENTRY_GENERATION` atomic, stamped in `make_session_entry`, the sole construction site for every session-creation path); `PromptChannelGuard` captures the generation at acquisition and skips its restore in `Drop` if the entry's current generation no longer matches — otherwise a session reloaded mid-turn (`do_load_session`/`do_resume_session` inserting a fresh entry over the same `SessionId`) could have its live receiver clobbered by a dead one from the superseded turn. A receiver restored after task abort/cancel can also carry stale queued events, so it is now drained both in `Drop` (cheap early filter) and in `acquire_prompt_channels` (under the sessions lock, right after `output_rx.take()` succeeds) to close the full inter-turn window rather than one instant. **(4)** #6684 gave `SessionEntry` a `agent_loop_handle: Mutex<Option<JoinHandle<()>>>` (set via `set_agent_loop_handle` at every spawn site: `session/new`, `fork`, `resume`, and the reload path); `do_close_session`/`do_delete_session` now abort and await this handle (bounded 5s timeout) before the entry is dropped, instead of only signalling `cancel_signal.notify_one()` and hoping the loop noticed — previously a reload/resume of the same `SessionId` could race a still-running old loop and corrupt the new turn's event stream; `SessionEntry`'s own `Drop` aborts the handle unconditionally as a safety net for LRU-eviction/reaper removal paths. The same PR also fixed `/review`: it was dispatched fire-and-forget via `input_tx.try_send` and returned `EndTurn` immediately, bypassing `acquire_prompt_channels` — once (3) started draining queued events at acquire time, `/review`'s own output was silently discarded. `/review` is now intercepted inside `do_prompt` (`turn.rs`, matched via `build_review_prompt`, `slash.rs`) and routed through the normal `acquire_prompt_channels`/drain turn path like any other prompt. |

> [!note] 2026-07 stale-body audit — resolved in v1.10
> A 2026-07 audit found the top summary (line 22) and changelog table were updated for the 1.8
> bump (`1.0.1` → `1.2.0` / `=1.1.0` → `=1.4.0`) but ~15 present-tense assertions in the body below
> still read `agent-client-protocol 1.0.1` / schema `=1.1.0`. The v1.10 update above reconciles all
> of them directly to the `2.0.0` migration target, skipping the intermediate `1.2.0` restatement.
> Sections narrating a specific **historical** transition (e.g. "Breaking Changes Resolution (SDK
> 0.14.0 → 1.0.1)") are unaffected — they correctly describe what changed in that past bump.

---

## Sources

### External
- ACP specification: https://agentclientprotocol.com/get-started/introduction
- ACP Rust SDK: https://github.com/agentclientprotocol/rust-sdk
- `agent-client-protocol` crate: https://crates.io/crates/agent-client-protocol

### Internal
| File | Contents |
|---|---|
| `crates/zeph-acp/src/lib.rs` | Public API, `AgentSpawner`, `AcpContext` |
| `crates/zeph-acp/src/transport/stdio.rs` | stdio transport |
| `crates/zeph-acp/src/transport/http.rs` | HTTP+SSE transport |
| `crates/zeph-acp/src/transport/ws.rs` | WebSocket transport |
| `crates/zeph-acp/src/transport/auth.rs` | Bearer token auth |
| `crates/zeph-acp/src/transport/router.rs` | axum router |
| `crates/zeph-acp/src/permission.rs` | `AcpPermissionGate`, TOML persistence |
| `crates/zeph-acp/src/agent/mod.rs` | Session lifecycle, `AgentSpawner` |
| `crates/zeph-acp/src/fs.rs` | `resolve_resource_link`, SSRF/path checks |
| `crates/zeph-acp/src/mcp_bridge.rs` | MCP passthrough |

---

`crates/zeph-acp/` (feature: `acp`) — enables IDE integration via Agent Client Protocol.

## Transports

| Transport | Feature | Notes |
|---|---|---|
| stdio | `acp` (base) | Primary; mutually exclusive with TUI |
| HTTP + SSE | `acp-http` | axum server, SSE for streaming |
| WebSocket | `acp` | tokio-tungstenite |

- ACP stdio and TUI are **mutually exclusive** — both own stdin/stdout
- Enforced at startup: attempting both → hard error with clear message

## Session Model

```
AcpSessionManager
├── sessions: LruCache<SessionId, AcpSession>  — bounded by max_sessions
├── max_sessions: usize                         — default 10
└── eviction: LRU policy
```

- Sessions are stateful: each has its own conversation history + tool context
- **LRU eviction**: oldest unused session is dropped when capacity is reached
- Session fork: create a new session branching from an existing session at a given turn
- Session resume: reconnect to an existing session by ID

## Implementation Structure (`agent/` module decomposition, #6624)

`ZephAcpAgentState` (the ACP session coordinator struct) had accumulated six unrelated
responsibilities spanning ~2955 lines of impl blocks inside a 4424-line `agent/mod.rs`:
builder wiring, idle-session reaping, LSP diagnostics, session lifecycle, slash-command/model
dispatch, and MCP extension handling. This increased merge-conflict surface on every ACP
feature PR and made it hard to reason about which methods were safe to test in isolation.

Each cluster was extracted into a sibling file under `crates/zeph-acp/src/agent/`, following
the existing `providers.rs`/`usage.rs` precedent (file-local `impl ZephAcpAgentState` blocks):

| Module | Responsibility |
|---|---|
| `builder.rs` | Dependency wiring / construction |
| `reaper.rs` | Idle-session reaping |
| `lsp_events.rs` | LSP diagnostics forwarding |
| `mcp_ext.rs` | MCP extension-method handling |
| `model.rs` | Model switching / model_config dispatch |
| `slash.rs` | Slash-command dispatch |
| `turn.rs` | Turn/prompt execution |
| `session.rs` | Session lifecycle (new/load/fork/resume/close) |

`mod.rs` shrank from 4424 to 1408 lines, retaining the struct definition, type aliases, and
thin protocol dispatch methods. Mechanical extraction only: all 25 fields stay on the
coordinator struct, no behavior change, no signature changes beyond the necessary
private-to-`pub(crate)` visibility bumps required for cross-file calls within the crate.

### Agent Spawner Contract

Agent sessions use the `Agent.builder()` / `run_agent()` pattern. Session state is `Arc`-wrapped.
Session tasks are launched via `tokio::task::spawn_local` inside a `LocalSet` — the
`AgentSpawner` closure returns `Pin<Box<dyn Future<Output = ()> + 'static>>` (`!Send`).

SDK 0.12.0 removed `McpAcpTransport` and the direct `tokio` re-export; the dead
`agent-client-protocol-tokio` crate was also removed entirely in the 0.14.0 bump.
Zeph is unaffected: `McpAcpTransport` was never used, Zeph has its own `tokio` dependency,
and `agent-client-protocol-tokio` was removed from both workspace `Cargo.toml` and `crates/zeph-acp/Cargo.toml`.

`session/close`, `session/resume`, `session/delete`, and `session/logout` are unconditional in
core 2.0.0 (unconditional since the 0.14.0 bump; unaffected by the 1.0.1 schema-path migration or
the 2.0.0 crate-API migration). The corresponding `unstable-session-*` Zeph feature flags
are tombstoned as no-op `= []` (retained only so root `Cargo.toml` forwarding resolves without
changes).

**Unchanged in 2.0.0**: the builder + `on_receive_request!`/`on_receive_notification!`/
`on_receive_dispatch!` macros, `Responder`, `ConnectionTo`, `ByteStreams`, `.block_task()`, and the
`acp::schema::v1::*` path are all confirmed unchanged by the 2.0.0 crate-major bump — grep-confirmed
zero usage of every renamed/removed 2.0.0 symbol (see "Breaking Changes Resolution (SDK 1.2.0 →
2.0.0)" below).

**Status: implemented** (SDK upgraded to 0.14.0 / schema =0.13.6; schema-path migrated to 1.0.1 /
=1.1.0; bumped to 1.2.0 / =1.4.0 in the 1.8 renovate bump; migrated to 2.0.0 / schema =1.5.0 in
this PR — see "Version Upgrade Note (1.2.0 → 2.0.0)" in the Addendum).

## Permission Model

```
AcpPermissionGate (TOML-backed, SQLite-persisted)
├── per-tool rules: Simple("allow"|"deny") | Patterned { default, patterns }
└── persistence: survives process restart
```

- Permissions stored in TOML config dir, loaded at startup
- For shell tools: extracts binary name (skips transparent prefixes: `env`, `exec`, `nice`, `nohup`, `time`)
- Patterns: `git = "allow"`, `rm = "deny"` — applied to binary names
- Async request queue: async lookup with oneshot reply channels — agent blocked until user answers
- Tool call lifecycle: `proposed → approved/denied → persisted → executed → result`

### Shell Interpreter Permission Cache Identity (#6511, #6485)

For ordinary binaries, the "Allow always"/"Reject always" cache key is the extracted
binary name (`build_permission_title` in `crates/zeph-acp/src/terminal.rs`), preserving
per-binary granularity. For shell interpreters (`SHELL_INTERPRETERS`: `bash`, `sh`, `zsh`,
`fish`, `dash`) the binary name alone does not determine what the command does — `bash -c
<script>` can run arbitrary code, and shell-stdin writes are equivalent to typing more
commands. Keying the cache on the interpreter name alone let one approved `bash -c` script
silently authorize every future invocation of that interpreter regardless of content,
including the args-form (`command` + structured `args`) variant and case-variant
interpreter names.

The cache key for shell interpreters now embeds a BLAKE3 digest of the effective
payload — the full command line, or (for the args-form) `command` joined with `args` via
a `\u{1}` separator (`effective_bash_payload`), or the stdin bytes for `bash_stdin` writes.
"Allow always" is therefore scoped to the exact command/payload: repeating the identical
command still short-circuits the prompt, but any different command triggers a fresh IDE
prompt.

#### Key Invariants

- A shell-interpreter "Allow always" grant MUST NOT extend to a different command/payload
  through the same interpreter — the cache identity binds to a digest of the payload, not
  the interpreter name alone
- This applies uniformly to both the inline-command and args-form (`command` + `args`)
  invocation styles, and to `bash_stdin` writes
- Breaking change: persisted shell-interpreter permission cache entries from before this
  fix are silently ignored and re-prompted

## Protocol Messages

- Rich content: images, file resources, binary data
- Model switching: client requests a specific model via `session/set_config_option` with `config_id="model"` (see Model Switching below)
- Terminal forwarding: tool output streams back to IDE terminal
- File tools: read/write/list within session working directory
- MCP passthrough: MCP tools are forwarded to ACP client via `mcp_passthrough` capability

## Configuration

ACP behavior is configured via the `[acp]` section in `config.toml`. The following fields
are available in PR4+:

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable ACP server |
| `agent_name` | String | `"zeph"` | Agent name advertised to clients |
| `transport` | String | `"stdio"` | Transport: `stdio`, `http`, `ws`, `both` |
| `additional_directories` | `Vec<String>` | `[]` | **Request-side allowlist.** Paths a client may pass in `sessionInit.additionalDirectories`. Paths not in this list are rejected at session start. This is NOT a protocol advertisement — it is a server-side gate. Field is unconditional (degated in the 0.14.0 bump; unaffected by the 1.0.1 schema-path migration). |
| `auth_methods` | `Vec<String>` | `["agent"]` | Accepted authentication methods. MVP: only `"agent"` is valid. Unknown values are rejected at deserialization. |

> **Changed in 0.14.0 bump**: `message_ids_enabled` is retained as a no-op field for config-schema
> compatibility (read by `acp_commands.rs`). The `PromptRequest.message_id` and
> `PromptResponse.user_message_id` protocol fields were deleted upstream in schema 0.13.6; the
> inbound message-id echo behaviour is removed.

### Key Invariants

- `additional_directories` is a **request-side allowlist**: paths requested by the client must be
  a prefix of a configured allowed path; requests with non-allowed paths are rejected with
  `AcpError::PermissionDenied` at session start — never silently ignored
- `auth_methods` must only contain `"agent"` for MVP; unknown variants cause a hard deserialization
  error at startup to prevent misconfigured deployments from silently accepting unexpected auth

## Session CRUD Endpoints (#3902, #4252)

ACP exposes REST-style endpoints for session lifecycle management alongside the existing WebSocket/SSE protocol paths.

| Method | Path | Description |
|--------|------|-------------|
| `POST /sessions` | Create new session | Returns `{ session_id, status }` |
| `GET /sessions/{id}` | Fetch session metadata | Returns current status, `working_dir`, created_at |
| `PATCH /sessions/{id}` | Update session (partial update) | Supports `working_dir` update |
| `DELETE /sessions/{id}` | Terminate session | Graceful teardown (same as `session/close`) |

### SessionStatus Enum

```
running  — session is active and processing messages
idle     — session is open but waiting for input
stopped  — session has been gracefully terminated
error    — session terminated due to an unhandled error
```

`SessionStatus` is `#[non_exhaustive]` — callers must handle unknown variants gracefully.

### PATCH working_dir rules

- The new `working_dir` must be within the `additional_directories` allowlist (same gate as session init)
- Path is canonicalized via `tokio::fs::canonicalize` — no blocking worker threads
- Paths outside the allowlist return `403 Forbidden` — never silently accepted

### Key Invariants

- `POST /sessions` response is synchronous — the session is ready to accept messages before the response returns
- `DELETE /sessions/{id}` follows the same flush-then-remove contract as `session/close`
- `GET /sessions/{id}` returns 404 after `DELETE` — session IDs are not reused

---

## Stable Features

### session/close

**Status: stable** (stabilized in schema 0.12.2, SDK 0.12.0; unconditional in core 2.0.0 (since the 0.14.0 bump; unaffected by the 2.0.0 crate-major migration))

`session/close` handler gracefully terminates an ACP session: flushes pending memory writes,
cancels in-flight tool calls, persists session state to SQLite, and removes the session from
the LRU cache. Previously named `session/stop` (renamed in schema 0.11.2).

The `reason` field on `session/close` is now part of the stable API; it carries a human-readable
string for diagnostics (e.g., `"user_initiated"`, `"timeout"`, `"error"`).

#### Key Invariants

- `session/close` must flush all pending writes before removing the session — no data loss on close
- In-flight tool calls receive a cancellation signal; callers must handle `ToolError::Cancelled`
- Session ID is invalidated after close — subsequent requests with the same session ID return 404

### session/resume

**Status: stable** (stabilized in schema 0.12.2, SDK 0.12.0; unconditional in core 2.0.0 (since the 0.14.0 bump; unaffected by the 2.0.0 crate-major migration))

Reconnect to an existing session by ID, restoring conversation history and tool context.
Previously gated behind `unstable-session-resume` feature flag in Zeph.

The `unstable-session-resume` Zeph feature flag is now a tombstone `= []`. All `#[cfg(feature =
"unstable-session-resume")]` gates are removed; the resume handler runs unconditionally.

### session/delete

**Status: stable** (unconditional in core 2.0.0 (since the 0.14.0 bump; unaffected by the 2.0.0 crate-major migration))

Remove a session from the `session/list` registry. Previously gated behind `unstable-session-delete`.
The `unstable-session-delete` Zeph feature flag is now a tombstone `= []`. All cfg gates removed.

Custom `_session/delete` extension (backward compat) is retained alongside the standard method.

### session/logout

**Status: stable** (unconditional in core 2.0.0 (since the 0.14.0 bump; unaffected by the 2.0.0 crate-major migration))

Previously gated behind `unstable-logout`. The `unstable-logout` Zeph feature flag is now a
tombstone `= []`. All cfg gates removed; logout handler runs unconditionally.

### Capability Negotiation

**Status: stable**

ACP server advertises its capabilities in the `initialize` response and via the
`/.well-known/acp.json` endpoint. `/agent.json` is a separate, static identity manifest for ACP
Registry listing — it does not carry protocol/capability/auth information (see below).

#### /.well-known/acp.json Endpoint

(Corrected 2026-07-27, pre-review pass: this subsection was titled "/agent.json Endpoint" and the
example below was presented as `/agent.json`'s response — it is not; this is `discovery_handler`'s
response, which backs `/.well-known/acp.json`. `/agent.json` is documented separately below with
its actual shape.)

`GET /.well-known/acp.json` returns a JSON document describing the agent's identity, declared
capabilities, supported protocol version, and authentication methods. This endpoint is
unauthenticated and used by IDE clients for discovery.

```json
{
  "name": "...",
  "version": "...",
  "protocol": "acp",
  "protocol_version": 1,
  "transports": { "http_sse": { "url": "/acp" }, "websocket": { "url": "/acp/ws" }, "health": { "url": "/health" } },
  "authentication": { "type": "bearer" },
  "readiness": { "stdio_notification": "zeph/ready", "http_health_endpoint": "/health" }
}
```

#### /agent.json Endpoint

`GET /agent.json` returns a static ACP Registry manifest — agent identity for registry listing,
distinct from the protocol/capability/auth information above. Also unauthenticated. Fields:
`id`, `name`, `version`, `description`, `distribution` (see "Protocol Version" below for the RFD
citation and full field-by-field mapping).

```json
{
  "id": "zeph",
  "name": "...",
  "version": "...",
  "description": "Lightweight Rust AI agent with hybrid inference, semantic memory, and multi-channel I/O",
  "distribution": { "type": "binary", "platforms": ["linux-x64", "darwin-arm64", "darwin-x64"] }
}
```

#### Protocol Version

Zeph uses `agent-client-protocol 2.0.0` / `schema =1.5.0` (migrated in this PR from `1.2.0` /
`=1.4.0`; see Addendum).
`transport/discovery.rs` serves two **distinct** handlers, wired to two different routes
(`transport/router.rs`) — they are not the same function and do not share a response shape:

| Route | Handler | Emits `protocol_version`? |
|---|---|---|
| `GET /.well-known/acp.json` | `discovery_handler` | Yes — `"protocol_version": acp::schema::ProtocolVersion::LATEST` (line 37) |
| `GET /agent.json` | `agent_json_handler` | **No** — the manifest has no `protocol_version` field at all |

(Corrected 2026-07-27, pre-review pass: an earlier draft of this section conflated the two routes
as sharing "the same `discovery_handler`" and claimed `/agent.json` carries `protocol_version` at
`discovery.rs:37` — neither is true; `discovery.rs:37` is inside `discovery_handler`, which only
backs `/.well-known/acp.json`. `agent_json_returns_expected_fields` correctly never asserts on
`protocol_version` because the field doesn't exist on that response.

**Investigated and resolved, not deferred**: `/agent.json` implements the ACP Registry manifest
format (`https://agentclientprotocol.com/rfds/acp-agent-registry.md`), a distinct, external
specification from the `agent-client-protocol`/`agent-client-protocol-schema` Rust crates — neither
crate's source or docs reference `agent.json`, `AgentManifest`, or "ACP Registry" at all (confirmed
by grep across both vendored crates; the registry format lives entirely on the docs site, not in
the SDK). Per that RFD, the manifest's **required** fields are exactly `id`, `name`, `version`,
`description`, `distribution` — `agent_json_handler` (added #2431, 2026-03-30, long predating this
PR) already implements precisely this set. `protocol_version` **does not exist in the ACP Registry
schema at all** — not optional, not removed, never part of it. `/agent.json`'s omission of
`protocol_version` is therefore correct as designed, not a gap to fix or a follow-up to file; the
original migration step ("assert `protocol_version: 1` on both endpoints") was itself based on a
mistaken premise about `/agent.json`'s shape, corrected here.

Aside, discovered during this investigation and explicitly **not fixed here** (out of scope for a
mechanical crate-version bump — noted for a separate issue if the lead wants to file it): the RFD's
optional fields (`repository`, `authors`, `license`, `icon`) are unimplemented, and its `binary`
distribution type requires listing Windows alongside Darwin/Linux — `agent_json_handler`'s
`distribution.platforms` currently omits any `windows-*` entry.)

`ProtocolVersion` stays flat at the crate root — not relocated under `schema::v1::` by the 1.0.1
migration, and unaffected by the 2.0.0 crate-major bump. `LATEST == V1` is unchanged across
0.14.0 → 1.0.1 → 1.2.0 → 2.0.0 / 0.13.6 → 1.1.0 → 1.4.0 → 1.5.0, so the `/.well-known/acp.json`
wire output does not change with the schema crate version. An older draft of this section
described the wire output as `"protocol": "acp/<schema-version>"` — that never matched the
implementation; this entry corrects the spec to match `discovery.rs`, not vice versa.

> **Crate major version ≠ wire protocol version.** The `agent-client-protocol` crate's `2.0.0`
> major bump is an in-process Rust API redesign; it does NOT change the ACP *wire* protocol
> version. The upstream schema crate defines:
> ```rust
> pub const V1: Self = Self(1);
> #[cfg(feature = "unstable_protocol_v2")] pub const V2: Self = Self(2);
> #[cfg(not(feature = "unstable_protocol_v2"))] pub const LATEST: Self = Self::V1;
> ```
> Zeph never forwards `unstable_protocol_v2` (see Feature Flags), so `ProtocolVersion::LATEST ==
> V1 == 1` at `agent/mod.rs:685` (`InitializeResponse::new(LATEST)`, the JSON-RPC `initialize`
> handshake response) and `transport/discovery.rs:37` (`discovery_handler`, i.e.
> `/.well-known/acp.json` `protocol_version` — **not** `/agent.json`, see table above). This is
> **compile-enforced, not just tested**: if `unstable_protocol_v2` were ever turned on, `LATEST`
> would not exist as a symbol at all, and both call sites would fail to compile — Zeph cannot
> accidentally advertise wire v2 by accident. A literal
> `const _: () = assert!(agent_client_protocol::schema::ProtocolVersion::LATEST.as_u16() == 1, ...)`
> regression guard lives in `crates/zeph-acp/src/lib.rs`. Note this guard is a tautology for the
> version pinned today (schema 1.5.0 hardcodes `LATEST = V1` whenever it compiles at all, and
> enabling `unstable_protocol_v2` deletes the `LATEST` symbol before the assertion could even run)
> — its value is as a regression guard against a *future* schema-pin bump that redefines `LATEST`
> to something other than `1`, which would otherwise only surface as a silent wire behavior
> change rather than a compile error. A separate runtime `#[test]` in `crates/zeph-acp/src/lib.rs`
> (`protocol_version_latest_is_hardcoded_wire_v1`) hardcodes the literal `1` independently of the
> live `LATEST` symbol, so it — unlike the const-assert or `discovery_returns_expected_json_fields`
> (both of which compare against the live symbol) — would actually fail if someone weakened either
> of those checks.
> `/.well-known/acp.json` continues emitting `protocol_version: 1` across the 2.0.0 crate
> migration — confirmed by the pre-merge wire gate, by `discovery_returns_expected_json_fields`,
> and by `protocol_version_latest_is_hardcoded_wire_v1`. `/agent.json` carries no
> `protocol_version` field, before or after this migration (unaffected either way).

#### Current Model in SessionInfoUpdate

`SessionInfoUpdate` messages include the `current_model` field so clients can display which
LLM model is active for the session. Also exposed in `session/list` response. The provider
field in relevant messages is now optional (stabilized in SDK 0.12.1 schema 0.12.1).

#### Key Invariants

- `/agent.json` and `/.well-known/acp.json` are always unauthenticated — bearer token must NOT be required for either endpoint
- `authMethods`/`authentication` (in `/.well-known/acp.json` — **not** `/agent.json`, which has no such field, see "Protocol Version" below) must reflect the actual authentication configuration — never hardcoded
- IPI duplication between ACP session init and MCP passthrough is eliminated — validate once, not twice
- Protocol version in `/.well-known/acp.json` (**not** `/agent.json`, which has no `protocol_version` field per the ACP Registry manifest schema) must match the compiled `agent-client-protocol` crate version
- Crate-major version bumps (e.g. `1.x` → `2.0.0`) MUST NOT be conflated with wire protocol version
  bumps — `ProtocolVersion::LATEST == V1 == 1` is compile-enforced while `unstable_protocol_v2`
  stays off; any future crate bump must re-verify this invariant before merging (see the pre-merge
  wire gate in the Addendum's "Version Upgrade Note (1.2.0 → 2.0.0)")

### Input Schemas for Tools

**Status: stable**

Tool definitions include `inputSchema` (JSON Schema) describing accepted parameters. ACP clients
use this for type-safe invocation. Zeph's tool definitions must populate `inputSchema` when
exposing tools over ACP.

---

## Feature Flags

| Flag | Status | Notes |
|------|--------|-------|
| `unstable-session-fork` | **active** | Still gated upstream (`unstable_session_fork`) |
| `unstable-session-usage` | **active** | Gate renamed upstream: now forwards `agent-client-protocol/unstable_end_turn_token_usage` (was `unstable_session_usage`). `Usage` struct + `PromptResponse.usage` field are ALL gated — not unconditional. |
| `unstable-elicitation` | **active** | Now also adds `agent-client-protocol/unstable_elicitation` passthrough so core wires `elicitation/create` |
| `unstable-llm-providers` | **active** | Still gated upstream (`unstable_llm_providers`); provider type renames apply here (see Providers API) |
| `unstable-auth-methods` | **active** | Still gated upstream (`unstable_auth_methods`) |
| `unstable-boolean-config` | **tombstone** `= []` | Stabilized — `SessionConfigOptionValue::Boolean` is unconditional since schema 1.1.0 (core 1.1.0 dropped its `unstable_boolean_config` forward). `do_set_session_config_option` always matches the enum; flag retained as no-op. |
| `unstable-session-delete` | **tombstone** `= []` | Stabilized — `session/delete` handler is unconditional in core 2.0.0 (since the 0.14.0 bump). Flag retained as no-op for workspace forwarding (root `Cargo.toml` references it). |
| `unstable-session-resume` | **tombstone** `= []` | Stabilized — `session/resume` handler is unconditional in core 2.0.0 (since the 0.14.0 bump). Flag retained as no-op. |
| `unstable-logout` | **tombstone** `= []` | Stabilized — logout handler is unconditional in core 2.0.0 (since the 0.14.0 bump). Flag retained as no-op. |
| `unstable-session-add-dirs` | **tombstone** `= []` | Stabilized — `additional_directories` field is plain `Vec<PathBuf>`, unconditional since schema 0.13.6 (currently schema 1.5.0; unaffected by the 2.0.0 migration). Flag retained as no-op. |
| `unstable-message-id` | **tombstone** `= []` | Removed — `PromptRequest.message_id` and `PromptResponse.user_message_id` deleted upstream. Entire inbound echo feature removed. Flag retained as no-op for workspace forwarding. |
| `unstable-cancel-request` | **active (local-only gate)** | Implemented (#5362). Core `1.1.0` made `$/cancel_request` unconditional and dropped the `unstable_cancel_request` feature entirely, so this Zeph flag no longer forwards to any upstream feature — it is now purely a local opt-in for the zeph-acp bridge itself. Not in `default`. The `session/prompt` handler (`agent/handlers/prompt.rs`) bridges `Responder::cancellation()`, scoped to that specific JSON-RPC request, onto the session's existing `cancel_signal: Arc<Notify>` (the same signal `session/cancel` notifies in `agent/handlers/cancel.rs`) via a short-lived watcher task that races cancellation against prompt completion. A low-level `CancelRequestNotification` handler is also registered in the `Agent.builder()` chain (`agent/mod.rs`) for tracing-only observability — the SDK updates per-request cancellation markers automatically regardless of whether a handler is registered. |
| `unstable-session-model` | **DELETED** | Removed entirely — `session/set_model` RPC deleted upstream. Feature name removed from Cargo.toml and root `Cargo.toml`. Model switching survives via `set_config_option`. |

> **Tombstone flags** are `= []` no-ops retained solely so root `Cargo.toml` feature forwarding
> resolves without changes. They add zero behavior.

> **2.0.0 migration note**: all currently-forwarded unstable features (`unstable_session_fork`,
> `unstable_end_turn_token_usage` / `unstable-session-usage`, `unstable_elicitation`,
> `unstable_llm_providers`, `unstable_auth_methods`) are confirmed present under the resolved schema
> `1.5.0` that ACP `2.0.0` pulls, and each was built standalone via `cargo check -p zeph-acp
> --features <name>` in this PR with zero errors. `unstable_protocol_v2` is **deliberately not
> forwarded** by Zeph — this is the wire-safety guard described in "Protocol Version" above, not an
> oversight.

---

## Model Switching

**Status: preserved via stable mechanism**

The dedicated `session/set_model` RPC method was removed upstream (deleted in `agent-client-protocol`
0.14.0 / schema 0.13.6). This is NOT a capability loss.

Model switching is FULLY preserved via two stable paths:

1. **`session/set_config_option`** with `config_id="model"` and `value=<model-name>` — the
   canonical stable path. Runs identical logic to the former `session/set_model`: calls
   `provider_factory(value)`, validates against `available_models_snapshot()`, updates
   `provider_override`, and emits `SessionInfoUpdate` with `model_meta`.
2. **`$/model` slash command** — IDE/CLI convenience; internally dispatches to the same
   `apply_session_config` path.

`session/set_mode` (behavioral persona switch: `code`/`architect`/`ask`) is an orthogonal
concept, NOT a replacement for model switching. Mode and model are independent.

> **NEVER** describe the removal of `session/set_model` as a capability loss. Model switching
> survives unconditionally via `session/set_config_option`.

### Model Parameters (`model_config` category)

**Status: implemented** (#5361)

Distinct from the `model` selector above, `session/set_config_option` with `config_id="temperature"`
(category `SessionConfigOptionCategory::ModelConfig`, stabilized unconditionally in schema 1.1.0)
adjusts a parameter of the *currently selected* model rather than switching models. Zeph exposes
one preset-based parameter today:

- **Sampling temperature** — discrete presets `precise` (0.2) / `balanced` (0.7, default) /
  `creative` (1.0), since the ACP `SessionConfigOption` select type only supports discrete values,
  not free-form numeric input. Applied via `zeph_llm::provider::GenerationOverrides` on top of the
  provider returned by `provider_factory(model_key)` — the same rebuild-on-switch mechanism the
  `model` option already used, so switching either option preserves the other's current setting.

Configured via `[acp.model_config].default_temperature_preset` (applies to new sessions); shown
to IDE clients alongside the `model` option (only when `available_models` is non-empty, since
both require the provider-factory machinery); changeable per-session via `set_config_option`.
Applied to the session's *effective* provider (not just the advertised dropdown value) from the
session's very first prompt, via the same `provider_with_temperature` rebuild mechanism used for
explicit switches.

`session/fork` and `session/resume` inherit `model`, `temperature_preset`, `thinking_enabled`,
and `auto_approve_level` from the source session rather than resetting to configured defaults
(#5373) — see "Fork/Resume Config Inheritance" below for the resolution order and edge cases.

---

## Fork/Resume Config Inheritance (#5373)

**Status: implemented**

`session/fork` and `session/resume` inherit the source session's current `model`,
`temperature_preset`, `thinking_enabled`, and `auto_approve_level` rather than resetting to
`[acp.model_config].default_temperature_preset` / the built-in `thinking_enabled=false` /
`auto_approve_level="suggest"` defaults. `session/new` and `session/load` are unaffected — they
have no "source" session to inherit from and continue to seed configured defaults.

Resolution order (`ZephAcpAgent::inherited_session_config`), in `crates/zeph-acp/src/agent/mod.rs`:

1. **Live in-memory state** — if the source session is still resident in the `AcpSessionManager`
   LRU cache (the common case for `session/fork`, and for `session/resume` when the source was
   never evicted), its current `SessionEntry` fields are read directly.
2. **Persisted close-time snapshot** — if not in memory, the session's `acp_sessions` row is
   checked for a config snapshot (`current_model`, `temperature_preset`, `thinking_enabled`,
   `auto_approve_level` columns, migration `105_acp_session_config`). The snapshot is written by
   `do_close_session` immediately before the in-memory entry is removed, so a session closed via
   `session/close` and later resumed inherits its last-known config even across process restarts.
3. **Configured defaults** — if neither is available (the source was evicted rather than closed
   gracefully — LRU capacity pressure or the idle reaper — or predates the snapshot migration),
   inheritance falls back to the same defaults `session/new` uses.

Additionally, an inherited `model` that is no longer present in the current
`available_models_snapshot()` (e.g. removed from `[[llm.providers]]` since the source session was
created) falls back to `initial_model()` rather than handing the spawner a dangling model key.

### Key Invariants

- Eviction paths (idle reaper timeout, LRU capacity eviction in `session/new`, `session/fork`, and
  `session/resume`) do **not** write a config snapshot — only graceful `session/close` does. A
  session that was evicted rather than closed falls back to configured defaults on later
  resume/fork; this is a known, documented boundary, not a bug.
- The snapshot is best-effort: a write failure is logged (`tracing::warn!`) and does not fail
  `session/close`, since losing the config snapshot is far less severe than failing to close the
  session.
- `session/fork`'s inheritance lookup runs before the LRU capacity-eviction pass in the same
  handler, since that pass does not exclude the fork source from eviction candidates (pre-existing
  behavior, unchanged by this feature) — reading inheritance first avoids a race where forking
  could otherwise evict the very session it inherits from.

---

## Message ID Echo (REMOVED)

**Status: removed in 0.14.0 bump**

`PromptRequest.message_id` and `PromptResponse.user_message_id` were deleted upstream in
schema 0.13.6. The entire inbound message-id echo feature is removed from Zeph:

- `message_ids_enabled` config field retained as no-op (config-schema compatibility)
- `current_message_id` session slot removed
- `build_prompt_response` no longer accepts or echoes a message ID
- `apply_message_id_to_chunk` removed (no live data source)
- `unstable-message-id` feature is a tombstone `= []`

`ContentChunk.message_id` field still exists in schema 0.13.6 for potential future
agent-generated per-chunk IDs, but Zeph does not inject it (no inbound source).

### MessageId Type

In schema 0.13.6, `MessageId` is a newtype: `MessageId(pub Arc<str>)`. The chunk builder
accepts `impl IntoOption<MessageId>`, where `IntoOption<MessageId>` is implemented for
`&str` **only** (not `String`). Passing `String` will not compile — always pass `&str`.

---

## New Protocol Features

### Providers API

**Status: implemented** (commit #4473, PR #4473)

Schema 0.11.7 introduced a providers management API (`unstable` in SDK):

| Method | Description |
|--------|-------------|
| `providers/list` | Returns available LLM providers for the session |
| `providers/set` | Sets the active provider for the session |
| `providers/disable` | Disables a provider for the session |

**Breaking change in 0.14.0 bump — type renames (singular):**

| Old type name | New type name |
|---------------|---------------|
| `SetProvidersRequest` | `SetProviderRequest` |
| `SetProvidersResponse` | `SetProviderResponse` |
| `DisableProvidersRequest` | `DisableProviderRequest` |
| `DisableProvidersResponse` | `DisableProviderResponse` |

All renamed types have `::new()` constructors. All four remain gated behind
`unstable_llm_providers` (Zeph flag `unstable-llm-providers` retained).

**Breaking change in schema 1.4.0 bump — field rename:** `SetProviderRequest`,
`DisableProviderRequest`, and `ProviderInfo` all renamed their `id: String` field to
`provider_id: ProviderId` (`ProviderId` is a new `#[non_exhaustive] struct ProviderId(pub Arc<str>)`
newtype implementing `Display`/`From<Arc<str>>`/`From<String>`/`From<&'static str>`). All
`providers.rs` handler and test call sites updated to the new field name and to convert via
`.0.as_ref()` / `.to_string()` where a borrowed `&str` or owned `String` is needed.

**Design note — impedance mismatch**: The Providers API is NOT a direct mapping to Zeph's
`[[llm.providers]]` TOML config. Key tensions:

1. **Startup resolution**: Zeph resolves providers at startup from the age vault. ACP providers
   are runtime-dynamic (client can set/disable per session). These are different lifecycles.
2. **Identity scheme**: ACP providers are identified by a provider ID string. Zeph's
   `[[llm.providers]]` uses a `name` field that is an internal reference, not an ACP-visible identity.
3. **Per-session override**: It is unclear whether `providers/set` should override the global
   provider for the session only, or affect the global registry. This requires an explicit
   architectural decision.
4. **`providers/disable` scope**: Does disabling a provider affect only the ACP session, the
   global registry, or the vault-resolved config?

**Open questions**:
- What is the ACP provider identity scheme? Is it the Zeph `name` field or something else?
- Should `providers/list` enumerate only providers active for the current session, or all
  configured providers?
- Should the client be able to add new providers dynamically (not in the TOML config)?

**Acceptance criteria** (for implementation):
- `providers/list` returns providers visible to the current ACP session, with their current status
- `providers/set` overrides the provider for the current session only — does not affect global config
- `providers/disable` disables a provider for the current session only
- Provider changes survive within the session but are not persisted after `session/close`
- Vault-resolved keys are never exposed in `providers/list` response

---

### Elicitation Protocol

**Status: implemented** (commit #4473, PR #4473; `elicitation_timeout_secs` wired from `_meta` in `mcp_bridge.rs` — commit #4453; `elicitation_enabled` read from `_meta` — commit #4441)

Schema 0.11.5 introduced structured user input (elicitation) across three scopes:
- **Session scope** (0.11.5, PR #792): agent requests structured input during session initialization
- **Tool call scope** (0.11.5, PR #769): agent requests structured input before executing a tool
- **Request scope** (0.11.5, PR #771): agent requests structured input during prompt processing
- **Scoped by mode** (0.11.6, PR #966): elicitation behavior varies by mode

**Current Zeph state**: `unstable-elicitation` in `crates/zeph-acp/Cargo.toml` now includes
`agent-client-protocol/unstable_elicitation` passthrough (added in the 0.14.0 bump). This wires
core's `elicitation/create` request dispatch path. Zeph already implements elicitation in
`elicitation.rs`; the core passthrough ensures `elicitation/create` is registered.

**Fixed**: `elicitation_timeout_secs` is now read from `_meta` in `mcp_bridge.rs` (commit #4453).
`elicitation_enabled` is read from `_meta` rather than being hardcoded to `false` (commit #4441).

**Broader hardcoding concern**: `terminal.rs` contains 10+ call sites with hardcoded 120s
shell execution timeout (`AcpShellExecutor::new(..., 120)`). This is separate from the
elicitation timeout but indicates a systemic hardcoding pattern in `zeph-acp` that should
be addressed when elicitation is implemented — expose a `[acp.timeouts]` config section.

**Open questions**:
- What data structures does ACP elicitation use? (JSON Schema form definitions, auth challenges, preference forms)
- How does elicitation flow through TUI vs CLI vs Telegram channels?
- Does the IDE client render elicitation forms, or does Zeph render them in the terminal?
- What is the protocol for elicitation cancellation or timeout?

**Acceptance criteria** (for implementation):
- Elicitation works across session, tool call, and request scopes
- `elicitation_timeout_secs` is configurable via `[acp]` config section, not hardcoded
- Shell execution timeouts are configurable via `[acp.timeouts]` config section
- Elicitation integrates with TUI status spinner (user sees "Waiting for input…")
- Elicitation failures (timeout, cancel) propagate cleanly — no session corruption

---

### MCP-over-ACP

**Status: unstable, tracking-only**

Schema 0.13.0 (PR #1185, #1173) introduced MCP servers communicating over ACP channels as a
new transport type. SDK 0.12.0 added `agent-client-protocol-rmcp` for MCP-over-ACP proxy.

In SDK 0.12.0, `McpAcpTransport` was **removed** and replaced by advertising MCP capabilities
via `mcpCapabilities.acp` in `InitializeResponse`. Zeph does not use `McpAcpTransport` (confirmed
by grep — zero hits). No immediate action required.

**Current Zeph state**: Zeph has MCP passthrough (IDE client → Zeph → MCP server) but not the
new ACP-channel-based MCP transport (MCP servers communicating over the ACP channel itself).

**No action needed now**. Track stabilization of `agent-client-protocol-rmcp`. Evaluate when
the feature reaches stable status in the SDK.

**2.0.0 note**: the SDK-local `McpAcpTransport`/`McpConnect*` types are fully removed in the crate's
`2.0.0` line (superseding the SDK 0.12.0 removal above, which only removed the transport struct —
2.0.0 removes the surrounding connect-request types too). The native replacement is
`unstable_mcp_over_acp` plus a new `McpServer::Acp` variant. Zeph defers adoption, matching the
existing deferral above. `mcp_bridge.rs`'s existing `_ => warn!+None` catch-all arm cleanly (not
silently) skips any `McpServer::Acp` variant a host might send; since Zeph does not enable
`unstable_mcp_over_acp`, compliant hosts should not send that variant regardless.

---

### Session Usage (Token/Cost Reporting)

**Status: implemented** (commit #4522, PR #4522; `session/usage` wired from `zeph-core` cost tracker)

Schema 0.10.8 (PR #454) introduced session usage messages for token consumption and context
window tracking.

**Direction clarification**: Zeph is the **agent** (server side), not the client. The correct
implementation direction is: Zeph **reports** usage TO the IDE client. Zeph consuming
ACP-reported usage from an upstream source is not applicable here.

Zeph already tracks token usage and costs internally in `zeph-core` metrics. The implementation
work is wiring this existing data to ACP session usage protocol messages.

**Protocol messages** (unstable):
- `session/usage` notification: agent → client, reports `{ prompt_tokens, completion_tokens, total_tokens, context_window_used, context_window_total }`

**Open questions**:
- Does ACP session usage include cost estimates, or token counts only?
- Is usage reported per-turn or as a cumulative session total?
- Does SDK 0.12.1 expose a typed `SessionUsage` struct, or is it raw JSON?

**Acceptance criteria** (for implementation):
- Zeph emits `session/usage` after each LLM round-trip
- Usage data comes from existing `zeph-core` cost tracker — no duplicate tracking
- `[cost]` config section in ACP mode changes from **Ignored** to **Active** in the Config Coverage table

---

### Session Delete

**Status: implemented** (commit #4464; standard `session/delete` handler added; `_session/delete` retained for backward compatibility)

Schema 0.13.1 (PR #1216, SDK 0.12.0 PR #165) introduced `session/delete` as an unstable
standard method for removing sessions from `session/list`.

**Current Zeph state**: Zeph implements a custom `_session/delete` extension (in `custom.rs:131`).
The `_` prefix on custom extensions is now required by schema 0.12.0 (PR #883 — empty extensions
without `_` prefix are rejected).

**Migration path**:
1. Keep `_session/delete` working for existing clients
2. Add standard `session/delete` handler (behind `unstable-session-delete` feature flag)
3. When `session/delete` stabilizes upstream, remove `_session/delete` and update clients
4. Document in CHANGELOG.md as a breaking change for ACP clients

**No immediate action** — custom extension works. Migrate when standard method stabilizes.

---

## Breaking Changes Resolution (SDK 0.11.1 → 0.12.1)

| Breaking Change | Impact on Zeph | Status |
|----------------|---------------|--------|
| `McpAcpTransport` struct removed | Zeph does not use `McpAcpTransport` (grep confirmed) | **Resolved — no action** |
| `McpConnectRequest.acp_url` renamed to `acp_id` | Zeph does not use `acp_url` (grep confirmed) | **Resolved — no action** |
| `tokio` re-export removed from SDK | Zeph uses its own `tokio` dependency — does not import tokio types from the SDK (grep confirmed) | **Resolved — no action** |
| `session/close` and `session/resume` stabilized | Feature flags removed; handlers unconditional | **Resolved** |
| `_` prefix required for extension methods | Zeph's custom extension is already `_session/delete` | **Resolved — compliant** |

## Breaking Changes Resolution (SDK 0.12.1 → 0.14.0)

| Breaking Change | Impact on Zeph | Status |
|----------------|---------------|--------|
| `agent-client-protocol` bumped to `0.14.0`, schema pinned `=0.13.6` | Workspace `Cargo.toml` updated; `=` pin required for schema | **Resolved** |
| `agent-client-protocol-tokio` dead dep removed | Dep line deleted from workspace + crate `Cargo.toml` | **Resolved** |
| `session/set_model` RPC deleted upstream | Handler + file + tests deleted; model switching preserved via `session/set_config_option` (config_id="model") | **Resolved** |
| `PromptRequest.message_id` removed upstream | Entire inbound message-id echo feature removed; `unstable-message-id` tombstoned | **Resolved** |
| `PromptResponse.user_message_id` removed upstream | Removed from `build_prompt_response`; was a hard compile break | **Resolved** |
| `SetProvidersRequest/Response` → `SetProviderRequest/Response` (singular) | Renamed at all ext-method dispatch sites | **Resolved** |
| `DisableProvidersRequest/Response` → `DisableProviderRequest/Response` (singular) | Renamed at all ext-method dispatch sites | **Resolved** |
| `unstable_session_usage` gate renamed to `unstable_end_turn_token_usage` | `unstable-session-usage` feature re-pointed; `Usage` struct + `PromptResponse.usage` still gated | **Resolved** |
| `unstable_elicitation` added to core 0.14.0 | `unstable-elicitation` feature now passes through to core | **Resolved** |
| `MessageId` type changed to newtype `MessageId(pub Arc<str>)` | `IntoOption<MessageId>` impl for `&str` only — no `String` | **Resolved** |
| `session/delete`, `session/resume`, `session/logout`, `additional_directories` stabilized | Feature flags tombstoned `= []`; all cfg gates removed | **Resolved** |

## Breaking Changes Resolution (SDK 0.14.0 → 1.0.1)

| Breaking Change | Impact on Zeph | Status |
|----------------|---------------|--------|
| `agent-client-protocol` bumped to `1.0.1`, schema pinned `=1.1.0` | Workspace `Cargo.toml` updated; `=` pin required for schema (unchanged convention) | **Resolved** |
| Schema crate `1.1.0` removed the flat `pub use v1::*` root re-export (schema types now live only under `schema::v1::`); ACP crate `1.0.1` mirrors this in `schema/mod.rs` | Mechanical `acp::schema::X` → `acp::schema::v1::X` reorg across `crates/zeph-acp/src/**` and `crates/zeph-acp/tests/**` (~506 live sites); `ProtocolVersion`, `MaybeUndefined`, `IntoOption`, `IntoMaybeUndefined` stay flat at crate root — explicitly excluded from the reorg | **Resolved** |
| Root re-exports `cookbook`, `handler`, `jsonrpcmsg`, and the six root enum re-exports (`AgentRequest`/`AgentResponse`/`AgentNotification`/`ClientRequest`/`ClientResponse`/`ClientNotification`) removed from the ACP crate root | Zeph used none of `cookbook`/`handler`/`jsonrpcmsg`; the only root-enum use site (`tests/integration.rs` `acp::ClientRequest::ExtMethodRequest`) repointed to `acp::schema::v1::ClientRequest::ExtMethodRequest` | **Resolved** |
| `Builder`/`ConnectionTo`/`Dispatch`/`Responder`/`ByteStreams`/`on_receive_request!`/`on_receive_dispatch!` builder API | Byte-identical between 0.14.0 and 1.0.1 for the methods Zeph uses — no handler, transport, or builder-chain code changed shape | **Resolved — no action** |
| Feature flags: ACP crate `[features]` add only `unstable_cancel_request`; schema `[features]` unchanged | No renames affecting Zeph's existing `unstable-*` feature mappings; `unstable_cancel_request` evaluated and deferred in this PR, since adopted via the `unstable-cancel-request` Zeph feature (#5362) | **Resolved** |
| `model_config` option category stabilized in schema 1.1.0 (reachable, schema 1.2.0 stabilizes `unstable_cancel_request` but is **not** reachable — ACP 1.0.1 pins schema `=1.1.0` exactly) | Evaluated and deferred to a follow-up issue (#5361) to keep this PR a clean mechanical bump | **Deferred — not capability loss** |
| 5 long-dead `#[cfg(any())]` test modules (153 of 616 `acp::schema::` src sites, pre-dating ACP 0.11) were unreachable by any feature toggle and contained stale pre-0.14.0 root-path references that didn't even compile | Deleted entirely: `terminal.rs`, `custom.rs`, `fs.rs`, `mcp_bridge.rs` (inline dead `mod tests`), `agent/mod.rs` + external `agent/tests.rs` (dead `mod tests;` declaration) — removes false-green risk where a sed-rewritten but type-unchecked block would silently mask path errors | **Resolved** |

## Breaking Changes Resolution (SDK 1.2.0 → 2.0.0)

**Status: implemented** (this PR). See Implementation Gap Tracker I23 and the Addendum's "Version
Upgrade Note (1.2.0 → 2.0.0)".

| Breaking Change | Impact on Zeph | Status |
|----------------|---------------|--------|
| `ResponseRouter::respond_*` renamed to `route_*` | Grep-confirmed zero usage — Zeph calls `Responder::respond` (a different type), never `ResponseRouter::respond_*` | **N/A — no action** |
| `Builder::with_responder` renamed to `with_runner` | Grep-confirmed zero usage of `with_responder` | **N/A — no action** |
| `MatchDispatch::if_message` renamed to `if_dispatch` | `client/driver.rs:285` uses `.if_notification().otherwise_ignore()` only — no renamed method called | **N/A — no action** |
| `run_indefinitely` renamed to `detach` | Grep-confirmed zero usage of `run_indefinitely` | **N/A — no action** |
| Removed: `send_error_notification`, `respond_with_error`, `DynamicHandler*`, `attach_session`, `util::both`, `process_stream_concurrently` | Grep-confirmed zero usage of every removed symbol | **N/A — no action** |
| `TypeNotification` (corrected 2026-07-27: not removed) — de-generified over `Role`, and its `new()` constructor lost the `cx` argument | Grep-confirmed zero usage of `TypeNotification` in Zeph — harmless either way, but an earlier draft of this table incorrectly listed it under "Removed" | **N/A — no action** |
| Also removed/changed, missing from an earlier draft of this table: `MatchDispatch::from_handled` (removed); `SessionBuilder::with_mcp_server` (now `#[cfg(feature = "unstable_mcp_over_acp")]`, previously unconditional) | Grep-confirmed zero usage of both in Zeph — harmless, `unstable_mcp_over_acp` is deliberately not forwarded (see "MCP-over-ACP") | **N/A — no action** |
| `agent-client-protocol-schema` `=1.4.0` → `=1.5.0` pin change (distinct from the `agent-client-protocol` 1.2.0→2.0.0 bump audited by the rest of this table) | The only v1-surface delta: removal of a `#[schemars(extend("discriminator"...))]` attribute on 4 elicitation enums (`v1/elicitation.rs`); everything else is the new v2-draft module. No impact — Zeph's `schema_for!` calls (`fs.rs`, `terminal.rs`) target only local parameter structs, never ACP schema types | **N/A — no action, audited 2026-07-27** |
| Added: `AcpAgent`/`AcpAgentConfig`, `TransportFrame`/batch support, `DynamicHandlerGuard`, native `unstable_mcp_over_acp` | Purely additive; Zeph adopts none of these new surfaces in this migration (native MCP-over-ACP deferred, see "MCP-over-ACP" above) | **N/A — no action (deferred)** |
| `ActiveSession::connection()` now returns `&ConnectionTo<_>` (signature change) | Used at `client/driver.rs:149/204/259` via `.connection().send_notification(...)` — compiled clean under 2.0.0 with no source change (autoref absorbed the signature change). `ActiveSession` is client-role-only (`client/driver.rs:15`, off the server-side permission-gate path); the only runtime exercise of this call site is the one-off manual `zeph acp run-agent` session, not an automated test | **Resolved — compiled clean, not automated-test-covered** |
| `Option<&T>` borrow-return changes on `modes`/`meta`/`id` accessors | `cargo check -p zeph --features full` produced zero errors. Corrected 2026-07-27: an earlier draft said "no accessor call site required a fix", implying verified coverage — in fact there are **zero** `.modes()`/`.meta()` call sites anywhere in the workspace, so there was nothing to verify or fix in the first place | **Resolved — zero call sites, nothing to verify** |
| `Dispatch<Req, Notif>` gains a new `Notif: JsonRpcNotification` bound | `agent/handlers/dispatch.rs:39` matches `Dispatch::Response(result, router)` and re-wraps into `Handled::No` without calling router methods — rename-immune; the new bound was satisfied automatically, no code change needed | **Resolved — compiled clean** |
| Dropping a `Responder` for one request now emits an Internal-Error fallback after dispatch (behavioral change) | Corrected 2026-07-27: an earlier draft claimed `prompt.rs:34`/`dispatch.rs:28` "always call `responder.respond(...)` on every handled path" — **false**; `dispatch.rs:22`, `dispatch.rs:23`, and `prompt.rs:29` all `?`-return and drop the `Responder` on error paths. The conclusion (no info leak, no behavior change) still holds, but for a different reason: a handler `Err` reaches the peer verbatim with its code/message/data preserved (`incoming_actor.rs:553-556,653-662`), unchanged from 1.2.0. The new generic Internal-Error fallback (`jsonrpc.rs:3717-3739`) fires only for batch slots and is overwritten by the handler's real error (`jsonrpc.rs:2619`); three guards prevent a double response (`jsonrpc.rs:2480-2487,2595-2623`). | **Resolved — verified via source inspection of vendored 1.2.0/2.0.0** |
| Ordered-response-callback barrier now enforced in 2.0.0 | Zeph registers zero `on_receiving_result`/`on_receiving_ok_result`/`on_session_start`/`with_runner` callbacks — that alone is sufficient, the barrier does not apply. (Corrected 2026-07-27: an earlier draft additionally cited "`block_task` calls sit inside request handlers (their own tasks)" as evidence of safety — that framing was backwards. `handle_prompt` was itself an `on_receive_request` handler that awaited the *whole turn* synchronously, holding the SDK's strictly-serial dispatch loop; the permission reply routed back to the `block_task().await` it spawned could never be delivered while that same loop was blocked. This was a **pre-existing HIGH-severity deadlock, identical in 1.2.0, not introduced or worsened by this migration** — tracked separately as #6656, not fixed in this PR; see security audit. **Fixed 2026-07-27 in #6660** (v1.14): `handle_prompt` now runs the turn via `cx.spawn` instead of awaiting it inline, freeing the dispatch loop to route the permission reply while the turn runs — see "Permission Model" and Spec Changelog v1.14.) | **Resolved — deadlock fixed in #6660 (v1.14), no longer pre-existing/tracked-separately** |
| Standard transports now accept JSON-RPC batches | Confirmed via a new integration test (`post_batch_body_dispatches_all_entries_and_returns_all_responses`, `transport/tests.rs`): both entries of a 2-entry batch POSTed to `/acp` get individually tracked, distinct-id responses, aggregated by the SDK into a single JSON-array reply on one SSE line — not silently dropped or short-circuited after the first entry. Security-reviewed for an amplification vector (unbounded batch size at the SDK layer, one authenticated request fanning out to N dispatches): bounded in practice by `transport/router.rs`'s `DefaultBodyLimit::max(1 MiB)` + bearer auth and `ws.rs`'s `max_message_size`; stdio is unbounded but is the IDE trust boundary. No ungated call can be smuggled through a batch — `frame_entries` dispatches every batch entry through the identical handler chain as a single message, no short-circuit path. | **Resolved — verified via new test + security review** |

---

## Implementation Gap Tracker

| # | Feature | Current State | Target | Priority |
|---|---------|--------------|--------|----------|
| I1 | SDK upgrade 0.11.1 → 0.12.1 | **Implemented** (#4464) | ✓ Done | — |
| I2 | `session/resume` stable API | **Implemented** — feature flags removed | ✓ Done | — |
| I3 | `session/delete` migration | **Implemented** — standard handler added (#4464) | Deprecate `_session/delete` when clients migrate | P4 |
| I4 | Providers API | **Implemented** (#4473) | ✓ Done | — |
| I5 | Elicitation protocol | **Implemented** (#4473, #4453, #4441) | ✓ Done | — |
| I6 | MCP-over-ACP transport | MCP passthrough only | Track stabilization | P3 |
| I7 | Session usage reporting | **Implemented** (#4522) | ✓ Done | — |
| I8 | `elicitation_timeout_secs` hardcoded | **Fixed** — read from `_meta` (#4453) | ✓ Done | — |
| I9 | Shell timeout hardcoded | 10+ sites in `terminal.rs` with 120s | `[acp.timeouts]` config section | P3 |
| I10 | Logout method | **Stable** — degated in 0.14.0 bump | ✓ Done | — |
| I11 | Agent telemetry export | Local tracing only | Follow upstream RFD (not yet in schema) | P4 |
| I12 | IDE-provided MCP servers | **Implemented** — wired into `do_new_session` (#4444) | ✓ Done | — |
| I13 | Blocking awaits in handlers | **Fixed** — bounded with configurable timeouts (#4538) | ✓ Done | — |
| I14 | SDK upgrade 0.12.1 → 0.14.0 | **Implemented** | ✓ Done | — |
| I15 | Remove `session/set_model` handler | **Implemented** | ✓ Done | — |
| I16 | Remove inbound message-id echo | **Implemented** | ✓ Done | — |
| I17 | Provider type renames (singular) | **Implemented** | ✓ Done | — |
| I18 | Re-point `unstable-session-usage` gate | **Implemented** | ✓ Done | — |
| I19 | Add elicitation core passthrough | **Implemented** | ✓ Done | — |
| I20 | SDK upgrade 0.14.0 → 1.0.1 (schema-path reorg) | **Implemented** (2026-06-30 migration, changelog v1.4 — a historical PR, not #6655) | ✓ Done | — |
| I21 | Adopt `model_config` option category | **Implemented** (#5361) | ✓ Done | — |
| I22 | Wire `unstable_cancel_request` ($/cancel_request handler) | **Implemented** (#5362) | ✓ Done | — |
| I23 | SDK migration 1.2.0 → 2.0.0 (`agent-client-protocol` crate-major bump) | **Implemented** (#6655) — schema resolved and pinned `=1.5.0`; wire gate passed; all 3 compiler-verify points compiled clean; full CI green; live round-trip verified | ✓ Done | — |

---

## Resource Link Rules (`resolve_resource_link`)

- `file://` URIs: canonicalize (resolve symlinks), must be under `session_cwd`
  - Reject: `/proc`, `/sys`, `/dev`, `/.ssh`, `/.gnupg`, `/.aws`
  - Null byte in content → treat as binary → reject
- `http(s)://` URIs: no redirects; post-fetch IP check (fail-closed on missing remote_addr)
  - Reject private IPs (SSRF protection)
  - Text-only MIME, 1 MiB limit, 10s timeout
  - Validate UTF-8 before returning

## Config Coverage

ACP mode uses the same `config/default.toml` and the same resolution order as CLI/TUI
(see `020-config-loading/spec.md`). However, not all config sections affect ACP agent
behavior. The table below is the authoritative source of truth.

| Config section | ACP status | Reason |
|---|---|---|
| `[agent]` | **Active** | Core agent identity, model, system prompt |
| `[llm]` | **Active** | Provider selection, model, token limits |
| `[skills]` | **Active** | Skill registry, matching thresholds |
| `[memory]` | **Active** | SQLite + Qdrant, recall, summarization |
| `[tools]` | **Active** | Shell executor, web scrape, audit |
| `[vault]` | **Active** | Secret resolution (same as all modes) |
| `[mcp]` | **Active** | MCP servers are wired in ACP sessions |
| `[acp]` | **Active** | ACP-specific: bind, auth, sessions, permissions |
| `[logging]` | **Active** | Logging config applied at early bootstrap |
| `[scheduler]` | **Active (config only)** | Executor wired; `--scheduler-disable` / `--scheduler-tick` CLI flags are **not available** in ACP — use config fields only |
| `[skills.learning]` | **Ignored** | Self-learning requires a session feedback loop not present over ACP; `judge_provider` is built but `.with_learning()` is not called |
| `[index]` | **Ignored** | Code indexing is an interactive CLI/TUI feature; not applicable per-session over ACP |
| `[lsp]` | **Ignored** | LSP hook injection is not wired in ACP agent initialization |
| `[agents]` | **Ignored** | Subagent delegation is not supported in ACP sessions |
| `[orchestration]` | **Ignored** | DAG planner and AgentRouter are not wired for ACP |
| `[cost]` | **Ignored** | Cost tracking not applied; will change to **Active** when Session Usage (I7) is implemented |
| `[experiments]` | **Ignored** | Benchmarking and eval sessions are not applicable in ACP mode |
| `[gateway]` | **Ignored** | HTTP webhook ingestion is spawned by `runner.rs` independently of ACP sessions |
| `[telegram]` / `[discord]` / `[slack]` | **Ignored** | ACP uses `LoopbackChannel` — external chat channels do not apply |

### Code annotation requirement

`build_acp_deps()` and `spawn_acp_agent()` in `src/acp.rs` **must** contain an explicit
comment block that mirrors the "Ignored" rows above, with a one-line reason per section.
This ensures the divergence is visible to any developer editing the initialization path.

**NEVER** silently drop a config section in ACP without updating this table first.

## Key Invariants

- ACP stdio transport is always mutually exclusive with TUI — enforced at startup
- Session IDs are stable UUIDs — never reassigned or reused after expiry
- LRU eviction is by last-access time, not creation time
- `file://` resource paths must stay under `session_cwd` — no `..` escape
- Null byte in file content = binary → reject unconditionally
- Bearer token comparison is constant-time (BLAKE3 + `ct_eq`) — never `==`
- MCP passthrough requires `mcp` crate active — verify capability at negotiation time
- Extension methods must start with `_` (schema 0.12.0) — bare extension names are rejected by the protocol
- Protocol version in `/.well-known/acp.json` (**not** `/agent.json`, which has no `protocol_version`
  field per the ACP Registry manifest schema — see "Protocol Version" above) must match the
  compiled `agent-client-protocol` crate version
- `agent-client-protocol` crate-major bumps NEVER imply an ACP wire protocol bump — verify
  `ProtocolVersion::LATEST.as_u16() == 1` before merging any crate version bump (see "Protocol
  Version" above)

---

## Future / v2 Tracking

**Status: tracking**

Upstream has scaffolded a v2 schema module (schema 0.13.0, PR #1099) behind a separate feature
flag. `unstable_protocol_v2` remains experimental through at least the schema `1.6.x` line (the
range reachable from the planned ACP `2.0.0` crate bump); `Client::v2()`/`Agent::v2()` exist
upstream but stay behind the gate. **This tracks the draft ACP *wire* protocol v2 — a different
axis from the `agent-client-protocol` crate's major-version bump to `2.0.0` being adopted now**
(see "Protocol Version" above for the crate-vs-wire distinction). Zeph does not forward
`unstable_protocol_v2` in either the pre- or post-2.0.0 crate state, so this remains gated and
deferred regardless of the crate bump. The v2 proposal includes breaking changes that will
require Zeph adaptation when stabilized:

| v2 Change | Expected Impact |
|-----------|----------------|
| New prompt lifecycle | Session init / turn structure changes |
| Message IDs (fork from specified IDs) | `message_ids_enabled` logic may change |
| Remote transports (streamable HTTP, WebSocket) | New transport implementations needed |
| Capabilities cleanup | Capability advertisement format changes |
| Enum variant extension (`_` prefix) | Already compliant (extension methods use `_`) |
| Streaming/non-streaming consistency | SSE/WebSocket streaming normalization |
| Session modes removal → config options | `[acp]` config section changes |
| Subagent support | Zeph subagent spawning may integrate with ACP subagent API |

**Deferred unstable surfaces (evaluated during the 1.0.1 migration, not adopted)**:
- `unstable_mcp_over_acp` — MCP-over-ACP transport; Zeph keeps the existing passthrough bridge
  (`mcp_bridge.rs`) instead, see "MCP-over-ACP" above. No reachability blocker, deferred on scope.
- `unstable_nes` (next-edit suggestions) — no Zeph use case identified yet; revisit if an IDE
  client requests it.
- `unstable_plan_operations` — distinct from the **stable** `acp::schema::v1::PlanEntryStatus`
  (3 live sites, unrelated and unaffected by this deferral); plan-operation RPCs themselves are
  not wired in Zeph.

**In pipeline (RFDs, not yet in schema)**:
- Agent telemetry export
- Proxy chains
- Next-edit suggestions
- Diff-delete
- Meta-propagation

`unstable_cancel_request` has graduated out of this list — it is implemented (not just RFD) at
the ACP-crate level since SDK 0.15.1, and Zeph adopted it via the `unstable-cancel-request`
feature (#5362); see "Feature Flags" above.

No action needed now. Monitor upstream v2 progress at https://github.com/agentclientprotocol/rust-sdk.

---

## Addendum: Interop Protocol Gap Analysis (2026-04-17, updated 2026-07-23)

Cross-reference: `specs/045-interop-protocol-gaps/spec.md`

### ACP Baseline vs. arXiv:2505.02279 Survey

Zeph's ACP implementation uses `agent-client-protocol = "2.0.0"` / schema `=1.5.0`
(workspace `Cargo.toml`, migrated from `1.2.0`/`=1.4.0` in this PR — see "Version Upgrade Note
(1.2.0 → 2.0.0)" below). `cargo tree -p agent-client-protocol` confirms `2.0.0` resolves schema
`1.5.0` exactly (not `1.6.0`).

The survey (arXiv:2505.02279) describes ACP's capability advertisement and re-negotiation
model as a differentiating feature vs. MCP and A2A.

**Capability re-negotiation status: Unverified.** Dynamic re-negotiation during an active
session has not been confirmed tested in Zeph's `AcpSessionManager`.

This does not block any current feature. It is tracked as a P3 follow-up in
`specs/045-interop-protocol-gaps/spec.md` under "P3 Follow-up: ACP capability re-negotiation
integration test".

### Version Upgrade Note (0.12.1 → 0.14.0, completed in this PR)

1. Review Breaking Changes Resolution table (SDK 0.12.1 → 0.14.0) above.
2. Workspace: `agent-client-protocol = "0.14.0"`, `agent-client-protocol-schema = "=0.13.6"`; delete `agent-client-protocol-tokio`.
3. Crate `Cargo.toml`: tombstone degated features as `= []`; fix `unstable-session-usage` → `["agent-client-protocol/unstable_end_turn_token_usage"]`; add core passthrough to `unstable-elicitation`.
4. Delete `handlers/set_session_model.rs`; remove all `session/set_model` handler code and tests.
5. Remove all inbound message-id plumbing; `message_ids_enabled` config field retained as no-op.
6. Rename `SetProvidersRequest/Response` and `DisableProvidersRequest/Response` to singular.
7. Degate all cfg sites for delete/logout/resume/add-dirs.
8. Build: `cargo check -p zeph-acp --features full`; `cargo nextest run -p zeph-acp --all-features`.
9. Live round-trip test: session/new → prompt → set_config_option{model} → set_mode → session/delete → logout.
10. Update `/.well-known/acp.json`'s `protocol` field to `"acp/0.13.6"` (corrected 2026-07-27: this
    bullet originally misattributed the field to `/agent.json`, which has never carried a
    `protocol` or `protocol_version` field — that manifest is served by a distinct
    `agent_json_handler`; see "Protocol Version" above).

### Version Upgrade Note (0.14.0 → 1.0.1, completed in this PR)

1. Review Breaking Changes Resolution table (SDK 0.14.0 → 1.0.1) above.
2. Workspace: `agent-client-protocol = "1.0.1"`, `agent-client-protocol-schema = "=1.1.0"`. Crate
   `Cargo.toml` `[dependencies]`/`[features]` blocks unchanged — every existing feature mapping
   still resolves against 1.0.1/1.1.0.
3. Delete the 5 long-dead `#[cfg(any())]` test modules first (`terminal.rs`, `custom.rs`, `fs.rs`,
   `mcp_bridge.rs`, `agent/mod.rs` + external `agent/tests.rs`) — before the path-reorg pass, not
   after, so every remaining edit is compiler-checked.
4. Mechanical `acp::schema::X` → `acp::schema::v1::X` reorg (and the `agent_client_protocol::schema::X`
   / `agent_client_protocol_schema::X` spellings, including nested `schema::{A, B, C}` use-blocks
   and local `use agent_client_protocol_schema as schema;` aliases) across `src/**` and `tests/**`,
   excluding `ProtocolVersion`/`MaybeUndefined`/`IntoOption`/`IntoMaybeUndefined`.
5. `tests/integration.rs`: `acp::ClientRequest::ExtMethodRequest` → `acp::schema::v1::ClientRequest::ExtMethodRequest`.
6. Do **not** add `unstable-cancel-request`, adopt `model_config`, or adopt the
   `agent-client-protocol-tokio`/`-rmcp`/`-http` helper crates — all deferred (#5361, #5362).
7. Build: `cargo +nightly fmt`; `cargo clippy --profile ci --workspace --all-targets --features
   "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings`; `cargo nextest run
   --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler"
   --lib --bins`; rustdoc gate with both `RUSTFLAGS="-D warnings"` and
   `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links"`. Additionally build `zeph-acp` standalone
   under each individually-toggled unstable feature (`unstable-session-fork`, `-session-usage`,
   `-elicitation`, `-llm-providers`, `-auth-methods`, `-boolean-config`, `acp-http`) — these are
   skipped by the default-feature build and would otherwise hide path errors behind a cfg gate.
8. Live round-trip test: `cargo nextest run -p zeph-acp --all-features` exercises
   `initialize_handshake`, `prompt_round_trip_returns_end_turn`, `cancel_before_prompt_returns_cancelled`,
   and `unknown_ext_method_returns_null` in `tests/integration.rs` — no panics, no serde errors.
9. `/.well-known/acp.json`'s `protocol` field is **unchanged** by this migration — it was never
   `"acp/<schema-version>"` in the implementation (see "Protocol Version" above, corrected
   2026-07-27: this bullet originally misattributed the field to `/agent.json`, which carries
   neither `protocol` nor `protocol_version` at all — that manifest is served by a distinct
   `agent_json_handler`); `/.well-known/acp.json` stays `"protocol": "acp"` + numeric
   `"protocol_version"`.

### Version Upgrade Note (1.2.0 → 2.0.0, completed in this PR)

**Status: implemented** — see Implementation Gap Tracker I23.

1. Bumped root `Cargo.toml`: `agent-client-protocol = "2.0.0"`; `cargo update`/`cargo tree` resolved
   schema `1.5.0` exactly (not `1.6.0`); set `agent-client-protocol-schema = "=1.5.0"`.
2. **PRE-MERGE WIRE GATE**: confirmed `ProtocolVersion::LATEST.as_u16() == 1` under schema `1.5.0`
   with Zeph's feature set (`unstable_protocol_v2` OFF) — both by direct inspection of
   `agent-client-protocol-schema-1.5.0/src/version.rs` and via a local literal
   `const _: () = assert!(agent_client_protocol::schema::ProtocolVersion::LATEST.as_u16() == 1, ...);`
   regression guard (with an assert message) added to `crates/zeph-acp/src/lib.rs`, plus an
   independent runtime `#[test] fn protocol_version_latest_is_hardcoded_wire_v1()` in the same file
   that hardcodes the literal `1` rather than deriving it from the live `LATEST` symbol (so it
   fails if the const-assert or `discovery_returns_expected_json_fields` is ever weakened — see
   "Protocol Version" above). No wire-breaking change — proceeded as a mechanical crate bump.
3. `cargo check -p zeph --features full` (zeph-acp has no standalone `full` feature; checked via the
   root binary's `full` bundle, which enables `ide` → `acp` + `acp-http`) compiled with zero errors.
   All three named compiler-verify points resolved with **no source changes required**:
   `ActiveSession::connection()` returning `&ConnectionTo<_>` (`client/driver.rs:149/204/259`) —
   autoref absorbed it; `Option<&T>` borrow-return changes on `modes`/`meta`/`id` accessors — zero
   call sites exist in the workspace at all, so there was nothing to verify or fix here; the new
   `Dispatch<Req, Notif>` `Notif: JsonRpcNotification` bound (`agent/handlers/dispatch.rs:39`) —
   Zeph's default `UntypedMessage` satisfies the bound automatically. Note `ActiveSession` is used
   only client-side (`client/driver.rs:15`, off the server permission-gate path) and is exercised
   at runtime only by the one-off `zeph acp run-agent` manual session in step 6 below, not by any
   automated test — flagged for future coverage, not a blocker for this mechanical bump.
4. Built `zeph-acp` standalone under each individually-toggled unstable feature
   (`unstable-session-fork`, `-session-usage`, `-elicitation`, `-llm-providers`, `-auth-methods`,
   `acp-http`) via `cargo check -p zeph-acp --features <name>` — all compiled clean **except** that
   the literal command as written fails on every single one of the six, with an identical error:
   `zeph-core`'s `summarize_tool_input`/`collect_json_strings`
   (`crates/zeph-core/src/agent/tool_execution/mod.rs:776,786`) become dead code under
   `.cargo/config.toml`'s `build.warnings = "deny"` because their only caller
   (`scheduler_loop.rs`) sits behind zeph-core's `scheduler` feature, which none of zeph-acp's own
   Cargo features forward. This is **pre-existing and unrelated to the ACP migration** — that file
   is untouched by this PR's diff — but it means the command must be run either with
   `CARGO_BUILD_WARNINGS=warn` prefixed, or scoped from a `--workspace --features "...,scheduler"`
   build, to get a real pass/fail signal; the bare command as documented in prior migration notes
   is not directly reproducible. Recorded here rather than fixed (separate zeph-core
   feature-unification gap, out of scope for this PR).
5. Full CI suite green: `cargo +nightly fmt --check`; `cargo clippy --profile ci --workspace
   --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings` (zero
   warnings); `cargo nextest run --config-file .github/nextest.toml --workspace --features
   "desktop,ide,server,chat,pdf,scheduler" --lib --bins` (15071 tests, 0 failed); rustdoc gate with
   both `RUSTFLAGS="-D warnings"` and `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links"`
   (clean); `cargo test --doc --workspace` with the same feature set (all doc-tests pass, including
   19 in `zeph_acp`); `gitleaks protect --staged` (no leaks); `cargo deny --config .github/deny.toml
   check advisories` (the actual CI invocation, `.github/workflows/security.yml:25` — **not** bare
   `cargo deny check advisories`, which skips `.github/deny.toml`'s ignore list and reports 2 false
   failures that are already accepted project-wide) reports **`advisories ok`, zero failures** —
   `.github/deny.toml`'s ignore list (RUSTSEC-2025-0134, RUSTSEC-2024-0436, RUSTSEC-2026-0173,
   RUSTSEC-2026-0192) is unchanged by this PR, so this is identical to baseline — **no new advisory
   from the bump** (the new `syn` 3.0.3 transitive, pulled in by `agent-client-protocol-derive`, is
   proc-macro/build-time only, not linked into the runtime binary). The distinct
   `agent-client-protocol-schema` `=1.4.0`→`=1.5.0` pin change was also
   separately audited: the only v1-surface delta is removal of a
   `#[schemars(extend("discriminator"...))]` attribute on 4 elicitation enums
   (`v1/elicitation.rs`) — no impact, since Zeph's own `schema_for!` calls (`fs.rs`, `terminal.rs`)
   only ever target local parameter structs, never ACP schema types.
6. **Live round-trip gate**: `zeph acp run-agent` self-hosted the round trip (own binary as ACP
   client via `zeph_acp::run_session`/`client/driver.rs`, spawning its own binary as ACP agent) —
   `initialize` → `session/new` → `session/prompt` against a real local LLM (Ollama `qwen2.5:7b`)
   completed with `stop_reason=EndTurn`, no errors, no deadlock.
   `fork`/`resume`/`session/delete`/`logout`/`set_config_option`/`set_mode`/`block_task` are
   exercised by the `zeph-acp` crate's test suite — 222 tests total across `src/**` unit tests plus
   the 33-test `tests/integration.rs` file (not "220" as an earlier draft imprecisely conflated the
   two counts) — via `cargo nextest run -p zeph-acp --no-default-features --features
   "sqlite,unstable-session-fork,unstable-llm-providers,unstable-elicitation,unstable-cancel-request,unstable-auth-methods,unstable-session-usage,acp-http"`
   (the bare `cargo nextest run -p zeph-acp` / `--all-features` forms either build nothing under
   `default = ["sqlite"]` feature-gated tests or hit the `zeph-db` `sqlite`+`postgres`
   mutual-exclusion `compile_error!` — this is the command that actually exercises the
   feature-gated suite), all passing under 2.0.0, including the four tests named in the 1.0.1
   migration note (`initialize_handshake`, `prompt_round_trip_returns_end_turn`,
   `cancel_before_prompt_is_a_no_op`, `unknown_ext_method_returns_null`) and the
   previously-unverified `fork_session_*`, `providers_list_reflects_configured_provider_names`, and
   `cancel_request_during_prompt_cancels` tests (confirmed individually re-run under the corrected
   command). A new `post_batch_body_dispatches_all_entries_and_returns_all_responses` test
   (`transport/tests.rs`) additionally proves the 2.0.0 JSON-RPC batch-transport addition reaches
   Zeph's HTTP relay (`post_handler`) end-to-end: both entries of a 2-entry batch get individually
   tracked, distinct-id responses — aggregated by the SDK into a single JSON-array reply on one SSE
   line (confirmed empirically), not silently dropped or short-circuited after the first entry.
   `protocol_version: 1` on `/.well-known/acp.json` (not `/agent.json`, which carries no such
   field — see "Protocol Version" above) confirmed via `discovery_returns_expected_json_fields` and
   the new hardcoded-literal `protocol_version_latest_is_hardcoded_wire_v1` test.
7. This spec updated (v1.11 initial completion entry, v1.12 pre-review correction pass — see
   changelog); `specs/README.md` unaffected (013-acp already indexed).

Open questions, resolved:
1. Native `unstable_mcp_over_acp` — deferred, matching the existing MCP-over-ACP deferral above.
2. Schema pin — resolved to `=1.5.0` by `cargo tree` (not `=1.6.0`).
3. Pure mechanical bump — confirmed; no new 2.x surface (batch framing, `unstable_protocol_v2`
   draft, `AcpAgent`/`AcpAgentConfig`) adopted.
