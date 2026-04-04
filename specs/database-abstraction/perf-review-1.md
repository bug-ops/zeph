# Performance Review: Database Abstraction Layer Spec

**Reviewer**: rust-performance-engineer
**Spec**: `.local/specs/database-abstraction/spec.md`
**Date**: 2026-03-28
**Scope**: Architecture-level performance analysis — no code changes

Severity scale: **Critical** (blocks correct behavior or causes data loss under load) | **High** (measurable latency/throughput regression in the normal operating path) | **Medium** (overhead acceptable in isolation but compounds with other issues; should be addressed before v1.0) | **Low** (minor, negligible in practice, or only affects edge cases)

---

## 1. Connection Pool Sizing

**Spec reference**: §6.1 (`sqlite_pool_size = 5`, `postgres_max_connections = 10`)
**Severity**: High (SQLite) / Medium (PostgreSQL)

### SQLite: 5 connections is the wrong default for a WAL database

With `journal_mode = WAL`, SQLite allows one concurrent writer and unlimited concurrent readers. A pool of 5 for a single-agent desktop process is oversized for the write path and may be undersized for bursty read patterns. More critically, the spec does not set `min_connections`. sqlx's `SqlitePoolOptions` defaults to 0 idle connections. Under a burst (e.g., context build + memory search + graph lookup arriving within the same agent turn), sqlx may attempt to open all 5 connections simultaneously against the same WAL-locked file. Each excess writer beyond the first will block for `busy_timeout` (5 s per §4.6), degrading response latency.

**The actual bottleneck**: SQLite WAL allows concurrent readers freely but serializes writers. A pool size > 1 for the write path provides no benefit and creates unnecessary lock contention. The right configuration for a single-agent SQLite deployment is:

- `max_connections = 1` for the write pool (eliminates `SQLITE_BUSY` entirely)
- `max_connections = N` (4–8) for a read-only pool

The spec does not distinguish read and write pools. This is the core gap. With a single unified pool of 5 and no write/read split, the agent will experience intermittent 5-second stalls on concurrent writes during memory-heavy turns. The spec's claim in §18.8 that "query performance" is unaffected by the schema change is correct in isolation, but the pool sizing issue pre-exists and is exacerbated by adding the `agent_id` bind parameter (one more potential write contender).

**Recommendation**: Add a `max_write_connections = 1` (SQLite only) constraint to `DbConfig`, backed by a dedicated 1-connection writer pool. The reader pool uses the remaining `max_connections - 1`. This is a well-known pattern for SQLite under sqlx and eliminates `SQLITE_BUSY` without requiring `BEGIN IMMEDIATE`.

### PostgreSQL: 10 connections is plausible but context-dependent

