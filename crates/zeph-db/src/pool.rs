// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::DbPool;
use crate::error::DbError;

/// Configuration for database pool construction.
pub struct DbConfig {
    /// Database URL. `SQLite`: file path or `:memory:`. `PostgreSQL`: connection URL.
    pub url: String,
    /// Maximum number of connections in the pool, passed to `sqlx`'s
    /// `.max_connections()` builder call for both backends. Default 5.
    ///
    /// `SQLite`: `BEGIN IMMEDIATE` serializes concurrent writers at the `SQLite` level,
    /// so this bound controls read concurrency only. In-memory databases are always
    /// forced to a single connection regardless of this value, since each new
    /// `:memory:` connection opens an isolated, unmigrated database.
    pub pool_size: u32,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            pool_size: 5,
        }
    }
}

impl DbConfig {
    /// Connect to the database and run migrations.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] if connection or migration fails.
    #[tracing::instrument(name = "db.pool.connect", skip_all, err)]
    pub async fn connect(&self) -> Result<DbPool, DbError> {
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        {
            Self::connect_sqlite(&self.url, self.pool_size).await
        }
        #[cfg(feature = "postgres")]
        {
            Self::connect_postgres(&self.url, self.pool_size).await
        }
    }

    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    async fn connect_sqlite(path: &str, pool_size: u32) -> Result<DbPool, DbError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let url = if path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            let db_path = std::path::PathBuf::from(path);

            if let Some(parent) = db_path.parent()
                && !parent.as_os_str().is_empty()
            {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Pre-create with 0o600 so sqlx inherits the mode rather than using the
            // process umask. sqlx reopens the existing file via SQLITE_OPEN_CREATE.
            // WAL/SHM sidecars are created by sqlx after the pool opens and will still
            // inherit the process umask (sqlx limitation — best-effort chmod below).
            if tokio::fs::metadata(&db_path).await.is_err() {
                let p = db_path.clone();
                tokio::task::spawn_blocking(move || {
                    zeph_common::fs_secure::open_private_truncate(&p)
                })
                .await
                .map_err(|e| std::io::Error::other(format!("spawn_blocking panicked: {e}")))??;
            }
            format!("sqlite:{path}?mode=rwc")
        };

        let opts = SqliteConnectOptions::from_str(&url)
            .map_err(DbError::Sqlx)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5))
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        // BEGIN IMMEDIATE serializes concurrent writers at the SQLite level; pool_size
        // controls read concurrency only. In-memory databases are connection-scoped:
        // each new connection is a separate empty DB. Force a single connection so all
        // queries share the migrated schema.
        let effective_max = if path == ":memory:" { 1 } else { pool_size };
        let pool = SqlitePoolOptions::new()
            .max_connections(effective_max)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect_with(opts)
            .await
            .map_err(DbError::Sqlx)?;

        crate::migrate::run_migrations(&pool).await?;

        // Best-effort chmod for .db, .db-wal, and .db-shm. The .db itself was
        // pre-created with 0o600 above; the WAL/SHM sidecars are created by sqlx
        // after the pool opens and inherit the process umask, so we fix them here.
        // There is a small race window between sidecar creation and this chmod;
        // there is no way to close it without upstream sqlx support.
        #[cfg(unix)]
        if path != ":memory:" {
            let path_owned = path.to_owned();
            tokio::task::spawn_blocking(move || {
                use std::os::unix::fs::PermissionsExt as _;
                for suffix in &["", "-wal", "-shm", "-journal"] {
                    let p = format!("{path_owned}{suffix}");
                    if let Ok(metadata) = std::fs::metadata(&p) {
                        let mut perms = metadata.permissions();
                        perms.set_mode(0o600);
                        let _ = std::fs::set_permissions(&p, perms);
                    }
                }
            })
            .await
            .map_err(|e| std::io::Error::other(format!("spawn_blocking panicked: {e}")))?;
        }

        // Run a passive WAL checkpoint after migrations to avoid unbounded WAL growth.
        // Skipped for in-memory databases (no WAL file).
        if path != ":memory:" {
            sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                .execute(&pool)
                .await
                .map_err(DbError::Sqlx)?;
        }

        Ok(pool)
    }

    #[cfg(feature = "postgres")]
    async fn connect_postgres(url: &str, pool_size: u32) -> Result<DbPool, DbError> {
        use sqlx::postgres::PgPoolOptions;

        if !url.contains("sslmode=") {
            tracing::warn!(
                "postgres connection string has no sslmode; plaintext connections are allowed"
            );
        }

        let pool = PgPoolOptions::new()
            .max_connections(pool_size)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(url)
            .await
            .map_err(|e| DbError::Connection {
                url: redact_url(url).unwrap_or_else(|| "[redacted]".into()),
                source: e,
            })?;

        crate::migrate::run_migrations(&pool).await?;

        Ok(pool)
    }
}

