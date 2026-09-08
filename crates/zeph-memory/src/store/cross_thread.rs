// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Generic namespaced cross-thread key-value store (spec-080, #6363).
//!
//! `LangGraph` `Store` parity: a `put`/`get`/`delete`/`list`/`search` primitive addressable
//! by `(owner_key, namespace, key)`, distinct from every other sub-store in this crate —
//! `preferences.rs` is global (no owner/namespace scope), `persona.rs` carries only
//! session-id *provenance*, and `semantic/cross_session.rs` is a search surface, not an
//! addressable KV. This is the shared-state channel `zeph-orchestration`'s `Command.update`
//! writes into (via `zeph-core`, per spec-080 §5.1 — `zeph-orchestration` itself never
//! depends on `zeph-memory`).
//!
//! Every method takes `owner_key` as the first parameter and every query filters on it —
//! no method can read or write a row belonging to a different `owner_key` (FR-A-006).

use tracing::Instrument as _;
use zeph_db::ActiveDialect;
#[allow(unused_imports)]
use zeph_db::sql;

use super::SqliteStore;
use crate::error::MemoryError;

/// A single row of the cross-thread store.
///
/// `value` is an opaque JSON payload — this crate does not interpret its contents, only
/// persists and returns it. `version` starts at `1` and is incremented on every successful
/// `store_put`, enabling optimistic-concurrency writes via `expected_version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreItem {
    pub owner_key: String,
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    /// Identity of the code path that supplied a `writer_id` on the most recent write that
    /// specified one (issue #6773); an anonymous write preserves whatever was already here
    /// rather than clearing it. `None` only when no write to this row has ever supplied one
    /// (pre-migration-116 rows, or every write so far was anonymous).
    pub writer_id: Option<String>,
}

type StoreItemTuple = (
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    Option<String>,
);

/// Options for [`SqliteStore::store_put`], collapsing what would otherwise be five trailing
/// scalar parameters (clippy `too_many_arguments` fires above 7 args including `self`).
///
/// Construct via [`Self::new`] then chain [`Self::with_expected_version`] and/or
/// [`Self::with_writer`] as needed.
///
/// # Examples
///
/// ```rust
/// use zeph_memory::store::StorePutOptions;
///
/// let opts = StorePutOptions::new(65536, 256).with_writer("task:42");
/// assert_eq!(opts.max_value_bytes, 65536);
/// assert_eq!(opts.max_namespace_rows, 256);
/// assert_eq!(opts.writer_id, Some("task:42"));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct StorePutOptions<'a> {
    /// Reject writes whose `value` exceeds this UTF-8 byte length (FR-A-005).
    pub max_value_bytes: usize,
    /// Max rows retained per `(owner_key, namespace)`; `0` = unlimited (same sentinel as
    /// `store_list`'s `limit`). Overflow evicts oldest-first after a confirmed successful
    /// write (#6774).
    pub max_namespace_rows: usize,
    /// Compare-then-write gate — see [`SqliteStore::store_put`]'s doc comment.
    pub expected_version: Option<i64>,
    /// Identity of the writing code path, recorded as last-writer provenance (#6773).
    pub writer_id: Option<&'a str>,
}

impl<'a> StorePutOptions<'a> {
    /// Build options with no version gate and no writer identity set.
    #[must_use]
    pub fn new(max_value_bytes: usize, max_namespace_rows: usize) -> Self {
        Self {
            max_value_bytes,
            max_namespace_rows,
            expected_version: None,
            writer_id: None,
        }
    }

    /// Require the existing row to be at exactly this `version` (optimistic concurrency).
    #[must_use]
    pub fn with_expected_version(mut self, version: i64) -> Self {
        self.expected_version = Some(version);
        self
    }

    /// Record `writer_id` as the identity of the writing code path (#6773).
    #[must_use]
    pub fn with_writer(mut self, writer_id: &'a str) -> Self {
        self.writer_id = Some(writer_id);
        self
    }
}

/// Row ordering for [`SqliteStore::store_list`] (GitHub #6768).
///
/// `KeyAsc` is the original, deterministic listing order every explicit LLM- or
/// operator-driven call (`/store list`, the `store_list` slash command) must keep using —
/// reproducible pagination matters more than recency there. `RecentFirst` exists solely
/// for the ambient, unauthenticated, multi-writer `<shared-state>` read
/// (`append_shared_state_block`, `zeph-core`): under `KeyAsc`, a writer choosing
/// low-sorting keys (`"0000"`, `"0001"`, ...) can write them once and permanently occupy
/// every slot under the row cap, starving every other writer's keys out of the truncated
/// view forever.
///
/// `RecentFirst` closes the *write-once* version of that attack (a one-time flood of
/// low-sorting keys ages out as soon as any honest node writes), but does **not** close a
/// *sustained-write* attacker: `store_put` with `expected_version = None` is an
/// unconditional upsert that refreshes `updated_at` on every call
/// (`SqliteStore::store_put`), so a compromised node re-writing its own `SHARED_STATE_MAX_ROWS`
/// keys in a loop keeps them all newer than any write-once honest key, starving it just as
/// permanently as before (GitHub #6768's sustained-write case, still open). Per-writer
/// provenance is recorded (`writer_id`, #6773) so a cross-writer overwrite is at least
/// detectable and attributable, but the fairness quota that would *consume* that provenance
/// to durably close the sustained-write case is still not implemented.
///
/// `RecentFirst`'s second, independent residual gap: `SQLite`'s `updated_at` is
/// `datetime('now')`, i.e. second-granularity (`crates/zeph-db/src/dialect.rs`). Rows
/// written within the same second tie on `updated_at` and fall back to `(namespace, key)`
/// order, so a flood landing in the same second as an honest write can still win that one
/// second — accepted as a residual limitation rather than switching `SQLite` to sub-second
/// timestamps, which would diverge from migration 110's column `DEFAULT` and every other
/// table's timestamp convention in this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StoreListOrder {
    /// `ORDER BY namespace, key` — lexicographic, deterministic across repeated calls.
    #[default]
    KeyAsc,
    /// `ORDER BY updated_at DESC, namespace ASC, key ASC` — most-recently-written rows
    /// first, so a row cap and a downstream byte cap evict the same (oldest) rows.
    RecentFirst,
}

fn item_from_tuple(t: StoreItemTuple) -> StoreItem {
    StoreItem {
        owner_key: t.0,
        namespace: t.1,
        key: t.2,
        value: t.3,
        version: t.4,
        created_at: t.5,
        updated_at: t.6,
        writer_id: t.7,
    }
}

/// `created_at`/`updated_at` are `TIMESTAMPTZ` on Postgres (`TEXT` on `SQLite`); project both
/// through `Dialect::select_as_text` so they decode into the `String` fields on [`StoreItem`],
/// mirroring `acp_sessions.rs`'s fix for the same mismatch.
fn select_columns() -> String {
    let created_at_sel = <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
    let updated_at_sel = <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("updated_at");
    format!(
        "owner_key, namespace, key, value, version, {created_at_sel}, {updated_at_sel}, writer_id"
    )
}

