---
aliases:
  - Knowledge Ingest Implementation Plan
tags:
  - plan
  - memory
  - cross-cutting
created: 2026-06-07
status: draft
spec_id: "067"
related:
  - "[[067-knowledge-ingest/spec]]"
  - "[[067-knowledge-ingest/srs]]"
  - "[[067-knowledge-ingest/nfr]]"
  - "[[constitution]]"
---

# Implementation Plan: Knowledge Ingest (067)

## Phases

### Phase 0 — Provenance and Rollback Prerequisite (Wave 1, PRs F1 + F2)

**Goal:** Add the schema columns that make graph imports reversible and identifiable. No ingest
logic ships in this phase — only migration + ledger table + recall isolation flag. This phase
unblocks both the notes rollback path (N2) and the graph batch API (G1).

**Deliverables:**

- `crates/zeph-db/migrations/sqlite/<NNN>_knowledge_provenance.sql`:
  - `ALTER TABLE graph_edges ADD COLUMN origin TEXT NOT NULL DEFAULT 'conversation'`
  - `ALTER TABLE graph_edges ADD COLUMN import_batch_id TEXT`
  - `ALTER TABLE graph_edges ADD COLUMN source_uri TEXT`
  - `ALTER TABLE graph_entities ADD COLUMN origin TEXT NOT NULL DEFAULT 'conversation'`
  - `ALTER TABLE graph_entities ADD COLUMN import_batch_id TEXT`
  - Backfill: `UPDATE graph_edges SET origin = 'conversation' WHERE origin IS NULL` (pre-migration rows)
- Matching `crates/zeph-db/migrations/postgres/<NNN>_knowledge_provenance.sql` with parity guard
- `[knowledge].recall_include_imported: bool` field in `zeph-config` (FR-003, FR-060)
- `knowledge_ingest_ledger` table (FR-080):

  ```sql
  CREATE TABLE IF NOT EXISTS knowledge_ingest_ledger (
      source_uri      TEXT    NOT NULL,
      content_hash    TEXT    NOT NULL,
      import_batch_id TEXT    NOT NULL,
      ingested_at     TEXT    NOT NULL DEFAULT (datetime('now')),
      entities        INTEGER NOT NULL DEFAULT 0,
      edges           INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (source_uri, content_hash)
  );
  ```

- `IngestLedger` repository struct in `crates/zeph-memory/src/graph/ingest/ledger.rs`
  (FR-012, FR-025, NFR-4.3)
- Unit tests: ledger `is_ingested` / `mark_ingested` round-trip; migration parity check

**Acceptance:** `cargo nextest run -p zeph-memory -p zeph-db` passes; existing graph integration
tests pass against the migrated schema (no regressions on existing column set).

---

### Phase 1 — CLI Scaffold, Config, and Static Artifacts to Semantic Notes (Wave 1+2, PRs F3 + N1 + N2 + N3)

**Goal:** Ship the lowest-risk, highest-value slice: a working `zeph knowledge ingest --source specs`
(and the other static sources) that feeds the existing `IngestionPipeline` with a dry-run preview
and idempotent re-runs. The rollback and status commands ship alongside (they are trivial for the
notes path since notes are not rollback-tracked beyond the Qdrant pipeline).

**Deliverables:**

F3 — CLI scaffold + config + wizard:
- `Command::Knowledge { Ingest { .. }, Rollback { .. }, Status }` in `src/cli.rs`
- Dispatch in `src/runner.rs` to `src/commands/knowledge.rs` (mirrors `ingest.rs`)
- `[knowledge]` config section in `zeph-config/src/knowledge.rs` with `#[serde(default)]`
- `--migrate-config` migration step adding `[knowledge]` with defaults (FR-062)
- `--init` wizard `step_knowledge()` (FR-061)
- `ProviderName` resolution chain: `ingest_provider → extract_provider → primary` (FR-041)

N1 — Static notes sink:
- `src/commands/knowledge.rs`: disk-walking for each source kind (specs, changelog, handoff,
  coverage, git-log); `--dry-run` preview (FR-013); progress streaming via `IngestProgress`
  (FR-014); confirmation gate (`--yes` / interactive) before Qdrant writes (FR-040)
