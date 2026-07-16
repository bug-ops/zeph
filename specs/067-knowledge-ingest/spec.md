---
aliases:
  - Knowledge Ingest
  - Knowledge Ingest Command
  - spec-067
tags:
  - sdd
  - spec
  - memory
  - cross-cutting
created: 2026-06-07
status: draft
spec_id: "067"
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
  - "[[004-memory/spec]]"
  - "[[004-memory/004-9-memory-write-gate]]"
  - "[[004-memory/004-6-graph-memory]]"
  - "[[012-graph-memory/spec]]"
  - "[[018-index/spec]]"
  - "[[041-sanitizer/spec]]"
  - "[[056-autoskill-trace-extraction/spec]]"
  - "[[constitution]]"
---

# Spec-067: Knowledge Ingest (`zeph knowledge ingest`)

**GitHub:** #5012 (epic) — child issues #5015–#5024; milestone M29
**Branch:** `feat/m29/knowledge-ingest`
**Crate:** `zeph-memory` (extends), `zeph-db` (migration), `zeph-config` (extends), `zeph` (binary, new `src/commands/knowledge.rs`), reuses `zeph-sanitizer`
**Status:** draft — pre-implementation; Phase 2 graph path is **gated behind a measurement spike** (see §7).

---

## Summary

A one-shot operator command that loads project knowledge into Zeph's memory subsystems on demand,
instead of relying solely on the background extraction tied to live conversation.

The feature has **two distinct sinks**, decided after architect + adversarial-critic review
(handoffs: `.local/handoff/knowledge-ingest-architect.md`, `.local/handoff/knowledge-ingest-critique.md`):

1. **Static project artifacts → semantic notes (Qdrant)** via the *existing* `IngestionPipeline`
   used by `zeph ingest`. No graph writes. Proven path, zero relational-recall risk. This is the
   default and lowest-risk slice.
2. **Agent episodes → knowledge graph** (relational, multi-hop). The graph is reserved **only** for
   the cross-agent episode layer, where multi-hop traversal genuinely pays off. MVP source is Zeph's
   own subagent transcripts (current project, already `zeph_llm::Message`-typed). This path is gated:
   it ships **only if** a measurement spike proves signal-to-noise and a real multi-hop query benefit.

Raw source code is **explicitly out of scope** — it is already owned by `zeph-index`
(tree-sitter → Qdrant + symbol index + repo map). Duplicating it in the conversational graph would
flood relational recall with hub nodes.

External-agent transcript import (Claude Code / OpenAI Codex) is **deferred to a later gated phase**
(§9, Phase 3) due to undocumented/unstable schemas, cross-project privacy blast radius, and the
verbatim-no-PII-redaction write path.

---

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-memory/src/semantic/graph.rs` | `SemanticMemory::extract_and_store()` (the reuse seam); new `ingest_documents()` |
| `crates/zeph-memory/src/graph/extractor.rs` | `GraphExtractor`, hardcoded conversational system prompt (lines 12-60) |
| `crates/zeph-memory/src/graph/rpe.rs` | RPE per-turn surprise gate (agent-layer only) |
| `crates/zeph-core/src/agent/persistence/extract.rs` | `enqueue_graph_extraction_task` — where RPE + guards live |
| `crates/zeph-config/src/memory/graph.rs` | `[memory.graph]` config, `extract_provider` (line 549), `write_gate` |
| `src/commands/ingest.rs` | Existing doc-loader precedent → `IngestionPipeline` → Qdrant notes |
| `crates/zeph-subagent/src/transcript.rs` | `TranscriptEntry { seq, timestamp, message }` JSONL reader |
| `crates/zeph-sanitizer/` | PII / exfiltration validators for the write path |
| `crates/zeph-db/migrations/sqlite/021_knowledge_graph.sql` | `graph_edges` / `graph_entities` schema (no provenance column today) |
| `crates/zeph-db/migrations/sqlite/029_graph_edge_dedup.sql` | `UNIQUE(source_entity_id, target_entity_id, relation)` active-edge dedup |

### Related specs
- `[[004-memory/004-9-memory-write-gate]]` — MemReader write-quality gate; project evidence that
  *write pollution collapses recall faster than scoring drift*. **This spec MUST honor that gate, not bypass it.**
- `[[004-memory/004-3-admission-control|A-MAC Admission Control]]` — admission control (INV §14).
- `[[018-index/spec]]` — code RAG; the boundary this spec must not cross.
- `[[056-autoskill-trace-extraction/spec]]` — precedent for background extraction from session history
  with a per-session idempotency table (`skill_trace_sessions`) and `*_provider` config.

---

## 1. Overview

### Problem Statement

Zeph's knowledge graph and semantic memory are populated only as a side effect of live conversation
(background, RPE-gated, fire-and-forget). There is no way to deliberately seed memory with the
project's accumulated knowledge — its specs, changelog, handoffs, and the episodic record of work
that agents (including Zeph's own subagents) have done. A new operator or a fresh database starts
blind to all prior project context.

### Goal

A `zeph knowledge ingest` command that, on demand and idempotently:

- loads high-signal static project artifacts into **semantic notes** (Qdrant) so they are recallable
  by vector similarity; and
- (gated) extracts the **relational episode layer** from subagent transcripts into the **knowledge graph**,
  enabling multi-hop recall over cross-session project decisions — *only if* this is measurably better
  than semantic notes alone.

When done: an operator can run one command to make a fresh Zeph instance project-aware, with full
provenance, idempotent re-runs, a `--dry-run` preview, and a rollback path for graph imports.

### Out of Scope

- **Raw source code → graph.** Owned by `zeph-index`. Never duplicated here.
- **External-agent transcript import (Claude Code / Codex).** Deferred to Phase 3 (§9), gated on
  provenance (Phase 0), write-path PII redaction maturity, and a version-pinned strict parser.
- **Bypassing the write-quality gate (004-9) or admission control (004-3).** Only RPE is bypassed,
  and only because it is a per-turn conversational heuristic with no meaning for batch documents.
- **Automatic / scheduled ingest.** This is an explicit operator command. A future hook/scheduler
  tap is out of scope (a TODO marker is left for it).
- **Structural code graph (call/def edges into `graph_edges`).** Out of scope; `zeph-index` MCP tools cover it.

---

## 2. Architecture / Design

### 2.1 Two sinks, one command

```
zeph knowledge ingest --source <SRC>... [--dry-run] [--max-documents N] [--provider NAME] [--yes]
zeph knowledge rollback <import_batch_id>
zeph knowledge status                      # list import batches + ledger summary