impl SqliteStore {
    /// Insert or update a cross-thread store row (FR-A-002..004).
    ///
    /// Without `expected_version`, this is a plain upsert: a new row starts at `version = 1`;
    /// an existing row has its `value` replaced, `version` incremented, and `updated_at`
    /// refreshed (FR-A-004).
    ///
    /// With `expected_version = Some(v)`, the write is a compare-then-write: it only
    /// succeeds if a row exists at exactly that version, in one statement (`WHERE version =
    /// ?`, checked via `RETURNING`) — never a silent overwrite. A mismatch, including the
    /// case where no row exists yet at all, returns [`MemoryError::VersionConflict`]
    /// (FR-A-003).
    ///
    /// `opts.max_value_bytes` rejects the write outright when `value`'s UTF-8 byte length
    /// exceeds it (FR-A-005) rather than truncating — callers pass
    /// `[memory.store].max_value_bytes` from config; this method itself does not depend on
    /// `zeph-config`.
    ///
    /// `opts.max_namespace_rows` bounds how many rows a single `(owner_key, namespace)` may
    /// hold; once this write has actually landed, if the namespace now exceeds the cap the
    /// oldest rows (by `updated_at`, excluding the row just written) are evicted first
    /// (#6774) — a growth bound, not a hard invariant: concurrent writers can each observe
    /// the namespace over cap and each run an eviction pass, but eviction runs *after* the
    /// write via a single atomic count-and-delete statement (mirroring
    /// `SqliteStore::prune_skill_versions`'s idiom), so each pass recomputes the true current
    /// count rather than acting on an earlier snapshot — a later pass sees the excess an
    /// earlier pass already removed and evicts nothing further, closing the double-eviction
    /// race two concurrent writers to the *same new key* used to hit (issue #6773 review).
    /// Evicting only after a confirmed successful write also means eviction can never run for
    /// a write that ends up failing (a stale `expected_version` CAS, or any other rejection):
    /// there is no "evict for nothing" case to special-case.
    ///
    /// `opts.writer_id`, when set, is recorded as this row's last-writer provenance (#6773).
    /// When `opts.writer_id` is `None` (an anonymous write), any existing `writer_id` is
    /// *preserved*, not cleared (`COALESCE(?, writer_id)` in the underlying `UPDATE`) — an
    /// anonymous write must never erase a prior writer's attribution. Both of the following are
    /// logged via `tracing::warn!` **after** the write is confirmed to have applied — not at
    /// probe time, so a stale CAS that ultimately fails with `VersionConflict` never logs a
    /// false-positive warning (issue #6773 review): the row already carried a *different*
    /// writer's id and this write supplies its own (detected and attributed, never rejected —
    /// a later node legitimately updating shared state is the designed semantics, spec-080 §3,
    /// not an attack to block); or the row already carried a writer's id and this write is
    /// anonymous, so `COALESCE` preserves that id under content the prior writer did not
    /// actually write (review round 4, N4) — the row's `writer_id` does not change in this
    /// second case, the warning exists only so the now-stale attribution is visible.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::InvalidInput`] if `value` exceeds `opts.max_value_bytes`,
    /// [`MemoryError::VersionConflict`] on an `opts.expected_version` mismatch, or a database
    /// error if the query fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn example() -> Result<(), zeph_memory::MemoryError> {
    /// use zeph_memory::store::{SqliteStore, StorePutOptions};
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// let item = store
    ///     .store_put(
    ///         "local",
    ///         "orch/graph-1",
    ///         "finding",
    ///         "{\"x\":1}",
    ///         StorePutOptions::new(65536, 256).with_writer("task:1"),
    ///     )
    ///     .await?;
    /// assert_eq!(item.version, 1);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn store_put(
        &self,
        owner_key: &str,
        namespace: &str,
        key: &str,
        value: &str,
        opts: StorePutOptions<'_>,
    ) -> Result<StoreItem, MemoryError> {
        if value.len() > opts.max_value_bytes {
            return Err(MemoryError::InvalidInput(format!(
                "store_put value for namespace={namespace:?} key={key:?} is {} bytes, \
                 exceeds max_value_bytes={}",
                value.len(),
                opts.max_value_bytes
            )));
        }

        // Snapshot the pre-write writer_id only to compare against after a confirmed success —
        // never used to gate or short-circuit the write itself, so a race here can only ever
        // affect the accuracy of the cross-writer/anonymous-overwrite warnings below, never
        // correctness of the write. Fetched unconditionally (not only when
        // `opts.writer_id.is_some()`) so an anonymous write can still be compared against an
        // existing writer (review round 4, N4).
        let existing_writer = self.existing_writer_id(owner_key, namespace, key).await?;

        let now = <ActiveDialect as zeph_db::dialect::Dialect>::NOW;
        let cols = select_columns();

        let row: Option<StoreItemTuple> = if let Some(expected) = opts.expected_version {
            let raw = format!(
                "UPDATE cross_thread_store \
                 SET value = ?, version = version + 1, updated_at = {now}, \
                     writer_id = COALESCE(?, writer_id) \
                 WHERE owner_key = ? AND namespace = ? AND key = ? AND version = ? \
                 RETURNING {cols}"
            );
            let query_sql = zeph_db::rewrite_placeholders(&raw);
            zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
                .bind(value)
                .bind(opts.writer_id)
                .bind(owner_key)
                .bind(namespace)
                .bind(key)
                .bind(expected)
                .fetch_optional(&self.pool)
                .await?
        } else {
            let raw = format!(
                "INSERT INTO cross_thread_store (owner_key, namespace, key, value, writer_id) \
                 VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT(owner_key, namespace, key) DO UPDATE SET \
                   value = excluded.value, \
                   version = cross_thread_store.version + 1, \
                   updated_at = {now}, \
                   writer_id = COALESCE(excluded.writer_id, cross_thread_store.writer_id) \
                 RETURNING {cols}"
            );
            let query_sql = zeph_db::rewrite_placeholders(&raw);
            zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
                .bind(owner_key)
                .bind(namespace)
                .bind(key)
                .bind(value)
                .bind(opts.writer_id)
                .fetch_optional(&self.pool)
                .await?
        };

        let Some(t) = row else {
            return Err(MemoryError::VersionConflict {
                owner_key: owner_key.to_owned(),
                namespace: namespace.to_owned(),
                key: key.to_owned(),
                expected: opts.expected_version.unwrap_or(0),
            });
        };
        let item = item_from_tuple(t);

        // Only warn once the write is confirmed to have actually applied (`row` is `Some`) —
        // a stale CAS whose `expected_version` no longer matches returns `None` above and
        // must never log a false-positive overwrite (issue #6773 review).
        match (existing_writer.as_deref(), opts.writer_id) {
            (Some(prev), Some(new)) if prev != new => {
                tracing::warn!(
                    owner_key,
                    namespace,
                    key,
                    previous_writer = prev,
                    new_writer = new,
                    "cross-thread store row overwritten by a different writer"
                );
            }
            // Anonymous write (review round 4, N4): `COALESCE` preserves `prev` under content
            // this write actually changed, so — unlike the arm above — the row's `writer_id`
            // does NOT change here; the warning exists purely so an operator can see that the
            // attribution is now stale relative to the content.
            (Some(prev), None) => {
                tracing::warn!(
                    owner_key,
                    namespace,
                    key,
                    previous_writer = prev,
                    "cross-thread store row anonymously overwritten; retaining prior writer_id \
                     attribution, which may now be stale"
                );
            }
            _ => {}
        }

        if opts.max_namespace_rows > 0 {
            self.evict_overflow(owner_key, namespace, key, opts.max_namespace_rows)
                .await?;
        }

        Ok(item)
    }

    /// Fetch the current `writer_id` of `(owner_key, namespace, key)`, or `None` if no row
    /// exists yet — called unconditionally by [`Self::store_put`] (every write, including
    /// anonymous ones) to compare against `opts.writer_id` for cross-writer and
    /// anonymous-overwrite detection (#6773). Read-only; never gates or mutates.
    async fn existing_writer_id(
        &self,
        owner_key: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<String>, MemoryError> {
        let raw = "SELECT writer_id FROM cross_thread_store \
                    WHERE owner_key = ? AND namespace = ? AND key = ?";
        let sql = zeph_db::rewrite_placeholders(raw);
        let row: Option<(Option<String>,)> = zeph_db::query_as(sqlx::AssertSqlSafe(sql))
            .bind(owner_key)
            .bind(namespace)
            .bind(key)
            .fetch_optional(&self.pool)
            .instrument(tracing::debug_span!("memory.cross_thread.existing_writer"))
            .await?;
        Ok(row.and_then(|(w,)| w))
    }

    /// Post-write row-cap eviction for [`Self::store_put`] (#6774): when `(owner_key,
    /// namespace)` now holds more than `max_namespace_rows` rows, delete the oldest (by
    /// `updated_at`, excluding `just_written_key`) down to the cap, in one atomic
    /// count-and-delete statement — mirrors `SqliteStore::prune_skill_versions`'s idiom.
    ///
    /// Called only after `store_put`'s write is confirmed to have landed, so eviction can
    /// never run for a write that ends up failing. Fusing the count into the same statement
    /// as the delete (rather than a separate round-trip whose result is cached in Rust) means
    /// each call recomputes the true current count at execution time: two concurrent writers
    /// that both push the namespace over cap will each attempt an eviction, but whichever
    /// runs second sees the first's already-applied deletion and evicts nothing further —
    /// closing the double-eviction race a stale, pre-write-cached count previously allowed
    /// (issue #6773 review).
    async fn evict_overflow(
        &self,
        owner_key: &str,
        namespace: &str,
        just_written_key: &str,
        max_namespace_rows: usize,
    ) -> Result<(), MemoryError> {
        let greatest_fn = <ActiveDialect as zeph_db::dialect::Dialect>::GREATEST_FN;
        let max_rows = i64::try_from(max_namespace_rows).unwrap_or(i64::MAX);
        let del_raw = format!(
            "DELETE FROM cross_thread_store \
             WHERE owner_key = ? AND namespace = ? AND key IN ( \
               SELECT key FROM cross_thread_store \
                WHERE owner_key = ? AND namespace = ? AND key <> ? \
                ORDER BY updated_at ASC, key DESC \
                LIMIT {greatest_fn}(0, (SELECT COUNT(*) FROM cross_thread_store \
                  WHERE owner_key = ? AND namespace = ?) - ?))"
        );
        let del_sql = zeph_db::rewrite_placeholders(&del_raw);
        let result = zeph_db::query(sqlx::AssertSqlSafe(del_sql))
            .bind(owner_key)
            .bind(namespace)
            .bind(owner_key)
            .bind(namespace)
            .bind(just_written_key)
            .bind(owner_key)
            .bind(namespace)
            .bind(max_rows)
            .execute(&self.pool)
            .instrument(tracing::debug_span!("memory.cross_thread.evict"))
            .await?;
        if result.rows_affected() > 0 {
            tracing::warn!(
                owner_key,
                namespace,
                evicted = result.rows_affected(),
                max_namespace_rows,
                "cross-thread store namespace at row cap; evicted oldest rows"
            );
        }
        Ok(())
    }

    /// Fetch a single row by `(owner_key, namespace, key)`.
    ///
    /// Returns `Ok(None)` — never an error — when no row exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn example() -> Result<(), zeph_memory::MemoryError> {
    /// use zeph_memory::store::SqliteStore;
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// assert!(store.store_get("local", "orch/graph-1", "finding").await?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn store_get(
        &self,
        owner_key: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<StoreItem>, MemoryError> {
        let cols = select_columns();
        let raw = format!(
            "SELECT {cols} FROM cross_thread_store \
             WHERE owner_key = ? AND namespace = ? AND key = ?"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let row: Option<StoreItemTuple> = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
            .bind(owner_key)
            .bind(namespace)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(item_from_tuple))
    }

    /// Delete a single row by `(owner_key, namespace, key)`.
    ///
    /// Returns `true` iff a row was deleted; `false` when no matching row existed
    /// (not an error).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn example() -> Result<(), zeph_memory::MemoryError> {
    /// use zeph_memory::store::{SqliteStore, StorePutOptions};
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// store
    ///     .store_put(
    ///         "local",
    ///         "orch/graph-1",
    ///         "finding",
    ///         "{\"x\":1}",
    ///         StorePutOptions::new(65536, 0),
    ///     )
    ///     .await?;
    ///
    /// assert!(store.store_delete("local", "orch/graph-1", "finding").await?);
    /// assert!(!store.store_delete("local", "orch/graph-1", "finding").await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn store_delete(
        &self,
        owner_key: &str,
        namespace: &str,
        key: &str,
    ) -> Result<bool, MemoryError> {
        let result = zeph_db::query(sql!(
            "DELETE FROM cross_thread_store WHERE owner_key = ? AND namespace = ? AND key = ?"
        ))
        .bind(owner_key)
        .bind(namespace)
        .bind(key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List rows under a namespace prefix, scoped to `owner_key`.
    ///
    /// `namespace_prefix` matches every namespace starting with it — pass e.g.
    /// `"orch/graph-1"` to match exactly that namespace (and any longer namespace that
    /// starts with it), or a shorter prefix to match several. `order` selects between
    /// `(namespace, key)` lexicographic order and most-recently-written-first — see
    /// [`StoreListOrder`]. Pass `limit = 0` for unlimited.
    ///
    /// Implemented as an explicit `namespace >= lower AND namespace < upper` range scan
    /// rather than `namespace LIKE 'prefix%'` (perf finding NFR-004/5): `SQLite` only
    /// converts a `LIKE`-prefix match into an index range scan when
    /// `PRAGMA case_sensitive_like = ON` is set, which this codebase never sets — without
    /// it, `LIKE` degrades to a full scan of every row under `owner_key`, applying the
    /// prefix filter as a residual row-by-row check (confirmed via `EXPLAIN QUERY PLAN`
    /// against the real migration-110 schema). A plain `>=`/`<` range is index-usable via
    /// `idx_cross_thread_store_owner_ns(owner_key, namespace)` regardless of that pragma,
    /// on both `SQLite` and Postgres, and needs no wildcard-escaping.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn example() -> Result<(), zeph_memory::MemoryError> {
    /// use zeph_memory::store::{SqliteStore, StoreListOrder, StorePutOptions};
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// store
    ///     .store_put(
    ///         "local",
    ///         "orch/graph-1",
    ///         "finding",
    ///         "{\"x\":1}",
    ///         StorePutOptions::new(65536, 0),
    ///     )
    ///     .await?;
    ///
    /// let items = store
    ///     .store_list("local", "orch/graph-1", 0, StoreListOrder::KeyAsc)
    ///     .await?;
    /// assert_eq!(items.len(), 1);
    /// assert_eq!(items[0].key, "finding");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn store_list(
        &self,
        owner_key: &str,
        namespace_prefix: &str,
        limit: usize,
        order: StoreListOrder,
    ) -> Result<Vec<StoreItem>, MemoryError> {
        let cols = select_columns();
        let (limit_clause, limit_bind) = zeph_db::limit_clause(limit as u64);
        let upper = prefix_range_upper_bound(namespace_prefix);
        // The `cross_thread_store.` qualifier on `updated_at` is load-bearing: on
        // Postgres, `select_columns()` projects `updated_at::text` under the output alias
        // `updated_at`, and a bare `ORDER BY updated_at` binds to that text-rendered output
        // column rather than the underlying timestamp column — variable-length fractional
        // seconds and a session-`TimeZone`-dependent offset make that not reliably
        // chronological. A qualified reference always resolves to the source table column.
        let order_clause = match order {
            StoreListOrder::KeyAsc => "namespace, key",
            StoreListOrder::RecentFirst => {
                "cross_thread_store.updated_at DESC, namespace ASC, key ASC"
            }
        };
        let raw = if upper.is_some() {
            format!(
                "SELECT {cols} FROM cross_thread_store \
                 WHERE owner_key = ? AND namespace >= ? AND namespace < ? \
                 ORDER BY {order_clause}{limit_clause}"
            )
        } else {
            format!(
                "SELECT {cols} FROM cross_thread_store \
                 WHERE owner_key = ? AND namespace >= ? \
                 ORDER BY {order_clause}{limit_clause}"
            )
        };
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let mut query = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
            .bind(owner_key)
            .bind(namespace_prefix.to_owned());
        if let Some(ref upper) = upper {
            query = query.bind(upper.clone());
        }
        if let Some(lim) = limit_bind {
            query = query.bind(lim);
        }
        let rows: Vec<StoreItemTuple> = query.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(item_from_tuple).collect())
    }

    /// List rows under a namespace prefix whose `value` contains `query` (case-sensitive
    /// substring match), scoped to `owner_key`.
    ///
    /// Namespace scoping uses the same index-usable range scan as [`Self::store_list`];
    /// `value` keyword matching stays `LIKE '%query%'` (a substring search has no
    /// prefix-range equivalent regardless of the pragma).
    ///
    /// MVP keyword search only — no embedding index (`[memory.store] search_provider` is
    /// reserved for a future semantic-search extension, spec-080 §1 Out of Scope). Pass
    /// `limit = 0` for unlimited.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn example() -> Result<(), zeph_memory::MemoryError> {
    /// use zeph_memory::store::{SqliteStore, StorePutOptions};
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// store
    ///     .store_put(
    ///         "local",
    ///         "orch/graph-1",
    ///         "finding",
    ///         "{\"summary\":\"needle in haystack\"}",
    ///         StorePutOptions::new(65536, 0),
    ///     )
    ///     .await?;
    ///
    /// let hits = store.store_search("local", "orch/graph-1", "needle", 0).await?;
    /// assert_eq!(hits.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn store_search(
        &self,
        owner_key: &str,
        namespace_prefix: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<StoreItem>, MemoryError> {
        let cols = select_columns();
        let (limit_clause, limit_bind) = zeph_db::limit_clause(limit as u64);
        let upper = prefix_range_upper_bound(namespace_prefix);
        let raw = if upper.is_some() {
            format!(
                "SELECT {cols} FROM cross_thread_store \
                 WHERE owner_key = ? AND namespace >= ? AND namespace < ? \
                 AND value LIKE ? ESCAPE '\\' \
                 ORDER BY namespace, key{limit_clause}"
            )
        } else {
            format!(
                "SELECT {cols} FROM cross_thread_store \
                 WHERE owner_key = ? AND namespace >= ? AND value LIKE ? ESCAPE '\\' \
                 ORDER BY namespace, key{limit_clause}"
            )
        };
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let mut q = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
            .bind(owner_key)
            .bind(namespace_prefix.to_owned());
        if let Some(ref upper) = upper {
            q = q.bind(upper.clone());
        }
        q = q.bind(like_contains(query));
        if let Some(lim) = limit_bind {
            q = q.bind(lim);
        }
        let rows: Vec<StoreItemTuple> = q.fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(item_from_tuple).collect())
    }
}

