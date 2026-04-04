# zeph-db

Database abstraction layer providing a unified API across SQLite and PostgreSQL backends.

## Purpose

`zeph-db` replaces direct `sqlx` usage scattered across workspace crates with a centralized, type-safe database interface. It eliminates raw SQL string leaks, enforces parameterized queries, and provides automatic schema migration.

## Architecture

```text
zeph-db
├── migrations/         Schema migrations (shared across backends)
├── sqlite.rs           SQLite backend implementation
├── postgres.rs         PostgreSQL backend implementation (feature-gated)
├── query/              Typed query builders
└── pool.rs             Connection pool management
```

## Phase History

- **Phase 1**: SQLite backend, typed query builders, migration framework
- **Phase 2**: PostgreSQL backend, connection pooling, concurrent access
- **Phase 3**: `zeph db migrate` CLI subcommand, init wizard integration, CI postgres build

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend (via sqlx) |
| `postgres` | no | PostgreSQL backend (via sqlx) |
