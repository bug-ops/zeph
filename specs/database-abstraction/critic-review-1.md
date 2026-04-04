# Adversarial Critique: Database Abstraction Layer Spec

**Reviewer**: rust-critic
**Date**: 2026-03-28
**Spec**: `.local/specs/database-abstraction/spec.md`
**Verdict**: SIGNIFICANT (must address before implementation)

---

## Summary

The spec is thorough and well-structured. It correctly identifies the core
incompatibilities between SQLite and PostgreSQL, rejects `sqlx::Any` with valid
reasoning, and proposes a compile-time monomorphization approach that avoids
runtime dispatch overhead. The agent identity model (Section 18) is
well-considered for multi-tenant scenarios.

However, several findings range from Critical to Minor. Two are correctness bugs
that will produce wrong results or panics if implemented as written. Others are
design gaps that will cause pain at integration time or in production.

---

## CRITICAL Findings

### C1. `sql!` macro corrupts PostgreSQL JSON operators

**Dimension**: Counterexample hunt
**Section**: 4.5, 11.5

PostgreSQL uses `?` as a JSON containment operator: `jsonb_column ? 'key'`,
`jsonb_column ?| array[...]`, `jsonb_column ?& array[...]`. The
`rewrite_placeholders()` function (lines 313-332) treats every unquoted `?` as a
bind parameter placeholder and rewrites it to `$N`.

**Concrete counterexample**: The spec's own Section 2.3 identifies that
PostgreSQL uses `JSONB` with operators like `@>` and `->>`. If any query uses the
`?` operator (which is the standard JSONB key-existence check), it will be
silently rewritten to `$N`, producing a malformed query.

Section 11.5 claims: "PostgreSQL does not use `?` for any other purpose, so false
positives outside strings are unlikely." This statement is **factually wrong**.
PostgreSQL's `?`, `?|`, and `?&` are documented core JSONB operators. The spec's
own FTS/JSON migration plan (Section 5.2) converts `TEXT` to `JSONB`, making
these operators a natural choice for PostgreSQL-side queries.

**Impact**: Any PostgreSQL query using JSONB key-existence checks will produce a
SQL syntax error or incorrect bind parameter numbering.

**Recommendation**: The rewriter must distinguish bind-parameter `?` from operator
`?`. Options: (a) use a different placeholder convention internally (e.g., `$?`)
and rewrite that, (b) require PostgreSQL-targeted queries to use `$N` directly
and only apply rewriting to shared queries, (c) use a proper SQL tokenizer that
understands operator context. Option (a) is simplest.

---

### C2. `WHERE agent_id = ?` never matches shared/NULL rows

**Dimension**: Counterexample hunt
**Section**: 18.3, 18.5

The spec uses `agent_id = NULL` for shared/global rows (Section 18.3, 18.10
invariant 3). However, in SQL, `WHERE agent_id = ?` where the bind value is
`NULL` evaluates to `NULL` (not `TRUE`) due to SQL's three-valued logic. The
expression `agent_id = NULL` is never true.

The `AgentScope::filter_value()` method (line 1768) returns `None` in Shared
mode. But the query patterns shown at line 1857 use `WHERE agent_id = ?` with a
bind of the agent_id string in Isolated mode. In Shared mode, the spec shows the
`agent_filter_clause()` helper returning an empty string (line 1910), which means
no agent filter is applied -- this is correct for reads in shared mode.

**However**, the spec does not address the critical transitional case: when a
subsystem defaults to Shared but is overridden to Isolated via config (Section
18.2 configurable overrides). In that scenario, shared tables already contain rows
with `agent_id = NULL` (written by other agents in shared mode). An agent querying
in Isolated mode with `WHERE agent_id = 'my-agent'` will miss all pre-existing
NULL rows. Those rows become invisible -- effectively data loss for the
newly-isolated agent.

**Impact**: Switching a subsystem from Shared to Isolated mode silently hides all
previously-written global rows. No migration path is proposed to backfill
`agent_id` on existing NULL rows.

