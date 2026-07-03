// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// SQL fragments that differ between database backends.
///
/// Implemented by zero-sized marker types ([`Sqlite`], [`Postgres`]).
/// All associated constants are `&'static str` for zero-cost usage.
pub trait Dialect: Send + Sync + 'static {
    /// Auto-increment primary key DDL fragment.
    ///
    /// `SQLite`: `INTEGER PRIMARY KEY AUTOINCREMENT`
    /// `PostgreSQL`: `BIGSERIAL PRIMARY KEY`
    const AUTO_PK: &'static str;

    /// `INSERT OR IGNORE` prefix for this backend.
    ///
    /// `SQLite`: `INSERT OR IGNORE`
    /// `PostgreSQL`: `INSERT` (pair with `CONFLICT_NOTHING` suffix)
    const INSERT_IGNORE: &'static str;

    /// Suffix for conflict-do-nothing semantics.
    ///
    /// `SQLite`: empty string (handled by `INSERT OR IGNORE` prefix)
    /// `PostgreSQL`: `ON CONFLICT DO NOTHING`
    const CONFLICT_NOTHING: &'static str;

    /// Case-insensitive collation suffix for `ORDER BY` / `WHERE` clauses.
    ///
    /// `SQLite`: `COLLATE NOCASE`
    /// `PostgreSQL`: empty string (use `ILIKE` or `LOWER()` instead)
    const COLLATE_NOCASE: &'static str;

    /// Current epoch seconds expression.
    ///
    /// `SQLite`: `unixepoch('now')`
    /// `PostgreSQL`: `EXTRACT(EPOCH FROM NOW())::BIGINT`
    const EPOCH_NOW: &'static str;

    /// Current timestamp expression, for direct assignment into a `TEXT`/`TIMESTAMPTZ`
    /// `updated_at`-style column (as opposed to [`Self::EPOCH_NOW`], which yields an integer).
    ///
    /// `SQLite`: `datetime('now')`
    /// `PostgreSQL`: `NOW()`
    const NOW: &'static str;

    /// Case-insensitive comparison expression for a column.
    ///
    /// `SQLite`: `{col} COLLATE NOCASE`
    /// `PostgreSQL`: `LOWER({col})`
    fn ilike(col: &str) -> String;

    /// Epoch seconds expression for a timestamp column.
    ///
    /// Wraps the column in the backend-specific function that converts a stored
    /// timestamp to a Unix epoch integer, coalescing `NULL` to `0`.
    ///
    /// `SQLite`: `COALESCE(CAST(strftime('%s', {col}) AS INTEGER), 0)`
    /// `PostgreSQL`: `COALESCE(CAST(EXTRACT(EPOCH FROM {col}) AS BIGINT), 0)`
    fn epoch_from_col(col: &str) -> String;

    /// Cast suffix for a bind parameter carrying pre-serialized JSON text, destined
    /// for a JSON-typed column.
    ///
    /// `sqlx` sends string bind parameters as `TEXT`/`VARCHAR`. `SQLite` stores JSON
    /// columns as `TEXT` so no cast is needed. `PostgreSQL` stores them as `JSONB`,
    /// which requires an explicit cast from the bound `TEXT` value — otherwise the
    /// backend rejects the insert/update with "column is of type jsonb but expression
    /// is of type text". Append this suffix directly after the `?` placeholder for
    /// that bind position, e.g. `format!("VALUES (?{json_cast})", json_cast = ...)`.
    ///
    /// `SQLite`: empty string
    /// `PostgreSQL`: `::jsonb`
    const JSON_CAST: &'static str;

    /// Cast suffix for a bind parameter carrying a pre-formatted timestamp string, destined
    /// for a timestamp-typed column.
    ///
    /// `sqlx` sends string bind parameters as `TEXT`/`VARCHAR`. `SQLite` stores timestamp
    /// columns as `TEXT` so no cast is needed. `PostgreSQL` stores them as `TIMESTAMPTZ`, which
    /// has no implicit cast from `TEXT` — binding a plain string fails with
    /// `PgDatabaseError` 42804 ("column is of type timestamptz but expression is of type
    /// text"). Append this suffix directly after the `?` placeholder for that bind position,
    /// e.g. `format!("VALUES (?{timestamptz_cast})", timestamptz_cast = ...)`.
    ///
    /// `SQLite`: empty string
    /// `PostgreSQL`: `::timestamptz`
    const TIMESTAMPTZ_CAST: &'static str;

    /// Project a non-`TEXT` column so it decodes into a plain `String`.
    ///
    /// `SQLite` is dynamically typed and stores JSON/timestamp columns as `TEXT`, so
    /// the column already decodes into `String` directly — no cast needed. `PostgreSQL`
    /// stores the same logical data in natively-typed columns (`JSONB`, `TIMESTAMPTZ`,
    /// ...); decoding those straight into `String` fails (`sqlx`'s `String:
    /// Decode<Postgres>` only covers `TEXT`-family OIDs), so the column must be cast to
    /// `::text` in the `SELECT` list for call sites that only need the raw text
    /// representation (rather than a typed decode via `sqlx::types::Json<T>` or
    /// `chrono::DateTime`).
    ///
    /// `SQLite`: `{col}`
    /// `PostgreSQL`: `{col}::text`
    fn select_as_text(col: &str) -> String;

    /// Scalar function name returning the greatest of two (or more) numeric arguments.
    ///
    /// `SQLite`'s `max(a, b, ...)` is a scalar multi-argument function. `PostgreSQL`'s
    /// `MAX()` is exclusively an aggregate (requires `GROUP BY`) — the scalar equivalent
    /// is `GREATEST(a, b, ...)`. Use this constant when building a two-argument "largest
    /// of" expression that must work as a plain scalar call, not an aggregate.
    ///
    /// `SQLite`: `MAX`
    /// `PostgreSQL`: `GREATEST`
    const GREATEST_FN: &'static str;

    /// Scalar function name returning the least of two (or more) numeric arguments.
    ///
    /// `SQLite`'s `min(a, b, ...)` is a scalar multi-argument function. `PostgreSQL`'s
    /// `MIN()` is exclusively an aggregate (requires `GROUP BY`) — the scalar equivalent
    /// is `LEAST(a, b, ...)`. Use this constant when building a two-argument "smallest
    /// of" expression that must work as a plain scalar call, not an aggregate.
    ///
    /// `SQLite`: `MIN`
    /// `PostgreSQL`: `LEAST`
    const LEAST_FN: &'static str;

    /// Timestamp expression for binding a Unix epoch-seconds value into a
    /// `TEXT`/`TIMESTAMPTZ` `compacted_at`-style column.
    ///
    /// Wraps the given bind-parameter placeholder (e.g. `?`, later rewritten to `$N`
    /// by [`crate::rewrite_placeholders`]) in the backend-specific conversion from
    /// Unix epoch seconds to the column's native timestamp representation.
    ///
    /// `SQLite` stores such columns as bare epoch-seconds `TEXT`, so the placeholder
    /// passes through unchanged. `PostgreSQL` stores them as `TIMESTAMPTZ`; unlike an
    /// ISO-8601 string, a bare epoch-seconds string has no valid `timestamptz` input
    /// syntax — `'1735999999'::timestamptz` fails to parse even with
    /// [`Self::TIMESTAMPTZ_CAST`] appended. The bound value must instead be routed
    /// through `to_timestamp()`, which both performs the epoch conversion and yields a
    /// `TIMESTAMPTZ` directly, so no additional cast is needed on the result.
    ///
    /// The placeholder itself still needs an explicit `::double precision` cast:
    /// callers typically bind a Rust `String`/`&str` (the value is formatted with
    /// `format!("{secs}")` before binding), which `sqlx` sends with the `TEXT` type OID.
    /// `to_timestamp()` has only `to_timestamp(double precision)` and
    /// `to_timestamp(text, text)` overloads — there is no implicit `text` ->
    /// `double precision` cast, so an unqualified `to_timestamp({placeholder})` fails
    /// function-argument resolution with `ERROR 42883: function to_timestamp(text) does
    /// not exist`. The `::double precision` cast on the placeholder is what makes the
    /// bound text resolve to the numeric overload.
    ///
    /// `SQLite`: `{placeholder}`
    /// `PostgreSQL`: `to_timestamp({placeholder}::double precision)`
    fn timestamptz_from_epoch(placeholder: &str) -> String;
}

