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
}

type StoreItemTuple = (String, String, String, String, i64, String, String);

fn item_from_tuple(t: StoreItemTuple) -> StoreItem {
    StoreItem {
        owner_key: t.0,
        namespace: t.1,
        key: t.2,
        value: t.3,
        version: t.4,
        created_at: t.5,
        updated_at: t.6,
    }
}

/// `created_at`/`updated_at` are `TIMESTAMPTZ` on Postgres (`TEXT` on `SQLite`); project both
/// through `Dialect::select_as_text` so they decode into the `String` fields on [`StoreItem`],
/// mirroring `acp_sessions.rs`'s fix for the same mismatch.
fn select_columns() -> String {
    let created_at_sel = <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
    let updated_at_sel = <ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("updated_at");
    format!("owner_key, namespace, key, value, version, {created_at_sel}, {updated_at_sel}")
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
    /// `max_value_bytes` rejects the write outright when `value`'s UTF-8 byte length exceeds
    /// it (FR-A-005) rather than truncating — callers pass `[memory.store].max_value_bytes`
    /// from config; this method itself does not depend on `zeph-config`.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::InvalidInput`] if `value` exceeds `max_value_bytes`,
    /// [`MemoryError::VersionConflict`] on an `expected_version` mismatch, or a database
    /// error if the query fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn example() -> Result<(), zeph_memory::MemoryError> {
    /// use zeph_memory::store::SqliteStore;
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// let item = store
    ///     .store_put("local", "orch/graph-1", "finding", "{\"x\":1}", 65536, None)
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
        max_value_bytes: usize,
        expected_version: Option<i64>,
    ) -> Result<StoreItem, MemoryError> {
        if value.len() > max_value_bytes {
            return Err(MemoryError::InvalidInput(format!(
                "store_put value for namespace={namespace:?} key={key:?} is {} bytes, \
                 exceeds max_value_bytes={max_value_bytes}",
                value.len()
            )));
        }

        let now = <ActiveDialect as zeph_db::dialect::Dialect>::NOW;
        let cols = select_columns();

        let row: Option<StoreItemTuple> = if let Some(expected) = expected_version {
            let raw = format!(
                "UPDATE cross_thread_store \
                 SET value = ?, version = version + 1, updated_at = {now} \
                 WHERE owner_key = ? AND namespace = ? AND key = ? AND version = ? \
                 RETURNING {cols}"
            );
            let query_sql = zeph_db::rewrite_placeholders(&raw);
            zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
                .bind(value)
                .bind(owner_key)
                .bind(namespace)
                .bind(key)
                .bind(expected)
                .fetch_optional(&self.pool)
                .await?
        } else {
            let raw = format!(
                "INSERT INTO cross_thread_store (owner_key, namespace, key, value) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(owner_key, namespace, key) DO UPDATE SET \
                   value = excluded.value, \
                   version = cross_thread_store.version + 1, \
                   updated_at = {now} \
                 RETURNING {cols}"
            );
            let query_sql = zeph_db::rewrite_placeholders(&raw);
            zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
                .bind(owner_key)
                .bind(namespace)
                .bind(key)
                .bind(value)
                .fetch_optional(&self.pool)
                .await?
        };

        match row {
            Some(t) => Ok(item_from_tuple(t)),
            None => Err(MemoryError::VersionConflict {
                owner_key: owner_key.to_owned(),
                namespace: namespace.to_owned(),
                key: key.to_owned(),
                expected: expected_version.unwrap_or(0),
            }),
        }
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
    /// use zeph_memory::store::SqliteStore;
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// store
    ///     .store_put("local", "orch/graph-1", "finding", "{\"x\":1}", 65536, None)
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
    /// starts with it), or a shorter prefix to match several. Results are ordered by
    /// `(namespace, key)`. Pass `limit = 0` for unlimited.
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
    /// use zeph_memory::store::SqliteStore;
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// store
    ///     .store_put("local", "orch/graph-1", "finding", "{\"x\":1}", 65536, None)
    ///     .await?;
    ///
    /// let items = store.store_list("local", "orch/graph-1", 0).await?;
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
    ) -> Result<Vec<StoreItem>, MemoryError> {
        let cols = select_columns();
        let (limit_clause, limit_bind) = zeph_db::limit_clause(limit as u64);
        let upper = prefix_range_upper_bound(namespace_prefix);
        let raw = if upper.is_some() {
            format!(
                "SELECT {cols} FROM cross_thread_store \
                 WHERE owner_key = ? AND namespace >= ? AND namespace < ? \
                 ORDER BY namespace, key{limit_clause}"
            )
        } else {
            format!(
                "SELECT {cols} FROM cross_thread_store \
                 WHERE owner_key = ? AND namespace >= ? \
                 ORDER BY namespace, key{limit_clause}"
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
    /// use zeph_memory::store::SqliteStore;
    ///
    /// let store = SqliteStore::new(":memory:").await?;
    /// store
    ///     .store_put(
    ///         "local",
    ///         "orch/graph-1",
    ///         "finding",
    ///         "{\"summary\":\"needle in haystack\"}",
    ///         65536,
    ///         None,
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
            .store_put("local", "orch/g1", "finding", "{\"x\":1}", MAX_BYTES, None)
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
        s.store_put("local", "ns", "k", "v1", MAX_BYTES, None)
            .await
            .unwrap();
        let updated = s
            .store_put("local", "ns", "k", "v2", MAX_BYTES, None)
            .await
            .unwrap();
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
        s.store_put("local", "ns-a", "k", "value-a", MAX_BYTES, None)
            .await
            .unwrap();
        s.store_put("local", "ns-b", "k", "value-b", MAX_BYTES, None)
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
        s.store_put("owner-a", "ns", "k", "value-a", MAX_BYTES, None)
            .await
            .unwrap();
        s.store_put("owner-b", "ns", "k", "value-b", MAX_BYTES, None)
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
        let first = s
            .store_put("local", "ns", "k", "v1", MAX_BYTES, None)
            .await
            .unwrap();
        assert_eq!(first.version, 1);

        // Correct expected_version succeeds.
        let second = s
            .store_put("local", "ns", "k", "v2", MAX_BYTES, Some(1))
            .await
            .unwrap();
        assert_eq!(second.version, 2);

        // Stale expected_version (the row is now at version 2) is rejected.
        let err = s
            .store_put("local", "ns", "k", "v3", MAX_BYTES, Some(1))
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
            .store_put("local", "ns", "no-such-key", "v", MAX_BYTES, Some(1))
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::VersionConflict { .. }));
    }

    #[tokio::test]
    async fn put_rejects_value_exceeding_max_bytes() {
        let s = store().await;
        let err = s
            .store_put("local", "ns", "k", "0123456789", 5, None)
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
        s.store_put("local", "orch/g1", "a", "1", MAX_BYTES, None)
            .await
            .unwrap();
        s.store_put("local", "orch/g1", "b", "2", MAX_BYTES, None)
            .await
            .unwrap();
        s.store_put("local", "orch/g2", "c", "3", MAX_BYTES, None)
            .await
            .unwrap();

        let g1 = s.store_list("local", "orch/g1", 0).await.unwrap();
        assert_eq!(g1.len(), 2);
        assert!(g1.iter().all(|i| i.namespace == "orch/g1"));

        let all_orch = s.store_list("local", "orch/", 0).await.unwrap();
        assert_eq!(all_orch.len(), 3);
    }

    #[tokio::test]
    async fn list_respects_limit() {
        let s = store().await;
        for i in 0..5u8 {
            s.store_put("local", "ns", &format!("k{i}"), "v", MAX_BYTES, None)
                .await
                .unwrap();
        }
        let limited = s.store_list("local", "ns", 2).await.unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[tokio::test]
    async fn list_empty_namespace_returns_empty_vec() {
        let s = store().await;
        let rows = s.store_list("local", "no/such/ns", 0).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn search_matches_value_keyword() {
        let s = store().await;
        s.store_put(
            "local",
            "orch/g1",
            "a",
            "{\"finding\":\"needle in haystack\"}",
            MAX_BYTES,
            None,
        )
        .await
        .unwrap();
        s.store_put(
            "local",
            "orch/g1",
            "b",
            "{\"finding\":\"nothing here\"}",
            MAX_BYTES,
            None,
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
        s.store_put("local", "orch/g1", "a", "needle", MAX_BYTES, None)
            .await
            .unwrap();
        s.store_put("local", "orch/g2", "b", "needle", MAX_BYTES, None)
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
        s.store_put("local", "ns", "a", "50% off", MAX_BYTES, None)
            .await
            .unwrap();
        s.store_put("local", "ns", "b", "50x off", MAX_BYTES, None)
            .await
            .unwrap();

        // A literal "%" in the search query must not act as a wildcard.
        let hits = s.store_search("local", "ns", "50%", 0).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "a");
    }
}