**Recommendation**: (1) Document that switching from Shared to Isolated requires a
data migration step (`UPDATE graph_entities SET agent_id = 'target-agent' WHERE
agent_id IS NULL`). (2) Add a startup check: if a subsystem is configured as
Isolated but the table contains NULL `agent_id` rows, emit a warning or fail with
an actionable message. (3) Consider `WHERE (agent_id = ? OR agent_id IS NULL)`
for the transitional period, with a config flag to opt out.

---

## SIGNIFICANT Findings

### S1. Feature flag mutual exclusivity breaks under `cargo test --all-features`

**Dimension**: Assumption audit
**Section**: 4.2, 7.4

The `compile_error!` on line 176-177 fires when both `sqlite` and `postgres` are
active. However, `cargo test --all-features` (which CI frequently runs) and
`cargo clippy --all-features` (common in CI) will activate both features,
producing a hard compile error.

The spec says `full,postgres` is the PostgreSQL combination (Section 7.4), but
`full` does not explicitly exclude `sqlite` -- it inherits `sqlite` from
`zeph-db`'s `default` feature. So `--features full,postgres` actually activates
both `sqlite` AND `postgres`, triggering the `compile_error!`.

Cargo feature unification means if *any* crate in the workspace depends on
`zeph-db` without `default-features = false`, the `sqlite` default leaks in.

**Impact**: `--all-features` builds break. CI workflows that use `--all-features`
for linting or doc generation will fail. The root `Cargo.toml` feature definition
(line 698) does not disable `zeph-db/default` when `postgres` is enabled.

**Recommendation**: (1) The `postgres` feature in the root `Cargo.toml` must
explicitly disable the default: `postgres = ["zeph-db/postgres",
"zeph-db/default-features-off"]` -- but Cargo does not support negative features.
Instead, restructure: remove `sqlite` from `zeph-db` default features entirely
and make both opt-in, with the root `Cargo.toml` default including
`zeph-db/sqlite`. (2) Add a CI job comment explaining `--all-features` is
incompatible. (3) Consider using the `mutually_exclusive_features` crate pattern
instead.

### S2. `LazyLock<String>` is not zero-cost on SQLite

**Dimension**: Assumption audit
**Section**: 4.5 (lines 337-354)

The spec claims: "For the SQLite feature, `sql!()` returns the literal `&str`
directly, so `LazyLock` is unnecessary (the optimizer eliminates it)."

This is incorrect. `LazyLock<String>` always heap-allocates a `String` when first
accessed, even if the input is a `&'static str`. The compiler cannot eliminate the
`LazyLock` machinery -- it includes an `OnceLock`, an atomic state, and the
closure. The `.to_string()` on line 349 forces a heap allocation regardless of
feature flag.

For SQLite, `sql!("...")` expands to the literal `&str`. But the `LOAD_HISTORY_SQL`
static (line 342) wraps it in `LazyLock::new(|| sql!("...").to_string())`. This
allocates a `String` on first access and keeps it alive for the program's
lifetime. With hundreds of queries, this is hundreds of unnecessary heap
allocations at startup.

**Impact**: Not a correctness bug, but contradicts the spec's "zero-cost" claim
and adds unnecessary memory overhead and startup latency for the SQLite path.

**Recommendation**: For SQLite, query statics should be `&'static str` constants,
not `LazyLock<String>`. Use conditional compilation:
```rust
#[cfg(feature = "sqlite")]
static LOAD_HISTORY_SQL: &str = "SELECT ...";
#[cfg(feature = "postgres")]
static LOAD_HISTORY_SQL: LazyLock<String> = LazyLock::new(|| rewrite_placeholders("SELECT ..."));
```

### S3. `GlobalScope` has no authorization boundary

**Dimension**: Assumption audit
**Section**: 18.5 (lines 1784-1803)

`GlobalScope::new(pool: DbPool)` takes a raw `DbPool` and bypasses all
agent-scoped filtering. The spec says it is "constructed explicitly by admin CLI
commands, never by the normal agent loop" (line 2155). But this is a convention
enforced by code review, not by the type system.