SRC ∈ { specs, changelog, handoff, coverage, git-log,    ← Phase 1: → semantic notes (Qdrant)
        subagents,                                        ← Phase 2: → knowledge graph (gated)
        claude-code, codex }                              ← Phase 3: deferred (rejected pre-gate)
```

- **Notes sink (Phase 1):** static artifacts are read from disk, chunked with the *existing*
  `TextLoader` / `TextSplitter`, and fed to the *existing* `IngestionPipeline` (the same path
  `src/commands/ingest.rs` uses). No new loader. No graph writes.
- **Graph sink (Phase 2):** subagent transcripts are normalized to `IngestDocument`s and fed to a new
  batch extraction API on `SemanticMemory` that **reuses `extract_and_store()`** per document.

### 2.2 Crate placement (no new crate, no new dependency edges)

| Layer | Lives in | Responsibility |
|---|---|---|
| Batch extraction API + ledger + adapter trait | `zeph-memory` (`src/graph/ingest/`) | Graph owner. Pure text→document adapters (no I/O on peer crates). |
| Disk walking, transcript discovery, external-format reading, progress render | `zeph` binary (`src/commands/knowledge.rs`) | Already depends on `zeph-subagent` + `zeph-memory`; mirrors `ingest.rs`. |
| Config | `zeph-config` (`[knowledge]` + `[memory.graph]` extensions) | Pure data. |
| Migration | `zeph-db` (sqlite + postgres parity) | Provenance columns + ledger table. |

A standalone `zeph-knowledge` crate is **rejected** for MVP (no second consumer; would force a DRY or
dependency-direction violation — see INV-1 watch in §8).

### 2.3 The synchronous batch API (graph sink)

```rust
// crates/zeph-memory/src/semantic/graph.rs
impl SemanticMemory {
    /// Extract knowledge from a batch of documents into the graph.
    ///
    /// Bypasses the RPE per-turn gate (not meaningful for batch docs) but routes every
    /// candidate edge through the write-quality gate (004-9) and admission control (004-3).
    /// Each document is content-hash-checked against the ingest ledger; unchanged inputs
    /// are skipped with no LLM call. Progress is streamed on `progress`.
    pub async fn ingest_documents(
        &self,
        documents: Vec<IngestDocument>,
        provider: AnyProvider,           // resolved ingest_provider → extract_provider → primary
        config: GraphExtractionConfig,
        validator: PostExtractValidator, // zeph-sanitizer hook on the write path (mandatory for ingest)
        batch_id: ImportBatchId,
        concurrency: usize,              // bounded; futures::buffer_unordered(concurrency)
        progress: mpsc::Sender<IngestProgress>,
    ) -> Result<IngestReport, MemoryError>;
}
```

Internally: `is_ingested(uri, hash)` filter → `buffer_unordered(concurrency)` over
`extract_and_store(doc.content, doc.context, …, Some(Provenance { origin, source_uri, batch_id }))`
→ on success `mark_ingested`. **Reuse `extract_and_store()` verbatim** — it does not contain RPE
(RPE lives in the agent layer at `extract.rs`). Fail strategy: **collect-errors, continue** — one bad
transcript MUST NOT abort the batch; failures are reported in `IngestReport`.

### 2.4 Key types (`crates/zeph-memory/src/graph/ingest/`)

| Type | Pattern | Purpose |
|---|---|---|
| `IngestSourceKind` | `#[non_exhaustive]` enum | `StaticArtifact`, `SubagentTranscript`, `ExternalAgent` (Phase 3). Maps to `graph_edges.origin`. |
| `IngestDocument` | valid-by-construction struct, private fields | `content`, `context: Vec<String>`, `provenance: Provenance`, `content_hash: blake3::Hash`. Built only via adapters. |
| `Provenance` | newtype | `{ kind: IngestSourceKind, source_uri: String, batch_id: ImportBatchId }`. `source_uri` e.g. `"subagent:<task_id>"`, `"specs/004/spec.md@<git-sha>"`. |
| `ImportBatchId` | newtype over UUID/ULID string | One per ingest run; the rollback key. |
| `IngestSourceAdapter` | sealed trait, one impl per format | `fn parse(raw: &str) -> Result<Vec<IngestDocument>, MemoryError>`. Pure. MVP: `SubagentJsonl` (trivial — already `Message`). |
| `IngestLedger` | repository over new table | `is_ingested(uri, hash)`, `mark_ingested(uri, hash, batch, counts)`. Re-read/cost guard **only** (see INV-5). |
| `IngestProgress` / `IngestReport` | progress enum + summary, modeled after `IndexProgress` | Streamed to CLI/TUI spinner; report lists per-source counts + failures. |

