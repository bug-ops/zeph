// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_core::bootstrap::resolve_config_path;
use zeph_core::config::Config;
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
    let config = Config::load(&config_path).unwrap_or_default();

    let db_url = crate::db_url::resolve_db_url(&config);

    // C-001: validate that the URL matches the compiled-in backend.
    #[cfg(feature = "postgres")]
    if !zeph_db::is_postgres_url(db_url) {
        let safe = redact_url(db_url).unwrap_or_else(|| db_url.to_owned());
        anyhow::bail!(
            "postgres build requires a postgres:// or postgresql:// URL, but got: {safe:?}. \
             Set database_url in [memory] config or run: \
             zeph vault set ZEPH_DATABASE_URL \"postgres://user:pass@localhost:5432/zeph\""
        );
    }
    #[cfg(feature = "sqlite")]
    if zeph_db::is_postgres_url(db_url) {
        let safe = redact_url(db_url).unwrap_or_else(|| db_url.to_owned());
        anyhow::bail!(
            "sqlite build cannot connect to a postgres:// URL: {safe:?}. \
             Recompile with --features postgres or use a sqlite file path."
        );
    }

    let display_url = redact_url(db_url).unwrap_or_else(|| db_url.to_owned());
    eprintln!("Running migrations on: {display_url}");

    let db_config = DbConfig {
        url: db_url.to_owned(),
        max_connections: 1,
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