/// Compute an exclusive upper bound for a `namespace >= prefix AND namespace < upper`
/// index-usable range scan matching every string that starts with `prefix` — the same
/// set `namespace LIKE 'prefix%'` would match, without depending on
/// `PRAGMA case_sensitive_like` (see [`SqliteStore::store_list`]'s doc comment).
///
/// Increments the *last `char`* (not byte) of `prefix` by one Unicode scalar value,
/// skipping the UTF-16 surrogate range (not valid as a standalone `char`) — this keeps
/// the result valid UTF-8 unconditionally, unlike a raw byte increment. Falls back to
/// bumping the next-to-last character when the last one is already `char::MAX`
/// (practically never happens for `orch/{graph_id}`-shaped namespace prefixes).
///
/// Returns `None` when `prefix` is empty, or every character is `char::MAX` — callers
/// fall back to an unbounded `namespace >= prefix` scan in that case, which still
/// matches every namespace (an empty prefix is a match-everything prefix by definition).
fn prefix_range_upper_bound(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let mut next = last as u32 + 1;
        if (0xD800..=0xDFFF).contains(&next) {
            next = 0xE000; // skip the surrogate range, invalid as a standalone char
        }
        if let Some(incremented) = char::from_u32(next) {
            chars.push(incremented);
            return Some(chars.into_iter().collect());
        }
        // `last` was char::MAX (or bumped into it) — drop it, try the previous char.
    }
    None
}