Errors consolidate into `MemoryError::Ingest` — **no new error enum / no new crate**.

### 2.5 Technical-document extraction prompt

The hardcoded extractor prompt (`extractor.rs:12-60`) is tuned for first-person conversational claims
and explicitly *rejects* code, config keys, file paths, tool names, and structured data — i.e. exactly
what specs/changelog/handoffs/transcripts contain. Feeding them the conversational prompt silently
under-extracts.

→ Add a **second `const` system prompt** ("technical-document" mode) selectable by `IngestSourceKind`,
keeping entity types `project / tool / concept / file` but dropping the conversational-filler rules.
This applies to the graph sink only (Phase 2). The notes sink (Phase 1) does not run extraction at all.

### 2.6 Multi-model provider wiring (CLAUDE.md compliance)

Add `[knowledge].ingest_provider: ProviderName`. Resolution chain:
`ingest_provider` (if non-empty) → `[memory.graph].extract_provider` → primary. Resolve via the
existing `resolve_background_provider` path (bypasses the quality gate for JSON extraction).
Ingest is a simple/medium task → default config points it at the `fast` provider. **Never hardcode a model.**

---

## 3. Functional Requirements

### Phase 0 — Provenance + rollback prerequisite (MUST land before any graph write)

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL add `origin TEXT NOT NULL DEFAULT 'conversation'` to `graph_edges` AND `graph_entities`; existing rows backfill to `'conversation'` | must |
| FR-002 | THE SYSTEM SHALL add `import_batch_id TEXT NULL` and `source_uri TEXT NULL` to `graph_edges` (and `import_batch_id` to `graph_entities`) | must |
| FR-003 | WHEN recall scores edges, THE SYSTEM SHALL be able to exclude or down-weight rows where `origin != 'conversation'`, controlled by `[knowledge].recall_include_imported` (default `true`) | must |
| FR-004 | THE SYSTEM SHALL provide `zeph knowledge rollback <import_batch_id>` that deletes all entities/edges carrying that `import_batch_id` (and orphaned imported entities) | must |

### Phase 1 — Static artifacts → semantic notes

| ID | Requirement | Priority |
|----|------------|----------|
| FR-010 | WHEN `--source specs\|changelog\|handoff\|coverage\|git-log`, THE SYSTEM SHALL load the corresponding artifacts of the **current project only** and ingest them as semantic notes via the existing `IngestionPipeline` | must |
| FR-011 | THE SYSTEM SHALL reuse the existing `TextLoader`/`TextSplitter`/`IngestionPipeline` — NOT a parallel loader | must |
| FR-012 | THE SYSTEM SHALL skip unchanged inputs via the content-hash ledger (no re-embed of unchanged files) | must |
| FR-013 | WHEN `--dry-run`, THE SYSTEM SHALL report files discovered, chunks that would be produced, and estimated embedding token cost, and write nothing | must |
| FR-014 | THE SYSTEM SHALL stream progress (`IngestProgress`) to a TUI/CLI status indicator (`Ingesting knowledge: <uri>…`) | must |