For a single-agent deployment against a shared PG instance, 10 is reasonable. However, the spec does not specify `min_connections`, `acquire_timeout`, or `idle_timeout`. Without `acquire_timeout`, a saturated pool (e.g., 10 concurrent async tasks all waiting for a DB connection) will deadlock silently in tokio — the tasks wait on a channel that never receives. The default sqlx acquire timeout is effectively infinite. This must be set explicitly (recommendation: 5–30 s matching the agent's per-turn timeout budget).

The 10-connection default also assumes the Postgres server accepts at least `max_connections` = 100 (PG default). In containerized deployments (e.g., the testcontainers CI setup in §16.2), `postgres:16-alpine` defaults to `max_connections = 100`, which is fine. But the spec should document the assumption.

---

## 2. `LazyLock<String>` for Query Caching

**Spec reference**: §4.5
**Severity**: Medium

### The approach is correct, but the overhead model is incomplete

The spec correctly identifies that `rewrite_placeholders` should run at most once per distinct query string and proposes `LazyLock<String>` as the caching mechanism. This is appropriate.

However, the spec states "for the SQLite feature, `sql!()` returns the literal `&str` directly, so `LazyLock` is unnecessary (the optimizer eliminates it)." This is only true if the `LazyLock<String>` is written with the SQLite path short-circuiting at the `sql!` macro expansion level, before the `LazyLock::new` call is emitted. If the `LazyLock` body calls `sql!().to_string()` and `sql!()` on SQLite just returns `$query`, then:

- SQLite path: `LazyLock::new(|| $query.to_string())` — allocates a `String` on first call, then serves `&String` on subsequent calls. This is a heap allocation per distinct query string at first use, never freed.
- The optimizer will not eliminate the `LazyLock` or the `String` allocation because `LazyLock<String>` has a non-trivial `Drop`.

For the SQLite path specifically, the correct pattern is to keep query strings as `&'static str` literals (no `LazyLock`) and reserve `LazyLock<String>` strictly for the PostgreSQL path. The spec acknowledges this but does not enforce it — the example shows `.to_string()` unconditionally, which causes the unnecessary heap allocation on the SQLite path.

Concretely: with 49 migration-era queries, each represented as a `LazyLock<String>` on the SQLite path, you get 49 permanent heap-allocated `String` objects of ~50–200 bytes each. Total overhead: ~5–10 KB of heap, live for the process lifetime. This is negligible in absolute terms but contradicts the spec's claim of "zero-cost repeated use" for SQLite.

### Alternative with no overhead on SQLite

```rust
#[cfg(feature = "sqlite")]
macro_rules! cached_sql {
    ($query:literal) => { $query };  // pure &'static str, no allocation
}

#[cfg(feature = "postgres")]
macro_rules! cached_sql {
    ($query:literal) => {{
        static S: std::sync::LazyLock<String> =
            std::sync::LazyLock::new(|| $crate::rewrite_placeholders($query));
        S.as_str()
    }};
}
```

This pattern gives truly zero cost on SQLite and one-time allocation on PostgreSQL. The spec's current `sql!` + external `LazyLock<String>` requires the caller to manage the static, which is error-prone. The macro should encapsulate both the rewrite and the cache.

---

## 3. `AgentScope` as a DB Gateway — `Arc<str>` Clone Per Query

**Spec reference**: §18.5
**Severity**: Low (single-agent SQLite) / Medium (high-QPS PostgreSQL multi-agent)

### Cost analysis

`AgentScope` is cloned at store construction time (§2011–2038 of the spec), not per query. The `pool.clone()` at construction is a reference count increment on the pool's internal `Arc`. The `agent_id.clone()` is an `Arc<str>` clone — another atomic increment. This construction-time cost is O(1) and happens once per store per startup.

Per-query cost: `self.scope.agent_id().as_str()` is a no-cost deref (`Arc<str>` -> `&str`). The actual bind to sqlx is `.bind(self.scope.agent_id().as_str())`, which copies the `&str` into sqlx's bind buffer. This is a `memcpy` of 1–64 bytes (the agent ID string length), indistinguishable from noise relative to network round-trip cost on PostgreSQL or SQLite I/O.

**There is no performance problem with `Arc<str>` clone here.** The spec's own comment in §18.9.1 noting "a minor runtime cost (string allocation)" for `agent_filter_clause()` via `format!` is the actual concern — not the `Arc<str>`.

### The real overhead: `agent_filter_clause()` with `format!`

The `find_entity` example in §18.5 uses:
```rust
let sql = format!("SELECT * FROM graph_entities WHERE name = ?{filter}");
```

This allocates a `String` per call to `find_entity`. For a hot-path query called inside a graph BFS traversal or entity lookup loop, this is a heap allocation on every invocation. At 1,000 graph lookups per agent turn (a plausible number for a knowledge-intensive query), this is 1,000 heap allocations of ~60 bytes each. The allocator overhead is measurable.

The spec correctly flags this and suggests `LazyLock` with pre-built variants. However, the `agent_filter_clause()` helper is proposed as a general pattern for shared tables, creating a latent performance trap for any developer who uses it without understanding the allocation implication.

**Recommendation**: For shared tables with hot-path queries, require two `&'static str` variants (one with the filter, one without) selected once at `AgentScope` construction, not per call. The `agent_filter_clause()` helper should be documented as "not for hot paths."

---

## 4. Composite Indexes — Coverage Analysis

**Spec reference**: §18.4.2, §18.4.3
**Severity**: Medium

### Index coverage assessment per query pattern

The spec proposes these indexes:

| Index | Columns |
|-------|---------|
| `idx_messages_agent_conv` | `(agent_id, conversation_id, id)` WHERE `deleted_at IS NULL` |
| `idx_conversations_agent` | `(agent_id, id)` |
| `idx_summaries_agent_conv` | `(agent_id, conversation_id)` |
| `idx_session_digest_agent` | `(agent_id, conversation_id)` |
| `idx_task_graphs_agent` | `(agent_id, status)` |
| `idx_mem_scenes_agent` | `(agent_id)` |
| `idx_acp_sessions_agent` | `(agent_id)` |
| `idx_experiment_results_agent` | `(agent_id, session_id)` |

**`idx_messages_agent_conv` is covering for `load_history`** — the query `WHERE conversation_id = ? AND agent_id = ? AND deleted_at IS NULL ORDER BY id DESC LIMIT ?` uses all three index columns plus the partial condition. The planner can use `Index Only Scan` (PG) or `Index Scan` (SQLite). This is correct.

**`idx_messages_agent_conv` does NOT cover `search_fts`** — the FTS query on PostgreSQL (`WHERE tsv @@ plainto_tsquery(...)`) is not covered by this index. It requires a GIN index on `tsv`. The spec covers this via the FTS abstraction module but the GIN index is defined in the migration porting guide (§5.2) rather than in the index section (§18.4), making it easy to miss. There is a risk that the GIN index for `messages.tsv` is created but the covering composite index for `agent_id + tsv` is not. A query like `WHERE agent_id = ? AND tsv @@ plainto_tsquery(?)` would use the GIN index (bitmap scan on `tsv`) combined with a heap scan for the `agent_id` filter — two scan steps instead of one.

**Gap: `embeddings_metadata` has no composite index**. The spec adds `agent_id` to this table but proposes no composite index. The `EmbeddingStore` queries embeddings by `conversation_id` and `message_id`. Without `(agent_id, conversation_id)` index, the planner falls back to a full scan of `embeddings_metadata` filtered by `agent_id`, then a second filter by `conversation_id`. At scale (thousands of embeddings), this is a sequential scan on a frequently-hit table.

**Gap: `user_corrections` and `learned_preferences` have no indexes**. These tables are added to the isolated list but receive no composite indexes. If the agent queries corrections by `(agent_id, ...)` at turn time, this is an unindexed scan.

**Gap: `response_cache` partial index**. `response_cache` is marked Shared with `agent_id TEXT DEFAULT NULL`. The most common query is `WHERE cache_key = ?`. No index on `(agent_id, cache_key)` is proposed, meaning in shared mode, the cache lookup hits the existing `cache_key` index, but in isolated mode (overridden), the plan changes to `(agent_id, cache_key)` — which has no index. This causes a full table scan on cache lookup when the subsystem is switched to isolated mode.

**Low-cardinality `agent_id` on PostgreSQL**: With a single agent (the common case), `agent_id` has cardinality 1. PostgreSQL's planner will recognize this and may prefer a sequential scan over an index scan for small tables, which is correct behavior. The composite indexes become valuable only in the multi-agent case. The spec's claim in §18.9.2 that "PostgreSQL's query planner handles it well with Index Only Scan" is accurate for tables large enough to make index scans worthwhile, but for small tables (< ~100 rows), the planner will ignore the index regardless.

---

## 5. `JSONB` vs `TEXT` for Memory Entries

**Spec reference**: §5.2 (migration porting guide)
**Severity**: Medium

The spec proposes `TEXT NOT NULL DEFAULT '[]'` → `JSONB NOT NULL DEFAULT '[]'::jsonb` for PostgreSQL. This is a significant storage and query behavior difference that the spec does not analyze in depth.

### Write performance

`JSONB` on PostgreSQL stores a binary decomposed representation. Every INSERT or UPDATE of a JSON field requires parsing and binary encoding. For Zeph's write pattern (message parts stored as JSON arrays in the `parts` column), this adds a parsing step on every message save. At the agent's typical write rate (1–5 messages per turn), this overhead is negligible — likely < 0.1 ms per write.

### Read performance

The primary benefit of `JSONB` is operator support (`->`, `->>`, `@>`, GIN indexing). The agent's read pattern for `parts` is: fetch the entire column and deserialize in Rust via `serde_json`. There is no server-side JSON filtering on `parts` content in the current codebase. Therefore, `JSONB` provides no query acceleration benefit for `parts` under the current access pattern.

Where `JSONB` genuinely helps: if any future query filters on JSON content server-side (e.g., `WHERE parts @> '[{"type": "image"}]'`), `JSONB` with a GIN index would be orders of magnitude faster than `TEXT` with `json_extract()`. This is a forward-looking investment.

### Storage overhead

`JSONB` is typically 10–20% larger than `TEXT` for small JSON objects because of the binary header overhead per key. For Zeph's `parts` arrays (typically 1–3 elements), the overhead per row is ~50–100 bytes. At 100,000 messages, this is ~5–10 MB extra storage — negligible.

### Risk: `JSONB` rejects malformed JSON at INSERT time

`TEXT` silently stores any string. `JSONB` raises an error on malformed JSON. If any code path stores non-JSON in a `JSONB` column (e.g., empty string instead of `'[]'`), the INSERT fails. The SQLite path accepts this silently. This is a correctness difference, not a performance issue, but it will manifest as a runtime error in production that doesn't appear in SQLite testing.

**Recommendation**: Accept the `JSONB` proposal for columns actively used in server-side JSON queries. For `parts` (which is only ever fetched whole and decoded in Rust), `TEXT` vs `JSONB` is performance-neutral. Document which columns use `JSONB` for query purposes vs. which use it only for type safety.

---

## 6. FTS: `tsvector` vs FTS5 Latency

**Spec reference**: §5.3, §11.3
**Severity**: Low

### Query latency comparison

For Zeph's typical FTS workload — short queries (1–5 words), small-medium datasets (< 100,000 messages) — the latency difference between FTS5 and `tsvector`/GIN is marginal:

- **FTS5**: inverted index maintained by SQLite in a virtual table. Lookup is `O(matching terms)`, typically < 1 ms on datasets up to 1M rows on local storage.
- **tsvector + GIN**: PostgreSQL GIN index has higher per-query overhead due to MVCC visibility checking but scales better under concurrent read load. At < 100K rows, expect 1–5 ms for a network-connected PG instance (dominated by RTT, not index performance).

The dominant latency factor for the PostgreSQL case is network round-trip, not the FTS implementation. On a local Docker container (as in CI), both are sub-millisecond.

### Ranking difference

FTS5's `rank` function and PostgreSQL's `ts_rank_cd` produce different orderings for the same query. The spec correctly accepts this divergence. No performance concern here.

### Tokenizer mismatch

`plainto_tsquery('english', ...)` applies English stemming (Porter stemmer). FTS5 defaults to a Unicode tokenizer with no stemming. A query for "running" in FTS5 finds only "running"; in PostgreSQL it also finds "run", "runs". This means PostgreSQL FTS returns more rows for the same query, which is a latency concern if many rows match: the agent processes more results. For typical agent memory queries (proper nouns, code identifiers), stemming has little effect.

**Overall assessment**: No significant performance issue. Accept the behavioral divergence as specified.

---

## 7. `BEGIN IMMEDIATE` Removal on PostgreSQL

**Spec reference**: §4.7, §11.2
**Severity**: High (specific write patterns on PostgreSQL)

### The MVCC replacement gap

The spec removes `BEGIN IMMEDIATE` for PostgreSQL and replaces it with a standard `BEGIN` (deferred transaction). The two SQLite usages are in `skills.rs` for skill trust score updates. The spec's mitigation note says "use `SELECT ... FOR UPDATE` where row-level locking is needed" but does not mandate this change — it says "audit" and "document."

This is a concurrency correctness risk that compounds into a performance risk on PostgreSQL:

**Lost update scenario**: Two agents (A and B) simultaneously update `skill_trust` for the same skill:

1. A: `BEGIN` (deferred)
2. B: `BEGIN` (deferred)
3. A: `SELECT trust_score FROM skill_trust WHERE skill_name = 'foo'` → reads 0.8
4. B: `SELECT trust_score FROM skill_trust WHERE skill_name = 'foo'` → reads 0.8
5. A: `UPDATE skill_trust SET trust_score = 0.85 WHERE skill_name = 'foo'` → writes 0.85
6. B: `UPDATE skill_trust SET trust_score = 0.75 WHERE skill_name = 'foo'` → overwrites to 0.75

B's update silently discards A's change. With `BEGIN IMMEDIATE` on SQLite, B's transaction would block at step 2 until A commits, preventing the lost update. With standard PostgreSQL `BEGIN`, the lost update occurs silently at READ COMMITTED isolation (PG default).

**Performance implication of the fix**: The correct PostgreSQL replacement is `SELECT ... FOR UPDATE` inside the transaction, which acquires a row-level lock before the update. This is correct and cheap for low-contention scenarios. However, under high write concurrency (unlikely for a skill trust update but architecturally relevant), this becomes a serialization point. The spec should mandate `FOR UPDATE` explicitly for the skill trust path, not leave it as an audit item.

**The spec also misses the `response_cache` upsert race**: `response_cache` uses `INSERT OR REPLACE` (SQLite) rewritten to `INSERT ... ON CONFLICT DO UPDATE`. On PostgreSQL with the default isolation, concurrent upserts on the same `cache_key` can cause serialization failures (`ERROR: could not serialize access due to concurrent update`) if the table is also being read in the same transaction with a snapshot. The `ON CONFLICT DO UPDATE` on PostgreSQL handles this at the statement level (it retries internally), so this is less of a risk than the skill trust scenario, but it should be called out.

---

## 8. Migration Runner on Every Startup

**Spec reference**: §4.6 (`crate::migrate::run_migrations(&pool)` in `connect()`)
**Severity**: Low (SQLite) / Medium (PostgreSQL, multi-instance)

### SQLite startup latency

`sqlx::migrate!()` on SQLite: the migration runner queries the `_sqlx_migrations` table, compares the applied migration hashes against the embedded set, and runs any pending migrations. On a schema that is fully up-to-date (the common case after initial setup), this is:

1. `SELECT * FROM _sqlx_migrations` — a full scan of a table with 49–50 rows
2. Hash comparison for each row (in-process, negligible)
3. No SQL executed (all migrations already applied)

On a local SQLite file, step 1 takes < 1 ms. The PRAGMA WAL checkpoint that follows (also in `connect()`) takes < 5 ms on a healthy WAL file. Total migration-check overhead at startup: **< 10 ms**. Acceptable for a desktop agent.

### PostgreSQL startup latency — advisory lock contention

The spec correctly notes (§18.7.2) that sqlx acquires `pg_advisory_lock` during migration. For the no-op case (all migrations applied), the sequence is:

1. Acquire advisory lock (network round-trip to PG)
2. `SELECT * FROM _sqlx_migrations` (network round-trip)
3. Compare hashes, find nothing to apply
4. Release advisory lock (network round-trip)

On a local PG: ~3–10 ms. On a remote PG (cloud): 3 × RTT, e.g., 3 × 20 ms = 60 ms of startup overhead. For a long-running agent process, this is a one-time cost at startup — negligible.

**The multi-instance case**: If 10 agent instances start simultaneously (e.g., a rolling restart in a Kubernetes deployment), they all contend for the same advisory lock. The first acquires the lock, checks migrations (fast, all applied), releases the lock. The remaining 9 queue up. Total serialized wait: 9 × 10 ms = 90 ms overhead for the last instance. Acceptable — not a blocking concern.

### Missing: migration fast-path check

The spec does not propose any way to skip migration detection. A `PRAGMA user_version` (SQLite) or a custom version table could allow a fast pre-check before acquiring the advisory lock, but this is an optimization the spec does not need — the overhead is already low.

**Recommendation**: No change required. The startup overhead is acceptable for both backends. Document the expected startup latency range (< 10 ms SQLite, 10–100 ms PG depending on RTT) in the `DbConfig::connect()` doc comment so callers understand the async cost.

---

## 9. SQLite WAL + Multi-Reader After `agent_id` Addition

**Spec reference**: §18.4, §18.8
**Severity**: Low

### WAL checkpoint behavior with larger indexes

The migration adds 8 composite indexes to high-traffic tables (`messages`, `conversations`, `summaries`, etc.). Each additional index increases the size of the WAL file per write transaction (because index pages are also written to WAL). On a `PASSIVE` checkpoint (as configured in §4.6), SQLite transfers WAL pages to the main database file only when no readers hold a WAL snapshot. The `PASSIVE` mode is non-blocking, so it may leave the WAL growing indefinitely if readers are continuous.

With the new indexes, a single `INSERT INTO messages (...)` write transaction generates:
- 1 page for the `messages` row
- 1–3 pages for `idx_messages_agent_conv` (B-tree update)
- Additional pages for any other existing `messages` indexes

The WAL write amplification increases by roughly the number of indexes on the table. For `messages` (the highest-write table), existing indexes plus the new `idx_messages_agent_conv` may increase WAL size per transaction by 2–3x versus the pre-index state.

**Practical impact**: For a single-agent desktop deployment (the default), WAL growth is bounded by the `PASSIVE` checkpoint that runs at startup (§4.6) and any OS-level page cache pressure that triggers additional checkpoints. The write rate for a conversational agent (a few messages per turn, one turn every few seconds) is far too low for WAL accumulation to become a problem. A `PASSIVE` checkpoint at startup is sufficient.

**WAL mode and the write-pool gap**: As noted in finding #1, the spec lacks a 1-connection write pool. With a single writer at a time (WAL's natural constraint), the WAL grows linearly with turns. The `PASSIVE` checkpoint at startup correctly drains the WAL from the previous session. No change needed here if finding #1 is addressed.

**Recommendation**: No WAL checkpoint strategy change required. The `PASSIVE` checkpoint at startup is correct. If write throughput increases significantly in future (bulk imports, high-frequency scheduler ticks), consider adding `PRAGMA wal_autocheckpoint` configuration to `DbConfig`.

---

## 10. Build Time: `postgres` Feature and sqlx Compile-Time Behavior

**Spec reference**: §11.1, §7.2, SC-005
**Severity**: High

### The spec's claim is incorrect

The spec states: "Only one backend is compiled at a time (mutually exclusive features). The `postgres` feature is never in `default` and only activated explicitly. No impact on default builds."

This is partially wrong. The statement is correct for *runtime* behavior and *binary size*, but misses sqlx's compile-time verification behavior.

**sqlx compile-time query verification**: When `sqlx/macros` feature is enabled (which the spec includes: `features = ["macros", "runtime-tokio-rustls", "migrate"]` in §7.2), the `query!` and `query_as!` macros verify SQL against a live database at compile time (or against an `sqlx-data.json` offline cache). The spec correctly notes that zero `query!` macros are used (§2.2), so this specific overhead does not apply to the existing codebase.

However, adding `sqlx = { workspace = true, features = ["macros", "runtime-tokio-rustls", "migrate"] }` to `zeph-db` means `sqlx` itself must compile with its macro infrastructure. sqlx is one of the slowest crates in the Rust ecosystem to compile. On a cold build:

- `sqlx-core` alone: ~20–40 s
- `sqlx-macros` (proc macro crate): ~5–15 s
- Backend-specific crates (`sqlx-sqlite`, `sqlx-postgres`): ~10–20 s each

The spec introduces a new `zeph-db` crate that depends on sqlx. Even though only one backend compiles at a time, both `sqlite` and `postgres` backend code paths exist in sqlx's dependency tree because `sqlx = { workspace = true, features = ["macros", "runtime-tokio-rustls", "migrate"] }` in `zeph-db` will compile sqlx with `migrate` but without a backend — and then the backend is activated via feature unification from the root binary.

**The actual build-time risk**: Currently, sqlx is a direct dependency of `zeph-memory`. After the migration, it becomes a dependency of `zeph-db`. If `zeph-db` is compiled before `zeph-memory` in the dependency graph (which it must be, since `zeph-memory` depends on `zeph-db`), the sqlx compilation moves earlier in the build graph. This does not increase total compile time, but it may affect incremental build time if `zeph-db` changes frequently invalidate sqlx's compiled artifacts in sccache.

**Feature flag interaction with sccache**: sccache caches compiled crate artifacts keyed on (crate version, features, target, etc.). Adding `zeph-db` as a new crate with the `postgres` feature flag means the first `--features postgres` build compiles sqlx with additional feature flags not in the sccache key from previous sqlite builds. This is a one-time cold-cache event per distinct feature set — not ongoing overhead. The spec's CI matrix addition (`--features full,postgres`) will produce sccache misses on first run but cache hits on subsequent runs.

**The `macros` feature is unnecessary overhead for `zeph-db`**: Since `zeph-db` uses zero `query!` macros (by design), including `sqlx/macros` in `zeph-db`'s dependency adds the proc-macro compilation without any benefit. Drop `macros` from `zeph-db`'s sqlx dependency. Consumer crates that need `sqlx/macros` can add it directly.

**Build time verdict**: The spec's "no impact" claim is roughly correct for incremental builds with sccache warm. On cold builds, introducing `zeph-db` as a new crate adds a crate compilation step (~2–5 s for the crate itself) but does not add sqlx compilation time (sqlx was already being compiled). The `macros` feature inclusion is a small unnecessary overhead.

---

## Summary Table

| # | Finding | Severity | Section(s) |
|---|---------|----------|------------|
| 1 | SQLite pool size 5 with no write/read split causes `SQLITE_BUSY` stalls under bursty write patterns | **High** | §6.1, §4.6 |
| 2 | PostgreSQL pool missing `acquire_timeout`; silent deadlock risk under saturation | **Medium** | §6.1 |
| 3 | `sql!` macro with `LazyLock<String>` allocates a `String` on the SQLite path, contradicting "zero-cost" claim; should use `&'static str` on SQLite path | **Medium** | §4.5 |
| 4 | `agent_filter_clause()` with `format!` allocates a `String` per hot-path query; documented as "minor" but compounds in BFS/graph loops | **Medium** | §18.5 |
| 5 | `embeddings_metadata` table missing composite index on `(agent_id, conversation_id)`; full scan risk | **Medium** | §18.4.2 |
| 6 | `response_cache` missing `(agent_id, cache_key)` index; isolated-mode override causes full scan on cache lookup | **Medium** | §18.4, §18.3 |
| 7 | PostgreSQL FTS: no GIN index covering `(agent_id, tsv)` proposed; multi-agent FTS falls back to bitmap+heap scan | **Medium** | §5.2, §18.4.3 |
| 8 | `BEGIN IMMEDIATE` removal without mandatory `SELECT FOR UPDATE` replacement creates lost-update scenario on `skill_trust` under concurrent PostgreSQL writers | **High** | §4.7, §11.2 |
| 9 | `user_corrections` and `learned_preferences` tables missing composite indexes despite being in isolated table list | **Low** | §18.4.2 |
| 10 | `JSONB` rejects malformed JSON at INSERT time; SQLite accepts silently — undetected data bugs in SQLite become hard errors on PostgreSQL | **Medium** (correctness, not perf) | §5.2 |
| 11 | `sqlx/macros` feature included in `zeph-db` despite no `query!` usage; adds unnecessary proc-macro compilation | **Low** | §7.2 |
| 12 | Spec claim "no impact on default builds" from adding `postgres` feature is misleading; incremental sccache impact on first `--features postgres` run; `macros` feature adds cold build overhead | **Low** | §11.1 |
| 13 | Migration startup overhead is acceptable (< 10 ms SQLite, < 100 ms PG); no fast-path bypass needed | Low / Informational | §4.6, §18.7 |
| 14 | WAL checkpoint strategy with new indexes: no change required for single-agent desktop workload | Low / Informational | §18.8 |
| 15 | `Arc<str>` clone per query bind: zero cost in practice; not an issue | Non-issue | §18.5 |
| 16 | FTS5 vs `tsvector` latency: negligible difference for the agent's workload size and query patterns | Non-issue | §5.3 |

---

## Priority Recommendations for Spec Authors

**Before implementation begins:**

1. **(High)** Add a SQLite 1-connection write pool or document a `max_write_connections = 1` pattern in `DbConfig`. Without this, `SQLITE_BUSY` stalls are the most likely performance regression from this change.

2. **(High)** Mandate `SELECT ... FOR UPDATE` in `begin_write()` for PostgreSQL on the skill trust update path (not just "audit"). Demote this from an open risk to a concrete spec requirement.

3. **(Medium)** Add composite index for `embeddings_metadata(agent_id, conversation_id)` to migration 050.

4. **(Medium)** Add composite index for `response_cache(agent_id, cache_key)` — or document that response cache isolated-mode override is not supported without manual index creation.

5. **(Medium)** Add `acquire_timeout` to PostgreSQL pool configuration defaults (suggest 30 s).

6. **(Medium)** Change `sql!` macro design to return `&'static str` on the SQLite path and `LazyLock<String>` on the PostgreSQL path, within the macro itself — not via external `LazyLock` wrappers.

7. **(Low)** Remove `macros` from `zeph-db`'s sqlx features; add a comment explaining why.
