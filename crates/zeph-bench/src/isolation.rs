// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use crate::BenchError;

/// Per-scenario storage isolation for benchmark runs.
///
/// Before each scenario starts, call [`reset`] to delete and recreate the
/// bench-namespaced `SQLite` database so earlier scenario memories cannot
/// contaminate later ones.
///
/// # Collection naming
///
/// The Qdrant collection name follows the pattern `bench_{dataset}_{run_id}`.
/// Production collections (`zeph_memory`, `zeph_skills`, etc.) are never touched.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use zeph_bench::BenchIsolation;
///
/// let iso = BenchIsolation::new("locomo", "run-2026-01-01", Path::new("/data/bench"));
/// assert_eq!(iso.qdrant_collection, "bench_locomo_run-2026-01-01");
/// assert!(iso.sqlite_db_path.ends_with("bench-run-2026-01-01.db"));
/// ```
///
/// [`reset`]: BenchIsolation::reset
pub struct BenchIsolation {
    /// Qdrant collection name: `bench_{dataset}_{run_id}`.
    pub qdrant_collection: String,
    /// Absolute path to the bench-namespaced `SQLite` database.
    pub sqlite_db_path: PathBuf,
}

impl BenchIsolation {
    /// Create a new isolation context for a benchmark run.
    ///
    /// The Qdrant collection is named `bench_{dataset}_{run_id}` and the `SQLite`
    /// database is placed at `{data_dir}/bench-{run_id}.db`.
    ///
    /// # Note
    ///
    /// `dataset` and `run_id` are not sanitized. Callers should use alphanumeric
    /// values (plus hyphens/underscores) to ensure a valid Qdrant collection name
    /// and a safe filesystem path component.
    #[must_use]
    pub fn new(dataset: &str, run_id: &str, data_dir: &Path) -> Self {
        Self {
            qdrant_collection: format!("bench_{dataset}_{run_id}"),
            sqlite_db_path: data_dir.join(format!("bench-{run_id}.db")),
        }
    }

    /// Reset isolation state for a fresh scenario run.
    ///
    /// Deletes the `SQLite` database file at [`sqlite_db_path`] if it exists, so
    /// memories from a previous scenario cannot bleed into the next one.
    ///
    /// Qdrant isolation is currently a no-op: `zeph-bench` does not depend on
    /// `qdrant-client`, so collection cleanup must be performed externally if
    /// needed. The collection is overwritten on the next run anyway.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Io`] if the `SQLite` file exists but cannot be deleted.
    ///
    /// [`sqlite_db_path`]: BenchIsolation::sqlite_db_path
    // The async signature is part of the public API (callers may await it alongside
    // other futures). The body is synchronous because file deletion is O(1) and
    // std::fs is sufficient without the tokio "fs" feature.
    #[allow(clippy::unused_async)]
    pub async fn reset(&self) -> Result<(), BenchError> {
        if self.sqlite_db_path.exists() {
            // Use std::fs (not tokio::fs) to avoid requiring the tokio "fs" feature.
            std::fs::remove_file(&self.sqlite_db_path)?;
        }
        // Qdrant isolation requires the qdrant feature; currently a no-op.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn collection_name_follows_bench_prefix() {
        let iso = BenchIsolation::new("locomo", "run42", Path::new("/tmp"));
        assert!(iso.qdrant_collection.starts_with("bench_"));
        assert!(!iso.qdrant_collection.contains("zeph_memory"));
        assert!(!iso.qdrant_collection.contains("zeph_skills"));
        assert_eq!(iso.qdrant_collection, "bench_locomo_run42");
    }

    #[test]
    fn sqlite_path_inside_data_dir() {
        let iso = BenchIsolation::new("locomo", "run42", Path::new("/data"));
        assert_eq!(iso.sqlite_db_path, Path::new("/data/bench-run42.db"));
    }

    #[tokio::test]
    async fn reset_deletes_sqlite_file() {
        let dir = tempfile::tempdir().unwrap();
        let iso = BenchIsolation::new("test", "r1", dir.path());
        std::fs::write(&iso.sqlite_db_path, b"data").unwrap();
        iso.reset().await.unwrap();
        assert!(!iso.sqlite_db_path.exists());
    }

    #[tokio::test]
    async fn reset_succeeds_when_db_absent() {
        let dir = tempfile::tempdir().unwrap();
        let iso = BenchIsolation::new("test", "r1", dir.path());
        // No file created — should not error.
        iso.reset().await.unwrap();
    }

    /// Integration test: verifies the NFR-007 reset time budget.
    #[tokio::test]
    #[ignore]
    async fn reset_completes_under_2_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let iso = BenchIsolation::new("test", "timing", dir.path());
        std::fs::write(&iso.sqlite_db_path, b"data").unwrap();
        let start = std::time::Instant::now();
        iso.reset().await.unwrap();
        assert!(start.elapsed().as_secs() < 2, "reset exceeded 2s NFR-007");
    }
}