### Phase 2 — Subagent transcripts → graph (GATED, see §7)

| ID | Requirement | Priority |
|----|------------|----------|
| FR-020 | WHEN `--source subagents`, THE SYSTEM SHALL read Zeph subagent transcripts of the current project from the configured `transcript_dir`, normalize each to `IngestDocument`s, and call `ingest_documents()` | must |
| FR-021 | WHEN extracting graph knowledge during ingest, THE SYSTEM SHALL route every candidate edge through the write-quality gate (004-9) and admission control (004-3); it SHALL NOT bypass them | must |
| FR-022 | THE SYSTEM SHALL run the `zeph-sanitizer` PII/exfiltration validator as the `PostExtractValidator` for every ingest document | must |
| FR-023 | THE SYSTEM SHALL select the technical-document extraction prompt for non-conversational sources | must |
| FR-024 | THE SYSTEM SHALL stamp every imported edge/entity with `origin`, `source_uri`, and `import_batch_id` | must |
| FR-025 | THE SYSTEM SHALL skip documents whose `(source_uri, content_hash)` is already in the ledger (no LLM call) | must |
| FR-026 | WHEN `--dry-run`, THE SYSTEM SHALL report documents, turns, estimated extraction tokens, projected entity/edge counts, AND the projected hub-degree distribution (top-N entities by degree), and write nothing | must |
| FR-027 | THE SYSTEM SHALL bound concurrency (`[knowledge].concurrency`, default 2–4) and cap total work via `--max-documents` / `[knowledge].max_documents` | must |
| FR-028 | THE SYSTEM SHALL collect per-document failures and continue; a single failed transcript MUST NOT abort the batch | must |
| FR-029 | `zeph knowledge status` SHALL list import batches (`import_batch_id`, source, timestamp, entity/edge counts) and ledger summary | should |

### Cross-cutting

| ID | Requirement | Priority |
|----|------------|----------|
| FR-040 | THE SYSTEM SHALL require explicit confirmation (`--yes` or interactive) before any write that targets the graph | must |
| FR-041 | THE SYSTEM SHALL resolve the ingest provider via `ingest_provider → extract_provider → primary`; unknown names warn and fall back, never panic | must |
| FR-042 | THE SYSTEM SHALL refuse sources outside the current project root (path allowlist), failing with a clear error | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Reliability | Ingest failure (parse, LLM, DB) MUST be logged and reported in `IngestReport`; it MUST NEVER crash the process or leave a half-written batch unrollbackable |
| NFR-002 | Cost | A full backfill MUST be cost-bounded: ledger skip + bounded concurrency + `max_documents` + `--dry-run` token estimate before any write |
| NFR-003 | Security | Imported content MUST pass the sanitizer validator before graph write; sources MUST be confined to the current project (FR-042) |
| NFR-004 | Idempotency | Re-running ingest on unchanged inputs MUST NOT re-issue LLM calls (ledger) and MUST NOT create duplicate rows (existing entity/edge dedup as defense in depth) |
| NFR-005 | Observability | Tracing spans `memory.ingest.*` for adapter parse, ledger check, extraction, write; per-source counts logged |
| NFR-006 | Latency | Ingest is an operator command; it MUST NOT run during a live turn and MUST NOT add latency to the agent loop |
| NFR-007 | Portability | Migration MUST be sqlite + postgres parity (enforced by `migration_parity.rs`) |

---

## 5. Key Invariants

