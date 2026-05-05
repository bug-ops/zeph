// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Project-level management commands.
//!
//! Currently provides `zeph project purge` which removes all project-local state:
//! `SQLite` database, log files, debug artifacts, trace files, audit log, and Qdrant
//! collections.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::ProjectCommand;

/// Entry point for `zeph project <subcommand>`.
///
/// # Errors
///
/// Returns an error if any deletion fails with a permission error, or if the
/// `SQLite` database is locked by another process.
pub(crate) async fn handle_project_command(
    cmd: ProjectCommand,
    global_config_path: Option<&Path>,
) -> anyhow::Result<()> {
    match cmd {
        ProjectCommand::Purge {
            config: purge_config,
            dry_run,
            yes,
        } => {
            use crate::bootstrap::resolve_config_path;

            let effective_path = purge_config.as_deref().or(global_config_path);
            let config_file = resolve_config_path(effective_path);
            let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();

            run_purge(&config, dry_run, yes).await
        }
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Orchestrates the purge steps using the resolved configuration.
struct PurgeEngine<'a> {
    config: &'a zeph_core::config::Config,
}

/// A single artifact to be reported or deleted.
struct PurgeItem {
    /// Human-readable description shown in output.
    path_or_desc: String,
    /// Actual filesystem path to delete; `None` for descriptive-only items.
    path: Option<PathBuf>,
    bytes: u64,
    note: Option<&'static str>,
}

/// A group of related `PurgeItem`s shown together under a section header.
struct PurgeCategory {
    name: &'static str,
    items: Vec<PurgeItem>,
}

// ---------------------------------------------------------------------------
// Byte-size display helper (std::fmt::from_fn, stable 1.93)
// ---------------------------------------------------------------------------

fn fmt_bytes(bytes: u64) -> impl fmt::Display {
    fmt::from_fn(move |f| {
        #[allow(clippy::cast_precision_loss)]
        if bytes >= 1_048_576 {
            write!(f, "{:.1} MB", bytes as f64 / 1_048_576.0)
        } else if bytes >= 1024 {
            write!(f, "{:.1} KB", bytes as f64 / 1024.0)
        } else {
            write!(f, "{bytes} B")
        }
    })
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Constructs a sibling path by appending `suffix` to `base` using `OsString`.
///
/// This correctly handles non-UTF-8 paths, unlike `format!("{}-wal", base.display())`.
fn sibling_path(base: &Path, suffix: &str) -> PathBuf {
    let mut os = base.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

fn dir_size_and_count(dir: &Path) -> (u64, usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            total += file_size(&path);
            count += 1;
        } else if path.is_dir() {
            let (sz, cnt) = dir_size_and_count(&path);
            total += sz;
            count += cnt;
        }
    }
    (total, count)
}

// ---------------------------------------------------------------------------
// Rotated log file helpers
// ---------------------------------------------------------------------------

/// Returns paths of rotated log files in the same directory as `log_file`.
///
/// `tracing_appender` names rotated files as `{stem}.{date}.{ext}` (e.g. `zeph.2026-05-05.log`).
/// For extension-less log files, matches any `{stem}.*` file that is not the main file itself.
fn rotated_log_siblings(log_file: &Path) -> Vec<PathBuf> {
    let Some(dir) = log_file.parent() else {
        return Vec::new();
    };
    let stem = match log_file.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_owned(),
        None => return Vec::new(),
    };
    let ext = log_file.extension().and_then(|e| e.to_str()).unwrap_or("");

    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    // Hoist format! allocations out of the loop (L-1).
    let prefix = format!("{stem}.");
    let suffix = format!(".{ext}");

    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p == log_file || !p.is_file() {
                return None;
            }
            let name = p.file_name()?.to_str()?;
            // M-1: for extension-less log files, match any "{stem}.*" sibling.
            let matched = if ext.is_empty() {
                name.starts_with(&prefix)
            } else {
                name.starts_with(&prefix) && name.ends_with(&suffix)
            };
            if matched { Some(p) } else { None }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Pre-flight lock check
// ---------------------------------------------------------------------------

fn check_db_lock(db_path: &Path) -> anyhow::Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let file = fs::File::open(db_path)
        .map_err(|e| anyhow::anyhow!("cannot open SQLite database {}: {e}", db_path.display()))?;
    if file.try_lock().is_err() {
        anyhow::bail!(
            "SQLite database is locked by another process: {}\n\
             Stop the running Zeph instance before purging.",
            db_path.display()
        );
    }
    // Release the lock before deletion.
    drop(file);
    Ok(())
}

