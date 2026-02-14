use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::executor::MAX_TOOL_OUTPUT_CHARS;

/// Default overflow directory under user home.
fn overflow_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".zeph/data/tool-output")
}

/// Save full output to overflow file if it exceeds `MAX_TOOL_OUTPUT_CHARS`.
/// Returns the path to the saved file, or `None` if output fits.
pub fn save_overflow(output: &str) -> Option<PathBuf> {
    if output.len() <= MAX_TOOL_OUTPUT_CHARS {
        return None;
    }
    let dir = overflow_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("failed to create overflow dir: {e}");
        return None;
    }
    let id = uuid::Uuid::new_v4();
    let path = dir.join(format!("{id}.txt"));
    if let Err(e) = std::fs::write(&path, output) {
        tracing::warn!("failed to write overflow file: {e}");
        return None;
    }
    Some(path)
}

/// Remove overflow files older than `max_age`. Creates directory if missing.
pub fn cleanup_overflow_files(max_age: Duration) {
    let dir = overflow_dir();
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
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() > max_age
            && let Err(e) = std::fs::remove_file(entry.path())
        {
            tracing::warn!("failed to remove stale overflow file: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_no_overflow() {
        assert!(save_overflow("short").is_none());
    }

    #[test]
    fn overflow_creates_file() {
        let long = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 100);
        let path = save_overflow(&long);
        assert!(path.is_some());
        let p = path.unwrap();
        assert!(p.exists());
        let contents = std::fs::read_to_string(&p).unwrap();
        assert_eq!(contents.len(), long.len());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn stale_files_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("old.txt");
        std::fs::write(&file, "data").unwrap();
        // Set mtime to past by using filetime
        let old_time = std::time::SystemTime::now() - Duration::from_secs(86_500);
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
}
