---
aliases:
  - Knowledge Ingest BRD
tags:
  - brd
  - memory
  - cross-cutting
created: 2026-06-07
status: draft
spec_id: "067"
---

# BRD: Knowledge Ingest (`zeph knowledge ingest`)

## 1. Business Problem

Zeph's knowledge graph and semantic memory are populated only as a side effect of live
conversation — background, RPE-gated, fire-and-forget. There is no way to deliberately seed
memory with the project's accumulated knowledge: its specifications, changelog, agent handoffs,
coverage reports, and the episodic record of work that agents (including Zeph's own subagents)
have already done.

The result is that a new operator or a fresh database starts blind to all prior project context,
despite that context being fully available on disk. The agent re-discovers known facts across
sessions, wastes tokens, and cannot answer cross-session questions such as "why did we switch
the durable backend to a dedicated SQLite file?" without manual context injection.

Competitor tools (Goose, Codex, Aider) do not solve this problem at the memory-subsystem level
either — Zeph's graph + semantic-note architecture provides a unique opportunity to make a fresh
instance project-aware in a single operator command.

## 2. Stakeholders

| Role | Concern |
|---|---|
| Operator / team lead | Seed a fresh Zeph instance with full project context in one command; verify with `--dry-run` before committing |
| Developer using Zeph daily | Recall cross-session project decisions without re-injecting context manually each session |
| Security-conscious operator | Guarantee imported content passes sanitizer and stays confined to the current project; ability to roll back any batch |
| CI/CD integrator | Understand that ingest is an explicit, manual command — not a background side effect that could pollute production memory |

## 3. Business Requirements

**BR-1 — Project-context bootstrap.** An operator must be able to run a single command to load
accumulated project knowledge (specs, changelog, handoffs, coverage status, git log) into Zeph's
semantic memory so that a fresh instance answers cross-session questions without manual context
injection.

**BR-2 — Idempotent re-runs.** Running the ingest command multiple times on unchanged inputs
must not re-issue LLM calls or create duplicate memory entries, so that operators can run it
safely at any time (e.g., after adding new specs) without unexpected cost.

**BR-3 — Dry-run preview.** Before writing anything to memory, the operator must be able to
preview what would be ingested: files discovered, projected chunk/token count, and — for the
graph path — projected entity/edge counts and hub-degree distribution. No surprises, no
unexpected charges.

**BR-4 — Provenance and rollback.** Every imported edge and entity must be tagged with the
source batch so that an operator can reverse a bad import with `zeph knowledge rollback <batch>`
without affecting conversation-derived knowledge.

**BR-5 — Gated episode layer.** The relational graph is reserved for the cross-agent episode
layer (subagent transcripts) where multi-hop traversal genuinely pays off. This path must be
gated behind a measurement spike that proves a real multi-hop recall benefit over the simpler
semantic-notes path before it ships. If the spike fails, the graph path is abandoned and episode
sources are rerouted to the notes pipeline.

## 4. Constraints

- No new inter-crate dependency edges beyond what already exists in the workspace.
- Sources are confined to the current project root — no traversal into other projects.
- The write-quality gate (spec 004-9) and admission control (004-3) must remain active; only
  the RPE per-turn heuristic is bypassed (it has no meaning for batch documents).
- Every imported document must pass the `zeph-sanitizer` validator before any entity or fact
  string is persisted.
- Graph writes require explicit confirmation (`--yes` or interactive) before execution.
- External-agent transcripts (Claude Code / Codex) are out of scope until provenance, write-path
  PII redaction, and a version-pinned strict parser are in place.
- Migration must be SQLite + PostgreSQL parity, enforced by `migration_parity.rs`.
- Ingest must never run during a live agent turn.

## 5. Success Criteria

| Criterion | Measurement |
|---|---|
| A fresh instance becomes project-aware in one command | Manual test: query cross-session decision after `--source specs` ingest; correct recall without manual context |
| Re-run on unchanged inputs costs zero LLM/embed calls | Unit test: ledger hit rate = 100% on second identical ingest run |
| `--dry-run` writes nothing and reports accurate preview | Integration test: zero Qdrant/graph/ledger writes; chunk count matches actual ingest count |
| `rollback <batch>` removes all imported edges/entities without touching conversation knowledge | Integration test: conversation-origin row count unchanged after rollback |
| Sanitizer validation is enforced on every graph document | Unit test: document with synthetic PII pattern is rejected at the validator; edge count = 0 |
| Phase 2 ships only if §7 spike passes all three thresholds | Manual gate: spike results recorded in playbook before Phase 2 PR is opened |
| Sources outside the current project root are rejected fast | Unit test: path outside root → clear error, zero writes |

## 6. Out of Scope

- **Raw source code ingestion.** Owned by `zeph-index` (tree-sitter → Qdrant + symbol index +
  repo map). Duplicating it in the knowledge graph floods relational recall with hub nodes.
- **External-agent transcript import (Claude Code / Codex).** Deferred to Phase 3 (#5024):
  requires provenance migration (Phase 0), write-path PII redaction, and a version-pinned
  strict allowlist parser.
- **Automatic / scheduled ingest.** Ingest is an explicit operator command. A future hook or
  scheduler tap is out of scope for M29; a TODO marker is left in `src/commands/knowledge.rs`.
- **Structural code graph (call/def edges into `graph_edges`).** Covered by `zeph-index` MCP tools.
- **Multi-project transcript import.** Sources are confined to the current project root (INV-6).
- **Drift reconciliation via the content-hash ledger.** The ledger is a re-read/cost guard only,
  not a semantic-equivalence guarantee across model versions (INV-5).