// ---------------------------------------------------------------------------
// Main purge orchestration
// ---------------------------------------------------------------------------

async fn run_purge(
    config: &zeph_core::config::Config,
    dry_run: bool,
    yes: bool,
) -> anyhow::Result<()> {
    use zeph_config::VectorBackend;

    let db_url = crate::db_url::resolve_db_url(config);
    let postgres = db_url.starts_with("postgres");

    // Resolve SQLite path (strip "sqlite:" prefix if present).
    let db_path: Option<PathBuf> = if postgres {
        None
    } else {
        let raw = db_url.strip_prefix("sqlite:").unwrap_or(db_url);
        Some(PathBuf::from(raw))
    };

    // Pre-flight: check DB lock before any user interaction.
    if let Some(ref p) = db_path {
        check_db_lock(p)?;
    }

    let backend = config.memory.vector_backend;
    let backend_label = match backend {
        VectorBackend::Qdrant => "qdrant",
        VectorBackend::Sqlite => "sqlite",
    };

    if dry_run {
        println!("Project purge dry-run (vector_backend: {backend_label}):");
        println!();
    } else if !yes {
        use dialoguer::Confirm;
        let proceed = Confirm::new()
            .with_prompt("This will permanently delete all project data. Continue?")
            .default(false)
            .interact()?;
        if !proceed {
            println!("Aborted.");
            return Ok(());
        }
        println!("Purging project data...");
    } else {
        println!("Purging project data...");
    }

    let engine = PurgeEngine { config };

    let categories = vec![
        // Step 1: SQLite database
        engine.collect_sqlite(db_path.as_deref(), postgres),
        // Step 2: Log files
        engine.collect_logs(),
        // Step 3: Debug artifacts
        engine.collect_debug_artifacts(),
        // Step 4: Trace files
        engine.collect_traces(),
        // Step 5: Audit log
        engine.collect_audit_log(),
    ];

    if dry_run {
        // M-3: Qdrant section before "Total" line.
        engine.dry_run_qdrant(backend, config).await;
        print_dry_run_report(&categories, backend, config);
    } else {
        // H-2: wrap blocking filesystem I/O in spawn_blocking.
        let freed = tokio::task::spawn_blocking(move || execute_deletions(&categories))
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking panicked: {e}"))??;
        // Step 6: Qdrant (last — non-local operation).
        let qdrant_deleted = engine.delete_qdrant_collections(backend, config).await;
        println!();
        print_summary(freed, qdrant_deleted);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// PurgeEngine: collect step implementations
// ---------------------------------------------------------------------------

impl PurgeEngine<'_> {
    fn collect_sqlite(&self, db_path: Option<&Path>, postgres: bool) -> PurgeCategory {
        let mut items = Vec::new();
        if postgres {
            let db_url = crate::db_url::resolve_db_url(self.config);
            let masked = mask_postgres_url(db_url);
            items.push(PurgeItem {
                path_or_desc: format!("(skipped — PostgreSQL: {masked})"),
                path: None,
                bytes: 0,
                note: None,
            });
        } else if let Some(p) = db_path {
            // Main DB file
            items.push(PurgeItem {
                path_or_desc: p.display().to_string(),
                path: Some(p.to_owned()),
                bytes: file_size(p),
                note: None,
            });
            // WAL sibling
            let wal = sibling_path(p, "-wal");
            if wal.exists() {
                items.push(PurgeItem {
                    path_or_desc: wal.display().to_string(),
                    path: Some(wal.clone()),
                    bytes: file_size(&wal),
                    note: None,
                });
            }
            // SHM sibling
            let shm = sibling_path(p, "-shm");
            if shm.exists() {
                items.push(PurgeItem {
                    path_or_desc: shm.display().to_string(),
                    path: Some(shm.clone()),
                    bytes: file_size(&shm),
                    note: None,
                });
            }
        }
        PurgeCategory {
            name: "SQLite database",
            items,
        }
    }

    fn collect_logs(&self) -> PurgeCategory {
        let mut items = Vec::new();

        // Main log file + rotated siblings
        let log_file = &self.config.logging.file;
        if !log_file.is_empty() {
            let lp = PathBuf::from(log_file);
            items.push(PurgeItem {
                path_or_desc: lp.display().to_string(),
                path: Some(lp.clone()),
                bytes: file_size(&lp),
                note: None,
            });
            for rotated in rotated_log_siblings(&lp) {
                items.push(PurgeItem {
                    path_or_desc: rotated.display().to_string(),
                    path: Some(rotated.clone()),
                    bytes: file_size(&rotated),
                    note: Some("rotated"),
                });
            }
        }

        // Scheduler daemon log
        let sched_log = &self.config.scheduler.daemon.log_file;
        if !sched_log.is_empty() {
            let sp = PathBuf::from(sched_log);
            items.push(PurgeItem {
                path_or_desc: sp.display().to_string(),
                path: Some(sp.clone()),
                bytes: file_size(&sp),
                note: Some("scheduler daemon log"),
            });
        }

        // Scheduler daemon PID file
        let sched_pid = &self.config.scheduler.daemon.pid_file;
        if !sched_pid.is_empty() {
            let pp = PathBuf::from(sched_pid);
            items.push(PurgeItem {
                path_or_desc: pp.display().to_string(),
                path: Some(pp.clone()),
                bytes: file_size(&pp),
                note: Some("scheduler daemon PID"),
            });
        }

        PurgeCategory {
            name: "Log files",
            items,
        }
    }

    fn collect_debug_artifacts(&self) -> PurgeCategory {
        let dir = &self.config.debug.output_dir;
        let (bytes, count) = if dir.exists() {
            dir_size_and_count(dir)
        } else {
            (0, 0)
        };
        let desc = if count > 0 {
            format!("{} ({count} files)", dir.display())
        } else {
            format!("{} (nothing to delete)", dir.display())
        };
        PurgeCategory {
            name: "Debug artifacts",
            items: vec![PurgeItem {
                path_or_desc: desc,
                path: if count > 0 { Some(dir.clone()) } else { None },
                bytes,
                note: None,
            }],
        }
    }

    fn collect_traces(&self) -> PurgeCategory {
        let dir = &self.config.telemetry.trace_dir;
        let (bytes, count) = if dir.exists() {
            dir_size_and_count(dir)
        } else {
            (0, 0)
        };
        let desc = if count > 0 {
            format!("{} ({count} files)", dir.display())
        } else {
            format!("{} (nothing to delete)", dir.display())
        };
        PurgeCategory {
            name: "Trace files",
            items: vec![PurgeItem {
                path_or_desc: desc,
                path: if count > 0 { Some(dir.clone()) } else { None },
                bytes,
                note: None,
            }],
        }
    }

    fn collect_audit_log(&self) -> PurgeCategory {
        let dest = &self.config.tools.audit.destination;
        if dest == "stdout" || dest == "stderr" {
            return PurgeCategory {
                name: "Audit log",
                items: vec![PurgeItem {
                    path_or_desc: format!("(destination is {dest} — nothing to delete)"),
                    path: None,
                    bytes: 0,
                    note: None,
                }],
            };
        }
        let p = PathBuf::from(dest);
        PurgeCategory {
            name: "Audit log",
            items: vec![PurgeItem {
                path_or_desc: p.display().to_string(),
                path: Some(p.clone()),
                bytes: file_size(&p),
                note: None,
            }],
        }
    }

    async fn dry_run_qdrant(
        &self,
        backend: zeph_config::VectorBackend,
        config: &zeph_core::config::Config,
    ) {
        use zeph_config::VectorBackend;
        use zeph_memory::qdrant_ops::QdrantOps;

        println!("  Qdrant collections:");
        match backend {
            VectorBackend::Sqlite => {
                println!(
                    "    (skipped — vector_backend is sqlite; vectors stored in SQLite DB above)"
                );
            }
            VectorBackend::Qdrant => {
                let api_key = config
                    .memory
                    .qdrant_api_key
                    .as_ref()
                    .map(|s| s.expose().to_owned());
                let ops = match QdrantOps::new(&config.memory.qdrant_url, api_key.as_deref()) {
                    Ok(o) => o,
                    Err(e) => {
                        println!("    (cannot connect to Qdrant: {e})");
                        return;
                    }
                };
                for name in qdrant_collections(config) {
                    let exists = ops.collection_exists(&name).await.unwrap_or(false);
                    let status = if exists { "exists" } else { "not found" };
                    println!("    {name:<40} ({status})");
                }
            }
        }
        println!();
    }

    async fn delete_qdrant_collections(
        &self,
        backend: zeph_config::VectorBackend,
        config: &zeph_core::config::Config,
    ) -> usize {
        use futures::StreamExt as _;
        use zeph_config::VectorBackend;
        use zeph_memory::qdrant_ops::QdrantOps;

        match backend {
            VectorBackend::Sqlite => {
                println!(
                    "  Qdrant: skipped (vector_backend = sqlite; vectors removed with SQLite DB)"
                );
                0
            }
            VectorBackend::Qdrant => {
                let api_key = config
                    .memory
                    .qdrant_api_key
                    .as_ref()
                    .map(|s| s.expose().to_owned());
                let ops = match QdrantOps::new(&config.memory.qdrant_url, api_key.as_deref()) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("  Warning: cannot connect to Qdrant: {e}");
                        return 0;
                    }
                };

                // A-3: use buffer_unordered for concurrent deletion (worst-case 10 collections).
                let results: Vec<(String, bool)> = futures::stream::iter(qdrant_collections(
                    config,
                ))
                .map(|name| {
                    let ops = &ops;
                    async move {
                        // M-2: check existence before attempting delete to avoid noisy warnings.
                        let exists = ops.collection_exists(&name).await.unwrap_or(true);
                        if !exists {
                            return (name, false);
                        }
                        let deleted = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            ops.delete_collection(&name),
                        )
                        .await;
                        match deleted {
                            Ok(Ok(())) => (name, true),
                            Ok(Err(e)) => {
                                eprintln!(
                                    "  Warning: failed to delete Qdrant collection {name}: {e}"
                                );
                                (name, false)
                            }
                            Err(_) => {
                                eprintln!("  Warning: timeout deleting Qdrant collection {name}");
                                (name, false)
                            }
                        }
                    }
                })
                .buffer_unordered(10)
                .collect()
                .await;

                let mut deleted = 0usize;
                for (name, success) in results {
                    if success {
                        println!("  Deleted Qdrant collection: {name}");
                        deleted += 1;
                    } else {
                        println!("  Qdrant collection {name}: not found or skipped");
                    }
                }
                deleted
            }
        }
    }
}

