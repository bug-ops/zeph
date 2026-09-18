---
aliases:
  - Subagent Peer Messaging
  - Live Inter-Subagent Messaging
  - PeerRouter
tags:
  - sdd
  - spec
  - subagent
  - security
created: 2026-09-06
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[033-subagent-context-propagation/spec]]"
  - "[[044-subagent-lifecycle/spec]]"
  - "[[010-security/spec]]"
  - "[[026-tui-subagent-management/spec]]"
---

# Spec: Subagent Peer Messaging (`zeph-subagent::peer`)

> [!info]
> Live, addressable peer-to-peer messaging between a running sub-agent and its spawner, its
> siblings, and its own descendants — a spawner, coordinator, or sibling sub-agent can redirect
> or query a sub-agent while it is still running, instead of only exchanging a single-shot final
> result via `/agent spawn`/`/agent resume`. GitHub #5871.

> [!warning] Spec-number correction
> Every doc comment shipped in this feature's source (25+ sites across `zeph-subagent`,
> `zeph-config`, `zeph-sanitizer`, `zeph-core`, `zeph-tui`) cites this spec as
> `046-subagent-peer-messaging-parity`. Spec ID `046` is already assigned to
> `[[046-march-quality/spec]]` (MARCH Proposer+Checker pipeline) — the design's own draft `/sdd`
> spec was never committed to `/specs/` under that number, so the mismatch was never caught. This
> document is that spec, filed retroactively at the next available ID (`087`). The in-source
> citations are a documentation-only defect (not a `rustdoc::broken_intra_doc_links` failure,
> since they are plain-text, not intra-doc link syntax) — fix in a follow-up PR, not here (spec
> maintenance does not change source code).

## 1. Overview

### Problem Statement

