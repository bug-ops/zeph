# zeph-db Guide

Database abstraction layer (`DbPool`, `DbRow`, `DbTransaction`, `DbQueryResult`) with compile-time SQLite/PostgreSQL backend selection lives here.

- Start with crate-local checks: `cargo build -p zeph-db`, `cargo nextest run -p zeph-db`, `cargo clippy -p zeph-db --all-targets -- -D warnings`.
- All queries must use parameterized statements — reject any string-interpolated SQL to prevent injection.
- Schema changes require a migration; never alter existing migration files after they have been applied.
- Test with both backends (`--features sqlite` and `--features postgres`) before merging; silent divergence between backends is a first-class bug.
- If the public API changes, run `cargo build --workspace` — downstream crates depend on these type aliases.
- List-style queries with "0 means unlimited" semantics must use `zeph_db::limit_clause()` — never bind `LIMIT -1` (rejected by PostgreSQL) or a `NULL` bind value (rejected by SQLite); it is the only cross-backend-safe way to omit the clause (#6121).
- Never log or print a raw database URL; always redact it through `zeph_db::redact_url()` first (covers userinfo, libpq query-param, and key-value DSN credential forms — a password containing `@` used to leak its tail, #6013). A `None` return means no recognized credential form was found, not a guarantee the URL is safe.
