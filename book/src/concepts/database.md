# Database Abstraction

Zeph uses the `zeph-db` crate as a unified database abstraction layer. All SQL operations go through typed query builders instead of raw SQL strings, eliminating sqlx leaks and dynamic query injection vectors.

## Supported Backends

| Backend | Feature | Use Case |
|---------|---------|----------|
| SQLite | default | Single-user, local, zero-dependency |
| PostgreSQL | `postgres` | Multi-user, production, concurrent access |

The backend is selected at build time via feature flags. All query interfaces are identical regardless of backend — application code does not branch on database type.

## Migration

Database schema migrations are managed by `zeph-db` and applied automatically on startup. You can also run them manually:

```bash
zeph db migrate                    # apply pending migrations
zeph db migrate --status           # show migration status
```

The `migrate-config` wizard detects backend changes and generates the appropriate connection string.

## Configuration

SQLite (default):

```toml
[memory]
database_url = "sqlite://~/.zeph/data/zeph.db"
```

PostgreSQL:

```toml
[memory]
database_url = "postgres://user:pass@localhost/zeph"
```

Store the PostgreSQL connection string in the vault for production use:

```bash
zeph vault set ZEPH_DATABASE_URL "postgres://user:pass@localhost/zeph"
```

## Security Hardening

- All queries use parameterized statements — no string interpolation
- Dynamic column/table names are validated against an allowlist at compile time
- Connection pool settings are tuned per-backend (SQLite: single writer, PostgreSQL: configurable pool size)
