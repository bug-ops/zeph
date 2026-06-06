# zeph-durable Guide

Layer-0 durable execution: journals control flow (steps, inputs/outputs, promises, timers) so a crashed or interrupted execution resumes at the point of failure instead of restarting. Analogous in placement to `zeph-db` and `zeph-common`.

- Start with crate-local checks: `cargo build -p zeph-durable`, `cargo nextest run -p zeph-durable`, `cargo clippy -p zeph-durable --all-targets -- -D warnings`.
- Test both backends — they are mutually exclusive: `cargo nextest run -p zeph-durable --no-default-features --features sqlite` and `... --features postgres`.
- Read `specs/064-durable-execution/spec.md` before any change; honor its `## Key Invariants` and `NEVER` sections. INV references below are from that spec.
- INV-1 (purity): this is a pure Layer-0 infra primitive — it sees opaque serialized payloads, never domain types. It MUST NOT depend on `zeph-llm`, `zeph-memory`, `zeph-core`, `zeph-sanitizer`, or any business-layer crate. Domain meaning lives in thin adapter modules inside each consuming crate.
- INV-14 (schema ownership): this crate owns no `.sql` files and no `sqlx::migrate!`. All `durable_*` schema lives as numbered migrations in `zeph-db/migrations/{sqlite,postgres}/`, applied via `zeph_db::run_migrations` against a dedicated `durable.db` pool. Any schema change goes through `zeph-db` with both-backend migration parity.
- Crypto is fail-closed and security-sensitive: `PayloadCipher` is AEAD, `PayloadAad` binds entries, and the read-side `max_payload` guard rejects oversized payloads. Never weaken the cipher contract, AAD binding, or the guard without an explicit security review.
- Preserve the `JournalWriter` actor invariants: group-commit for buffered appends, flush-before-commit ACK for exactly-once entries, and `MAX(seq)` restart resume. The `ExecutionBackend` trait is sealed — keep dispatch through `DurableBackendEnum`.
