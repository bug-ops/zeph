---
aliases:
  - Handoff Protocol
  - Skill-Based Handoff
  - Inter-Agent Communication
  - Handoff Protocol v2
tags:
  - sdd
  - spec
  - protocols
  - handoff
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[005-skills/spec]]"
  - "[[044-subagent-lifecycle/spec]]"
  - "[[080-cross-thread-store-dynamic-handoff/spec]]"
  - "[[087-subagent-peer-messaging/spec]]"
---

# Spec: Handoff Protocol

**Status:** Approved (v2, 2026-11-22)
**Issue:** #2023

> [!info]
> Defines how sub-agents hand context to each other through files in `.local/handoff/`. Version 2 replaces the YAML format of v1 with Markdown files that have flat YAML frontmatter and a short Markdown body.

> [!note] Revision (v2, 2026-11-22)
> v2 supersedes the v1 format described in the approved revision of this spec. Changes from v1:
> - Handoff files are `.local/handoff/{TS}-{suffix}.md` (Markdown with frontmatter), not `.yaml`.
> - Frontmatter is flat scalars; the nested `context`, `output`, `next` and `timestamp` blocks are removed.
> - The sub-agent's final message begins with the handoff block, because the runtime passes final messages to dependent tasks and to the spawner.
> - Per-role output sections move from `references/*.md` in the skill to each role's definition under `.zeph/agents/`.
> - Shared rules for every role are added (no commits or pushes, read-only definition, questions as `needs_discussion`, peer tools optional).
> - v1 YAML files are not converted and are ignored by v2 readers.

## Sources