- Reuse `TextLoader`/`TextSplitter`/`IngestionPipeline` verbatim (FR-011, NFR-4.2)
- Ledger integration: skip unchanged inputs (FR-012), mark on success

N2 — Rollback + status:
- `zeph knowledge rollback <batch>`: `DELETE FROM graph_edges WHERE import_batch_id = ?`
  + orphan entity cleanup (FR-004)
- `zeph knowledge status`: list batches from ledger (FR-029)

N3 — TUI integration:
- Command palette entries: `/knowledge ingest <source>`, `/knowledge status`,
  `/knowledge rollback <batch>`
- Spinner with `Ingesting knowledge: <uri>...` (FR-014, FR-071, NFR-6.3)

**Acceptance:** `cargo run -- knowledge ingest --source specs --dry-run` reports files and
projected chunks and writes nothing. `cargo run -- knowledge ingest --source specs --yes`
ingests all spec files; re-run produces zero LLM/embed calls (ledger hit rate 100%).

---

### Phase 2 — Subagent Transcripts to Knowledge Graph (Wave 3+4+5, PRs G1 + G0 (gate) + G2)

**Goal:** Extend the batch extraction path to the knowledge graph for subagent transcripts.
This phase is gated: G1 builds the batch API and types; G0 runs the measurement spike; G2
connects the graph write path only if G0 passes.

**Deliverables:**

G1 — Batch API + types + technical-document prompt:
- `crates/zeph-memory/src/graph/ingest/mod.rs`: `IngestSourceKind` (`#[non_exhaustive]`),
  `IngestDocument`, `Provenance`, `ImportBatchId`, `IngestProgress`, `IngestReport`
  (NFR-7.3, spec §2.4)
- `crates/zeph-memory/src/graph/ingest/adapter.rs`: sealed `IngestSourceAdapter` trait;
  `SubagentJsonl` adapter (pure, no I/O) (spec §2.4, NFR-4.1)
- `crates/zeph-memory/src/graph/extractor.rs`: second `const` technical-document extraction
  prompt selectable by `IngestSourceKind` (FR-023, spec §2.5)
- `SemanticMemory::ingest_documents()` on `crates/zeph-memory/src/semantic/graph.rs`
  (spec §2.3): ledger check → `buffer_unordered(concurrency)` → `extract_and_store()` reuse
  → `mark_ingested`; collect-errors fail strategy (FR-028, NFR-2.1)
- `MemoryError::Ingest` variant in `crates/zeph-memory/src/error.rs` (NFR-4.3)
- Dry-run hub-degree projection report (FR-026)
- TODO deferred markers at `IngestSourceKind::ExternalAgent` and `ingest_documents` (spec §9)

G0 — Measurement spike (gate):
- Run `--dry-run` over 10 specs + all current-project subagent transcripts
- Author 3 concrete multi-hop recall queries; measure recall@5 vs. notes baseline on ≥10
  held-out cross-session questions (FR-050)
- Verify hub-degree < 15% and S/N ≥ 50% (FR-051, FR-052)
- Record GO / NO-GO decision and evidence in `.local/testing/playbooks/knowledge-ingest.md`
- If any kill-criterion is met (FR-053): abandon graph sink, route `--source subagents` to
  Phase 1 notes pipeline, close G2 as "wontfix per §7 kill-criterion"

G2 — Graph go-live + sanitizer (conditional on G0 GO):
- Wire `--source subagents` in `src/commands/knowledge.rs` to `ingest_documents()`:
  JSONL reading + `TranscriptEntry` discovery (binary-level; no new `zeph-memory` deps)
- `PostExtractValidator` wired to `zeph-sanitizer` on the write path (FR-022, INV-4)
- Provenance stamping: `origin`, `source_uri`, `import_batch_id` on every edge/entity (FR-024)
- Write-quality gate and admission control enforcement assertion (FR-021, INV-3)
- Integration tests: rollback removes tagged rows; conversation rows untouched; sanitizer
  rejects synthetic PII; gate mock invoked per candidate edge