/// `SQLite` dialect marker type.
pub struct Sqlite;

impl Dialect for Sqlite {
    const AUTO_PK: &'static str = "INTEGER PRIMARY KEY AUTOINCREMENT";
    const INSERT_IGNORE: &'static str = "INSERT OR IGNORE";
    const CONFLICT_NOTHING: &'static str = "";
    const COLLATE_NOCASE: &'static str = "COLLATE NOCASE";
    const EPOCH_NOW: &'static str = "unixepoch('now')";
    const NOW: &'static str = "datetime('now')";
    const JSON_CAST: &'static str = "";
    const TIMESTAMPTZ_CAST: &'static str = "";
    const GREATEST_FN: &'static str = "MAX";
    const LEAST_FN: &'static str = "MIN";

    fn ilike(col: &str) -> String {
        format!("{col} COLLATE NOCASE")
    }

    fn epoch_from_col(col: &str) -> String {
        format!("COALESCE(CAST(strftime('%s', {col}) AS INTEGER), 0)")
    }

    fn select_as_text(col: &str) -> String {
        col.to_string()
    }

    fn timestamptz_from_epoch(placeholder: &str) -> String {
        placeholder.to_string()
    }
}

/// `PostgreSQL` dialect marker type.
pub struct Postgres;

impl Dialect for Postgres {
    const AUTO_PK: &'static str = "BIGSERIAL PRIMARY KEY";
    const INSERT_IGNORE: &'static str = "INSERT";
    const CONFLICT_NOTHING: &'static str = "ON CONFLICT DO NOTHING";
    const COLLATE_NOCASE: &'static str = "";
    const EPOCH_NOW: &'static str = "EXTRACT(EPOCH FROM NOW())::BIGINT";
    const NOW: &'static str = "NOW()";
    const JSON_CAST: &'static str = "::jsonb";
    const TIMESTAMPTZ_CAST: &'static str = "::timestamptz";
    const GREATEST_FN: &'static str = "GREATEST";
    const LEAST_FN: &'static str = "LEAST";

