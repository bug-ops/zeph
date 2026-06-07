---
aliases:
  - Knowledge Ingest NFR
tags:
  - nfr
  - memory
  - quality
created: 2026-06-07
status: draft
spec_id: "067"
standard: "ISO/IEC 25010:2011"
---

# NFR: Knowledge Ingest (`zeph knowledge ingest`)

Standard: ISO/IEC 25010:2011 (product quality characteristics). Eight characteristic areas are
evaluated. Where a characteristic does not apply, the reason is stated.

---

## NFR-1 — Performance Efficiency

**NFR-1.1 — Bounded concurrency.**
Extraction concurrency must not exceed `[knowledge].concurrency` (default 3). The implementation
must use `futures::buffer_unordered(concurrency)` — unbounded `join_all` over LLM calls is
prohibited. This protects local Ollama single-threading and respects cloud rate limits.
Verification: code review confirming `buffer_unordered` usage; load test with `concurrency = 1`
showing sequential execution.

**NFR-1.2 — Ledger skip overhead.**
The `is_ingested(uri, hash)` ledger query must complete in < 5 ms per document on a SQLite
database with up to 10 000 ledger entries. Measurement: `criterion` benchmark against a
pre-seeded ledger.

**NFR-1.3 — No agent loop latency.**
Ingest is an operator command. It must not add any latency to a live agent turn. The ingest
path must not share a connection pool with the agent's hot database path when ingest is active.
Verification: integration test asserting agent response time is unaffected when `ingest` runs
concurrently in a separate process.

**NFR-1.4 — Dry-run token estimate accuracy.**
The `--dry-run` token estimate for the notes path must be within 20% of actual embedding token
consumption on a test corpus of 50 documents. Measurement: post-run comparison of estimated
vs. billed tokens.

---

## NFR-2 — Reliability

**NFR-2.1 — No batch abort on single failure.**
A parse error, LLM timeout, or DB write failure on one `IngestDocument` must not abort the
batch. The system must collect the error, mark the document failed in `IngestReport`, leave
it absent from the ledger (so re-run can retry it), and continue processing the remainder.
Verification: unit test injecting a failing LLM call on document N; documents N+1..M must
complete and appear in the ledger.

**NFR-2.2 — Rollback completeness.**
After `zeph knowledge rollback <batch>`, no `graph_edges` or `graph_entities` row with that
`import_batch_id` must remain. Conversation-origin rows must be unmodified. Verification:
integration test asserting row counts before/after rollback.

**NFR-2.3 — Interrupted-run resumability.**
If an ingest run is interrupted (SIGINT, process kill), already-committed documents appear
in the ledger with their `import_batch_id`. Re-running resumes the remainder. `rollback <batch>`
removes the partial batch cleanly. Verification: manual test interrupting a run mid-batch;
re-run processes only the remaining documents.

**NFR-2.4 — Idempotent re-runs.**
Running ingest twice on unchanged inputs must produce zero additional LLM calls, zero new
ledger rows, and zero new Qdrant or graph rows (the existing entity/edge dedup provides
defense-in-depth). Verification: unit test asserting LLM mock was called exactly N times
across two consecutive identical runs.

---

## NFR-3 — Security

**NFR-3.1 — Sanitizer enforcement.**
Every `IngestDocument` must pass the `zeph-sanitizer` `PostExtractValidator` before any entity
or fact string is persisted to the graph. A document rejected by the sanitizer must result in
zero entity/edge writes; the rejection must be logged at DEBUG and counted in `IngestReport`.
Verification: unit test with a synthetic PII pattern; assert entity count = 0 and dropped count = 1.

**NFR-3.2 — Project-root confinement.**
Source paths must be validated against the current project root before any file is read.
Paths outside the root must be rejected immediately with a clear error and zero reads.
Verification: unit test providing a path to `/tmp`; assert `MemoryError::Ingest` with the
"outside project root" message.

**NFR-3.3 — Write-gate enforcement.**
The write-quality gate (spec 004-9) and admission control (spec 004-3) must execute for every
candidate edge produced by graph extraction. Verification: unit test asserting the gate mock
was invoked once per candidate edge; a gate-rejected edge must not appear in the graph.

**NFR-3.4 — No privilege escalation.**
The ingest command must not require elevated privileges (sudo, admin) for any operation.
Verification: code review; run as non-root user in CI.

---

## NFR-4 — Maintainability

