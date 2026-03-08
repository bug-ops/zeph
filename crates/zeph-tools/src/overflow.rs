// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::OverflowConfig;

fn overflow_dir(custom: Option<&Path>) -> PathBuf {
    if let Some(p) = custom {
        return p.to_path_buf();
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zeph/data/tool-output")
}

/// Save full output to overflow file if it exceeds `config.threshold`.
/// Returns the full absolute path of the saved file, or `None` if output fits or write fails.
pub fn save_overflow(output: &str, config: &OverflowConfig) -> Option<PathBuf> {
    if output.len() <= config.threshold {
        return None;
    }
    let dir = overflow_dir(config.dir.as_deref());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("failed to create overflow dir: {e}");
        return None;
    }
    let canonical_dir = match std::fs::canonicalize(&dir) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to canonicalize overflow dir: {e}");
            return None;
        }
    };
    let filename = format!("{}.txt", uuid::Uuid::new_v4());
    let path = canonical_dir.join(&filename);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(output.as_bytes())
            }) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("failed to write overflow file: {e}");
                return None;
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = std::fs::write(&path, output) {
            tracing::warn!("failed to write overflow file: {e}");
            return None;
        }
    }

    Some(path)
}

/// Remove overflow files older than `config.retention_days`. Creates directory if missing.
pub fn cleanup_overflow_files(config: &OverflowConfig) {
    let dir = overflow_dir(config.dir.as_deref());
    let max_age = Duration::from_secs(config.retention_days * 86_400);
    cleanup_overflow_files_in(&dir, max_age);
}

fn cleanup_overflow_files_in(dir: &Path, max_age: Duration) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("failed to create overflow dir: {e}");
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("failed to read overflow dir: {e}");
            return;
        }
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        // Use symlink_metadata to avoid following symlinks — we only remove regular files.
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        // TOCTOU race between metadata check and remove_file is benign here:
        // the worst case is a spurious warning if another process removes the file first.
        if now.duration_since(modified).unwrap_or_default() > max_age
            && let Err(e) = std::fs::remove_file(entry.path())
        {
            tracing::warn!("failed to remove stale overflow file: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(threshold: usize) -> OverflowConfig {
        OverflowConfig {
            threshold,
            retention_days: 7,
            dir: None,
        }
    }

    #[test]
    fn small_output_no_overflow() {
        assert!(save_overflow("short", &cfg(50_000)).is_none());
    }

    #[test]
    fn overflow_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = OverflowConfig {
            threshold: 50_000,
            retention_days: 7,
            dir: Some(dir.path().to_path_buf()),
        };
        let long = "x".repeat(50_001);
        let path = save_overflow(&long, &config);
        assert!(path.is_some());
        let p = path.unwrap();
        assert!(p.exists());
        let contents = std::fs::read_to_string(&p).unwrap();
        assert_eq!(contents.len(), long.len());
    }

    #[test]
    fn custom_threshold_respected() {
        let dir = tempfile::tempdir().unwrap();
        let output = "x".repeat(1_001);

        let config_low = OverflowConfig {
            threshold: 1_000,
            retention_days: 7,
            dir: Some(dir.path().to_path_buf()),
        };
        assert!(save_overflow(&output, &config_low).is_some());

        assert!(save_overflow(&output, &cfg(2_000)).is_none());
    }

    #[test]
    fn save_returns_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = OverflowConfig {
            threshold: 0,
            retention_days: 7,
            dir: Some(dir.path().to_path_buf()),
        };
        let path = save_overflow("any", &config).unwrap();
        // Must be an absolute path ending in UUID.txt
        assert!(path.is_absolute());
        assert!(path.to_string_lossy().ends_with(".txt"));
    }

    #[test]
    fn custom_dir_used() {
        let dir = tempfile::tempdir().unwrap();
        let config = OverflowConfig {
            threshold: 0,
            retention_days: 7,
            dir: Some(dir.path().to_path_buf()),
        };
        let path = save_overflow("any", &config);
        assert!(path.is_some());
        assert!(path.unwrap().exists());
    }

    #[test]
    fn stale_files_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("old.txt");
        std::fs::write(&file, "data").unwrap();
        let old_time = SystemTime::now() - Duration::from_secs(86_500);
        let ft = filetime::FileTime::from_system_time(old_time);
        filetime::set_file_mtime(&file, ft).unwrap();
        cleanup_overflow_files_in(dir.path(), Duration::from_secs(86_400));
        assert!(!file.exists());
    }

    #[test]
    fn fresh_files_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fresh.txt");
        std::fs::write(&file, "data").unwrap();
        cleanup_overflow_files_in(dir.path(), Duration::from_secs(86_400));
        assert!(file.exists());
    }

    #[test]
    fn missing_dir_created() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub/dir");
        cleanup_overflow_files_in(&sub, Duration::from_secs(86_400));
        assert!(sub.exists());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        // Create a regular file outside the overflow dir that the symlink points to
        let outside_dir = tempfile::tempdir().unwrap();
        let target = outside_dir.path().join("target.txt");
        std::fs::write(&target, "data").unwrap();

        // Create a symlink inside the overflow dir pointing to the external file
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Age the symlink mtime (not the target)
        let old_time = SystemTime::now() - Duration::from_secs(86_500);
        let ft = filetime::FileTime::from_system_time(old_time);
        filetime::set_file_mtime(&link, ft).unwrap();

        cleanup_overflow_files_in(dir.path(), Duration::from_secs(86_400));

        // The symlink is not a regular file — cleanup must not remove it
        assert!(link.exists(), "symlink should not be removed by cleanup");
        // The external target must also be untouched
        assert!(target.exists(), "symlink target must not be removed");
    }

    #[test]
    fn cleanup_uses_retention_days() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("stale.txt");
        std::fs::write(&file, "data").unwrap();
        let old_time = SystemTime::now() - Duration::from_secs(7 * 86_400 + 100);
        let ft = filetime::FileTime::from_system_time(old_time);
        filetime::set_file_mtime(&file, ft).unwrap();

        let config = OverflowConfig {
            threshold: 50_000,
            retention_days: 7,
            dir: Some(dir.path().to_path_buf()),
        };
        cleanup_overflow_files(&config);
        assert!(!file.exists());
    }

    #[test]
    fn default_config_values() {
        let config = OverflowConfig::default();
        assert_eq!(config.threshold, 50_000);
        assert_eq!(config.retention_days, 7);
        assert!(config.dir.is_none());
    }
}