/// Escape `%`/`_` LIKE wildcards in a caller-supplied substring, then wrap it in `%...%`.
fn like_contains(substring: &str) -> String {
    format!("%{}%", escape_like(substring))
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    const MAX_BYTES: usize = 65536;

    /// Default options for tests not exercising the row cap or write provenance: no cap
    /// (`max_namespace_rows = 0`), no writer identity.
    fn opts() -> StorePutOptions<'static> {
        StorePutOptions::new(MAX_BYTES, 0)
    }

    // ── prefix_range_upper_bound (perf finding NFR-004/5, index-usable range scan) ──

    #[test]
    fn prefix_range_upper_bound_bumps_last_char() {
        assert_eq!(
            prefix_range_upper_bound("orch/g1").as_deref(),
            Some("orch/g2")
        );
    }

    #[test]
    fn prefix_range_upper_bound_bumps_slash_to_digit_zero() {
        // '/' (0x2F) + 1 = '0' (0x30) — confirms the bound genuinely brackets every
        // "orch/..." continuation, not just alphabetic ones.
        assert_eq!(prefix_range_upper_bound("orch/").as_deref(), Some("orch0"));
    }

    #[test]
    fn prefix_range_upper_bound_empty_prefix_returns_none() {
        assert_eq!(prefix_range_upper_bound(""), None);
    }

    #[test]
    fn prefix_range_upper_bound_brackets_every_continuation_and_nothing_else() {
        let prefix = "ns";
        let upper = prefix_range_upper_bound(prefix).unwrap();
        assert!(
            prefix < upper.as_str(),
            "prefix itself must fall in [prefix, upper)"
        );
        assert!(
            format!("{prefix}-anything") < upper,
            "any continuation of prefix must sort before upper bound"
        );
        assert!(
            "nt" >= upper.as_str(),
            "an unrelated namespace one step past the prefix family must not be < upper"
        );
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let s = store().await;
        let item = s
            .store_put("local", "orch/g1", "finding", "{\"x\":1}", opts())
            .await
            .unwrap();
        assert_eq!(item.version, 1);
        assert_eq!(item.value, "{\"x\":1}");

        let fetched = s
            .store_get("local", "orch/g1", "finding")
            .await
            .unwrap()
            .expect("row must exist");
        assert_eq!(fetched.value, "{\"x\":1}");
        assert_eq!(fetched.version, 1);
        assert_eq!(fetched.owner_key, "local");
        assert_eq!(fetched.namespace, "orch/g1");
        assert_eq!(fetched.key, "finding");
    }

    #[tokio::test]
    async fn put_upserts_and_bumps_version() {
        let s = store().await;
        s.store_put("local", "ns", "k", "v1", opts()).await.unwrap();
        let updated = s.store_put("local", "ns", "k", "v2", opts()).await.unwrap();
        assert_eq!(updated.version, 2);
        assert_eq!(updated.value, "v2");

        let fetched = s.store_get("local", "ns", "k").await.unwrap().unwrap();
        assert_eq!(fetched.value, "v2");
        assert_eq!(fetched.version, 2);
    }

    /// US-002 (spec-080): same key in two namespaces never collides.
    #[tokio::test]
    async fn namespace_isolation() {
        let s = store().await;
        s.store_put("local", "ns-a", "k", "value-a", opts())
            .await
            .unwrap();
        s.store_put("local", "ns-b", "k", "value-b", opts())
            .await
            .unwrap();

        let a = s.store_get("local", "ns-a", "k").await.unwrap().unwrap();
        let b = s.store_get("local", "ns-b", "k").await.unwrap().unwrap();
        assert_eq!(a.value, "value-a");
        assert_eq!(b.value, "value-b");
    }

    /// FR-A-006: two distinct `owner_key`s cannot read or overwrite each other's rows
    /// under the same `(namespace, key)`.
    #[tokio::test]
    async fn owner_key_isolation() {
        let s = store().await;
        s.store_put("owner-a", "ns", "k", "value-a", opts())
            .await
            .unwrap();
        s.store_put("owner-b", "ns", "k", "value-b", opts())
            .await
            .unwrap();

        let a = s.store_get("owner-a", "ns", "k").await.unwrap().unwrap();
        let b = s.store_get("owner-b", "ns", "k").await.unwrap().unwrap();
        assert_eq!(a.value, "value-a");
        assert_eq!(b.value, "value-b");

        assert!(s.store_delete("owner-a", "ns", "k").await.unwrap());
        // owner-b's row must survive owner-a's delete.
        assert!(s.store_get("owner-b", "ns", "k").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn version_conflict_on_stale_expected_version() {
        let s = store().await;
        let first = s.store_put("local", "ns", "k", "v1", opts()).await.unwrap();
        assert_eq!(first.version, 1);

        // Correct expected_version succeeds.
        let second = s
            .store_put("local", "ns", "k", "v2", opts().with_expected_version(1))
            .await
            .unwrap();
        assert_eq!(second.version, 2);

        // Stale expected_version (the row is now at version 2) is rejected.
        let err = s
            .store_put("local", "ns", "k", "v3", opts().with_expected_version(1))
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::VersionConflict { .. }));

        // The rejected write must not have applied.
        let fetched = s.store_get("local", "ns", "k").await.unwrap().unwrap();
        assert_eq!(fetched.value, "v2");
        assert_eq!(fetched.version, 2);
    }

    #[tokio::test]
    async fn version_conflict_when_row_does_not_exist() {
        let s = store().await;
        let err = s
            .store_put(
                "local",
                "ns",
                "no-such-key",
                "v",
                opts().with_expected_version(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::VersionConflict { .. }));
    }

    #[tokio::test]
    async fn put_rejects_value_exceeding_max_bytes() {
        let s = store().await;
        let err = s
            .store_put("local", "ns", "k", "0123456789", StorePutOptions::new(5, 0))
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidInput(_)));

        // The rejected write must not have created a row.
        assert!(s.store_get("local", "ns", "k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_returns_false_for_missing_row() {
        let s = store().await;
        assert!(!s.store_delete("local", "ns", "no-such").await.unwrap());
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_row() {
        let s = store().await;
        assert!(
            s.store_get("local", "ns", "no-such")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_by_namespace_prefix() {
        let s = store().await;
        s.store_put("local", "orch/g1", "a", "1", opts())
            .await
            .unwrap();
        s.store_put("local", "orch/g1", "b", "2", opts())
            .await
            .unwrap();
        s.store_put("local", "orch/g2", "c", "3", opts())
            .await
            .unwrap();

        let g1 = s
            .store_list("local", "orch/g1", 0, StoreListOrder::KeyAsc)
            .await
            .unwrap();
        assert_eq!(g1.len(), 2);
        assert!(g1.iter().all(|i| i.namespace == "orch/g1"));

        let all_orch = s
            .store_list("local", "orch/", 0, StoreListOrder::KeyAsc)
            .await
            .unwrap();
        assert_eq!(all_orch.len(), 3);
    }

    #[tokio::test]
    async fn list_respects_limit() {
        let s = store().await;
        for i in 0..5u8 {
            s.store_put("local", "ns", &format!("k{i}"), "v", opts())
                .await
                .unwrap();
        }
        let limited = s
            .store_list("local", "ns", 2, StoreListOrder::KeyAsc)
            .await
            .unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[tokio::test]
    async fn list_empty_namespace_returns_empty_vec() {
        let s = store().await;
        let rows = s
            .store_list("local", "no/such/ns", 0, StoreListOrder::KeyAsc)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    /// M1 (GitHub #6768): `RecentFirst` returns rows ordered by `updated_at DESC`,
    /// independent of key lexicographic order — the opposite of `KeyAsc`'s natural
    /// insertion order for these keys, so the test only passes if the ordering actually
    /// switched.
    #[tokio::test]
    async fn recent_first_orders_by_updated_at_descending() {
        let s = store().await;
        s.store_put("local", "orch/g1", "z-oldest", "v", opts())
            .await
            .unwrap();
        s.store_put("local", "orch/g1", "m-middle", "v", opts())
            .await
            .unwrap();
        s.store_put("local", "orch/g1", "a-newest", "v", opts())
            .await
            .unwrap();

        // Force distinct, deterministic `updated_at` values via a raw UPDATE — avoids a
        // wall-clock sleep and works around SQLite's second-granularity clock.
        for (key, ts) in [
            ("z-oldest", "2026-01-01T00:00:00Z"),
            ("m-middle", "2026-01-01T00:00:01Z"),
            ("a-newest", "2026-01-01T00:00:02Z"),
        ] {
            zeph_db::query(sql!(
                "UPDATE cross_thread_store SET updated_at = ? WHERE key = ?"
            ))
            .bind(ts)
            .bind(key)
            .execute(&s.pool)
            .await
            .unwrap();
        }

        let recent = s
            .store_list("local", "orch/g1", 0, StoreListOrder::RecentFirst)
            .await
            .unwrap();
        let keys: Vec<&str> = recent.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["a-newest", "m-middle", "z-oldest"]);

        // `limit` composes with `RecentFirst`: the newest N rows, not the first N by key.
        let top_two = s
            .store_list("local", "orch/g1", 2, StoreListOrder::RecentFirst)
            .await
            .unwrap();
        let top_two_keys: Vec<&str> = top_two.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(top_two_keys, vec!["a-newest", "m-middle"]);
    }

    #[tokio::test]
    async fn search_matches_value_keyword() {
        let s = store().await;
        s.store_put(
            "local",
            "orch/g1",
            "a",
            "{\"finding\":\"needle in haystack\"}",
            opts(),
        )
        .await
        .unwrap();
        s.store_put(
            "local",
            "orch/g1",
            "b",
            "{\"finding\":\"nothing here\"}",
            opts(),
        )
        .await
        .unwrap();

        let hits = s
            .store_search("local", "orch/g1", "needle", 0)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "a");
    }

    #[tokio::test]
    async fn search_scoped_by_namespace_prefix() {
        let s = store().await;
        s.store_put("local", "orch/g1", "a", "needle", opts())
            .await
            .unwrap();
        s.store_put("local", "orch/g2", "b", "needle", opts())
            .await
            .unwrap();

        let hits = s
            .store_search("local", "orch/g1", "needle", 0)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].namespace, "orch/g1");
    }

    #[tokio::test]
    async fn like_wildcards_in_query_are_escaped() {
        let s = store().await;
        s.store_put("local", "ns", "a", "50% off", opts())
            .await
            .unwrap();
        s.store_put("local", "ns", "b", "50x off", opts())
            .await
            .unwrap();

        // A literal "%" in the search query must not act as a wildcard.
        let hits = s.store_search("local", "ns", "50%", 0).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "a");
    }

    // ── writer_id provenance and max_namespace_rows eviction (#6773/#6774) ──────────

    #[tokio::test]
    async fn store_put_records_writer_id() {
        let s = store().await;
        let item = s
            .store_put("local", "ns", "k", "v", opts().with_writer("task:1"))
            .await
            .unwrap();
        assert_eq!(item.writer_id.as_deref(), Some("task:1"));

        let fetched = s.store_get("local", "ns", "k").await.unwrap().unwrap();
        assert_eq!(fetched.writer_id.as_deref(), Some("task:1"));
    }

    #[tokio::test]
    async fn store_put_with_no_writer_leaves_writer_id_none() {
        let s = store().await;
        let item = s.store_put("local", "ns", "k", "v", opts()).await.unwrap();
        assert_eq!(item.writer_id, None);
    }

    /// #6773: a write from a different writer than the row's current one must NOT be
    /// rejected — it is detected/attributed (via a `tracing::warn!`, not asserted here) and
    /// the write proceeds, updating `writer_id` to the new writer. Rejecting would make a
    /// namespace one writer can permanently lock out another from.
    #[tokio::test]
    async fn cross_writer_overwrite_is_not_rejected_and_updates_writer_id() {
        let s = store().await;
        s.store_put("local", "ns", "k", "v1", opts().with_writer("task:1"))
            .await
            .unwrap();

        let updated = s
            .store_put("local", "ns", "k", "v2", opts().with_writer("task:2"))
            .await
            .unwrap();
        assert_eq!(updated.value, "v2");
        assert_eq!(updated.writer_id.as_deref(), Some("task:2"));

        let fetched = s.store_get("local", "ns", "k").await.unwrap().unwrap();
        assert_eq!(fetched.writer_id.as_deref(), Some("task:2"));
    }

    /// #6773 (critic finding S5): an anonymous write (no `writer_id` supplied) must not erase
    /// an existing row's provenance — `COALESCE(?, writer_id)` preserves it. Upsert branch
    /// (`expected_version = None`).
    #[tokio::test]
    async fn anonymous_overwrite_preserves_prior_writer_id_on_upsert() {
        let s = store().await;
        s.store_put("local", "ns", "k", "v1", opts().with_writer("task:1"))
            .await
            .unwrap();

        let updated = s.store_put("local", "ns", "k", "v2", opts()).await.unwrap();
        assert_eq!(updated.value, "v2");
        assert_eq!(
            updated.writer_id.as_deref(),
            Some("task:1"),
            "an anonymous write must not erase prior write provenance"
        );
    }

    /// Same as above, CAS branch (`expected_version = Some(..)`).
    #[tokio::test]
    async fn anonymous_overwrite_preserves_prior_writer_id_on_cas() {
        let s = store().await;
        let first = s
            .store_put("local", "ns", "k", "v1", opts().with_writer("task:1"))
            .await
            .unwrap();

        let updated = s
            .store_put(
                "local",
                "ns",
                "k",
                "v2",
                opts().with_expected_version(first.version),
            )
            .await
            .unwrap();
        assert_eq!(updated.value, "v2");
        assert_eq!(
            updated.writer_id.as_deref(),
            Some("task:1"),
            "an anonymous CAS write must not erase prior write provenance"
        );
    }

    /// #6774: once a namespace holds `max_namespace_rows` rows, writing one more distinct
    /// key evicts the oldest (by `updated_at`) row first, never the newly-written key.
    #[tokio::test]
    async fn row_cap_evicts_oldest_row_first() {
        let s = store().await;
        let capped = StorePutOptions::new(MAX_BYTES, 3);

        for key in ["a", "b", "c"] {
            s.store_put("local", "ns", key, "v", capped).await.unwrap();
        }
        // Force deterministic, distinct updated_at ordering (avoids SQLite's
        // second-granularity clock flaking this test).
        for (key, ts) in [
            ("a", "2026-01-01T00:00:00Z"),
            ("b", "2026-01-01T00:00:01Z"),
            ("c", "2026-01-01T00:00:02Z"),
        ] {
            zeph_db::query(sql!(
                "UPDATE cross_thread_store SET updated_at = ? WHERE key = ?"
            ))
            .bind(ts)
            .bind(key)
            .execute(&s.pool)
            .await
            .unwrap();
        }

        // Writing a 4th distinct key at the cap must evict "a" (the oldest), not "d".
        s.store_put("local", "ns", "d", "v", capped).await.unwrap();

        assert!(
            s.store_get("local", "ns", "a").await.unwrap().is_none(),
            "oldest row must be evicted to stay within max_namespace_rows"
        );
        assert!(s.store_get("local", "ns", "b").await.unwrap().is_some());
        assert!(s.store_get("local", "ns", "c").await.unwrap().is_some());
        assert!(s.store_get("local", "ns", "d").await.unwrap().is_some());
    }

    /// An update to an *existing* key must never trigger eviction — the row cap only
    /// applies when a brand-new key would grow the namespace past the limit.
    #[tokio::test]
    async fn row_cap_does_not_evict_on_update_of_existing_key() {
        let s = store().await;
        let capped = StorePutOptions::new(MAX_BYTES, 2);
        s.store_put("local", "ns", "a", "v1", capped).await.unwrap();
        s.store_put("local", "ns", "b", "v1", capped).await.unwrap();

        // Namespace is already at the cap (2 rows); updating "a" again must not evict "b".
        s.store_put("local", "ns", "a", "v2", capped).await.unwrap();

        assert!(s.store_get("local", "ns", "a").await.unwrap().is_some());
        assert!(s.store_get("local", "ns", "b").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn max_namespace_rows_zero_disables_the_cap() {
        let s = store().await;
        let uncapped = StorePutOptions::new(MAX_BYTES, 0);
        for i in 0..10 {
            s.store_put("local", "ns", &format!("k{i}"), "v", uncapped)
                .await
                .unwrap();
        }
        let items = s
            .store_list("local", "ns", 0, StoreListOrder::KeyAsc)
            .await
            .unwrap();
        assert_eq!(
            items.len(),
            10,
            "max_namespace_rows = 0 must not evict anything"
        );
    }

    /// Critic finding M7(ii): a namespace overshooting the cap by more than one row (not
    /// just the `overflow == 1` case every other eviction test exercises) must evict exactly
    /// `overflow` rows, oldest-first, in one `store_put` call.
    #[tokio::test]
    async fn row_cap_evicts_multiple_rows_when_overflow_exceeds_one() {
        let s = store().await;
        // Seed 5 rows uncapped, each with a distinct, deterministic updated_at.
        for i in 0..5 {
            s.store_put("local", "ns", &format!("k{i}"), "v", opts())
                .await
                .unwrap();
        }
        for (i, ts) in (0..5).zip([
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:01Z",
            "2026-01-01T00:00:02Z",
            "2026-01-01T00:00:03Z",
            "2026-01-01T00:00:04Z",
        ]) {
            zeph_db::query(sql!(
                "UPDATE cross_thread_store SET updated_at = ? WHERE key = ?"
            ))
            .bind(ts)
            .bind(format!("k{i}"))
            .execute(&s.pool)
            .await
            .unwrap();
        }

        // A 6th distinct key under a cap of 2: overflow = 5 + 1 - 2 = 4 rows evicted at once.
        s.store_put("local", "ns", "k5", "v", StorePutOptions::new(MAX_BYTES, 2))
            .await
            .unwrap();

        for evicted in ["k0", "k1", "k2", "k3"] {
            assert!(
                s.store_get("local", "ns", evicted).await.unwrap().is_none(),
                "{evicted} (older) must be evicted"
            );
        }
        for kept in ["k4", "k5"] {
            assert!(
                s.store_get("local", "ns", kept).await.unwrap().is_some(),
                "{kept} (newest, plus the new write) must survive"
            );
        }
    }

    /// Review round 2, bug #1: a second eviction pass after the namespace is already back at
    /// cap must be a no-op — it recomputes the count fresh rather than acting on an earlier
    /// over-cap observation. This is the property that makes eviction self-correcting under
    /// two concurrent writers racing on the same over-cap condition (see the end-to-end
    /// version below).
    #[tokio::test]
    async fn evict_overflow_second_pass_after_first_is_a_no_op() {
        let s = store().await;
        for key in ["a", "b", "c"] {
            s.store_put("local", "ns", key, "v", opts()).await.unwrap();
        }
        for (key, ts) in [
            ("a", "2026-01-01T00:00:00Z"),
            ("b", "2026-01-01T00:00:01Z"),
            ("c", "2026-01-01T00:00:02Z"),
        ] {
            zeph_db::query(sql!(
                "UPDATE cross_thread_store SET updated_at = ? WHERE key = ?"
            ))
            .bind(ts)
            .bind(key)
            .execute(&s.pool)
            .await
            .unwrap();
        }

        // First pass evicts the real overflow (namespace holds 3, cap 2 → evict 1: "a").
        s.evict_overflow("local", "ns", "c", 2).await.unwrap();
        assert!(s.store_get("local", "ns", "a").await.unwrap().is_none());
        assert!(s.store_get("local", "ns", "b").await.unwrap().is_some());

        // A second pass — simulating a concurrent writer's independent eviction attempt based
        // on the same stale over-cap observation — must evict nothing further.
        s.evict_overflow("local", "ns", "c", 2).await.unwrap();
        assert!(
            s.store_get("local", "ns", "b").await.unwrap().is_some(),
            "a redundant eviction pass must not evict further once back at cap"
        );
    }

    /// Review round 2, bug #1 (end-to-end): two concurrent `store_put` calls targeting the
    /// SAME brand-new key (e.g. a duplicate/retried dispatch) with the namespace already at
    /// cap must not over-evict. Both writes collapse into one net new row via `ON CONFLICT`,
    /// so eviction must remove exactly one old row, not two.
    #[tokio::test]
    async fn concurrent_writes_to_same_new_key_do_not_double_evict() {
        let s = store().await;
        let capped = StorePutOptions::new(MAX_BYTES, 2);
        s.store_put("local", "ns", "a", "v", capped).await.unwrap();
        s.store_put("local", "ns", "b", "v", capped).await.unwrap();
        for (key, ts) in [("a", "2026-01-01T00:00:00Z"), ("b", "2026-01-01T00:00:01Z")] {
            zeph_db::query(sql!(
                "UPDATE cross_thread_store SET updated_at = ? WHERE key = ?"
            ))
            .bind(ts)
            .bind(key)
            .execute(&s.pool)
            .await
            .unwrap();
        }

        let (r1, r2) = tokio::join!(
            s.store_put("local", "ns", "c", "v1", capped),
            s.store_put("local", "ns", "c", "v2", capped),
        );
        r1.unwrap();
        r2.unwrap();

        let items = s
            .store_list("local", "ns", 0, StoreListOrder::KeyAsc)
            .await
            .unwrap();
        assert_eq!(
            items.len(),
            2,
            "two concurrent writes to the same new key must add exactly one net row, leaving \
             the namespace at (not under) its cap: {items:?}"
        );
        assert!(s.store_get("local", "ns", "c").await.unwrap().is_some());
        assert!(s.store_get("local", "ns", "b").await.unwrap().is_some());
        assert!(
            s.store_get("local", "ns", "a").await.unwrap().is_none(),
            "the oldest row must still be the one evicted"
        );
    }

    /// Critic finding M7(iii): an `expected_version`-gated write to a brand-new key must
    /// never evict — the write is about to fail with `VersionConflict` (no row exists yet at
    /// that version), so evicting rows for a write that never lands would destroy state for
    /// nothing (FR-A-012's documented no-evict branch).
    #[tokio::test]
    async fn row_cap_does_not_evict_on_expected_version_gated_write_to_new_key() {
        let s = store().await;
        let capped = StorePutOptions::new(MAX_BYTES, 1);
        s.store_put("local", "ns", "a", "v1", capped).await.unwrap();

        // "b" does not exist, so expected_version=Some(1) must fail with VersionConflict —
        // and must NOT evict "a" on the way to failing.
        let err = s
            .store_put("local", "ns", "b", "v1", capped.with_expected_version(1))
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::VersionConflict { .. }));

        assert!(
            s.store_get("local", "ns", "a").await.unwrap().is_some(),
            "a write that never lands must not evict an existing row"
        );
        assert!(s.store_get("local", "ns", "b").await.unwrap().is_none());
    }

    /// Critic finding M7(iv): the cross-writer overwrite detection signal itself (#6773) —
    /// every other cross-writer test only asserts the resulting `writer_id` value, never that
    /// the `tracing::warn!` this whole feature exists to produce actually fires.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn cross_writer_overwrite_logs_a_warning() {
        let s = store().await;
        s.store_put("local", "ns", "k", "v1", opts().with_writer("task:1"))
            .await
            .unwrap();
        s.store_put("local", "ns", "k", "v2", opts().with_writer("task:2"))
            .await
            .unwrap();

        assert!(
            logs_contain("cross-thread store row overwritten by a different writer"),
            "a cross-writer overwrite must be logged"
        );
    }

    /// Same-writer overwrite (or the first write to a row) must NOT log the cross-writer
    /// warning — a false positive here would train operators to ignore the signal.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn same_writer_overwrite_does_not_log_a_warning() {
        let s = store().await;
        s.store_put("local", "ns", "k", "v1", opts().with_writer("task:1"))
            .await
            .unwrap();
        s.store_put("local", "ns", "k", "v2", opts().with_writer("task:1"))
            .await
            .unwrap();

        assert!(!logs_contain("cross-thread store row overwritten"));
    }

    /// Review round 4, N4: an anonymous write (no `writer_id` supplied) to a row that already
    /// carries a writer's id must log a warning — `COALESCE` preserves that id under content
    /// the prior writer did not actually write, so an operator needs a signal that the
    /// attribution may now be stale, even though `writer_id` itself does not change.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn anonymous_overwrite_of_attributed_row_logs_a_warning() {
        let s = store().await;
        s.store_put("local", "ns", "k", "v1", opts().with_writer("task:1"))
            .await
            .unwrap();

        let updated = s.store_put("local", "ns", "k", "v2", opts()).await.unwrap();
        assert_eq!(
            updated.writer_id.as_deref(),
            Some("task:1"),
            "writer_id must still be preserved, not cleared"
        );
        assert!(
            logs_contain("cross-thread store row anonymously overwritten"),
            "an anonymous overwrite of an attributed row must be logged"
        );
    }

    /// An anonymous write to a row with no prior writer (or a brand-new key) must not log the
    /// anonymous-overwrite warning — there is no stale attribution to flag.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn anonymous_write_to_unattributed_row_does_not_log_a_warning() {
        let s = store().await;
        s.store_put("local", "ns", "k", "v1", opts()).await.unwrap();
        s.store_put("local", "ns", "k", "v2", opts()).await.unwrap();

        assert!(!logs_contain(
            "cross-thread store row anonymously overwritten"
        ));
    }

    /// Review round 2, bug #2: a stale CAS write whose `expected_version` no longer matches
    /// must not log a cross-writer overwrite warning even though the pre-write probe observed
    /// a different existing writer — no overwrite actually happened, since the write itself
    /// failed with `VersionConflict`.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn stale_cas_with_different_writer_does_not_log_overwrite_warning() {
        let s = store().await;
        let first = s
            .store_put("local", "ns", "k", "v1", opts().with_writer("task:1"))
            .await
            .unwrap();
        // Advance the row past `first.version` so the CAS below is stale.
        s.store_put("local", "ns", "k", "v2", opts().with_writer("task:1"))
            .await
            .unwrap();

        let err = s
            .store_put(
                "local",
                "ns",
                "k",
                "v3",
                opts()
                    .with_writer("task:2")
                    .with_expected_version(first.version),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::VersionConflict { .. }));
        assert!(
            !logs_contain("cross-thread store row overwritten"),
            "a stale CAS that never applied must not log a false-positive overwrite warning"
        );
    }
}