    fn ilike(col: &str) -> String {
        format!("LOWER({col})")
    }

    fn epoch_from_col(col: &str) -> String {
        format!("COALESCE(CAST(EXTRACT(EPOCH FROM {col}) AS BIGINT), 0)")
    }

    fn select_as_text(col: &str) -> String {
        format!("{col}::text")
    }

    fn timestamptz_from_epoch(placeholder: &str) -> String {
        format!("to_timestamp({placeholder}::double precision)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sqlite")]
    mod sqlite {
        use super::*;

        #[test]
        fn auto_pk() {
            assert_eq!(Sqlite::AUTO_PK, "INTEGER PRIMARY KEY AUTOINCREMENT");
        }

        #[test]
        fn insert_ignore() {
            assert_eq!(Sqlite::INSERT_IGNORE, "INSERT OR IGNORE");
            assert_eq!(Sqlite::CONFLICT_NOTHING, "");
        }

        #[test]
        fn epoch_now() {
            assert_eq!(Sqlite::EPOCH_NOW, "unixepoch('now')");
        }

        #[test]
        fn now() {
            assert_eq!(Sqlite::NOW, "datetime('now')");
        }

        #[test]
        fn epoch_from_col() {
            assert_eq!(
                Sqlite::epoch_from_col("created_at"),
                "COALESCE(CAST(strftime('%s', created_at) AS INTEGER), 0)"
            );
        }

        #[test]
        fn select_as_text() {
            assert_eq!(Sqlite::select_as_text("parts"), "parts");
        }

        #[test]
        fn ilike() {
            assert_eq!(Sqlite::ilike("name"), "name COLLATE NOCASE");
        }

        #[test]
        fn json_cast() {
            assert_eq!(Sqlite::JSON_CAST, "");
        }

        #[test]
        fn timestamptz_cast() {
            assert_eq!(Sqlite::TIMESTAMPTZ_CAST, "");
        }

        #[test]
        fn greatest_fn() {
            assert_eq!(Sqlite::GREATEST_FN, "MAX");
        }

        #[test]
        fn least_fn() {
            assert_eq!(Sqlite::LEAST_FN, "MIN");
        }

        #[test]
        fn timestamptz_from_epoch() {
            assert_eq!(Sqlite::timestamptz_from_epoch("?"), "?");
        }
    }

    #[cfg(feature = "postgres")]
    mod postgres {
        use super::*;

        #[test]
        fn auto_pk() {
            assert_eq!(Postgres::AUTO_PK, "BIGSERIAL PRIMARY KEY");
        }

        #[test]
        fn insert_ignore() {
            assert_eq!(Postgres::INSERT_IGNORE, "INSERT");
            assert_eq!(Postgres::CONFLICT_NOTHING, "ON CONFLICT DO NOTHING");
        }

        #[test]
        fn epoch_now() {
            assert_eq!(Postgres::EPOCH_NOW, "EXTRACT(EPOCH FROM NOW())::BIGINT");
        }

        #[test]
        fn now() {
            assert_eq!(Postgres::NOW, "NOW()");
        }

        #[test]
        fn epoch_from_col() {
            assert_eq!(
                Postgres::epoch_from_col("created_at"),
                "COALESCE(CAST(EXTRACT(EPOCH FROM created_at) AS BIGINT), 0)"
            );
        }

        #[test]
        fn select_as_text() {
            assert_eq!(Postgres::select_as_text("parts"), "parts::text");
        }

        #[test]
        fn ilike() {
            assert_eq!(Postgres::ilike("name"), "LOWER(name)");
        }

        #[test]
        fn json_cast() {
            assert_eq!(Postgres::JSON_CAST, "::jsonb");
        }

        #[test]
        fn timestamptz_cast() {
            assert_eq!(Postgres::TIMESTAMPTZ_CAST, "::timestamptz");
        }

        #[test]
        fn greatest_fn() {
            assert_eq!(Postgres::GREATEST_FN, "GREATEST");
        }

        #[test]
        fn least_fn() {
            assert_eq!(Postgres::LEAST_FN, "LEAST");
        }

        #[test]
        fn timestamptz_from_epoch() {
            assert_eq!(
                Postgres::timestamptz_from_epoch("?"),
                "to_timestamp(?::double precision)"
            );
        }
    }
}