/// Strip credentials from a database URL for safe logging.
///
/// Replaces the whole userinfo component (`user[:password]@`) with `[redacted]@`.
/// Uses [`url::Url`] rather than a regex so the split between userinfo and host
/// follows the same rule real clients use: the LAST unescaped `@` before the
/// path/port delimits userinfo, so a password containing `@` (e.g.
/// `postgres://user:p@ss@host/db`) is redacted in full instead of leaving its
/// tail exposed.
///
/// Also recognizes two non-userinfo credential forms accepted by libpq and
/// redacts the whole URL for them, rather than attempting a partial rewrite:
/// - query-param URIs, e.g. `postgresql://host/db?password=secret`
/// - key-value DSNs, e.g. `host=localhost dbname=zeph password=secret`
///
/// Returns `None` if the URL matches none of the recognized credential forms
/// above (already safe). Returns `Some(redacted)` otherwise. This covers only
/// the known common forms listed above — a URL carrying credentials in some
/// other, unrecognized shape can still return `None`, so callers that fall
/// back to the raw URL on `None` should not treat that as an absolute
/// guarantee against leaking credentials.
///
/// # Examples
///
/// ```
/// use zeph_db::redact_url;
///
/// assert_eq!(
///     redact_url("postgres://user:secret@host:5432/db").unwrap(),
///     "postgres://[redacted]@host:5432/db"
/// );
/// assert_eq!(redact_url("sqlite:///data/zeph.db"), None);
/// ```
#[must_use]
pub fn redact_url(url: &str) -> Option<String> {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
            let has_password_param = parsed.query().is_some_and(contains_password_assignment);
            if !has_userinfo && !has_password_param {
                return None;
            }
            if has_password_param {
                // Query-string credentials (libpq's `?password=...`) aren't safe
                // to selectively strip without parsing every possible param
                // dialect; redact the whole URL instead.
                return Some("[redacted]".to_string());
            }
            // Clear userinfo entirely (an empty username + no password serializes
            // with no "@" at all), then splice in the literal "[redacted]@" marker.
            // Setting the username directly to "[redacted]" would percent-encode
            // the brackets (`%5Bredacted%5D`), which is correct but not the
            // human-readable marker this function promises callers.
            if parsed.set_username("").is_err() || parsed.set_password(None).is_err() {
                return Some("[redacted]".to_string());
            }
            let without_userinfo = parsed.as_str();
            let host_start = without_userinfo.find("://")? + "://".len();
            let mut redacted = String::with_capacity(without_userinfo.len() + 11);
            redacted.push_str(&without_userinfo[..host_start]);
            redacted.push_str("[redacted]@");
            redacted.push_str(&without_userinfo[host_start..]);
            Some(redacted)
        }
        Err(_) if url.contains('@') || contains_password_assignment(url) => {
            Some("[redacted]".to_string())
        }
        Err(_) => None,
    }
}

/// Case-insensitive check for a `password=` key-value assignment, as used by
/// libpq query-param URIs (`?password=...`) and key-value DSNs
/// (`host=... password=...`). Whitespace between `password` and `=` is
/// tolerated since libpq DSNs allow it (`password = secret`).
fn contains_password_assignment(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(idx) = lower[offset..].find("password") {
        let abs = offset + idx;
        if lower[abs + "password".len()..]
            .trim_start()
            .starts_with('=')
        {
            return true;
        }
        offset = abs + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_replaces_credentials() {
        let url = "postgres://user:secret@localhost:5432/zeph";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "postgres://[redacted]@localhost:5432/zeph");
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn redact_url_returns_none_for_no_credentials() {
        // URL without credentials — no match, returns None
        let url = "postgres://localhost:5432/zeph";
        assert!(redact_url(url).is_none());
    }

    #[test]
    fn redact_url_handles_sqlite_path() {
        let url = "sqlite:/path/to/db";
        assert!(redact_url(url).is_none());
    }

    #[test]
    fn redact_url_fully_redacts_password_with_single_at() {
        // Regression test for #5969: a password containing `@` used to leave
        // its tail exposed because the old regex stopped at the first `@`.
        let url = "postgres://user:p@ss@host:5432/db";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "postgres://[redacted]@host:5432/db");
        assert!(!redacted.contains("ss@host") && !redacted.contains("p@ss"));
    }

    #[test]
    fn redact_url_fully_redacts_password_with_multiple_at() {
        let url = "postgres://user:pa@ss@wo@rd@host:5432/db";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "postgres://[redacted]@host:5432/db");
        assert!(!redacted.contains("pa@ss@wo@rd"));
    }

    #[test]
    fn redact_url_fully_redacts_username_with_at() {
        let url = "postgres://us@er:pass@host:5432/db";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "postgres://[redacted]@host:5432/db");
        assert!(!redacted.contains("pass") && !redacted.contains("us@er"));
    }

    #[test]
    fn redact_url_redacts_username_only_no_password() {
        // No password separator — the bare username is still userinfo and is
        // redacted too (a change from the old regex, which required a `:` and
        // left a lone username exposed).
        let url = "postgres://user@host:5432/db";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "postgres://[redacted]@host:5432/db");
    }

    #[test]
    fn redact_url_unparseable_with_at_is_conservatively_redacted() {
        let url = "not a valid url but has user:pass@host in it";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "[redacted]");
    }

    #[test]
    fn redact_url_unparseable_without_at_returns_none() {
        let url = "not a valid url at all";
        assert!(redact_url(url).is_none());
    }

    #[test]
    fn redact_url_redacts_libpq_query_param_password() {
        // libpq accepts credentials as query params, not just userinfo.
        let url = "postgresql://localhost/db?user=admin&password=s3cr3t";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "[redacted]");
        assert!(!redacted.contains("s3cr3t"));
    }

    #[test]
    fn redact_url_redacts_libpq_key_value_dsn_password() {
        // libpq key-value DSNs are not URLs at all and fail url::Url::parse.
        let url = "host=localhost dbname=zeph password=s3cr3t";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "[redacted]");
        assert!(!redacted.contains("s3cr3t"));
    }

    #[cfg(all(unix, feature = "sqlite", not(feature = "postgres")))]
    #[tokio::test]
    async fn sqlite_precreated_with_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let cfg = DbConfig {
            url: db_path.to_str().unwrap().to_owned(),
            pool_size: 1,
        };
        cfg.connect().await.unwrap();
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "SQLite DB file must be created with mode 0o600"
        );
    }
}