Prior to this feature, a sub-agent's only communication channel with its spawner (or any other
agent) was its final result string, returned once the sub-agent's loop terminates. A spawner
that wanted to redirect a running sub-agent — supply a missing piece of information, correct a
wrong assumption, answer a clarifying question — had no way to do so short of cancelling and
respawning it, discarding all accumulated progress. Multi-sub-agent coordination patterns (a
lead sub-agent delegating to siblings, siblings cross-checking each other's work) had no
in-process channel at all.

### Goal

Give every spawned sub-agent, its spawner, and its siblings/descendants a live, addressable,
authorized messaging channel: three built-in tools (`send_peer_message`, `check_messages`,
`list_peers`) on the sub-agent side, and `/agent msg`/`/agent inbox` slash commands on the
parent side, backed by a bounded, non-blocking, deny-by-default routing table.

### Out of Scope

- Cross-spawn-tree messaging (a sub-agent from one `/plan` execution addressing a sub-agent from
  an unrelated one, or from the interactive session) — always denied, not a future phase (US-003)
- Auto-injection of peer messages into a sub-agent's context — messages are retrieved only via
  explicit `check_messages` polling, never pushed into the prompt
- Persistence of mailbox contents across process restart — mailboxes are in-memory `mpsc`
  channels, scoped to the `PeerRouter`'s lifetime
- Nested peer-messaging trees beyond parent/sibling/descendant (e.g. a grandchild replying
  through an intermediate hop) — `authorize()`'s ancestor-walk supports it structurally, but no
  caller registers a node deeper than one level below a root today (tracked as a follow-up, see
  §7)

---

## 2. User Stories

### US-001: Spawner Redirects a Running Sub-Agent

As the interactive session (or an orchestration plan), I want to send a message to a specific
running sub-agent by task ID or name, so it can incorporate new information without being
cancelled and respawned.

**Acceptance**: `/agent msg <id-prefix> <body>` resolves a unique task-ID prefix (same matching
rule as `/agent cancel`/`/agent resume`) and delivers the message into that sub-agent's mailbox.
The sub-agent observes it on its next `check_messages` call.

### US-002: Sub-Agent Reports Back Mid-Run

As a sub-agent, I want to send a question or status update to my spawner without terminating, so
the spawner can answer or acknowledge without me losing my accumulated context.

**Acceptance**: `send_peer_message` targeting the spawner's `AgentId::Root` delivers into the
parent's own mailbox; the parent surfaces it as a channel notice (`[peer message from {name}]
...`) the moment it next drains (`notify_peer_messages`, at most one turn of latency) and it is
retrievable afterward via `/agent inbox`.

### US-003: Spawn-Tree Isolation

As the operator, I want a sub-agent dispatched by one orchestration plan execution to be
completely unable to discover or message a sub-agent dispatched by an unrelated plan execution
or by the interactive session, so a compromised or misbehaving sub-agent in one tree cannot
reach across trust boundaries into another.

**Acceptance**: `PeerGroupId::Session` (the interactive session's own root) and each
`PeerGroupId::Plan(graph_id)` (one per dispatching orchestration plan execution) are disjoint
groups. A cross-group `send` or `peers_for` query is authorized identically to a target that
does not exist at all (`DeliveryError::TargetNotFound`, never `Unauthorized`) — see NFR-003.

### US-004: Non-Blocking Delivery

As the agent loop, I want a peer-message send to never stall the sender's tool-call turn, so a
full or unreachable mailbox degrades to a fast error instead of a hang.

**Acceptance**: `PeerRouter::send` holds no `.await` on its critical path — one brief read-lock
for lookup/authorization/sender-clone, dropped before a non-blocking `mpsc::try_send`.

---

## 3. Architecture

### 3.1 Routing State Ownership

Routing state lives in an `Arc<PeerRouter>`, shared between `SubAgentManager` and every spawned
sub-agent's own tool-executor decorator (`PeerToolExecutor`) — **never** on `SubAgentHandle`,
since a running sub-agent task cannot reach back into the `&mut SubAgentManager` that owns its
handle. This is the structural fix for the rejected first-draft design (mailbox-on-`SubAgentHandle`),
which a running sub-agent could never actually reach.

```
                    ┌─────────────────┐
                    │  SubAgentManager │
                    │  (owns Arc<PeerRouter>)
                    └────────┬─────────┘
                             │ registers/deregisters nodes at spawn/teardown
                             ▼
                    ┌─────────────────┐
                    │   PeerRouter     │◄──── PeerToolExecutor (per spawned sub-agent)
                    │  (Arc-shared)    │◄──── PeerToolExecutor (sibling)
                    │  RwLock<nodes>   │◄──── parent's own Root(PeerGroupId) node
                    └─────────────────┘
```

A send is a direct, non-blocking `try_send` from the sender's tool call into the target's own
bounded `mpsc` mailbox; it never touches the parent agent's turn loop and introduces no new
`tokio::spawn` call site.

### 3.2 Addressing (`AgentId`, `PeerGroupId`)

- `AgentId::Root(PeerGroupId)` — the parent agent for one spawn-tree root
- `AgentId::Task(task_id)` — a spawned sub-agent, keyed by its `task_id`
- `PeerGroupId::Session` — the interactive session's own root (`/agent run`/`spawn`/`resume`);
  registered once for the manager's lifetime
- `PeerGroupId::Plan(graph_id)` — one root per orchestration plan execution, lazily registered
  the first time that plan dispatches a peer-messaging-eligible sub-agent and released on plan
  teardown

### 3.3 Authorization (`authorize()`, FR-004, US-003)

A sender `S` may address a target `T` iff, after confirming `S.group == T.group` (same spawn
tree) and `S != T` (no self-addressing):

1. `T` is `S`'s own parent, **or**
2. `T` shares the same parent as `S` (siblings), **or**
3. `S` is an ancestor of `T` (walk `T`'s parent chain looking for `S`)

Any other same-group pair, or any cross-group pair, is denied. A cross-group target is resolved
to `DeliveryError::TargetNotFound` — **never** `Unauthorized` — because `resolve_target` filters
candidates to the sender's own group before the id/name match runs at all, so the sender cannot
distinguish "no such agent" from "an agent that exists in a tree I can't see" (NFR-003).

### 3.4 Message Pipeline

**Send** (`PeerRouter::send`, on the sender's tool-call path):
1. Resolve `target` (task-id or unique display-name match, scoped to the sender's own group)
2. `authorize()` (§3.3)
3. Mask secrets in the body via `SecretMaskRegistry::mask` (optional; `None` when no registry is
   configured for the manager)
4. Non-blocking `try_send` into the target's bounded mailbox

**Receive** (`check_messages` tool, or the parent's `notify_peer_messages` drain):
1. Drain the mailbox (non-blocking; `check_messages` may additionally wait up to a clamped
   `wait_ms`, racing a periodic progress-heartbeat tick and cancellation)
2. `ContentSanitizer::sanitize` with `ContentSourceKind::SubagentPeerMessage` (classified
   `ExternalUntrusted` — a sibling/parent can only ever influence the receiver through this
   message body, same trust tier as an A2A message even though the sender runs in-process;
   see `[[010-security/spec]]`) — injection patterns are logged, never silently swallowed
3. `ExfiltrationGuard::scan_output` on the sanitized body
4. Only the fully sanitized body reaches the LLM (`check_messages`'s tool output) or the parent's
   channel notice / `/agent inbox` display

Secret masking happens once, at send time, in the router (step 3 of Send) — sanitization and
exfiltration scanning happen at read time, per-reader, in `PeerToolExecutor::sanitize_body` and
in `Agent::notify_peer_messages`. Peer messages are **never** auto-injected into a sub-agent's
context; they are retrievable only via an explicit `check_messages` tool call.

### 3.5 Tools (`PeerToolExecutor`, FR-012)

| Tool | Purpose |
|---|---|
| `send_peer_message(target, body)` | Send `body` to the agent addressable as `target` (task_id or unique display name). Returns `{"delivered": bool, "error"?: string}` — never throws on an authorization/capacity failure, so the sub-agent's own LLM can react to the rejection reason. |
| `check_messages(wait_ms?)` | Drain the caller's own mailbox. `wait_ms` (default/absent = 0) waits up to `min(wait_ms, peer_messaging.max_wait_ms)` for a new message if the mailbox is currently empty, emitting a `PROGRESS_TICK`-interval (5s) heartbeat so orchestration idle-timeout detection does not reap a legitimately-waiting sub-agent. Returns `{"messages": [{sender, body, sent_at}]}`, each `body` sanitized per §3.4. |
| `list_peers()` | Enumerate every node the caller is authorized to address, with its relation (`parent`/`sibling`/`child`). Returns the exact addressable string `send_peer_message`'s `target` accepts back — a `Task` node's own `task_id`, or a `Root` node's display name (a root has no task_id and is the only resolvable string for it). |

Each `PeerToolExecutor` is constructed once per spawn with its own `AgentId` baked in — **never**
taken from a tool argument or `ToolCall.caller_id` — so a sub-agent's LLM cannot spoof its own
sender identity.

### 3.6 Parent-Side Surface

- `/agent msg <id-prefix> <body>` — resolves a unique task-ID prefix (matching `/agent
  cancel`/`resume`'s rule) and calls `SubAgentManager::send_to_subagent`
- `/agent inbox` — lists messages the parent has received from sub-agents this session, sanitized
  body stored (not the raw one — `/agent inbox` is an operator display surface, not LLM context,
  but re-showing unsanitized attacker-influenceable content there would defeat the sanitize step)
- `Agent::notify_peer_messages` — non-blocking per-turn poll (mirrors
  `notify_completed_subagents`'s shape) that drains messages addressed to the parent's own
  `AgentId::Root` and emits a channel notice; **known latency limitation**: a message is not
  observed until the parent reaches this drain point, at most one turn away — there is no push
  path into an in-flight parent turn
- TUI subagent sidebar: unread-message badge sourced from `PeerRouter::mailbox_depth` (`O(1)`,
  derived from `max_capacity() - capacity()`, no separate counter to keep in sync)
- `MetricsSnapshot`: a peer-mailbox-depth gauge (FR-010)

---

## 4. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | `PeerRouter::send(from, target, body)` SHALL resolve `target`, authorize the send, mask secrets, and non-blockingly enqueue into the target's mailbox | must |
| FR-002 | (reserved — message-format/serialization requirement) | must |
| FR-003 | (reserved — mailbox registration lifecycle requirement) | must |
| FR-004 | A send from `S` to `T` SHALL be authorized iff `T` is `S`'s parent, a sibling of `S`, or a descendant of `S`, and `S.group == T.group`; every other pair SHALL be denied (US-003) | must |
| FR-005 | A target mailbox at capacity SHALL fail the send with `DeliveryError::MailboxFull` rather than blocking or silently dropping (NFR-002) | must |
| FR-007 | (reserved — target-string length cap requirement, see `DeliveryError::TargetTooLarge`) | must |
| FR-008 | `PeerRouter::peers_for(from)` SHALL return every node `from` is authorized to address, tagged with its relation (`Parent`/`Sibling`/`Child`), and SHALL NEVER include a node from another spawn-tree group (NFR-003) | must |
| FR-009 | A send to a target with no registered route (deregistered, or never existed) SHALL fail fast with `DeliveryError::TargetNotFound`/`TargetTerminated` rather than blocking or silently dropping (US-004) | must |
| FR-010 | The current queued (undelivered) mailbox depth for a given `AgentId` SHALL be exposed for TUI unread-badge and metrics use | must |
| FR-011 | Messages SHALL be delivered in mailbox arrival order, on a best-effort basis (no delivery guarantee beyond the bounded queue's own ordering) | should |
| FR-012 | Every sub-agent's tool catalog SHALL include `send_peer_message`, `check_messages`, and `list_peers` when `peer_messaging.enabled = true`, and SHALL NOT include them when `false` | must |
| FR-013 | The parent SHALL be able to drain messages addressed to its own `AgentId::Root` via a non-blocking per-turn poll | must |
| FR-014 | Draining the parent's own inbox SHALL sanitize each message body before display/storage in `/agent inbox` | must |

## 5. Non-Functional Requirements

| ID | Requirement |
|----|-------------|
| NFR-001 | `PeerRouter::send` SHALL hold no `.await` on its critical path — lookup, authorization, and mailbox-sender-clone happen under one brief read-lock, dropped before a non-blocking `try_send` |
| NFR-002 | Every mailbox SHALL be bounded (`peer_messaging.mailbox_capacity`, default 32); an over-capacity send SHALL fail fast, never grow unbounded or block |
| NFR-003 | A cross-spawn-tree-group query or send SHALL be indistinguishable from a nonexistent target — an attacker-controlled sub-agent MUST NOT be able to learn that a target exists in another tree via the `Unauthorized` vs. `TargetNotFound` distinction |
| NFR-004 | An authorization denial SHALL carry a structured `UnauthorizedReason` (not a bare boolean), assertable in tests and legible in `tracing` events |
| NFR-005 | (reserved — kill-switch/config-gating requirement, `peer_messaging.enabled`) |
| NFR-007 | Every message body SHALL be sanitized (`ContentSanitizer`) and exfiltration-scanned (`ExfiltrationGuard`) before it reaches an LLM or an operator-facing display surface |

> [!note]
> FR-002/003/007 and NFR-005/006 are reserved numbers preserved from the original (uncommitted)
> design draft's numbering, to keep the ~25 in-source doc-comment citations (which reference
> these IDs directly) meaningful once the spec-number correction above is applied in a follow-up
> code PR. Their content is folded into the adjacent requirements' prose in this document; they
> are not independently testable gaps.

---

## 6. Configuration: `[agents.peer_messaging]`

```toml
[agents.peer_messaging]
enabled = true          # kill switch — disables the three tools and all routing (default: true)
mailbox_capacity = 32   # bounded queue per agent; a full mailbox fails the send fast (default: 32)
max_body_bytes = 8192   # per-message body size cap (default: 8192)
max_wait_ms = 30000     # ceiling for check_messages' optional wait_ms argument (default: 30000)
```

- `PeerMessagingConfig` fields are all `#[serde(default)]`, so the section's absence is safe on
  load
- `--init` prompts for this section; `--migrate-config` inserts an advisory comment block into an
  existing active `[agents]` table (no-op if the section or an advisory comment already exists)
- `enabled = false` removes the three tools from every sub-agent's tool catalog and registers no
  routing node for any spawn — not merely a runtime no-op behind the tools

---

## 7. Key Invariants

- Routing state (`PeerRouter`) is owned independently of `SubAgentManager`, `Arc`-shared with
  every spawn's `PeerToolExecutor` — NEVER placed on `SubAgentHandle`, which a running sub-agent
  task cannot reach
- A `PeerToolExecutor`'s `AgentId` is baked in at construction, never derived from a tool
  argument or `ToolCall.caller_id` — a sub-agent MUST NOT be able to spoof its own sender identity
- Authorization is deny-by-default: the only permitted relations are parent, sibling, and
  descendant, scoped to a single `PeerGroupId`; cross-group is always denied and always resolves
  as "not found," never "unauthorized" (NFR-003)
- `PeerRouter::send` never blocks and introduces no new `tokio::spawn` call site — the transport
  is bounded `mpsc` channels with non-blocking `try_send`/`try_recv`
- Peer message bodies are masked for secrets once, at send time; sanitized and
  exfiltration-scanned at every read time (once per reader, not cached) — sanitization is never
  skipped for a "trusted" sender, since the trust classification (`ExternalUntrusted`) applies
  uniformly regardless of same-process origin
- A peer message is never auto-injected into a sub-agent's context; it is observable only via an
  explicit `check_messages` tool call
- `authorize()`'s ancestor-walk has no visited-set or depth cap: safe today because every
  registered node's `parent` is either `None` (a root) or a root's own `AgentId` — `manager::spawn`
  never registers a node nested deeper than one level below a root. A future nested-spawn feature
  MUST add a cycle guard before registering a node whose ancestry is not trivially a root, or a
  parent cycle would spin forever while holding the router's read lock, wedging every concurrent
  send

---

## 8. Traceability

| Requirement | Test / Evidence |
|---|---|
| FR-001, NFR-001 | `crates/zeph-subagent/src/peer/router.rs` `send()` unit tests |
| FR-004, US-003, NFR-003 | `cross_group_send_is_denied_as_target_not_found_not_unauthorized` and sibling/ancestor authorization tests in `router.rs` |
| FR-005, NFR-002 | Mailbox-full rejection tests in `router.rs` |
| FR-008 | `peers_for` group-isolation tests in `router.rs` |
| FR-012 | `crates/zeph-subagent/src/peer/tools.rs` tool-catalog gating tests |
| NFR-007 | `PeerToolExecutor::sanitize_body` and its tests in `tools.rs` |
| FR-013, FR-014 | `crates/zeph-core/src/agent/subagent_commands.rs::notify_peer_messages` |
| §6 config | `crates/zeph-config/src/agent.rs::PeerMessagingConfig` tests; `crates/zeph-config/src/migrate/subagent.rs::migrate_agents_peer_messaging_config` |

## References

- GitHub #5871 (feature request)
- PR #6776 (implementation)
- `[[033-subagent-context-propagation/spec]]` — parent→sub-agent one-shot context injection at
  spawn time (complementary, not overlapping: that spec covers spawn-time context, this one
  covers live mid-run messaging)
- `[[044-subagent-lifecycle/spec]]` — full sub-agent lifecycle this feature extends
- `[[026-tui-subagent-management/spec]]` — TUI sidebar, extended here with the unread-message badge
- `[[010-security/spec]]` — content trust classification (`ContentSourceKind::SubagentPeerMessage`)
