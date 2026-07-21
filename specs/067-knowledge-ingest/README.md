---
aliases:
  - Knowledge Ingest Spec Package
  - spec-067 index
tags:
  - sdd
  - spec
  - memory
  - cross-cutting
created: 2026-06-07
status: draft
github_issue: 5012
related:
  - "[[004-memory/spec]]"
  - "[[004-memory/004-9-memory-write-gate]]"
  - "[[004-memory/004-6-graph-memory]]"
  - "[[012-graph-memory/spec]]"
  - "[[017-index/spec]]"
  - "[[040-sanitizer/spec]]"
  - "[[056-autoskill-trace-extraction/spec]]"
  - "[[001-system-invariants/spec]]"
  - "[[constitution]]"
---

# Spec 067: Knowledge Ingest (`zeph knowledge ingest`)

One-shot operator command that populates Zeph's memory subsystems with accumulated project
knowledge — specs, changelog, handoffs, coverage reports, and agent episode transcripts —
instead of relying solely on background extraction from live conversation.

GitHub epic: #5012 — `feat(memory): knowledge ingest command (spec 067)`; milestone M29;
branch `feat/m29/knowledge-ingest`.

## Document Index

| Document | Purpose |
|---|---|
| `brd.md` | Business Requirements Document — business problem, stakeholders, success criteria |
| `srs.md` | Software Requirements Specification (ISO/IEC/IEEE 29148:2018) — EARS functional requirements |
| `nfr.md` | Non-Functional Requirements (ISO/IEC 25010:2011) — measurable quality targets |
| `spec.md` | Technical specification — design decisions, invariants, two-sink architecture, §7 gate |
| `plan.md` | Implementation plan — phases, milestones, dependencies, risk register |
| `tasks.md` | Task breakdown — implementable units with issue traceability (#5015–#5024) |

## Scope Summary

**Ships in M29:**

- `zeph knowledge ingest --source <SRC>... [--dry-run] [--max-documents N] [--provider NAME] [--yes]`
- `zeph knowledge rollback <import_batch_id>`
- `zeph knowledge status`
- **Phase 0 (prerequisite):** provenance columns (`origin`, `import_batch_id`, `source_uri`) on
  `graph_edges` + `graph_entities`; backfill; recall isolation flag; `knowledge rollback`
- **Phase 1:** static project artifacts (`specs`, `changelog`, `handoff`, `coverage`, `git-log`)
  ingested as semantic notes via the existing `IngestionPipeline` (no graph writes)
- **Phase 2:** subagent transcripts → knowledge graph — **gated by §7 measurement spike**
- `[knowledge]` config section; `--init` wizard step; `--migrate-config` migration
- TUI status spinner + command palette entries
- Content-hash ledger for idempotent re-runs (re-read/cost guard — not a drift guard)

**Explicitly excluded from M29:**

- Raw source code ingestion (owned by `zeph-index`)
- External-agent transcript import (Claude Code / Codex) — Phase 3, deferred (#5024)
- Automatic or scheduled ingest (explicit operator command only)
- Structural code graph (call/def edges) in `graph_edges`

## Traceability Map

```
BR-1..BR-5 (brd.md)
  └─ FR-001..FR-042 (srs.md / spec.md §3)
       ├─ NFR-001..NFR-007 (nfr.md / spec.md §4)
       └─ INV-1..INV-8 (spec.md §5)
            ├─ Phase 0: F1 #5015, F2 #5016
            ├─ Phase 1: F3 #5017, N1 #5018, N2 #5019, N3 #5020
            ├─ Phase 2 (gated): G1 #5021, G0 #5022 (GATE), G2 #5023
            └─ Deferred: D1 #5024
```