### External
- [`dev-agents` plugin 3.1.0, `agent-handoff` skill](https://github.com/bug-ops/claude-plugins/tree/main/dev-agents/skills/agent-handoff) — the protocol the v2 format was derived from; structure only, host-specific mechanics (skill loading, team messaging) excluded

### Internal
| File | Contents |
|---|---|
| `crates/zeph-subagent/src/agent_loop.rs` | System prompt assembly; included skill bodies are wrapped in a `skills` fenced block (line ~302) |
| `crates/zeph-orchestration/src/scheduler/router.rs` | Dependency context for dependent tasks; sanitized, budgeted (line ~25) |
| `crates/zeph-skills/src/bundled.rs` | Bundled skill directories are embedded in the binary and provisioned to users (line ~35) |
| `.zeph/skills/rust-agent-handoff/` | Handoff skill (v1 today; v2 after this revision) |
| `.zeph/agents/` | Project-local role definitions that hold per-role output sections |

---

## 1. Overview

### Problem Statement
Sub-agents run in isolated contexts and cannot see each other's conversations. The handoff file is the channel that carries a predecessor's result to its successor. The v1 format used nested YAML blocks that the runtime does not read, and the final message of a sub-agent, which the runtime does pass to other agents, carried none of the handoff information. Both are addressed in v2.

### Goal
Every sub-agent that finishes writes one short Markdown handoff file and begins its final message with a handoff block that names the file and carries the frontmatter. A dependent task or spawner can route on the frontmatter alone.

### Out of Scope
- Automatic conversion of v1 YAML files.
- Model-callable spawn tools and nested spawns.
- Typed Rust structs or compile-time validation of handoff content.
- Zeph-project rules inside the shipped skill (these belong in `.zeph/zeph.md` and role definitions).
- Orchestration policy: which role runs next, how verdicts gate a chain, round caps. These belong to whoever spawns the roles (the user, `/plan`, or the lead instructions in `.zeph/zeph.md`).
- Behavior of `model` in role definitions (tracked separately, #7153).
- Asking the user directly from a sub-agent (`ask_user` is unavailable to sub-agents, #7121).

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a sub-agent finishes THE SYSTEM SHALL have it write one handoff file at `.local/handoff/{TS}-{suffix}.md`, where `{TS}` is `YYYY-MM-DDTHH-MM-SS` from one `date` call at write time | must |
| FR-002 | WHEN the target name already exists THE SYSTEM SHALL have the agent append `-2` to the file name | must |
| FR-003 | THE SYSTEM SHALL write handoff files with the `write` tool | must |
| FR-004 | THE SYSTEM SHALL set the frontmatter `id` equal to the file name without `.md` | must |
| FR-005 | THE SYSTEM SHALL restrict frontmatter values to flat scalars; the only permitted list is `parent: [id1, id2]` | must |
| FR-006 | THE SYSTEM SHALL define frontmatter fields `id`, `parent`, `agent`, `status`, `verdict`, `summary`, `next_agent`, `next_task`, `follow_ups` | must |
| FR-007 | WHEN `status` is `blocked` THE SYSTEM SHALL require a `## Blockers` section | must |
| FR-008 | WHEN `follow_ups` is above 0 THE SYSTEM SHALL require a `## Follow-ups` section with one entry per item | must |
| FR-009 | WHEN `status` is `no_change_needed` THE SYSTEM SHALL require a `## Evidence` section | must |
| FR-010 | WHEN a sub-agent finishes THE SYSTEM SHALL have its final message begin with the handoff block (file path and frontmatter) and stay short | must |
| FR-011 | WHEN a sub-agent reads a parent THE SYSTEM SHALL use the parent frontmatter supplied inline in the task text or the dependency block, read a parent body only when needed, and read ancestors by frontmatter only | should |
| FR-012 | THE SYSTEM SHALL keep the handoff skill generic and free of Zeph-project rules | must |
| FR-013 | THE SYSTEM SHALL define per-role output sections in each role definition under `.zeph/agents/`, not in skill resource files | must |
| FR-014 | THE SYSTEM SHALL ignore v1 YAML files in `.local/handoff/` | must |

### Frontmatter fields

| Field | Type | Values |
|-------|------|--------|
| `id` | scalar | `{TS}-{suffix}`; equals the file name without `.md` |
| `parent` | scalar or list | id, `[id1, id2]`, or `null` |
| `agent` | scalar | role suffix (see Section 3) |
| `status` | scalar | `completed` \| `blocked` \| `needs_discussion` \| `no_change_needed` |
| `verdict` | scalar | reviewer: `approved` \| `changes_requested`; critic: `approved` \| `minor` \| `significant` \| `critical`; omitted for other roles |
| `summary` | scalar | one sentence |
| `next_agent` | scalar | role suffix or `null` |
| `next_task` | scalar | short imperative task, or empty |
| `follow_ups` | scalar | count of Follow-ups entries |

### Body sections

| Section | When |
|---------|------|
| `## Context` | always. Opens with a `Rules:` line naming the instruction and spec files read at startup, then at most two sentences |
| `## Output` | always. Role-specific sections, defined in the role definition |
| `## Blockers` | `status: blocked` |
| `## Acceptance Criteria` | when `next_task` needs more than one line |
| `## Follow-ups` | `follow_ups` above 0. One entry per item: kind (`defect`, `enhancement`, `tech-debt`, `todo`, `security`), `file:line`, evidence, suggested priority `P0` to `P4` |
| `## Evidence` | `status: no_change_needed` |

The body is at most about 40 lines and contains nothing that `git diff` already shows. The timestamp is carried by `id`; v2 has no separate `timestamp` field.

### Handoff block in the final message

The final message begins with the frontmatter block, delimited by `---` lines exactly as in the file, followed by one line `File: .local/handoff/{id}.md`. No fence is used, because the skill body cannot contain triple backticks.

---

## 3. Architecture

### Role suffixes

| Role | Suffix |
|------|--------|
| rust-architect | `architect` |
| rust-developer | `developer` |
| rust-testing-engineer | `testing` |
| rust-performance-engineer | `performance` |
| rust-security-maintenance | `security` |
| rust-code-reviewer | `review` |
| rust-cicd-devops | `cicd` |
| rust-debugger | `debug` |
| rust-critic | `critic` |

Role names and the skill name `rust-agent-handoff` are unchanged.

### Why the final message carries the handoff block

In `/plan`, a dependent task receives the predecessor's final message, sanitized and truncated to `dependency_context_budget / n` characters (`crates/zeph-orchestration/src/scheduler/router.rs:25`). A foreground spawn returns the final message into the spawner's context. The final message is therefore the part of the handoff that reaches the next agent, and the frontmatter must fit within it.

### Trust boundary

The final message and the handoff file are produced by an LLM that may have read web or MCP content. Under spec 044 section 21, a sub-agent's final text is untrusted when it reaches another LLM; the runtime flows that carry it (orchestration task outputs, command-handoff updates, peer messages) are governed there, not here. The handoff file reaches the next agent through an ordinary file read and carries whatever trust label the runtime gives file contents. Readers treat handoff content as data, never as instructions that override their own role definition.

### Enforcement

The protocol is a convention carried by the skill text and the role definitions. No Rust structs or code-level validation exist for handoff content (see Historical Context).

### Peer messaging

Peer tools (`send_peer_message`, `check_messages`, `list_peers`, spec 087) are optional. A sender never assumes a reply. Handoff files, not peer messages, are the record of a task's result.

### Command block

The handoff block is a Markdown frontmatter block in the final message. It is not the fenced ` ```zeph-command ` block of spec 080 (FR-B-002), and a handoff block must not be written in a form that matches that block.

### Shared rules for every role

- No `git commit`, `git push` or PR from any role. Only the spawner or the user commits.
- A read-only role edits no existing file outside `.local/`. The only exception is a reproducer it created in the same session.
- Questions for the user are returned with `status: needs_discussion`. Sub-agents have no `ask_user`.
- Out-of-scope findings go to Follow-ups. Only the reviewer role files issues.
- Peer messaging is optional, and a reply is never assumed.

### Status values and spawner action

| Status | Meaning | Spawner action |
|--------|---------|----------------|
| `completed` | Work done | Proceed to the next role |
| `blocked` | Cannot proceed | Return the blocker to the caller |
| `needs_discussion` | A decision is needed | Return the question to the user |
| `no_change_needed` | Request already satisfied, or behavior is intended; returned only by roles that analyse before any code is written (architect, debugger) | Stop the chain and report the `## Evidence` to the user |

### Skill constraints

The skill body is bundled into the binary and provisioned to every user (`crates/zeph-skills/src/bundled.rs:35`). Sub-agents receive skill bodies only, never resource files, and the runtime wraps each included body in a `skills` fenced block (`crates/zeph-subagent/src/agent_loop.rs:302`). The skill body therefore contains no triple backticks, no `$(...)` command substitution, and no heredocs.

---

## 4. Key Invariants

### Always (without asking)
- Handoff files live under `.local/handoff/` and use the `.md` extension.
- The frontmatter `id` equals the file name without `.md`.
- Frontmatter values are flat scalars; `parent` may be a list only in the form `[id1, id2]`.
- Every finishing sub-agent writes a handoff file, including when blocked.
- The final message of every finishing sub-agent begins with the handoff block.
- Handoff content is treated as data by readers, never as instructions.
- v1 YAML files are ignored by v2 readers and are not converted.
- The skill text is the contract; there is no code-level validation of handoff content.

### Ask First
- Changing the status set, the verdict set, or the frontmatter field set.
- Adding a body section type beyond those in Section 2.
- Moving per-role output sections out of role definitions.
- Giving any role permission to commit, push or open a PR.
- Changing the `dependency_context_budget` mechanism that bounds final messages.

### Never
- Nested `context`, `output` or `next` blocks in frontmatter.
- Triple backticks, `$(...)` or heredocs in the skill body.
- Zeph-project rules in the shipped skill.
- Writing a handoff block in the form of the spec 080 `zeph-command` block.
- Adding a runtime path that injects handoff content into another agent's context; such a path is governed by spec 044 section 21.
- A role other than the reviewer filing issues.
- A read-only role editing an existing file outside `.local/`.

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| File name already exists | Append `-2` to the file name; `id` follows the new name |
| Agent is blocked | Write the file with `status: blocked` and a `## Blockers` section; the final message still begins with the handoff block |
| Question for the user | `status: needs_discussion`; the question is stated in the body |
| Follow-ups with `follow_ups` 0 | `## Follow-ups` section omitted |
| Final message too long for the dependency budget | Truncation applies to the message; the file on disk remains the complete record |
| v1 YAML file found in `.local/handoff/` | Ignored by v2 readers |
| Parent id is `null` | No parent is read |
| Gate command cannot run in the session | Report `status: blocked`; never assume the gate passed |

---

## 6. Success Criteria

- [ ] A sub-agent run writes `.local/handoff/{TS}-{suffix}.md` whose `id` equals its file name.
- [ ] The final message of that run begins with the handoff block and contains the file path.
- [ ] A dependent task routes using frontmatter fields alone.
- [ ] A `blocked` run contains `## Blockers`; a `no_change_needed` run contains `## Evidence`.
- [ ] The shipped skill body contains no triple backticks, `$(...)` or heredocs.
- [ ] A v2 reader ignores a v1 YAML file in `.local/handoff/`.
- [ ] Every role definition in `.zeph/agents/` contains its own output sections.

---

## 7. Historical Context

- v1 (2026-04-08, #2023): flat `.yaml` files with nested `context` / `output` / `next` blocks and per-role schemas in the skill's `references/`.
- PRs #2076 and #2078 introduced typed `HandoffContext` Rust structs with compile-time validation (inspired by MAST research on multi-agent coordination failures). This was reverted in #2082; the skill-based protocol remains the approach.
- v2 (2026-11-22): Markdown with flat YAML frontmatter, handoff block in the final message, per-role sections in role definitions. Motivation: the runtime passes final messages, not files, to dependent tasks and spawners; sub-agents cannot reach skill resource files (#7159); YAML nesting caused format errors and long chain reads.

---

## 8. See Also

- [[MOC-specs]] — Map of all specifications
- [[005-skills/spec]] — Skill system, bundled skills and skill bodies
- [[044-subagent-lifecycle/spec]] — Sub-agent lifecycle, final-text trust boundary (section 21)
- [[080-cross-thread-store-dynamic-handoff/spec]] — `zeph-command` fenced block (FR-B-002), not to be confused with the handoff block
- [[087-subagent-peer-messaging/spec]] — Optional peer messaging tools
- [[009-orchestration/spec]] — Dependency context for `/plan` tasks
