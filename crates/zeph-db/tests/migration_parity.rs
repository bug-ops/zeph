// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Migration parity guard between the `SQLite` and `PostgreSQL` dialects.
//!
//! System-invariant 001 §13 requires the two migration directories to have matching file
//! counts and schema-equivalent content: a `PostgreSQL` deployment must end with the same logical
//! schema as a `SQLite` deployment. These tests are pure file-system checks (no database, no
//! feature flag) so they run on every `cargo test`/`nextest` invocation and fail the build the
//! moment the dialects drift apart — the divergence that motivated #4957.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

/// Tables that legitimately exist only in the `SQLite` dialect because `PostgreSQL` implements
/// the same capability through a different mechanism (`FTS5` virtual tables vs `tsvector`/`GIN`
/// indexes). Anything not listed here must exist in both dialects.
const SQLITE_ONLY_TABLES: &[&str] = &["graph_entities_fts", "messages_fts"];

fn migrations_dir(dialect: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(dialect)
}

/// Sorted list of `*.sql` file names in a dialect directory.
fn sql_file_names(dialect: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(migrations_dir(dialect))
        .expect("migrations directory must exist")
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
        })
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// Strip `--` line comments so commented-out DDL or prose mentioning `CREATE TABLE` does not
/// produce false matches.
fn strip_comments(sql: &str) -> String {
    sql.lines()
        .map(|line| line.split_once("--").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Set of table names defined across all migrations of a dialect, excluding transient
/// `*_new`/`*_v2` tables that `SQLite` creates only as part of the copy-and-rename rebuild
/// pattern.
fn defined_tables(dialect: &str) -> BTreeSet<String> {
    let pattern = regex::RegexBuilder::new(
        r#"create\s+(?:virtual\s+)?table\s+(?:if\s+not\s+exists\s+)?["`]?([a-z_][a-z0-9_]*)"#,
    )
    .case_insensitive(true)
    .build()
    .expect("valid regex");

    let mut tables = BTreeSet::new();
    for name in sql_file_names(dialect) {
        let sql = fs::read_to_string(migrations_dir(dialect).join(&name)).expect("readable sql");
        for caps in pattern.captures_iter(&strip_comments(&sql)) {
            let table = caps[1].to_string();
            if table.ends_with("_new") || table.ends_with("_v2") {
                continue;
            }
            tables.insert(table);
        }
    }
    tables
}

/// Both dialects must carry the same number of migration files (system-invariant 001 §13).
#[test]
fn file_counts_match() {
    let sqlite = sql_file_names("sqlite").len();
    let postgres = sql_file_names("postgres").len();
    assert_eq!(
        sqlite, postgres,
        "migration file counts diverge: sqlite={sqlite}, postgres={postgres} — every logical \
         migration must exist in both dialects (system-invariant 001 §13)"
    );
}

/// Every migration must exist in both dialects under the same logical name (file name with the
/// numeric prefix removed). Dialect-specific implementations of the same change must share the
/// logical name so the sequences stay aligned.
#[test]
fn logical_names_match() {
    let logical = |dialect: &str| -> BTreeSet<String> {
        sql_file_names(dialect)
            .into_iter()
            .map(|name| {
                name.split_once('_')
                    .map_or(name.clone(), |(_, rest)| rest.to_string())
            })
            .collect()
    };
    let sqlite = logical("sqlite");
    let postgres = logical("postgres");

    let only_sqlite: Vec<_> = sqlite.difference(&postgres).collect();
    let only_postgres: Vec<_> = postgres.difference(&sqlite).collect();
    assert!(
        only_sqlite.is_empty() && only_postgres.is_empty(),
        "migration logical names diverge — sqlite-only: {only_sqlite:?}, postgres-only: \
         {only_postgres:?}"
    );
}

/// Every table must be defined in both dialects, except the documented `SQLite`-only `FTS` tables.
/// This catches a table added to one dialect's migration but forgotten in the other (#4957).
#[test]
fn table_sets_equivalent() {
    let sqlite = defined_tables("sqlite");
    let postgres = defined_tables("postgres");

    let allowed: BTreeSet<String> = SQLITE_ONLY_TABLES
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    // The allowlist must not rot: every entry has to actually exist in SQLite.
    for table in &allowed {
        assert!(
            sqlite.contains(table),
            "allowlisted SQLite-only table `{table}` no longer exists — update SQLITE_ONLY_TABLES"
        );
    }

    let missing_in_postgres: Vec<_> = sqlite
        .difference(&postgres)
        .filter(|t| !allowed.contains(*t))
        .collect();
    let missing_in_sqlite: Vec<_> = postgres.difference(&sqlite).collect();

    assert!(
        missing_in_postgres.is_empty(),
        "tables defined in SQLite but missing in PostgreSQL: {missing_in_postgres:?} — add the \
         equivalent PostgreSQL migration (or allowlist it if intentionally dialect-specific)"
    );
    assert!(
        missing_in_sqlite.is_empty(),
        "tables defined in PostgreSQL but missing in SQLite: {missing_in_sqlite:?}"
    );
}
