// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Filesystem helpers that create files with owner-only permissions (0o600) on Unix.
//!
//! Every sensitive file written by Zeph (vault ciphertext, audit JSONL, debug dumps,
//! router state, transcript sidecars) must be created through one of these helpers so
//! that the permission guarantee is auditable in a single location.
//!
//! # Unix vs non-Unix
//!
//! On Unix the helpers set mode `0o600` via `OpenOptionsExt::mode`. On non-Unix
//! platforms (Windows) the helpers fall back to plain [`OpenOptions`] without extra
//! permissions — Windows uses ACLs rather than mode bits, and proper ACL hardening
//! requires additional platform-specific code (TODO: tracked for a follow-up issue).
//! The Windows fallback is **not atomic** for [`atomic_write_private`]: `std::fs::rename`
//! fails with `ERROR_ALREADY_EXISTS` when the destination already exists, unlike the
//! POSIX atomic-replace semantics.
//!
//! # Residual risks
//!
//! - The fixed `.tmp` suffix in [`atomic_write_private`] is a symlink-race target on
//!   shared directories. Callers that open files in directories they do not own must
//!   use `tempfile::NamedTempFile::persist` instead.
//! - `SQLite` WAL/SHM sidecar files (`.db-wal`, `.db-shm`) are created by sqlx after the
//!   pool opens and inherit the process umask. There is no way to prevent this without
//!   upstream sqlx support; see `zeph-db` for best-effort post-open chmod.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Create or truncate `path` with owner-read/write-only permissions on Unix (0o600).
///
/// Returns a writable [`File`] handle. The caller is responsible for writing content
/// and flushing. Use [`write_private`] for a one-shot write convenience.
///
/// On non-Unix platforms falls back to standard `OpenOptions` without extra permissions.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file cannot be opened or created.
///
/// # Examples
///
/// ```no_run
/// use std::io::Write as _;
/// use zeph_common::fs_secure;
///
/// let mut f = fs_secure::open_private_truncate(std::path::Path::new("/tmp/secret.txt"))?;
/// f.write_all(b"hello")?;
/// f.flush()?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn open_private_truncate(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
}

/// Open `path` in append mode, creating it with mode 0o600 on Unix if it does not exist.
///
/// Subsequent opens of an existing file do not change its permissions. Use this helper
/// for JSONL log files (audit, transcript) that grow across multiple process invocations.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file cannot be opened or created.
///
/// # Examples
///
/// ```no_run
/// use std::io::Write as _;
/// use zeph_common::fs_secure;
///
/// let mut f = fs_secure::append_private(std::path::Path::new("/tmp/audit.jsonl"))?;
/// writeln!(f, r#"{{"event":"start"}}"#)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn append_private(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().create(true).append(true).open(path)
    }
}

/// Write `data` to `path`, creating or truncating the file with mode 0o600 on Unix.
///
/// This is a one-shot convenience wrapper around [`open_private_truncate`] that handles
/// `write_all` and `flush`. For streaming writes use [`open_private_truncate`] directly.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file cannot be created, written to, or
/// flushed.
///
/// # Examples
///
/// ```no_run
/// use zeph_common::fs_secure;
///
/// fs_secure::write_private(std::path::Path::new("/tmp/dump.json"), b"{}")?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut f = open_private_truncate(path)?;
    f.write_all(data)?;
    f.flush()
}