### Always (without asking)
- **INV-1 — Two sinks, never crossed.** Static artifacts go to **semantic notes**; only the episode
  layer (subagent transcripts) goes to the **graph**. Code never goes to either (it is `zeph-index`'s).
- **INV-2 — Provenance is a prerequisite, not a feature.** No ingest may write to the graph until
  `origin` / `import_batch_id` / `source_uri` columns exist and recall can isolate imported rows.
  Every imported edge/entity carries a non-null `import_batch_id`.
- **INV-3 — Gates stay on.** Ingest bypasses **only** RPE (a per-turn conversational heuristic).
  The write-quality gate (004-9) and admission control (004-3) MUST run for every candidate edge.
- **INV-4 — Sanitizer on the write path.** Every ingest document passes the `zeph-sanitizer`
  validator before any entity/fact string is persisted.
- **INV-5 — The ledger is a re-read/cost guard, NOT a drift guard.** Content-hash idempotency only
  prevents re-reading unchanged input. It does NOT reconcile LLM extraction drift (same input → different
  relation verbs across model versions). Drift is mitigated by `canonical_relation` and by
  `import_batch_id` supersession on re-ingest — and this limitation is documented to operators.
- **INV-6 — Current project only.** Sources are confined to the current project root by an explicit
  allowlist. No traversal into other projects' artifacts or transcripts.
- **INV-7 — Operator-explicit.** Ingest runs only when invoked; graph writes require confirmation.
- **INV-8 — Provider resolved, never hardcoded.** `ingest_provider → extract_provider → primary`.

### Ask First
- Enabling the graph sink (Phase 2) for any source other than subagent transcripts.
- Relaxing the technical-document prompt's entity allow-list (risk of hub-node pollution).
- Changing `recall_include_imported` default to exclude (changes recall behavior project-wide).

### Never
- **NEVER** ingest raw source code into the graph (use `zeph-index`).
- **NEVER** bypass the write-quality gate (004-9) or admission control (004-3) for ingest.
- **NEVER** write imported edges/entities without `import_batch_id` (would be unrollbackable — the C1 blocker).
- **NEVER** persist an ingest fact that has not passed the sanitizer validator.
- **NEVER** ingest external-agent (Claude Code / Codex) transcripts in MVP — deferred to Phase 3 (§9).
- **NEVER** traverse outside the current project root.
- **NEVER** present the content-hash ledger as protection against extraction drift.
- **NEVER** hardcode the ingest model.
- **NEVER** run ingest synchronously inside a live agent turn.

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Empty / no artifacts found for a `--source` | Report 0 documents, exit 0, no rows written |
| Malformed transcript JSONL line | Skip the line (warn), continue the file; report skipped count |
| LLM extraction failure on one document | Log WARN, mark document failed in `IngestReport`, do NOT mark ledger, continue batch |
| Re-run after model upgrade (drift) | Unchanged inputs skipped (ledger); operator may `rollback <batch>` then re-ingest to supersede (INV-5) |
| Partial batch interrupted (Ctrl-C) | Already-extracted documents are in the ledger + tagged with `batch_id`; re-run resumes the rest; `rollback` removes the partial batch cleanly |
| Source path outside project root | Fail fast with `MemoryError::Ingest` "source outside project root" (FR-042 / INV-6) |
| `--dry-run` | Extraction-preview / chunk-preview only; **zero** writes to Qdrant, graph, or ledger |
| Sanitizer rejects a fact | Drop that edge, log at debug, continue; report dropped count |
| Hub-node explosion detected in dry-run | Surface in report; operator decides; Phase 2 gate (§7) may block the real run |

---

## 7. Phase 2 Gate — Measurement Spike & Kill-Criterion

Phase 2 (graph sink) ships **only if** the spike below passes. This directly answers the critic's
S1 (graph-vs-notes) and S2 (taxonomy/hub-node pollution).

**Spike (run via `--dry-run` over 10 specs + all current-project subagent transcripts):**
1. **Multi-hop benefit.** Author 3 concrete recall queries that (a) the team actually asks,
   (b) require ≥2-hop traversal, (c) demonstrably fail with semantic-note recall alone. Measure
   recall@5 of the graph path vs. the notes baseline on a held-out set of ≥10 cross-session questions.
2. **Hub-degree health.** No single entity may account for > 15% of projected edges; the top-10
   entities by degree must not dominate the graph (community structure must survive).
3. **Signal-to-noise.** ≥ 50% of projected edges must be non-trivial (not bare `tool`↔`project`
   "mentioned-in" links).

**Kill-criterion (any of):**
- The 3 multi-hop queries do not beat the notes baseline on recall@5 → **abandon the graph sink**;
  route subagent episodes to the notes pipeline instead and ship Phase 1 only.
- Hub-degree or S/N thresholds fail → **abandon** until the technical-document prompt / taxonomy is fixed.

The spike result and decision are recorded in the live-testing playbook before the Phase 2 PR is opened.

### 7.1 Re-measurement (2026-07-17, #5625)

Follow-up to #5467 (first live measurement, top entity `Python` at 24.3%) and its own fix PR,
which added the Rule 3 language/tool anchoring clause to `TECH_DOC_SYSTEM_PROMPT`. #5467's own
re-runs after that fix already showed the gate still failing in most runs (15.9/16.9/17.1/20.8%,
alternating `bash`/`Python` hubs) and filed this issue to re-measure on the full corpus before any
further go/no-go call.

**Corpus.** The real `.zeph/subagents/*.jsonl` corpus: 29 transcript files (29 paired `.meta.json`),
74 documents after chunking. Confirmed non-diverse: dominated by small CI-marker / smoke-test
scenarios (network-deny proof scripts using `curl`, hello-world `bash`/Python scripts, a
"Rust ownership" Q&A turn, a coindesk API-fetch script) rather than production
architecture/spec/code-review/debugging sessions. Not a constructed/synthetic corpus — this is
Option A (the real corpus) per the architect-revised, critic-approved plan; Option B (a
deliberately-diverse constructed corpus) was explicitly excluded from the decision path and was
not built.

**§7 spec-drift note (C4).** This section's spike header reads "run via `--dry-run` over 10 specs +
all current-project subagent transcripts." Verified in code (`src/commands/knowledge.rs`,
`_ => notes_sources` dispatch): `--source specs` routes to the **notes** sink and produces **zero
graph edges** — a no-op for the hub-degree arm specifically. This re-measurement is over the
subagent-transcript corpus only; the "10 specs" half of the header does not apply to this arm.

