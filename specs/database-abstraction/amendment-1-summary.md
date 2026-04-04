# Amendment 1 Summary (2026-03-28)

Targeted amendments to `spec.md` based on three independent reviews:
- `critic-review-1.md` (7 findings: 2 Critical, 4 Significant, 4 Minor)
- `perf-review-1.md` (16 findings: 2 High, 8 Medium, 6 Low/Informational)
- `security-review-1.md` (10 findings: 3 Medium, 7 Low)

## Changes Made

### CRITICAL / HIGH (7 items)

1. **JSONB operator corruption** (C1): Removed false claim from 11.5 that PG does not use `?` for other purposes. Added bypass convention for JSONB queries using `?`/`?|`/`?&` — must use `$N` directly with safety annotation. Added Key Invariant #7 in section 12.

2. **NULL row invisibility on mode switch** (C2): Added shared-to-isolated transition requirements in 18.3: data migration SQL, startup NULL-row warning, optional transitional query mode with `OR agent_id IS NULL`.

3. **CREATE INDEX CONCURRENTLY in transaction** (S5): Replaced all `CREATE INDEX CONCURRENTLY` with regular `CREATE INDEX` in 18.4.3 PostgreSQL migration DDL. Added note about manual out-of-band execution for large tables.

4. **SQLite write pool** (F1): Added `write_pool_size` field to `DbConfig` (default 1, SQLite only) in section 4.6 to prevent `SQLITE_BUSY` stalls from competing writers.

5. **PostgreSQL skill trust race** (F8): Mandated `SELECT ... FOR UPDATE` in sections 4.7 and 11.2 for the two `BEGIN IMMEDIATE` call sites in `skills.rs`. Changed from audit item to required Phase 2 implementation step.

6. **Credential exposure** (F-01): Added `redact_url()` helper in 4.6, credential-redacted `DbError::Connection` variant, `ZEPH_DATABASE_URL` vault key, startup warning for inline credentials, `RedactFilter` regex extension.

7. **GlobalScope authorization** (S3, F-02): Changed `GlobalScope::new()` to `pub(crate)` in 18.5. Deprecated `AgentScope::pool()` with `#[doc(hidden)]`. Added Key Invariant #8 in section 12. Added audit `tracing::warn!` on construction.

### MEDIUM (7 items)

8. **Feature flag design** (S1): Removed `sqlite` from `zeph-db` default features in 4.2 and 7.2. Root Cargo.toml default explicitly includes `zeph-db/sqlite`. Documented `--all-features` incompatibility in 7.4.

9. **LazyLock overhead on SQLite** (S2, F3): Fixed query static pattern in 4.5 — SQLite uses `&'static str` constants, PG uses `LazyLock<String>`. Removed `sqlx/macros` from `zeph-db` in 7.2.

10. **Graph privacy leak** (S4): Changed graph table default from Shared to Isolated in 18.3. Added `source_agent_id` column for provenance in 18.4.2 and 18.4.3.

11. **bool_val removal** (M3): Removed `Dialect::bool_val()` from 4.4 — sqlx Encode handles mapping automatically.

12. **Pool acquire_timeout** (F2): Added `acquire_timeout = 30s` to PgPoolOptions in 4.6.

13. **Missing composite indexes** (F5, F6): Added `(agent_id, conversation_id)` on `embeddings_metadata` and `(agent_id, cache_key)` on `response_cache` in both 18.4.2 and 18.4.3.

14. **testcontainers CI** (S6): Corrected serialization claim in 16.6. Added nextest profile example for cross-binary serialization.

### LOW (3 notes)

15. **AgentId hostname dots** (M1): Documented dot-to-dash conversion and fallback in 18.2.
16. **Migration parity** (M2): Strengthened to schema catalog comparison in 11.4.
17. **sql! rewriter limits** (F-03): Documented dollar-quoted string and comment limitations in 11.5.

## Sections Modified

4.2, 4.4, 4.5, 4.6, 4.7, 6.1, 7.2, 7.4, 11.2, 11.4, 11.5, 12, 16.6, 18.2, 18.3, 18.4.2, 18.4.3, 18.5 + new section 19 (Amendment Log).

## Not Changed (deferred or out of scope)

- M4 (critic): `ilike()` semantic mismatch — accepted as-is with existing documentation.
- F4 (perf): `agent_filter_clause()` per-call allocation — noted in spec, LazyLock optimization documented as recommendation.
- F-04 to F-10 (security Low): Accepted with existing mitigations or deferred to implementation.
- Perf findings 9-16: Informational, no spec changes needed.