**NFR-4.1 — Adapter isolation.**
`IngestSourceAdapter::parse` implementations inside `zeph-memory` must be pure functions with
no I/O and no imports from `zeph-subagent`, `zeph-core`, or `zeph-agent-*` crates. Disk-walking
and JSONL reading happen exclusively in the binary command (`src/commands/knowledge.rs`).
Verification: `cargo check -p zeph-memory` must compile without `zeph-subagent` in its
dependency tree.

**NFR-4.2 — No parallel loader.**
Phase 1 must reuse the existing `TextLoader`/`TextSplitter`/`IngestionPipeline` verbatim — not
duplicate them. Verification: code review confirming the same pipeline types are used in both
`src/commands/ingest.rs` and `src/commands/knowledge.rs`.

**NFR-4.3 — Error consolidation.**
Ingest errors must be represented as a `MemoryError::Ingest` variant — no new top-level error
enum and no new crate for error types. Verification: `grep -r "enum.*IngestError" crates/` must
return zero hits outside `error.rs`.

**NFR-4.4 — Doc comment coverage.**
All public types, traits, and methods introduced by this feature must have `///` doc comments
explaining what and why. `SemanticMemory::ingest_documents` and `IngestLedger` must include
`# Examples` sections with `no_run` doc-tests.
Verification: `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-memory` passes with zero warnings.

---

## NFR-5 — Portability

**NFR-5.1 — SQLite and PostgreSQL migration parity.**
The Phase 0 provenance migration and the ledger table creation must be provided in both
`crates/zeph-db/migrations/sqlite/` and `crates/zeph-db/migrations/postgres/`, and must pass
`migration_parity.rs` validation. Verification: CI parity check.

**NFR-5.2 — Cross-platform compilation.**
The `zeph knowledge` command must compile without errors or warnings on Linux x86_64, macOS
aarch64, and Windows x86_64 (where Rust is supported). No platform-specific code outside
`#[cfg(target_os)]` guards may be introduced. Verification: CI matrix.

---

## NFR-6 — Usability

**NFR-6.1 — Error message quality.**
All error messages from the ingest path must name the specific problem (not "ingest error"),
suggest a corrective action where applicable, and never expose an internal Rust backtrace to
the operator. Verification: manual review of error paths listed in spec §6.

**NFR-6.2 — Dry-run discoverability.**
The `--help` output for `zeph knowledge ingest` must mention `--dry-run` and explain that it
writes nothing. Verification: `zeph knowledge ingest --help` output contains the string
`--dry-run`.

**NFR-6.3 — Progress visibility.**
During any write operation, a `Ingesting knowledge: <uri>...` status indicator must be visible
in both TUI and CLI modes. In CLI mode this is a progress line; in TUI mode it is a spinner
in the system status area. Verification: manual test.

---

## NFR-7 — Compatibility

**NFR-7.1 — Config forward compatibility.**
The `[knowledge]` section uses `#[serde(default)]` throughout. An existing config without the
section must load without error. Verification: unit test loading a config that omits `[knowledge]`.

**NFR-7.2 — Backward-compatible migration.**
The Phase 0 `ALTER TABLE` statements are additive (new nullable columns with defaults). Existing
rows remain valid; existing queries that do not reference the new columns are unaffected.
Verification: run existing graph integration tests against the migrated schema.

**NFR-7.3 — Non-exhaustive enum forward compatibility.**
`IngestSourceKind` must be marked `#[non_exhaustive]` so that adding Phase 3 sources
(`ExternalAgent`) does not break downstream match arms compiled against an older version.
Verification: code review.

---

## NFR-8 — Safety

**NFR-8.1 — No graph write without provenance.**
THE SYSTEM must panic in debug mode (assert) and return `MemoryError::Ingest` in release mode
if `ingest_documents` is called without a valid `ImportBatchId`. An untagged graph write would
be permanently unrollbackable. Verification: unit test asserting the error variant when
`batch_id` is empty.

**NFR-8.2 — No silent content drop.**
If the sanitizer rejects a fact, the rejection must be logged at DEBUG level and counted in
`IngestReport.dropped`. The operator must be informed of the drop count at completion.
Verification: unit test asserting `IngestReport.dropped > 0` when a PII pattern is present.

**NFR-8.3 — Ledger limitation documented.**
The `knowledge_ingest_ledger` must carry a SQL comment and a corresponding `///` doc comment
on `IngestLedger` stating explicitly that it is a re-read/cost guard only — it does not protect
against LLM extraction drift across model versions (INV-5). Verification: code review.