Any code with access to a `DbPool` can construct a `GlobalScope`. Since
`AgentScope` exposes `.pool()` (line 1749), any store can extract the raw pool
and construct `GlobalScope::new(scope.pool().clone())`.

The spec's mitigation #3 (Clippy lint, line 2137) is described as "future" and
#5 (PostgreSQL RLS, line 2145) is "optional." Neither exists at implementation
time.

**Impact**: The security boundary between tenant-isolated and global access
depends entirely on developer discipline. A single oversight in any store method
could leak data across agents.

**Recommendation**: (1) Make `GlobalScope::new()` take a proof token that can
only be constructed in the CLI bootstrap module (e.g., a `pub(crate)` type in
`zeph` binary crate). (2) Do not expose `.pool()` on `AgentScope` -- instead,
provide `AgentScope::execute()` / `AgentScope::fetch_all()` etc. that always
inject the agent filter. (3) At minimum, mark `AgentScope::pool()` as
`#[doc(hidden)]` and add a safety comment.

### S4. Graph shared + memory isolated = privacy leak

**Dimension**: Second-order effects
**Section**: 18.3 (line 1508)

The spec defaults `graph_entities` and `graph_edges` to **Shared** mode, while
`conversations` and `messages` are **Isolated**. The knowledge graph is populated
by extracting entities from conversation messages (this is the graph extraction
pipeline in `zeph-memory`).

When Agent A has a private conversation mentioning "Project X merger with Company
Y", graph extraction creates entities "Project X" and "Company Y" with an edge
"merger_with". These entities are written with `agent_id = NULL` (shared mode) and
become visible to Agent B.

Agent B can now query the graph and discover that a merger between Project X and
Company Y was discussed, even though the source conversation is private to Agent
A. The entity names, edge types, and metadata (timestamps, confidence scores)
constitute derived information that leaks the substance of private conversations.

**Impact**: Shared graph mode leaks derived private information across agents.
This is a fundamental tension between "collective knowledge" and "conversation
privacy" that the spec does not address.

**Recommendation**: (1) Add a `source_agent_id` column to `graph_entities` and
`graph_edges` (nullable, for provenance tracking -- distinct from the isolation
`agent_id`). (2) Document the privacy trade-off explicitly: shared graph mode is
only appropriate when all agents belong to the same trust domain. (3) Consider a
"federated" mode where entities are shared but edges referencing private
conversations are not.

### S5. PostgreSQL `CREATE INDEX CONCURRENTLY` inside transaction

**Dimension**: Counterexample hunt
**Section**: 18.4.3 (lines 1650-1681)

The PostgreSQL migration (lines 1650-1674) uses `CREATE INDEX CONCURRENTLY`. The
spec itself notes at line 1677-1681 that this cannot run inside a transaction, and
`sqlx::migrate!` runs each migration inside a transaction.

The spec suggests splitting into `050b_agent_identity_indexes.sql` or using
regular `CREATE INDEX`. However, the actual migration code shown (lines
1650-1674) still uses `CONCURRENTLY`. This is not just a documentation issue -- if
this SQL is used as-is, the migration will fail at runtime with:

```
ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block
```

**Impact**: The PostgreSQL migration as written will fail on execution.

**Recommendation**: Remove `CONCURRENTLY` from the migration SQL. Use regular
`CREATE INDEX` which is acceptable for a one-time migration. If concurrent index
creation is desired for very large tables, split it into a separate
non-transactional migration file and document that `sqlx::migrate!` must be
configured to run that specific migration outside a transaction (which sqlx does
not natively support -- this would require manual execution).

### S6. testcontainers and `--test-threads 1` does not serialize across binaries

**Dimension**: Assumption audit
**Section**: 16.6 (lines 1336-1344)

The CI config uses `--test-threads 1` with `cargo nextest`. The spec comment says
this "ensures containers from different test binaries don't race on shared
ports" (line 1343). This is incorrect.

`cargo nextest` runs each test binary as a separate process. `--test-threads 1`
(or `-j 1`) controls parallelism *within* a single test binary. To serialize
across binaries, you need nextest's `--max-fail` (which stops on first failure
but does not serialize), or more precisely, you need a nextest profile with
`test-threads = 1` at the profile level AND limiting to one test binary at a time
via `threads-required` in the nextest config.

