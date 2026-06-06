# zeph-db Guide

Database abstraction layer (`DbPool`, `DbRow`, `DbTransaction`, `DbQueryResult`) with compile-time SQLite/PostgreSQL backend selection lives here.

- Start with crate-local checks: `cargo build -p zeph-db`, `cargo nextest run -p zeph-db`, `cargo clippy -p zeph-db --all-targets -- -D warnings`.
- All queries must use parameterized statements — reject any string-interpolated SQL to prevent injection.
- Schema changes require a migration; never alter existing migration files after they have been applied.
- Test with both backends (`--features sqlite` and `--features postgres`) before merging; silent divergence between backends is a first-class bug.
- If the public API changes, run `cargo build --workspace` — downstream crates depend on these type aliases.