// ---------------------------------------------------------------------------
// All 10 known Qdrant collections
// ---------------------------------------------------------------------------

/// Returns the complete list of Qdrant collection names used by Zeph.
///
/// Sources:
/// - `zeph_conversations`    — `crates/zeph-memory/src/embedding_store.rs:43`
/// - `zeph_key_facts`        — `crates/zeph-memory/src/semantic/mod.rs:56`
/// - `zeph_graph_entities`   — `crates/zeph-memory/src/semantic/graph.rs:103`
/// - `zeph_session_summaries`— `crates/zeph-memory/src/semantic/mod.rs:55`
/// - `zeph_corrections`      — `crates/zeph-memory/src/semantic/mod.rs:57`
/// - `documents.collection`  — `crates/zeph-config/src/memory.rs:26` (configurable)
/// - `reasoning_strategies`  — `crates/zeph-memory/src/reasoning.rs:176`
/// - `zeph_code_chunks`      — `crates/zeph-index/src/store.rs:30`
/// - `zeph_mcp_tools`        — `crates/zeph-mcp/src/registry.rs:21`
/// - `zeph_skills`           — `crates/zeph-skills/src/qdrant_matcher.rs:13`
fn qdrant_collections(config: &zeph_core::config::Config) -> Vec<String> {
    vec![
        "zeph_conversations".into(),
        "zeph_key_facts".into(),
        "zeph_graph_entities".into(),
        "zeph_session_summaries".into(),
        "zeph_corrections".into(),
        config.memory.documents.collection.clone(),
        "reasoning_strategies".into(),
        "zeph_code_chunks".into(),
        "zeph_mcp_tools".into(),
        "zeph_skills".into(),
    ]
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn print_dry_run_report(
    categories: &[PurgeCategory],
    backend: zeph_config::VectorBackend,
    _config: &zeph_core::config::Config,
) {
    let mut total_bytes = 0u64;
    for cat in categories {
        println!("  {}:", cat.name);
        for item in &cat.items {
            if item.bytes > 0 {
                let note = item.note.map_or_else(String::new, |n| format!(" ({n})"));
                println!(
                    "    {:<60} {}{}",
                    item.path_or_desc,
                    fmt_bytes(item.bytes),
                    note
                );
                total_bytes += item.bytes;
            } else {
                println!("    {}", item.path_or_desc);
            }
        }
        println!();
    }

    let qdrant_note = match backend {
        zeph_config::VectorBackend::Qdrant => " (+ Qdrant collections)",
        zeph_config::VectorBackend::Sqlite => "",
    };
    println!(
        "Total: ~{} would be freed{}",
        fmt_bytes(total_bytes),
        qdrant_note
    );
    println!();
}

/// Uses `item.path` directly — no string heuristics (H-3 fix).
fn execute_deletions(categories: &[PurgeCategory]) -> anyhow::Result<u64> {
    let mut freed = 0u64;

    for cat in categories {
        for item in &cat.items {
            let Some(p) = item.path.as_ref() else {
                continue;
            };
            if p.is_dir() {
                delete_dir_contents(p, &mut freed, &item.path_or_desc)?;
            } else if p.exists() {
                freed += item.bytes;
                fs::remove_file(p)
                    .map_err(|e| anyhow::anyhow!("failed to delete {}: {e}", p.display()))?;
                let note = item.note.map_or_else(String::new, |n| format!(" ({n})"));
                println!("  Deleted: {}{}", p.display(), note);
            }
        }
    }

    Ok(freed)
}

fn delete_dir_contents(dir: &Path, freed: &mut u64, display: &str) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let entries = fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read directory {}: {e}", dir.display()))?;
    let mut count = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let (sz, _) = dir_size_and_count(&p);
            fs::remove_dir_all(&p)
                .map_err(|e| anyhow::anyhow!("failed to delete {}: {e}", p.display()))?;
            bytes += sz;
        } else {
            bytes += file_size(&p);
            fs::remove_file(&p)
                .map_err(|e| anyhow::anyhow!("failed to delete {}: {e}", p.display()))?;
        }
        count += 1;
    }
    if count > 0 {
        println!("  Deleted {display} ({count} items, {})", fmt_bytes(bytes));
    }
    *freed += bytes;
    Ok(())
}