Each test binary that calls `pg_pool()` starts its own PostgreSQL container on a
random port (testcontainers maps random host ports). So port collision is not the
real risk -- Docker resource exhaustion (CPU, memory, container limit) from
multiple containers starting simultaneously is.

**Impact**: CI may flake due to Docker resource exhaustion when multiple test
binaries start PostgreSQL containers in parallel.

**Recommendation**: Use nextest profile configuration:
```toml
[profile.postgres]
test-threads = 1
[[profile.postgres.overrides]]
filter = 'package(zeph-db) | package(zeph-memory)'
threads-required = 2  # serialize heavy tests
```
Or consolidate all PostgreSQL integration tests into a single test binary.

---

## MINOR Findings

### M1. `AgentId` rejects Unicode and does not document hostname failure path

**Dimension**: Completeness check
**Section**: 18.2 (lines 1398-1405)

The regex `^[a-z0-9][a-z0-9_-]{0,63}$` rejects any hostname containing Unicode
characters, dots (e.g., `host.example.com`), or uppercase characters that were
not lowered. The hostname fallback (line 1404) translates non-matching characters
to `-`, but the validation on line 1405 rejects the result if it starts with `-`
(e.g., hostname `.local` becomes `-local`).

Also, `hostname::get()` can fail on some platforms. The `default_agent_id()`
function (line 1940) falls back to `"default"` if hostname resolution fails, but
`AgentId::parse()` is called later in bootstrap (line 2016). If the user sets an
invalid `[agent] id` in config, startup fails with `AgentIdError`. The error
message is clear, but the spec does not mention this failure mode in the "Impact
on Existing SQLite Deployment" section (18.8), which claims "No user action
required."

**Impact**: Minor. Most deployments will use `"default"` or a manually-set ID.
Edge case for containerized environments with unusual hostnames.

**Recommendation**: (1) Document that dots in hostnames are converted to `-`.
(2) Add a fallback chain: config -> sanitized hostname -> "default" in
`AgentId::parse`, so startup never fails due to agent ID. (3) Consider allowing
dots in the regex for hostname compatibility.

### M2. Migration parity check is structurally weak

**Dimension**: Completeness check
**Section**: 11.4 (lines 940-944)

The spec proposes: (a) same file count, (b) a `verify_migration_parity.sh` that
compares table/column names, (c) PR checklist.

File count is trivially satisfiable with different content. The shell script is
undefined -- it "compares table/column names" but the spec does not specify how
(parsing SQL DDL is non-trivial). PR checklist is a human-dependent control.

**Impact**: Migration drift can occur silently. The schemas could diverge in
column types, constraints, or index definitions without detection.

**Recommendation**: (1) Run both migration sets against their respective backends
in CI and compare the resulting schema catalogs (`information_schema.columns` for
PostgreSQL, `pragma_table_info` for SQLite). (2) Generate a normalized schema
diff as a CI artifact. This is more robust than DDL text comparison.

### M3. `bool_val` return type divergence

**Dimension**: Counterexample hunt
**Section**: 4.4 (lines 271-273)

`Dialect::bool_val(true)` returns `i64` on SQLite and `bool` on PostgreSQL.
These are different types. Any code calling `Dialect::bool_val(x)` cannot be
generic -- it must be behind `#[cfg]` or use a trait-based approach. The spec
does not acknowledge this type mismatch.

**Impact**: Minor. Callers will discover this at compile time. But it makes the
`Dialect` API asymmetric and potentially surprising.

**Recommendation**: Document the type divergence. Consider returning a wrapper
type `DbBool` that implements `Encode` for both backends, or remove `bool_val`
and let sqlx's `Encode` impl handle the mapping (which it already does for bind
parameters).

### M4. `Dialect::ilike()` semantic mismatch

**Dimension**: Counterexample hunt
**Section**: 4.4 (lines 241-248)

SQLite's `COLLATE NOCASE` is a collation applied to a column/expression.
PostgreSQL's `LOWER(col)` is a function call. These have different SQL grammar
positions:

