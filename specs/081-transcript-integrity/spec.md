---
aliases:
  - Transcript Integrity
  - Tamper-Evident Persisted History
  - Persisted Conversation Tamper Detection
tags:
  - sdd
  - spec
  - security
  - durable
  - memory
created: 2026-07-17
status: implemented
related:
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[010-security/spec]]"
  - "[[010-4-audit]]"
  - "[[064-durable-execution/spec]]"
---

> [!note] Broken cross-references — 2026-07 audit
> This document previously linked two nonexistent spec slugs, `039-durable-agent-turns-subagent-
> adapters-unwired/spec` and `056-vault-key-hardening-rotation/spec`. Neither exists under
> `/specs/` (039 is `039-background-task-supervisor`; 056 is `056-autoskill-trace-extraction`) —
> these were likely working titles for gaps that were folded into [[064-durable-execution/spec]]
> (which covers both durable-execution wiring gaps and, in its "Key Rotation Windows" section,
> vault-key rotation). The `[NEEDS CLARIFICATION]` items below that cited them now point at
> [[064-durable-execution/spec]] instead of a confirmed-nonexistent rename.

# Feature: Tamper-Evident Persisted Transcripts, Session Event Logs, and Durable Journal Entries

> [!info] Metadata
> **Author**: rust-agents:sdd (CI-1396, research finding)
> **Branch**: `feat/issue-6360/transcript-integrity`
> **Priority**: P3
> **Origin**: CI-1396 continuous-improvement cycle, competitive-parity finding against Claude Code 2.1.205 (July 2026)
> **Status note**: promoted from `.local/specs/069-transcript-integrity/` to this permanent
> location and renumbered from 069 (which collided with the already-registered
> `069-threat-model`) to 081, the next free number at promotion time. All `[NEEDS
> CLARIFICATION]` items in §9 were resolved during a 3-round architect/critic design review
> (`.local/handoff/2026-07-18T04-{24,37,41,49,50,58}*.md` in the implementing PR's worktree) and
> subsequently implemented — see the **Implementation Notes** section appended at the end of this
> document for the resolution of each and the as-shipped scope.

## 1. Overview

### Problem Statement

Claude Code 2.1.205 added an "auto mode rule blocking tampering with session transcript files to preserve conversation integrity," explicitly to close off fabricated in-transcript approvals as a prompt-injection lane — an attacker who can write to a persisted transcript file could inject a fake prior turn (e.g. "user approved this destructive action") that gets replayed back into context and trusted as genuine history.

Zeph has mature **live** injection defenses for content arriving mid-turn via tools, MCP servers, or web output ([[010-2-injection-defense]]: `IpiFilter::filter` in `crates/zeph-sanitizer/src/ipi_filter.rs`, `TurnCausalAnalyzer` in `crates/zeph-sanitizer/src/causal_ipi.rs`, `ShadowSentinel`'s LLM-isolation invariant in `crates/zeph-core/src/agent/shadow_sentinel.rs`). Those defenses share one property: the untrusted content is inspected **as it enters** the current turn. This spec is about a structurally different threat model — **at-rest tampering** of content that has already been written to disk and is later **read back and replayed as legitimate prior context**, with no live inspection step in between. A live IPI filter never re-scans a file it already trusted three turns ago; if that file is edited on disk after the filter ran, nothing notices.

Source verification for this finding (grep across `crates/zeph-subagent/`, `crates/zeph-session/`, `crates/zeph-memory/`, `crates/zeph-durable/`) found:

- **`crates/zeph-subagent/src/transcript.rs`** (`TranscriptWriter::append`, `TranscriptReader::load`/`load_strict`) — the `<task_id>.jsonl` sub-agent transcript files have no hash, signature, or chain-integrity field. `load_strict` validates JSON well-formedness per line and rejects the first malformed line, but a well-formed line with **substituted content** (e.g. a forged `assistant` turn claiming a destructive action was approved) passes silently.
- **`crates/zeph-session/src/log.rs`** (`SessionEventLog`, spec-068) — this, not `zeph-memory/src/store/history.rs` (which only holds CLI input-line text, not conversation content), is the actual "source of truth for conversation content" per its own module doc. It is the `events.jsonl` file read back by `zeph sessions resume` (`src/cli.rs` `SessionsCommand::Resume`, "replaying its event log to reconstruct history"). It implements INV-SP-2, a **torn-append** check: a garbled/incomplete trailing line is detected and dropped (or physically repaired by `open_exclusive`) because it can only occur from a crash mid-write. This is a **crash-consistency** guarantee, not a tamper-evidence guarantee — a complete, well-formed, but maliciously edited line anywhere in the file (not just the tail) is accepted without complaint.
- **`crates/zeph-durable/`** — partially addresses this class already, and is the project's own reference pattern to extend rather than a gap to fill from scratch:
  - `cipher.rs`'s `PayloadCipher` AEAD-seals `EntryKind::StepResult` payloads with `PayloadAad` binding `(execution_id, step_id, entry_kind, idem_key)`, so a sealed blob cannot be relocated to another step/execution (`DurableError::ReplayIntegrity` on failure).
  - `backend/local.rs`'s `compute_control_hmac`/`verify_control_hmac` stamp a **keyed BLAKE3 row HMAC** over `EntryKind::EffectIntent` control-entry identity when an HMAC key is configured via `with_hmac_key` (issue #6043), failing closed with `DurableError::ControlIntegrity` on a forged or relocated row.
  - **Gaps that remain even here**: (a) the row HMAC is **opt-in** ("when an HMAC key is configured") — no evidence of a default-on wiring; (b) both mechanisms bind an entry's own identity/position, not the **previous** entry's hash — there is no hash *chain*, so **wholesale deletion of trailing committed entries** (as opposed to a single forged/relocated row) is not distinguishable from a torn/incomplete write, which the crash-resume path is designed to tolerate; (c) neither mechanism extends to `zeph-subagent` or `zeph-session`.

**Why unattended durable replay raises the stakes**: `zeph durable resume <id>` (`src/cli.rs` `DurableCommand::Resume`) and the automatic crash-orphan sweep replay a journaled execution's control flow with no human watching. Claude Code's transcript-tampering scenario still has a human in the loop who is looking at the resumed session and could, in principle, notice something off. Zeph's durable resume path has no such backstop — a tampered `EffectIntent`/`StepResult` that passes whatever integrity check exists (or bypasses it because none is configured) is replayed and acted on autonomously. `zeph sessions resume` sits in between: a human triggers it and sees the replayed conversation, but is not expected to manually diff raw JSONL before trusting it.

### Goal

Every read path that treats a persisted transcript, session event log, or durable journal entry as legitimate prior context (`TranscriptReader::load`/`load_strict`, `SessionEventLog::read_all`/replay, `zeph-durable`'s `ReplayCursor`) can detect — and, per FR-004, refuse to silently trust — an entry that was modified outside its own normal append path, using the project's existing keyed-BLAKE3 pattern rather than a new cryptographic primitive.

### Out of Scope

- Confidentiality of transcript/session/journal contents at rest (already covered by `0o600` file permissions on `zeph-subagent` transcripts, `zeph-durable`'s `PayloadCipher` for durable payloads, and the age vault for secrets — this spec is about **integrity**, not encryption of new surfaces).
- Live, mid-turn prompt-injection defense (`IpiFilter`, `TurnCausalAnalyzer`, `ShadowSentinel`) — already covered by [[010-2-injection-defense]]; out of scope here by definition (see Problem Statement).
- Filesystem-level tamper prevention (immutable file flags, OS-level file integrity monitoring, `chattr +i`) — this spec is about **detection at the application read path**, not prevention at the OS layer.
- `zeph-memory/src/store/history.rs` (CLI input-line history) — verified during research to hold only free-text CLI input lines, not conversation/transcript content; not a meaningful replay-trust surface and excluded from scope.
- Redesigning `zeph-durable`'s existing AEAD/row-HMAC mechanism — this spec extends/generalizes it (see FR-005), it does not replace it.

## 2. User Stories

### US-001: Subagent transcript tamper detection
AS A Zeph operator running unattended sub-agent workflows
I WANT a sub-agent's persisted `<task_id>.jsonl` transcript to be verifiable against tampering before it is loaded back as conversation history
SO THAT a compromised tool, malicious MCP server, or filesystem-level actor cannot inject a fabricated "approved" turn that gets replayed as legitimate prior context.

**Acceptance criteria:**
```
GIVEN a completed sub-agent transcript written by TranscriptWriter::append
WHEN a byte inside any already-written JSONL line is modified on disk after the writer closed
THEN TranscriptReader::load_strict (or its FR-002 successor) SHALL detect the modification and refuse to return the tampered entry as valid history
```

### US-002: Session resume tamper detection
AS A user resuming a prior conversation via `zeph sessions resume <id>`
I WANT the replayed `events.jsonl` event log to be verified against tampering, distinct from the existing crash-consistency (torn-tail) check
SO THAT `zeph sessions resume` does not silently reconstruct a conversation containing a forged turn.

**Acceptance criteria:**
```
GIVEN a session's events.jsonl with N complete, well-formed lines
WHEN line k (k < N, not the trailing line) is edited to a different but still well-formed JSON payload
THEN SessionEventLog's read/replay path SHALL distinguish this from a torn-tail (INV-SP-2) condition and SHALL report a tamper-integrity failure rather than silently accepting line k
```

### US-003: Unattended durable-resume fail-closed behavior
AS A Zeph operator relying on automatic durable crash-resume with no human present
I WANT a detected integrity failure on a journaled entry to abort replay of that execution rather than proceed
SO THAT an unattended process never autonomously acts on a tampered control-flow or step-result entry.

**Acceptance criteria:**
```
GIVEN a durable execution journal with the opt-in row HMAC or AEAD seal enabled
WHEN ReplayCursor encounters an entry that fails the FR-001 chain check even though its own row HMAC/AEAD tag authenticates in isolation (i.e. a valid entry was deleted or reordered, not forged in place)
THEN the execution SHALL be aborted with a distinct error (extending DurableError::ReplayIntegrity/ControlIntegrity) rather than silently treated as Fresh/re-run
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN an entry is appended to a `TranscriptWriter`-owned JSONL file, a `SessionEventLog`, or (for entry kinds not already covered by `PayloadCipher`/`compute_control_hmac`) a `zeph-durable` journal, THE SYSTEM SHALL compute a keyed hash over the entry's content **and** the previous entry's hash (hash-chain), reusing the project's existing keyed-BLAKE3 primitive (`crates/zeph-durable/src/backend/local.rs`'s `compute_control_hmac` pattern) rather than introducing a new cryptographic dependency | must |
| FR-002 | WHEN `TranscriptReader::load`/`load_strict`, `SessionEventLog::read_all`/`read_chunked`, or `ReplayCursor`'s segment reads load persisted entries, THE SYSTEM SHALL recompute and verify the hash chain before the entries are treated as trusted prior context (i.e. before injection into the LLM message array or replay execution) | must |
| FR-003 | WHEN chain verification fails on one or more entries, THE SYSTEM SHALL fail closed for the affected scope: reject the affected entries (or the whole file/execution — see Open Questions on granularity) rather than silently proceeding with unverified data, and SHALL emit a distinct, structured log/error event separate from ordinary malformed-JSON errors (`TranscriptReader::load_strict`'s existing `SubAgentError` variant, a new `SessionError` variant, and `DurableError::ReplayIntegrity`/`ControlIntegrity` respectively) | must |
| FR-004 | WHEN the affected replay path is unattended (`zeph durable resume`, the durable crash-orphan sweep, or automatic sub-agent transcript reload on collection), THE SYSTEM SHALL abort the operation on a detected integrity failure rather than warn-and-proceed; interactive paths (`zeph sessions resume`, `zeph durable inspect --reveal`) MAY surface the failure to the human for an explicit decision instead of hard-aborting — exact interactive UX left to [NEEDS CLARIFICATION: should `zeph sessions resume` hard-fail like the durable path, or warn-and-let-the-user-decide? The finding's own reference feature (Claude Code) hard-blocks; Zeph's constitution has no existing "ask the human on integrity failure" precedent, but does have exactly this warn-vs-fail-closed choice documented as an open question in [[064-durable-execution/spec]] for a related durable-wiring gap] | must |
| FR-005 | THE hash-chain key SHALL be resolved from the age vault per CLAUDE.md's Secrets & Vault policy (no `ZEPH_*` environment-variable key material), consistent with how `zeph-durable`'s `PayloadCipher` is already keyed from the vault outside `zeph-durable` itself (INV-1: the crate stays a pure abstraction with no cryptographic dependency) | must |
| FR-006 | WHEN a pre-existing transcript/session-log/journal file created before this feature's rollout is read, THE SYSTEM SHALL treat it as unverifiable-legacy rather than tampered, per the migration posture resolved in [NEEDS CLARIFICATION: is legacy-unverifiable content (a) auto-trusted once with a one-time warning, (b) permanently flagged UNVERIFIED but still usable, or (c) required to be re-chained via a one-time backfill pass before first read? Affects every existing `.local/testing/` and production transcript/session on upgrade] | must |
| FR-007 | `TranscriptReader::load`'s existing lenient mode (warn-and-skip on a malformed line, distinct from `load_strict`) SHALL define its interaction with a broken hash chain: [NEEDS CLARIFICATION: does a chain break in lenient mode also warn-and-skip the affected and all subsequent entries (since the chain is now unverifiable from that point forward), or does lenient mode apply only to JSON-structural malformation and always defer to `load_strict`-equivalent strictness for chain breaks specifically?] | should |
| FR-008 | WHEN a legitimately relocated/copied session or transcript directory (e.g. migrated between machines, per [[064-durable-execution/spec#Key Rotation Windows (#6447, #6451, #6460)]]'s key-rotation scenario) is opened under a different vault identity than the one that wrote it, THE SYSTEM SHALL distinguish "chain valid, key mismatch" from "chain broken" in its error reporting, so operators are not misled into believing content was tampered with when it was only re-keyed | should |
| FR-009 | THE `zeph-durable` extension of this mechanism SHALL default the row-HMAC/chain check to **on** rather than requiring explicit opt-in via `with_hmac_key`, closing the current default-off gap noted in the Problem Statement — subject to [NEEDS CLARIFICATION: is flipping this default a breaking change requiring a migration step (per CLAUDE.md's config-migration integration-point requirement), and does it apply uniformly to single-process local deployments where the "forged/relocated row from a shared-database deployment" threat model in `local.rs`'s own doc comment is less applicable?] | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Chain computation on append SHALL add no measurable latency to the agent turn loop or the durable step-execution hot path — per CLAUDE.md's non-blocking contract, keyed BLAKE3 hashing (already used per-row in `zeph-durable`'s `compute_control_hmac` and for bearer-token comparison per the constitution) is the reference cost budget: target < 1ms added latency per append, measured via a new `tracing::info_span!` (`subagent.transcript.chain_append`, `session.log.chain_append`, `durable.journal.chain_verify`) per the Instrumentation requirement in `.claude/rules/continuous-improvement.md` |
| NFR-002 | Performance | Chain verification on `zeph-durable` replay SHALL preserve `ReplayCursor`'s existing `O(segment)` bounded-memory guarantee (NFR-DE-02) — verification must be computable incrementally per segment (carrying forward only the last verified hash as state) rather than requiring the full journal resident in memory |
| NFR-003 | Reliability | Chain verification SHALL NOT produce false-positive tamper failures on the legitimate torn-tail crash-recovery case already handled by `SessionEventLog`'s INV-SP-2 — a genuinely incomplete/garbled trailing line (crash mid-`fsync`) must remain distinguishable from a complete-but-substituted line anywhere else in the file; conflating the two would turn ordinary crash recovery into a hard failure |
| NFR-004 | Security | Fail-closed is the default posture for any ambiguous verification outcome (see FR-003, FR-004) — an integrity check that cannot be evaluated (e.g. missing key, corrupted chain-metadata sidecar) SHALL be treated as a failure, never silently skipped |
| NFR-005 | Observability | Every detected integrity failure SHALL be logged as a security-relevant event consistent with [[010-4-audit]]'s existing audit-trail invariants (tool invocations, authorization failures, IPI detections) — this is a new category of audit-log event, not folded silently into ordinary error logs |
| NFR-006 | Compatibility | The on-disk format addition (hash-chain field per JSONL line, or a companion sidecar) SHALL NOT break `TranscriptReader::load`'s existing lenient-mode consumers or `SessionEventLog`'s `read_chunked` bounded-buffer contract (≤ 100 lines per chunk per its existing doc comment) |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Chained transcript entry | An existing `TranscriptEntry` (JSONL line in `<task_id>.jsonl`) extended with chain metadata | `seq`, `timestamp`, `message` (existing) + a new keyed-hash field binding this entry's content and the previous entry's hash |
| Chained session event | An existing `SessionEventEnvelope` (JSONL line in `events.jsonl`) extended analogously | existing envelope fields + chain hash field |
| Journal chain state | Per-execution running "last verified hash" carried by `ReplayCursor` across segment reads | `execution_id`, last-verified chain hash, last-verified `JournalSeq` |
| Integrity failure event | A new structured audit-log entry emitted on FR-003 | affected file/execution id, entry seq/position, failure kind (forged-content vs. deleted-entry vs. key-mismatch), timestamp |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Trailing line is garbled (crash mid-write) vs. trailing line is a deliberately truncated valid entry | Both look like "content missing from the tail." NFR-003 requires these be distinguishable — garbled bytes fail JSON parsing (existing INV-SP-2 path), while a cleanly truncated valid entry leaves a "chain expects N entries, file has N-1" signature that FR-001/FR-002 must catch as a distinct failure kind, not conflated with crash recovery |
| A well-formed line's content is edited in place (not the tail) | Detected: recomputed hash for that entry no longer matches the stored hash, and every subsequent entry's chain hash (computed from the edited entry onward) also fails to verify |
| Two adjacent entries are swapped (reordering, not content edit) | Detected: each entry's stored "previous hash" no longer matches its actual predecessor's hash after the swap |
| File is legitimately migrated to a different machine/vault identity | See FR-008 — must report "key mismatch," not "tampered," to avoid false alarms during legitimate operational migration |
| Pre-feature-rollout file with no chain metadata at all | See FR-006 — legacy/unverifiable handling, not an automatic tamper verdict |
| `TranscriptReader::load` (lenient mode) encounters a chain break mid-file | See FR-007 — open question on lenient-mode semantics for chain breaks specifically |
| Durable journal entry kind not yet covered by `PayloadCipher`/`compute_control_hmac` (e.g. promise/timer entries per `local.rs`'s documented Scope section, which currently fail closed with `UnsupportedEntryKind` rather than being journaled at all) | Out of scope for this spec's FR-001 until those entry kinds are actually journaled — tracked as a dependency, not duplicated here |
| Concurrent writer holds `SessionEventLog::open_exclusive`'s `flock` while a read-only tool (`zeph sessions show --events`) reads the same file mid-append | Chain verification must apply the same INV-SP-2-aware tail tolerance as the existing read path — the last line may legitimately be in-flight, not tampered |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Deterministic tamper detection | 100% of single-byte-mutation trials against a stored JSONL entry (across `transcript.rs`, `zeph-session` log, and durable journal fixtures) are detected before the mutated entry is returned/replayed |
| SC-002 | Crash-consistency non-regression | 0 false-positive tamper failures across the existing torn-tail test fixtures (`SessionEventLog` INV-SP-2 tests, `TranscriptReader::load_strict` malformed-line tests) after this feature lands |
| SC-003 | Hot-path latency | < 1ms added p99 latency per append call, measured via the NFR-001 trace spans, with zero regression to existing `zeph-durable` step-journaling benchmarks |
| SC-004 | Coverage of unattended replay | `zeph durable resume` and the crash-orphan sweep abort (not warn) on 100% of injected-tamper fixtures, per FR-004 |

## 8. Agent Boundaries

### Always (without asking)
- Reuse the existing keyed-BLAKE3 pattern from `crates/zeph-durable/src/backend/local.rs` rather than adding a new hashing/crypto crate (constitution VII, Simplicity)
- Add the required `tracing::info_span!` instrumentation per `.claude/rules/continuous-improvement.md`'s Instrumentation requirement
- Resolve chain keys via the age vault, never a `ZEPH_*` environment variable (CLAUDE.md Secrets & Vault)

### Ask First
- Flipping `zeph-durable`'s row-HMAC default from opt-in to on-by-default (FR-009) — a behavior/compatibility change requiring an explicit config-migration step
- The fail-closed vs. warn-and-let-human-decide choice for `zeph sessions resume` (FR-004's open question)
- The on-disk format change to `TranscriptEntry`/`SessionEventEnvelope` (new field vs. companion sidecar file) — affects every consumer of these JSONL formats, including external tooling that may already parse them

### Never
- Silently downgrade a detected integrity failure to a warning on the unattended durable-resume path (FR-004)
- Introduce a new cryptographic primitive when the project's existing keyed-BLAKE3 pattern already solves the same problem class elsewhere (`compute_control_hmac`, bearer-token comparison)
- Store the chain-verification key in plaintext outside the vault, or accept it via environment variable

## 9. Open Questions

- [NEEDS CLARIFICATION: FR-004] Should `zeph sessions resume` hard-fail on a detected chain break like the durable path, or surface it to the human and let them opt to proceed anyway?
- [NEEDS CLARIFICATION: FR-006] Migration posture for pre-existing (pre-feature) transcript/session/journal files: auto-trust-once-with-warning, permanent UNVERIFIED flag, or mandatory one-time backfill re-chaining pass?
- [NEEDS CLARIFICATION: FR-007] Does `TranscriptReader::load`'s lenient mode treat a chain break the same as a malformed-JSON line (warn-and-skip), or always escalate to strict-mode failure for chain breaks specifically, since a chain break — unlike a single bad line — invalidates trust in everything downstream of it?
- [NEEDS CLARIFICATION: FR-009] Is defaulting `zeph-durable`'s row-HMAC to on-by-default a breaking change requiring `--migrate-config`, and should it even apply to single-process local deployments where `local.rs`'s own doc comment frames the row HMAC as a "shared-database deployment" concern?
- [NEEDS CLARIFICATION: FR-003 granularity] On a detected integrity failure, does the system reject only the specific tampered entry (and everything chained after it) while still trusting the untouched prefix, or does it reject the entire file/execution as untrusted? The former preserves more legitimate history; the latter is simpler to reason about and matches durable's existing whole-execution abort semantics
- [NEEDS CLARIFICATION] Should this mechanism extend to `zeph-memory`'s SQLite-backed `messages`/`agent_sessions` projection tables (the downstream read model that `SessionEventLog` reconciles into per INV-SP-1's "log-first ordering"), or is verifying the JSONL source-of-truth log sufficient since the projection is always rebuildable from a verified log? Current lean is the latter (verify the source of truth, not every derived projection), but this is not yet confirmed with the `zeph-session`/`zeph-memory` maintainers
- [NEEDS CLARIFICATION] Threat model boundary: does this spec need to defend against an attacker with the same vault access as the legitimate process (in which case a vault-derived key does not raise the bar, since the attacker could recompute a valid chain), or only against an attacker with filesystem write access but no vault access? This determines whether FR-005's vault-keyed approach is sufficient or whether an external anchor (e.g. append-only remote log, TPM-backed signing) is needed for the strongest threat model — likely out of scope for a P3 first iteration but should be stated explicitly in the plan phase

## 10. See Also

- [[constitution]] — project principles; Security (V) and Simplicity (VII) sections directly bound this spec's approach (reuse existing BLAKE3 pattern, vault-only key storage)
- [[001-system-invariants/spec]] — cross-cutting invariants; this spec proposes new invariants for the persisted-history read path
- [[010-security/spec]] and [[010-2-injection-defense]] — the live IPI-defense counterpart to this spec's at-rest defense; both close different lanes of the same injected-fake-history attack class
- [[010-4-audit]] — audit-trail precedent this spec's integrity-failure events (NFR-005) extend
- [[064-durable-execution/spec]] — related durable-journal wiring gap; documents a similar warn-vs-fail-closed open question referenced in FR-004
- [[064-durable-execution/spec#Key Rotation Windows (#6447, #6451, #6460)]] — vault key rotation/migration semantics this spec's FR-008 depends on
- [[MOC-specs]] — all specifications
- **External reference**: Claude Code 2.1.205 (July 2026) — "auto mode rule blocking tampering with session transcript files to preserve conversation integrity," the competitive-parity trigger for this finding; closes fabricated in-transcript approvals as an injection lane in a single-session, human-observed context, which this spec generalizes to Zeph's multi-surface (sub-agent transcript, session event log, durable journal) and partly-unattended (durable crash-resume) replay model

## 11. Implementation Notes (added at spec promotion, issue #6360)

Resolution of every §9 `[NEEDS CLARIFICATION]` item, as actually decided across a 3-round
architect/critic design review and implemented in issue #6360's PR. Full design rationale lives
in that PR's `.local/handoff/2026-07-18T04-*.md` chain; this section records only the resolved
decisions for future readers of the permanent spec.

- **FR-004 (interactive UX)**: `zeph sessions resume <id>` fails closed by default on a detected
  chain break, same as the unattended durable path. A deliberate, logged operator override
  (`--allow-unverified`) exists but — as shipped — only for the `--print` (one-shot raw-dump)
  path; it is rejected by the CLI (`#[arg(requires = "print")]`) if passed without `--print`,
  rather than silently accepted and ignored on the interactive live-agent resume path. Full
  interactive wiring is tracked as follow-up in issue #6449.
- **FR-006 (migration posture)**: auto-trust-once-with-warning, no backfill. A file with no chain
  metadata anywhere is legacy and trusted as-is; new appends from that point forward are chained.
  A file with *some* chain metadata but not on every post-chain-start line is a partial strip,
  treated as tamper, never legacy — this closes a downgrade lever an unconditional
  "any missing chain field is legacy" rule would have left open.
- **FR-007 (lenient-mode interaction)**: chain breaks always escalate to a hard failure in both
  `TranscriptReader::load` (lenient) and `load_strict` — never warn-and-skip. Lenient mode's
  warn-and-skip behavior is reserved for ordinary JSON-syntax malformation unrelated to chain
  verification.
- **FR-008 (rekey vs. tamper distinction)**: implemented via a `key_epoch` window (current +
  previous) on both the JSONL chain (`ChainKeyRing`) and the durable HWM
  (`with_previous_hwm_key`). A chain/HWM that resolves under a known prior epoch is reported as
  "re-keyed," not tampered; a chain/HWM carrying an epoch outside the known window fails closed
  (`ChainError::Unverifiable` / `DurableError::HighWaterMarkIntegrity`) rather than degrading to
  trusted-legacy — a chained/HWM-bearing file with an unresolvable epoch is never treated as
  pre-feature legacy, closing an epoch-based downgrade lever.
- **FR-009 (durable default)**: resolved differently than the spec's original framing assumed. A
  positional hash-chain for the durable journal was found to be fundamentally incompatible with
  `checkpoint_fold`'s routine compaction (it physically deletes committed rows), so durable does
  **not** use a chain at all. Instead: the existing row-HMAC's shared-DB-gated opt-in stays
  as-is for its own threat model, and a new, separate authenticated per-execution high-water-mark
  (HWM) activates unconditionally whenever `ZEPH_DURABLE_KEY` is provisioned — including
  single-user local deployments, which previously had no deletion-detection at all. This directly
  closes FR-009's default-off gap via a different mechanism than originally proposed.
- **FR-003 granularity**: prefix-trust + truncate-at-break for the JSONL adapters (an untampered
  prefix stays trusted; the tampered entry and everything chained after it does not); whole-
  execution abort for durable (matches its existing all-or-nothing replay semantics).
- **SQLite projection scope**: confirmed out of scope. Only the JSONL/durable source-of-truth
  logs are chain/HWM-verified; the `messages`/`agent_sessions` SQLite projections remain
  rebuildable caches, not independently verified.
- **Threat model boundary**: the shipped mechanism defends against an attacker with filesystem
  write access but *not* vault access. Within that model, in-place edits, reordering, partial
  chain-strips, and key-epoch tampering are all detected and fail closed. A **fully-consistent
  whole-file/whole-execution strip** (delete every chain field, or delete the durable
  `durable_execution_integrity` row entirely) was **not** resisted in this initial
  implementation — it was indistinguishable from genuine pre-feature legacy content without an
  external anchor outside filesystem-write reach. **Closed by issue #6449** (below).

### 11.1 Vault-anchor downgrade-resistance (issue #6449, closes the §11 whole-file/whole-row gap)

- **JSONL side**: a per-file **vault anchor** (`zeph_common::anchor::Anchor`, `{version, epoch,
  count, head, written_at}`) is written on finalize/close (`TranscriptWriter::finalize`,
  `SessionEventLog::finalize`) and checked on read. An age vault entry can only be removed by an
  attacker holding the age private key, so "legacy-looking file, but a live anchor for its
  identity" is an unambiguous whole-strip signature. An **absent** anchor is never a tamper
  signature — it cannot be attacker-induced without the age key, so it is trusted exactly like
  pre-#6449 behavior (this is what avoids bricking every session/transcript created before this
  feature). Session anchors are a prefix commitment as of the last clean close (documented
  residual: an attacker can roll back at most one run's worth of unanchored tail appends);
  transcripts have no such residual (finalize-once).
- **Durable side**: `zeph durable seal-integrity` writes a vault-presence marker
  (`ZEPH_DURABLE_INTEGRITY_SEALED`) after confirming no resumable (`status='running'`) execution
  has committed `StepResult`s without an integrity row (drain-before-seal). Once sealed, an
  absent row on a keyed, non-grandfathered execution with ≥1 committed result is unconditional
  tamper — no DB column sits on this boundary, closing the `created_at`-column defeat an earlier
  design iteration of this fix had (a DB-write attacker could otherwise forge the column the
  cutover compared against). `--grandfather <id,...>` records a vault-stored, permanent
  per-execution opt-out for operators who cannot drain a legacy execution.
- **Growth bound**: session anchors are never deleted on `sessions delete` (the event log itself
  survives that command), so a reconcile-and-cap sweep (`zeph-core::anchor_store`, startup +
  hourly) reaps orphaned anchors and evicts the oldest session anchors past
  `[integrity] max_session_anchors` (default 512) — ordered by the anchor-embedded `written_at`
  field, never filesystem mtime (attacker-writable). Eviction degrades a session to chain-only
  protection; it never bricks (an evicted session still opens per the "absent anchor" rule
  above).
- **Residuals, accepted and documented**: (1) session prefix-rollback (bounded, at most one
  run's unanchored tail); (2) a grandfathered `execution_id` is a *permanent* forge-able slot,
  not a frozen snapshot — each is an explicit, bounded operator opt-out; (3) sessions aged out
  past `max_session_anchors` fall back to chain-only (§11) protection; (4) **narrowed by issue
  #6462**: the reconcile sweep's orphan-reap step no longer treats a single sweep's file/
  session-directory absence as sufficient grounds to remove an anchor. `Anchor` carries a new
  `orphaned_since: Option<u64>` (wall-clock ms), stamped on the first sweep that observes the
  file absent and cleared (self-heal) if the file reappears; only once the file has stayed
  absent for a 24h grace window does a later sweep hard-delete the anchor. An attacker who
  deletes the real file, recreates a forged legacy-looking replacement under the same identity
  *within* the grace window still gets caught (the real anchor — count/head/written_at — is
  untouched during the window, so full checkpoint verification still applies); only sustaining
  the deletion for the full grace window reopens the residual (still overlapping, via FR-006's
  no-backfill posture, the accepted "fabricate a brand-new legacy session" residual — requires
  the same destructive file-write precursor the threat model already grants). The grace raises
  the attacker's cost from "wait out one sweep (≤ 1h, or a restart)" to "sustain deletion for
  24h+", not a fully closed gap.

### 11.2 HWM key-rotation window fix (issue #6460)

§11's FR-009 resolution states the HWM supports "a key-rotation window (`with_previous_hwm_key`)
that distinguishes 'possibly re-keyed' from 'TAMPER'." As shipped in #6453, that window was
unreachable: `key_epoch` was a standalone constant (`HWM_KEY_EPOCH = 0`) never bumped anywhere
in-tree, and the scheduler-daemon read path never attached an HWM key at all. Any
`zeph durable rotate-key` (see `[[064-durable-execution/spec]]` Key Rotation Windows) followed by
a restart force-aborted every execution with a committed `StepResult` on its next resume, and —
because both pre- and post-rotation rows carried the same frozen epoch — misreported the failure
as `hmac_mismatch` (TAMPER) rather than the correct `key_epoch_unresolvable` (possibly re-keyed).

**Fix:** the HWM epoch no longer has its own counter. It reuses the AEAD payload cipher's
`key_id`/`previous_key_id` lifecycle directly — `load_write_hwm_key` resolves a current + previous
`HwmSlot` pair from `config.durable.key_id`/`previous_key_id` (mirroring the control-HMAC's
`ControlHmacKeys`), failing closed if `previous_key_id` is declared but
`ZEPH_DURABLE_KEY_PREVIOUS` is missing. Every write/read channel that attaches an HWM key (P1
agent-turn, P2 orchestration, the scheduler daemon — previously unattached — and the CLI read
path) now also attaches the previous slot while a window is open. `key_id` already defaults to
`0`, matching the pre-fix `key_epoch = 0` rows, so no migration is needed for an un-rotated
deployment. `zeph durable rotate-key --drop-previous` gained a third safety scan
(`count_integrity_rows_under_epoch`) alongside the pre-existing AEAD blob-scan and the #6451
control-HMAC scan — see `[[064-durable-execution/spec]]` for the full three-scan mechanics.
