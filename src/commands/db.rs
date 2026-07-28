// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::bootstrap::{load_config_or_default, resolve_config_path};
use zeph_db::{DbConfig, redact_url};

/// Handle the `zeph db migrate` subcommand.
///
/// Loads config, resolves the database URL, validates it, connects (which runs
/// pending migrations), and prints the result to stderr.
///
/// # Errors
///
/// Returns an error if the config cannot be loaded, the URL is misconfigured, or
/// the database connection / migration fails.
pub(crate) async fn handle_db_migrate(config_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let config_path = resolve_config_path(config_path);
    let config = load_config_or_default(&config_path)?;

    let db_url = crate::db_url::resolve_db_url(&config);

    // C-001: validate that the URL matches the compiled-in backend.
    #[cfg(feature = "postgres")]
    if !zeph_db::is_postgres_url(db_url) {
        let safe = redact_url(db_url).unwrap_or_else(|| "[redacted]".to_owned());
        anyhow::bail!(
            "postgres build requires a postgres:// or postgresql:// URL, but got: {safe:?}. \
             Set database_url in [memory] config or run: \
             zeph vault set ZEPH_DATABASE_URL \"postgres://user:pass@localhost:5432/zeph\""
        );
    }
    #[cfg(feature = "sqlite")]
    if zeph_db::is_postgres_url(db_url) {
        let safe = redact_url(db_url).unwrap_or_else(|| "[redacted]".to_owned());
        anyhow::bail!(
            "sqlite build cannot connect to a postgres:// URL: {safe:?}. \
             Recompile with --features postgres or use a sqlite file path."
        );
    }

    let display_url = redact_url(db_url).unwrap_or_else(|| "[redacted]".to_owned());
    eprintln!("Running migrations on: {display_url}");

    let db_config = DbConfig {
        url: db_url.to_owned(),
        pool_size: 1,
    };

    // connect() runs migrations internally and returns the number applied via tracing.
    // Run RUST_LOG=info to see individual migration names.
    let _pool = db_config.connect().await?;

    eprintln!("Migrations complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command, DbCommand};
    use clap::Parser;

    /// Regression test for #6026: when `redact_url` returns `None` for a
    /// backend-mismatch URL, the fallback must never fall through to the raw
    /// URL. `None` means "unrecognized shape", not "credential-free" — this
    /// query string carries a secret under a key (`token`) that the redaction
    /// heuristic doesn't recognize, so `redact_url` returns `None` even though
    /// the URL is not safe to print as-is.
    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    #[tokio::test]
    async fn db_migrate_backend_mismatch_does_not_leak_raw_url() {
        use super::handle_db_migrate;

        let secret = "s3cr3t-token-do-not-leak";
        let db_url = format!("postgres://db.example.com/zeph?token={secret}");
        assert!(zeph_db::redact_url(&db_url).is_none());

        let toml = zeph_config::Config::dump_defaults().expect("dump default config");
        let toml = toml.replacen(
            "[memory]\n",
            &format!("[memory]\ndatabase_url = \"{db_url}\"\n"),
            1,
        );
        let dir = tempfile::tempdir().expect("create temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml).expect("write temp config");

        let err = handle_db_migrate(Some(&config_path))
            .await
            .expect_err("sqlite build must reject a postgres:// URL");
        let message = err.to_string();
        assert!(
            !message.contains(secret),
            "error message must not leak the raw URL/secret: {message}"
        );
        assert!(
            message.contains("[redacted]"),
            "error message must show the redacted placeholder: {message}"
        );
    }

    /// Mirror of `db_migrate_backend_mismatch_does_not_leak_raw_url` for the
    /// postgres-build backend-mismatch branch (a non-`postgres://` URL rejected
    /// by a postgres-only build). `sqlite` and `postgres` are mutually
    /// exclusive (`zeph-db`'s `compile_error!` guard), so this only compiles
    /// under `--no-default-features --features postgres`.
    #[cfg(all(feature = "postgres", not(feature = "sqlite")))]
    #[tokio::test]
    async fn db_migrate_backend_mismatch_does_not_leak_raw_url_postgres_build() {
        use super::handle_db_migrate;

        let secret = "s3cr3t-token-do-not-leak";
        let db_url = format!("sqlite:///data/zeph.db?token={secret}");
        assert!(zeph_db::redact_url(&db_url).is_none());

        let toml = zeph_config::Config::dump_defaults().expect("dump default config");
        let toml = toml.replacen(
            "[memory]\n",
            &format!("[memory]\ndatabase_url = \"{db_url}\"\n"),
            1,
        );
        let dir = tempfile::tempdir().expect("create temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml).expect("write temp config");

        let err = handle_db_migrate(Some(&config_path))
            .await
            .expect_err("postgres build must reject a non-postgres:// URL");
        let message = err.to_string();
        assert!(
            !message.contains(secret),
            "error message must not leak the raw URL/secret: {message}"
        );
        assert!(
            message.contains("[redacted]"),
            "error message must show the redacted placeholder: {message}"
        );
    }

    #[test]
    fn db_migrate_parses() {
        let cli = Cli::try_parse_from(["zeph", "db", "migrate"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Command::Db {
                command: DbCommand::Migrate
            })
        ));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn is_postgres_url_accepts_postgres_schemes() {
        assert!(zeph_db::is_postgres_url("postgres://localhost/test"));
        assert!(zeph_db::is_postgres_url("postgresql://localhost/test"));
        assert!(!zeph_db::is_postgres_url("/tmp/test.db"));
        assert!(!zeph_db::is_postgres_url("sqlite:///tmp/test.db"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn is_postgres_url_rejects_sqlite_paths() {
        assert!(!zeph_db::is_postgres_url("/tmp/test.db"));
        assert!(!zeph_db::is_postgres_url("sqlite:///tmp/test.db"));
        assert!(zeph_db::is_postgres_url("postgres://localhost/test"));
        assert!(zeph_db::is_postgres_url("postgresql://localhost/test"));
    }
}