/// Write `data` to `path` via a crash-safe replace: write to `<path>.tmp` (0o600 on
/// Unix), fsync the tmp file, rename it over the target, then fsync the parent directory.
///
/// Using [`Path::with_added_extension`] preserves the original extension:
/// `secrets.age` → `secrets.age.tmp` (not `secrets.tmp`).
///
/// On error during write or rename the `.tmp` file is removed to avoid orphan sidecars.
/// Any stale `.tmp` from a prior crash is removed before creating the exclusive tmp file.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if any step fails. The target file is untouched
/// when an error is returned.
///
/// # Examples
///
/// ```no_run
/// use zeph_common::fs_secure;
///
/// fs_secure::atomic_write_private(std::path::Path::new("/tmp/state.json"), b"{}")?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn atomic_write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_added_extension("tmp");

    // Remove any stale .tmp leftover (crash or attacker symlink) before creating
    // the tmp file exclusively. remove_file on a symlink removes the symlink itself,
    // not the target, so O_EXCL then succeeds safely.
    let _ = std::fs::remove_file(&tmp);

    // Write and fsync the tmp file; clean up on any error.
    let write_result = (|| -> io::Result<()> {
        let mut f = open_private_exclusive(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // Atomic rename; clean up tmp on failure.
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;

    // Fsync the parent directory so the rename is durable.
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }

    Ok(())
}

/// Create `path` exclusively (`O_EXCL` / `create_new`) with 0o600 on Unix.
///
/// Used internally by [`atomic_write_private`] for the `.tmp` file so that a
/// pre-existing leftover or attacker-placed symlink is never silently followed.
fn open_private_exclusive(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().write(true).create_new(true).open(path)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_private_creates_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret.txt");
        write_private(&p, b"hello").unwrap();
        assert_eq!(mode(&p), 0o600);
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn atomic_write_private_overwrites_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        // Pre-create with 0o644 to verify the replace changes the mode.
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o644)
                .open(&p)
                .unwrap()
                .write_all(b"old")
                .unwrap();
        }
        atomic_write_private(&p, b"new").unwrap();
        assert_eq!(mode(&p), 0o600);
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_private_preserves_extension_appends_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.age");
        // The tmp path must be "vault.age.tmp", not "vault.tmp".
        let tmp = p.with_added_extension("tmp");
        assert_eq!(tmp.file_name().unwrap(), "vault.age.tmp");
        atomic_write_private(&p, b"data").unwrap();
        assert!(p.exists());
        assert!(!tmp.exists(), "tmp must be cleaned up after success");
    }

    #[test]
    fn atomic_write_private_cleans_tmp_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data.json");
        atomic_write_private(&p, b"{}").unwrap();
        let tmp = p.with_added_extension("tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn atomic_write_private_errors_on_unwritable_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        // Verify that atomic_write_private returns an error when the directory is
        // not writable (no writeable dir → cannot create tmp), and the original
        // target file is untouched.
        let outer = tempfile::tempdir().unwrap();
        let inner = outer.path().join("sub");
        std::fs::create_dir(&inner).unwrap();
        let p = inner.join("data.json");

        // First write succeeds — establishes the file with content "first".
        atomic_write_private(&p, b"first").unwrap();

        // Make the directory read-only so the exclusive tmp create fails.
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = atomic_write_private(&p, b"second");
        // Restore perms for tempdir cleanup before asserting (avoids double-fault).
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err(), "write to read-only dir must fail");
        // Original file must be untouched.
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
    }

    #[test]
    fn atomic_write_private_stale_tmp_removed_before_write() {
        // Stale .tmp leftover should be removed before exclusive create.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let tmp = p.with_added_extension("tmp");
        std::fs::write(&tmp, b"stale").unwrap();
        atomic_write_private(&p, b"fresh").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"fresh");
        assert!(!tmp.exists());
    }

    #[test]
    fn append_private_creates_0600_on_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        {
            let mut f = append_private(&p).unwrap();
            writeln!(f, "line1").unwrap();
        }
        assert_eq!(mode(&p), 0o600);
    }

    #[test]
    fn append_private_preserves_mode_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        {
            let mut f = append_private(&p).unwrap();
            writeln!(f, "line1").unwrap();
        }
        {
            let mut f = append_private(&p).unwrap();
            writeln!(f, "line2").unwrap();
        }
        assert_eq!(mode(&p), 0o600);
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.contains("line1") && content.contains("line2"));
    }
}