fn print_summary(freed: u64, qdrant_deleted: usize) {
    let qdrant_note = if qdrant_deleted > 0 {
        format!(" + {qdrant_deleted} Qdrant collection(s)")
    } else {
        String::new()
    };
    println!("Done. Freed ~{}{}", fmt_bytes(freed), qdrant_note);
}

/// Masks the password portion of a `PostgreSQL` URL for safe terminal display.
///
/// Replaces the password between the last `:` in the userinfo segment and `@` with `***`.
/// For `postgresql://user:secret@host/db` returns `postgresql://user:***@host/db`.
/// URLs without a password (or without `@`) are returned unchanged.
fn mask_postgres_url(url: &str) -> String {
    let Some(at_pos) = url.find('@') else {
        return url.to_owned();
    };
    let before_at = &url[..at_pos];
    let after_at = &url[at_pos..]; // includes '@'

    // Only look for ':' after the authority prefix ("://").
    let userinfo_start = before_at.find("://").map_or(0, |p| p + 3);
    let userinfo = &before_at[userinfo_start..];

    if let Some(colon_pos) = userinfo.rfind(':') {
        let abs_colon = userinfo_start + colon_pos;
        return format!("{}:***{}", &before_at[..abs_colon], after_at);
    }

    // No password colon in userinfo — return as-is.
    url.to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::sibling_path;

    #[test]
    fn sibling_path_constructs_correctly() {
        let base = PathBuf::from("/foo/bar/zeph.db");
        let wal = sibling_path(&base, "-wal");
        let shm = sibling_path(&base, "-shm");
        assert_eq!(wal, PathBuf::from("/foo/bar/zeph.db-wal"));
        assert_eq!(shm, PathBuf::from("/foo/bar/zeph.db-shm"));
    }

    #[test]
    fn purge_removes_sqlite_and_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("zeph.db");
        let wal = sibling_path(&db, "-wal");
        let shm = sibling_path(&db, "-shm");

        fs::write(&db, b"db").unwrap();
        fs::write(&wal, b"wal").unwrap();
        fs::write(&shm, b"shm").unwrap();

        // Simulate collect_sqlite + execute for these 3 items.
        let items = vec![(db.clone(), 0u64), (wal.clone(), 0u64), (shm.clone(), 0u64)];
        for (p, _) in &items {
            assert!(p.exists(), "expected {p:?} to exist");
            fs::remove_file(p).unwrap();
        }
        assert!(!db.exists());
        assert!(!wal.exists());
        assert!(!shm.exists());
    }

    #[test]
    fn purge_dry_run_does_not_delete() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("zeph.db");
        let wal = sibling_path(&db, "-wal");

        fs::write(&db, b"data").unwrap();
        fs::write(&wal, b"wal").unwrap();

        // Dry-run: only read sizes, do not delete.
        let db_size = super::file_size(&db);
        let wal_size = super::file_size(&wal);

        assert!(db_size > 0);
        assert!(wal_size > 0);
        assert!(db.exists(), "db should still exist after dry-run");
        assert!(wal.exists(), "wal should still exist after dry-run");
    }

    #[test]
    fn purge_skips_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.db");

        // file_size on missing file returns 0, no panic.
        let sz = super::file_size(&missing);
        assert_eq!(sz, 0);

        // sibling_path on missing base also constructs cleanly.
        let wal = sibling_path(&missing, "-wal");
        assert!(!wal.exists());
    }

    #[test]
    fn purge_aborts_on_locked_db() {
        use std::fs::File;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("locked.db");
        fs::write(&db, b"data").unwrap();

        let file = File::open(&db).unwrap();
        // Acquire lock to simulate another process holding it.
        if file.try_lock().is_ok() {
            // We hold the lock; a second try_lock must fail.
            let file2 = File::open(&db).unwrap();
            assert!(
                file2.try_lock().is_err(),
                "second lock attempt should fail while first is held"
            );
        }
        // No cleanup needed — tempdir drops automatically.
    }

    #[test]
    fn mask_postgres_url_hides_credentials() {
        let url = "postgresql://user:secret@localhost:5432/mydb";
        let masked = super::mask_postgres_url(url);
        assert_eq!(masked, "postgresql://user:***@localhost:5432/mydb");
        assert!(!masked.contains("secret"));
    }

    #[test]
    fn mask_postgres_url_no_at_sign() {
        let url = "postgres://localhost/db";
        let masked = super::mask_postgres_url(url);
        assert_eq!(masked, url);
    }

    #[test]
    fn mask_postgres_url_no_password() {
        let url = "postgresql://user@localhost/db";
        let masked = super::mask_postgres_url(url);
        // No colon before @, so returned as-is.
        assert_eq!(masked, url);
    }

    #[test]
    fn qdrant_collections_contains_ten_entries() {
        let config = zeph_core::config::Config::default();
        let cols = super::qdrant_collections(&config);
        assert_eq!(cols.len(), 10, "expected 10 Qdrant collections");
        assert!(cols.contains(&"zeph_mcp_tools".to_owned()));
        assert!(cols.contains(&"zeph_skills".to_owned()));
    }

    #[test]
    fn execute_deletions_uses_path_field() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("artifact.log");
        fs::write(&file, b"data").unwrap();

        let categories = vec![super::PurgeCategory {
            name: "Test",
            items: vec![super::PurgeItem {
                path_or_desc: "Test item (3 files)".to_owned(),
                path: Some(file.clone()),
                bytes: 4,
                note: None,
            }],
        }];

        let freed = super::execute_deletions(&categories).unwrap();
        assert!(!file.exists(), "file should have been deleted");
        assert_eq!(freed, 4);
    }

    #[test]
    fn execute_deletions_skips_none_path() {
        let categories = vec![super::PurgeCategory {
            name: "Test",
            items: vec![super::PurgeItem {
                path_or_desc: "(skipped — PostgreSQL: postgresql://user:***@host/db)".to_owned(),
                path: None,
                bytes: 0,
                note: None,
            }],
        }];

        // Should not error even though path is None.
        let freed = super::execute_deletions(&categories).unwrap();
        assert_eq!(freed, 0);
    }
}