**Provider.** `[memory.graph] extract_provider = "openai-stt"` (gpt-4o-mini, cloud) was
**unreachable** this session — a live API call returned HTTP 429 `insufficient_quota` (account-level
billing stop, consistent with the project's known 2026-07 cloud-account exhaustion). Per the
plan's contingency, baseline was measured against a **local Ollama `qwen2.5:7b`** scratch provider
instead (`.local/config/testing.toml`, gitignored, not part of this PR's diff). Because the
conditional prompt fix could not be applied in this PR regardless (see below), the plan's
same-provider constraint between baseline and post-fix runs is moot here — there is no post-fix
arm to keep consistent with. **Caveat:** this measurement is not directly numerically comparable to
#5467's own gpt-4o-mini-based re-runs (15.9-20.8%) — a weaker local model may extract a less diverse
entity/relation set and could overstate hub concentration relative to the cloud model. The two
measurements agree qualitatively (gate fails, hub above 15%), which is the load-bearing finding
here, but the magnitudes should not be averaged or directly diffed across the two cycles.

**Measurement protocol.** N=5 fixed upfront (pre-registered, no discarding/re-rolling), command
`cargo run --features full -- --config .local/config/testing.toml knowledge ingest --source
subagents --dry-run`, identical corpus and provider across all 5 runs:

| Run | Top entity | % of edges | Verdict |
|-----|-----------|-----------|---------|
| 1 | `bash` | 27.8% | WARN |
| 2 | `bash` | 27.9% | WARN |
| 3 | `curl` | 25.8% | WARN |
| 4 | `curl` | 29.3% | WARN |
| 5 | `curl` | 28.8% | WARN |

Min 25.8%, median 27.9%, max 29.3%, count ≤15% = 0/5. Representative top-10 table (run 1):

```
Entity                                              Degree  % edges
--------------------------------------------------------------------
bash                                                    20    27.8% ⚠ HUB
curl                                                    19    26.4% ⚠ HUB
sleep                                                    5     6.9%
/tmp/zeph_netdeny_proof_allow.json                       5     6.9%
Python                                                   5     6.9%
Rust ownership                                           5     6.9%
echo                                                     4     5.6%
Zeph                                                     3     4.2%
marker file                                              3     4.2%
vault                                                    3     4.2%
--------------------------------------------------------------------
Top entity: 27.8% of edges — WARN — top entity exceeds hub-degree threshold
```

**Decision.** Per the pre-registered rule: no straddle (max 29.3% is nowhere near 15%, min 25.8% is
still clearly above it), median clearly >15%, 5/5 runs >15% → **clean, decisive FAIL**, not a
straddle or marginal-WARN. The Rule 3 language/tool anchoring fix from #5467 measurably relocated
degree away from `Python` specifically (`Python` is now a mid-table entry at 4.7-6.9%, not the top
entity in any of the 5 runs), but hub formation migrated to other incidentally-repeated command/tool
names (`bash`, `curl`) exactly as #5467's own root-cause analysis predicted ("degree just migrates
to a slightly-less-generic hub" on a corpus where nearly every transcript shares near-identical
CI-test anchors).

**Conditional fix precondition check (§4).** A clean decisive FAIL satisfies precondition 1 (a fix
may be considered). However, precondition 3 requires the re-measurement to cover **both** hub-degree
**and** the S/N (signal-to-noise, ≥50% non-trivial edges) arm on the same corpus+provider. The S/N
ratio is **not derivable** from the existing dry-run output: `IngestReport`
(`crates/zeph-memory/src/graph/ingest/report.rs`) carries only `entities_total`/`edges_total`/
`hub_degree` (entity name + degree), with no `edge_type`/relation breakdown anywhere in the printed
report or in any `tracing::debug!`/`trace!` call on the ingest path — confirmed by reading
`print_graph_ingest_report` and grepping the extractor/ingest modules. Deriving it would require
adding instrumentation, which is out of scope for this PR (would expand the change surface beyond
the conditionally-gated `prompt.rs` static). **Precondition 3 is not met → the conditional fix was
NOT applied. `crates/zeph-memory/src/graph/ingest/prompt.rs` is unchanged in this PR.**

Per the critic's C8 refinement: even had precondition 3 been met, a post-fix PASS on this same
non-representative corpus would not validate the gate for production — the corpus-non-diversity
caveat would still apply to any post-fix number. Since no fix was applied, the go/no-go conclusion
does not depend on that caveat at all here — it is the same as the honest-PASS case already
described above.

**Recommendation.** Defer the epic #5012 graph-sink go/no-go call. Two independent, non-representative
measurement cycles (#5467's cloud-model runs and this local-model re-measurement) both show the gate
failing above 15%, which is a real signal that a prompt-only fix targeting one hub identity at a
time (language/tool names, then presumably command names) does not durably solve hub formation on a
corpus dominated by repeated CI-test scaffolding — but neither cycle used a corpus representative of
production usage, so this does not by itself justify abandoning the graph sink either. Recommended
next steps (follow-up issues, not filed by this PR's author): (a) add S/N instrumentation to the
dry-run report so precondition 3 is measurable in a future cycle without expanding an unrelated PR's
scope, and (b) accumulate or construct a genuinely diverse production-representative subagent
transcript corpus (architecture/spec/code-review/debugging sessions) before attempting any further
prompt-level or structural hub-suppression fix, per this section's Option-A-only decision basis; and
(c) re-run the same 29-transcript corpus with the cloud provider (`openai-stt`/gpt-4o-mini) once
quota is restored, to isolate the provider variable and confirm the qualitative FAIL verdict holds
independent of model choice.

---

## 8. Integration Points (CLAUDE.md Development Rules 1–7)

### `zeph-config`
- New `[knowledge]` section: `ingest_provider`, `concurrency` (default 3), `max_documents` (default 0 = unlimited),
  `recall_include_imported` (default `true`), `transcript_scope = "current-project"`.
- `#[serde(default)]` on all fields; `--migrate-config` step adds the section to existing configs.

### CLI (`src/cli.rs`, `src/runner.rs`)
- `Command::Knowledge { Ingest { sources, dry_run, max_documents, provider, yes }, Rollback { batch_id }, Status }`.
- Dispatch to `src/commands/knowledge.rs` (mirrors `ingest.rs`).

### TUI (`zeph-tui`)
- Palette: `/knowledge ingest <source>`, `/knowledge status`, `/knowledge rollback <batch>`.
- Mandatory status spinner during ingest (`Ingesting knowledge: <uri>…`) per TUI rules.

### `--init` wizard
- `step_knowledge()` offering: enable graph episode sink (default off), choose `ingest_provider`.
  Emit `[knowledge]` only when the user opts into non-default values.

### `--migrate-config`
- Migration step adds `[knowledge]` with defaults; idempotent (`--in-place`).

### Database (`zeph-db`)
- One migration (sqlite + postgres) — see §3 Phase 0 + the ledger table below — parity-guarded.

### Live-testing playbook + coverage-status (MANDATORY before PR)
- Create `.local/testing/playbooks/knowledge-ingest.md` (scenarios in §10).
- Add `coverage-status.md` rows (status `Untested`) for: notes sink, graph sink, rollback, ledger,
  dry-run, sanitizer-on-write, provider resolution.

**INV-1 dependency watch:** adapters' `parse` / `into_documents` stay **pure** inside `zeph-memory`
(no `zeph-subagent` / `zeph-core` deps). The JSONL *reading* of transcripts and disk-walking happen in
the binary command (which already depends on those crates). This keeps `zeph-memory`'s dependency
direction clean (the project tracks INV-1 violations).

### Database schema

```sql
-- Provenance (Phase 0 prerequisite)
ALTER TABLE graph_edges    ADD COLUMN origin         TEXT NOT NULL DEFAULT 'conversation';
ALTER TABLE graph_edges    ADD COLUMN import_batch_id TEXT;       -- NULL for conversation edges
ALTER TABLE graph_edges    ADD COLUMN source_uri     TEXT;        -- e.g. "subagent:<task_id>"
ALTER TABLE graph_entities ADD COLUMN origin         TEXT NOT NULL DEFAULT 'conversation';
ALTER TABLE graph_entities ADD COLUMN import_batch_id TEXT;

-- Idempotency ledger (re-read/cost guard only — INV-5)
CREATE TABLE IF NOT EXISTS knowledge_ingest_ledger (
    source_uri      TEXT    NOT NULL,
    content_hash    TEXT    NOT NULL,          -- blake3 hex
    import_batch_id TEXT    NOT NULL,
    ingested_at     TEXT    NOT NULL DEFAULT (datetime('now')),
    entities        INTEGER NOT NULL DEFAULT 0,
    edges           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_uri, content_hash)
);
```
Migration number: next available in `crates/zeph-db/migrations/`. blake3 reuses the workspace dep.

---

## 9. Phases & Deferred Items

| Phase | Scope | Gate |
|---|---|---|
| **0** | Provenance columns + recall isolation flag + `rollback` | Hard prerequisite for any graph write |
| **1** | Static artifacts → semantic notes (existing pipeline) + `--dry-run` + ledger + CLI/TUI/config/migration | Ships first; lowest risk; no graph |
| **2** | Subagent transcripts → graph (gates on, sanitizer, technical-document prompt, provenance, dry-run) | **Gated by §7 measurement spike + kill-criterion** |
| **3 (deferred)** | External Claude Code / Codex transcript import | Requires Phase 0 + write-path PII redaction + version-pinned strict allowlist parser + current-project allowlist |

**Deferred markers (place verbatim at the named code sites):**

- `// TODO(critic D1): external-agent transcript ingest (Claude Code / Codex) deferred — requires graph provenance (Phase 0), write-path PII redaction (S4), and a version-pinned allowlist parser keyed on type==user/assistant only; current-project scope mandatory. Do not implement until all land.`
  → at the `IngestSourceKind::ExternalAgent` arm.
- `// TODO(critic D2): graph-backed ingest is gated on the §7 spike; if multi-hop benefit is unproven, route episodes to the notes pipeline and drop the graph sink.`
  → at `SemanticMemory::ingest_documents`.
- `// TODO: automatic/scheduled ingest (hook or scheduler tap) is out of MVP scope; this is an explicit operator command.`
  → at `src/commands/knowledge.rs`.

---

## 10. Success Criteria

```
GIVEN a fresh database and --source specs --dry-run
WHEN the command runs
THEN it reports files + projected chunks + estimated tokens
AND writes nothing to Qdrant, the graph, or the ledger

GIVEN --source specs (no dry-run, confirmed)
WHEN ingest completes
THEN current-project spec artifacts are present as semantic notes recallable by vector search
AND re-running the same command re-embeds nothing (ledger skip)

GIVEN Phase 0 has landed
WHEN --source subagents ingests transcripts
THEN every created edge/entity has origin='subagent' and a non-null import_batch_id
AND every candidate passed the write-quality gate, admission control, and the sanitizer

GIVEN a completed graph import with batch B
WHEN `zeph knowledge rollback B` runs
THEN all edges/entities tagged B are deleted
AND conversation-origin knowledge is untouched

GIVEN ingest provider = "fast" (valid named provider)
WHEN ingest runs
THEN extraction uses the "fast" provider; an unknown name warns and falls back without panic

GIVEN a source path outside the current project root
WHEN ingest is invoked
THEN it fails fast with a clear "outside project root" error and writes nothing

GIVEN the §7 spike fails the multi-hop / hub-degree / S-N thresholds
WHEN the team reviews results
THEN the graph sink is abandoned and subagent episodes are routed to the notes pipeline instead
```

Checklist:
- [ ] Phase 0 migration (sqlite + postgres parity) lands and backfills `origin='conversation'`
- [ ] `rollback` removes a batch cleanly, leaving conversation knowledge intact
- [ ] Phase 1 notes sink reuses `IngestionPipeline` (no parallel loader)
- [ ] `--dry-run` writes nothing for both sinks
- [ ] Ledger skips unchanged inputs (no LLM/embed call)
- [ ] Sanitizer runs on every graph-ingest document
- [ ] Write-quality gate + admission control run for every candidate edge (no RPE-bypass shortcut into raw writes)
- [ ] §7 spike executed and decision recorded in the playbook before the Phase 2 PR
- [ ] CLI + TUI + `[knowledge]` config + `--init` + `--migrate-config` integration points complete
- [ ] Playbook + coverage-status rows added

---

## 11. See Also

- [[004-memory/spec]] — memory subsystem, graph extraction, recall pipeline
- [[004-memory/004-9-memory-write-gate]] — write-quality gate that ingest MUST honor
- [[004-memory/004-6-graph-memory]] — MAGMA edges, SYNAPSE, A-MEM weights
- [[012-graph-memory/spec]] — entity graph, BFS recall, community detection
- [[018-index/spec]] — code RAG boundary (code never enters the graph)
- [[041-sanitizer/spec]] — PII / exfiltration validators for the write path
- [[056-autoskill-trace-extraction/spec]] — precedent: background extraction from session history + idempotency table
- [[001-system-invariants/spec]] — system contracts (INV-1 dependency direction, write-pollution)
- [[constitution]] — project-wide non-negotiable rules
```
