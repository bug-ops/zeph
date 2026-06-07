---
aliases:
  - Knowledge Ingest SRS
tags:
  - srs
  - memory
  - requirements
created: 2026-06-07
status: draft
spec_id: "067"
standard: "ISO/IEC/IEEE 29148:2018"
---

# SRS: Knowledge Ingest (`zeph knowledge ingest`)

Standard: ISO/IEC/IEEE 29148:2018. EARS notation is used for all functional requirements.
"shall" = mandatory; "should" = recommended; "may" = optional.

---

## 1. Phase 0 — Provenance and Rollback Prerequisite

This phase is a hard prerequisite for any graph write. It must land before Phase 2 begins.

**FR-001** (→ BR-4, INV-2)
THE SYSTEM SHALL add `origin TEXT NOT NULL DEFAULT 'conversation'` to both `graph_edges` and
`graph_entities` tables; all existing rows SHALL be backfilled to the value `'conversation'`
in the same migration.

**FR-002** (→ BR-4, INV-2)
THE SYSTEM SHALL add `import_batch_id TEXT NULL` and `source_uri TEXT NULL` to `graph_edges`;
THE SYSTEM SHALL add `import_batch_id TEXT NULL` to `graph_entities`. These columns SHALL be
NULL for conversation-origin rows and non-NULL for all imported rows.

**FR-003** (→ BR-4, INV-2)
WHEN recall scores edges, THE SYSTEM SHALL support excluding or down-weighting rows where
`origin != 'conversation'`, controlled by the config flag `[knowledge].recall_include_imported`
(default `true`).

**FR-004** (→ BR-4)
THE SYSTEM SHALL provide a `zeph knowledge rollback <import_batch_id>` command that deletes
all `graph_edges` and `graph_entities` rows carrying that `import_batch_id` and any orphaned
imported entities, without modifying rows where `origin = 'conversation'`.

---

## 2. Phase 1 — Static Artifacts to Semantic Notes

**FR-010** (→ BR-1, INV-1)
WHEN `--source specs`, `--source changelog`, `--source handoff`, `--source coverage`, or
`--source git-log` is specified, THE SYSTEM SHALL load the corresponding artifacts of the
current project only and ingest them as semantic notes via the existing `IngestionPipeline`.
No graph writes SHALL occur in Phase 1.

**FR-011** (→ INV-1, spec §2.1)
THE SYSTEM SHALL reuse the existing `TextLoader`, `TextSplitter`, and `IngestionPipeline`
for Phase 1; it SHALL NOT create a parallel or duplicate document loader.

**FR-012** (→ BR-2, INV-5)
THE SYSTEM SHALL skip documents whose `(source_uri, content_hash)` is already present in the
ingest ledger; no re-embedding or LLM call SHALL be issued for unchanged inputs.

**FR-013** (→ BR-3)
WHEN `--dry-run` is specified, THE SYSTEM SHALL report: files discovered, chunks that would
be produced, and estimated embedding token cost. THE SYSTEM SHALL write nothing to Qdrant,
the graph, or the ledger during a dry run.

**FR-014** (→ spec §8 TUI rules)
THE SYSTEM SHALL stream `IngestProgress` events to a TUI or CLI status indicator displaying
`Ingesting knowledge: <uri>...` during any write operation.

---

## 3. Phase 2 — Subagent Transcripts to Knowledge Graph (Gated)

> [!warning] Gate
> Phase 2 requirements are conditional. They are activated only if the §7 measurement spike
> (FR-050 through FR-053) passes all kill-criterion thresholds. If the spike fails, subagent
> episode sources are rerouted to the notes pipeline (Phase 1) and these requirements are abandoned.

**FR-020** (→ BR-5, INV-1)
WHEN `--source subagents` is specified, THE SYSTEM SHALL read Zeph subagent transcript files
for the current project from the configured `transcript_dir`, normalize each entry to an
`IngestDocument`, and invoke `SemanticMemory::ingest_documents()`.

**FR-021** (→ INV-3)
WHEN extracting graph knowledge during ingest, THE SYSTEM SHALL route every candidate edge
through the write-quality gate (spec 004-9) and admission control (spec 004-3). THE SYSTEM
SHALL NOT bypass either gate. Only the RPE per-turn heuristic is bypassed (it is a
conversational gate with no meaning for batch documents).

**FR-022** (→ INV-4, NFR-003)
THE SYSTEM SHALL run the `zeph-sanitizer` PII and exfiltration validator as the
`PostExtractValidator` callback for every ingest document before any entity or fact string
is persisted.

**FR-023** (→ spec §2.5)
THE SYSTEM SHALL select the technical-document extraction system prompt (the second `const`
in `extractor.rs`) for non-conversational sources (`IngestSourceKind != SubagentTranscript`
conversational turns). This prompt retains entity types `project`, `tool`, `concept`, `file`
but drops conversational-filler rejection rules.

**FR-024** (→ INV-2)
THE SYSTEM SHALL stamp every imported `graph_edges` row and `graph_entities` row with the
values `origin`, `source_uri`, and `import_batch_id` from the `Provenance` struct of the
originating `IngestDocument`.

**FR-025** (→ BR-2, INV-5)
THE SYSTEM SHALL skip any `IngestDocument` whose `(source_uri, content_hash)` is already
recorded in the `knowledge_ingest_ledger` table; no LLM extraction call SHALL be issued
for that document.

**FR-026** (→ BR-3)
WHEN `--dry-run` is specified for a graph-targeted source, THE SYSTEM SHALL report: document
count, turn count, estimated extraction token cost, projected entity and edge counts, AND a
projected hub-degree distribution showing the top-N entities by degree. THE SYSTEM SHALL write
nothing to the graph, Qdrant, or the ledger.