**Acceptance (G2):** `zeph knowledge ingest --source subagents --yes` ingests transcripts;
every created edge has `origin = 'subagent'` and non-null `import_batch_id`; `rollback <batch>`
removes them cleanly; sanitizer and write-gate mocks were invoked.

---

### Phase 3 — External Import (Deferred, PR D1)

Deferred to a post-M29 backlog issue (#5024). Prerequisites: Phase 0 provenance in production,
write-path PII redaction closed, version-pinned strict allowlist parser for Claude Code / Codex
schemas, and current-project scope enforcement. A `TODO(critic D1)` marker is placed at
`IngestSourceKind::ExternalAgent` (spec §9).

---

## Milestones

| Milestone | Content | Wave | Target |
|---|---|---|---|
| M29-W1 | Phase 0 + F3 scaffold (F1 #5015, F2 #5016, F3 #5017 merged in parallel) | Wave 1 | Sprint N |
| M29-W2 | Phase 1 value (N1 #5018, N2 #5019, N3 #5020) | Wave 2 | Sprint N |
| M29-W3 | Graph batch API (G1 #5021) | Wave 3 | Sprint N+1 |
| M29-W4 | Measurement spike GATE (G0 #5022) — GO or NO-GO | Wave 4 | Sprint N+1 |
| M29-W5 | Graph go-live if G0 GO (G2 #5023) | Wave 5 | Sprint N+2 |
| M29-Deferred | External import (D1 #5024) | Backlog | Post-M29 |

## Dependencies

- `blake3` — already in workspace (used by `zeph-index`); reuse for `content_hash`
- `futures` — already in workspace; `buffer_unordered` for bounded concurrency
- `zeph-sanitizer` — Layer 2; already a dependency of `zeph-agent-context`; binary can access it
- No new inter-crate dependency edges are introduced (binary already depends on
  `zeph-subagent` + `zeph-memory` + `zeph-sanitizer`)

## Risk Register

| Risk | Impact | Probability | Mitigation |
|---|---|---|---|
| Phase 0 migration contention on hot `graph_edges` table (5+ indexes) | High | Low | Run migration during maintenance window; additive `ALTER TABLE` does not rebuild indexes in SQLite; test on production-sized DB before PR |
| §7 spike fails → Phase 2 abandoned mid-implementation | Medium | Medium | G1 batch API still ships and provides value for future sources; G0 is an explicit gate; NO-GO is a defined outcome, not a failure |
| Technical-document prompt under-extracts specs/handoffs | High | Medium | Validate with `--dry-run` over 10 specs before G0; tune prompt if S/N < 50% |
| Hub-node explosion (PR numbers, crate names dominate graph) | High | Medium | Hub-degree check in dry-run (FR-026, FR-051); kill-criterion in G0 blocks G2 if exceeded |
| LLM non-determinism creates near-duplicate edges across runs | Medium | High | Documented as INV-5; existing `canonical_relation` + `import_batch_id` supersession is the mitigation; `rollback` + re-ingest is the operator escape hatch |
| Postgres migration diverges from SQLite | Medium | Low | `migration_parity.rs` enforced in CI |

## Constitution Compliance

| Principle | Status | Notes |
|---|---|---|
| No new inter-crate dependency edges | Compliant | Adapters stay pure in `zeph-memory`; JSONL reading in binary |
| Multi-model provider resolution | Compliant | `ingest_provider → extract_provider → primary` (INV-8) |
| SQLite + Postgres parity | Compliant | `migration_parity.rs` enforced |
| TUI spinner for background ops | Compliant | `Ingesting knowledge: <uri>...` spinner (NFR-6.3) |
| Tests before merge | Compliant | Unit + integration tests for each phase (NFR-2.1, NFR-3.1..3.3) |
| No `unwrap`/`expect` in production | Compliant | `MemoryError::Ingest` propagated via `?` throughout |
| Doc comments on public API | Compliant | NFR-4.4 mandates `///` on all public items |
| CLAUDE.md dev-rules 1–7 | Compliant | Config, CLI, TUI, `--init`, `--migrate-config`, playbook, coverage-status all explicitly required by tasks |
