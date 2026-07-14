# zeph-session Guide

Conversation-session persistence: the append-only `events.jsonl` event log, its SQLite/PostgreSQL
`acp_sessions`/`messages` projection, and the replay, condensation, and fork engines built on top.
Analogous in placement to `zeph-durable` (same journal-first design, message-level rather than
step-level).

- Start with crate-local checks: `cargo build -p zeph-session`, `cargo nextest run -p zeph-session`, `cargo clippy -p zeph-session --all-targets -- -D warnings`.
- Test both backends — mutually exclusive features: `cargo nextest run -p zeph-session --no-default-features --features sqlite` and `... --features postgres`.
- Read `specs/068-session-persistence/spec.md` before any change; honor its `## 13. Key Invariants` and `## 15. NEVER` sections.
- INV-SP-1 (log-first ordering): `events.jsonl` is flushed before the SQLite projection or `acp_sessions.last_seq` is updated — the projection never leads the log.
- INV-SP-2/INV-D2 (single writer, torn-tail truncation): only a session's `SessionActor` task (or the single active agent process) may write `events.jsonl`; any other writer breaks the torn-append-truncation guarantee. Session actors are spawned via `TaskSupervisor::spawn`, never raw `tokio::spawn`.
- INV-SP-3 (reconcile-from-log on open): the event log is always authoritative — SQLite is a derivable projection rebuilt forward from the log when `last_seq` lags.
- INV-SP-4 (condensation non-overlap): a `Condensation`/`Compaction` event's `replaced_seq_range` must never overlap a prior one in the same session log.
- Never make `zeph-session` depend on `zeph-durable`, or vice versa — the two journals are independent and reference each other only by opaque IDs.
- Hydration/resume paths must go through the bounded `ReplayEngine::replay`, not `ReplayEngine::fold`, to preserve the memory bound established for session-resume (#5861, #5844).
- Fork's blob copy (`ForkEngine::fork`) hard-links content-addressed blobs with fallback to `fs::copy` — but only when the destination genuinely does not exist; treat `ErrorKind::AlreadyExists` as a no-op, since copying onto an existing hard-link silently truncates the shared inode for every session pointing at it (#6157). Validate `image_refs` hashes as bare hex before using them in a path to prevent traversal (#6152).
- If external behavior changes, update `crates/zeph-session/README.md` and `book/src/advanced/session-persistence.md`.