- SQLite: `WHERE name = ? COLLATE NOCASE`
- PostgreSQL: `WHERE LOWER(name) = LOWER(?)`

The `ilike()` function returns `"{col} COLLATE NOCASE"` for SQLite and
`"LOWER({col})"` for PostgreSQL. But the PostgreSQL version only wraps the
column, not the comparison value. A query using `WHERE {ilike("name")} = ?` would
be `WHERE LOWER(name) = ?` on PostgreSQL -- the bound value is NOT lowered.

**Impact**: Case-insensitive comparison will be broken on PostgreSQL unless the
caller also lowercases the bind parameter.

**Recommendation**: Either (a) have `ilike()` return a full comparison expression
including both sides, or (b) document that callers must `LOWER()` the bind
parameter on PostgreSQL, or (c) use PostgreSQL's `ILIKE` operator instead.

---

## Strengths

1. **Compile-time monomorphization approach** is the right call. It avoids the
   runtime dispatch overhead and type-erasure limitations of `sqlx::Any`, while
   keeping business logic unified.

2. **Thorough SQLite inventory** (Section 2.1) maps every crate, every pool
   source, every SQLite-specific feature. This level of analysis prevents
   surprises during migration.

3. **Phased implementation plan** (Section 10) is well-sequenced: Phase 1 is
   non-breaking, Phase 2 adds PostgreSQL, Phase 3 adds tooling. Each phase has
   clear scope and can ship independently.

4. **Agent identity model** (Section 18) is thoughtfully designed. The `AgentId`
   newtype with validation, the `AgentScope` pre-binding pattern, and the
   configurable isolation modes are all sound architectural choices.

5. **Migration strategy** (two separate directories) correctly avoids the
   fragility of conditional DDL in a single migration file.

6. **Advisory lock analysis** (Section 18.7.2) correctly identifies that sqlx
   already handles concurrent PostgreSQL migrations, answering its own open
   question with evidence from the sqlx source.

7. **Defense-in-depth for data leakage** (Section 18.9.3) lists five
   progressively stronger mitigations, from type-system nudging to PostgreSQL
   RLS. The layered approach is appropriate.

---

## Open Questions for Authors

1. **JSON operators**: Has the team inventoried which PostgreSQL-side queries will
   use JSONB operators (`?`, `?|`, `?&`, `@>`)? If any are planned, the `sql!`
   macro design must change before implementation begins.

2. **`--all-features` story**: What is the plan for CI jobs and developer
   workflows that rely on `--all-features`? Will documentation explicitly state
   this is unsupported?

3. **Graph privacy model**: Is the team comfortable with the trade-off that shared
   graph mode leaks derived information from private conversations? Has this been
   discussed with stakeholders who might deploy multi-agent configurations?

4. **Migration rollback**: The spec does not address rollback for migration 050.
   If the agent identity migration causes issues, what is the rollback procedure?
   `ALTER TABLE DROP COLUMN` is supported in SQLite 3.35+ and PostgreSQL, but
   dropping columns with data is destructive.

5. **Scheduled jobs and agent_id**: Section 18.4.4 says `scheduled_jobs` has
   inline DDL in `zeph-scheduler`. The migration in Section 18.4.2 does not
   include `scheduled_jobs` (it is listed as "always isolated" but not in the
   `ALTER TABLE` list). Is this intentional, or will `scheduled_jobs` be migrated
   to `zeph-db` first?

6. **`vector_collections` and `vector_points` as shared**: These are listed as
   Shared (nullable `agent_id`) in Section 18.4.2, but Section 18.3 lists them
   under **Isolated** (`embeddings_metadata`, `vector_points`,
   `vector_collections`). This is a contradiction. Which is correct?

7. **PostgreSQL connection string in vault**: Section 6.1 shows
   `ZEPH_DATABASE_URL` resolved from vault. But the vault resolution happens at
   startup in `zeph-core`. If `zeph-db` pool construction (which needs the URL)
   happens before vault resolution, there is a chicken-and-egg problem. What is
   the resolution order?