**FR-027** (→ NFR-002)
THE SYSTEM SHALL bound extraction concurrency to `[knowledge].concurrency` (default 3) using
`futures::buffer_unordered`; total work MAY be capped with `--max-documents` or
`[knowledge].max_documents` (default 0 = unlimited).

**FR-028** (→ NFR-001)
THE SYSTEM SHALL collect per-document failures and continue processing the remainder of the
batch; a single failed transcript MUST NOT abort the ingest run. All failures SHALL be
reported in the `IngestReport` at completion.

**FR-029** (→ BR-4)
`zeph knowledge status` SHOULD list import batches (`import_batch_id`, source kind, timestamp,
entity count, edge count) and a ledger summary.

---

## 4. Cross-Cutting Requirements

**FR-040** (→ INV-7)
THE SYSTEM SHALL require explicit confirmation — either the `--yes` flag or an interactive
`y/N` prompt — before any write that targets the knowledge graph.

**FR-041** (→ INV-8)
THE SYSTEM SHALL resolve the ingest LLM provider via the chain:
`[knowledge].ingest_provider` (if non-empty) → `[memory.graph].extract_provider` → primary.
An unknown provider name SHALL emit a WARN and fall back to the primary; THE SYSTEM SHALL
NOT panic or abort on an unresolvable name.

**FR-042** (→ INV-6, NFR-003)
THE SYSTEM SHALL reject any source path that does not reside under the current project root,
failing fast with a `MemoryError::Ingest` "source outside project root" error and writing
nothing.

---

## 5. Phase 2 Gate — Measurement Spike

**FR-050** (→ BR-5, spec §7)
THE SYSTEM SHALL provide a spike evaluation procedure (`--dry-run` over a representative
corpus) that measures recall@5 of the graph path against the semantic-notes baseline on a
held-out set of at least 10 cross-session questions.

**FR-051** (→ BR-5, spec §7)
THE SYSTEM SHALL evaluate hub-degree health: no single entity may account for more than 15%
of projected edges in the dry-run report.

**FR-052** (→ BR-5, spec §7)
THE SYSTEM SHALL evaluate signal-to-noise: at least 50% of projected edges must be non-trivial
(not bare `tool`↔`project` "mentioned-in" links).

**FR-053** (→ BR-5, spec §7)
WHEN any of the following kill-criteria are met, Phase 2 SHALL be abandoned and subagent
episode sources SHALL be rerouted to the existing notes pipeline:
- The 3 designated multi-hop queries do not beat the notes baseline on recall@5;
- The hub-degree threshold (> 15%) is violated;
- The signal-to-noise threshold (< 50%) is violated.

---

## 6. Configuration Requirements

**FR-060**
THE SYSTEM SHALL add a `[knowledge]` section to `zeph-config` with `#[serde(default)]` on
all fields. Required fields: `ingest_provider: ProviderName` (default empty string, meaning
fall through to `extract_provider`), `concurrency: usize` (default 3), `max_documents: usize`
(default 0 = unlimited), `recall_include_imported: bool` (default `true`),
`transcript_scope: String` (default `"current-project"`).

**FR-061**
WHEN `zeph --init` runs, THE SYSTEM SHALL present a `step_knowledge()` wizard step offering
the operator the ability to enable the graph episode sink (default off) and choose
`ingest_provider`. THE SYSTEM SHALL emit the `[knowledge]` section only when the user
opts into non-default values.

**FR-062**
WHEN `zeph --migrate-config` runs and the `[knowledge]` section is absent from the existing
config, THE SYSTEM SHALL inject the section with all default values. The migration SHALL be
idempotent.

---

## 7. CLI Integration Requirements

**FR-070**
THE SYSTEM SHALL add a `Command::Knowledge` variant to `src/cli.rs` with three subcommands:
`Ingest { sources, dry_run, max_documents, provider, yes }`,
`Rollback { batch_id }`, and `Status`. Dispatch SHALL occur in `src/runner.rs` to
`src/commands/knowledge.rs`, mirroring the structure of the existing `src/commands/ingest.rs`.

**FR-071**
THE SYSTEM SHALL add command palette entries to `zeph-tui`: `/knowledge ingest <source>`,
`/knowledge status`, `/knowledge rollback <batch>`. A visible spinner with the message
`Ingesting knowledge: <uri>...` SHALL be displayed during any ingest write operation.

---

## 8. Database Migration Requirements

**FR-080** (→ FR-001, FR-002, NFR-007)
THE SYSTEM SHALL provide a single migration file in both `crates/zeph-db/migrations/sqlite/`
and `crates/zeph-db/migrations/postgres/` that adds the provenance columns
(`origin`, `import_batch_id`, `source_uri`) to `graph_edges` and `graph_entities`, creates
the `knowledge_ingest_ledger` table, and backfills `origin = 'conversation'`. The migration
SHALL be validated by `migration_parity.rs`.

---

## 9. Open Questions

| ID | Question | Owner | Decision needed by |
|---|---|---|---|
| OQ-1 | Should `recall_include_imported` default to `false` after the §7 spike to avoid importing noise into recall before the graph quality is proven? | team-lead | Before Phase 2 PR |
| OQ-2 | Should `zeph knowledge status` include per-batch recall@5 metrics collected during the §7 spike? | architect | Before G0 #5022 |
| OQ-3 | `max_documents = 0` means unlimited — should there be a hard safety cap (e.g., 10 000) to prevent runaway cost on large corpora? | team-lead | Before N1 #5018 |
