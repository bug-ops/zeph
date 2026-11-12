// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Filesystem helpers that create files with owner-only permissions (0o600) on Unix.
//!
//! Every sensitive file written by Zeph (vault ciphertext, audit JSONL, debug dumps,
//! router state, transcript sidecars) must be created through one of these helpers so
//! that the permission guarantee is auditable in a single location.
//!
//! # Platforms
//!
//! On Unix the helpers set mode `0o600` via `OpenOptionsExt::mode`. On Windows they create
//! files and directories with a protected DACL that grants access only to `LocalSystem` and
//! the current user (see the `windows` submodule); as with the Unix mode, it applies only
//! when an object is created, never to an existing one. Other targets fall back to plain
//! [`std::fs::OpenOptions`] without extra permissions.
//!
//! [`atomic_write_private`] replaces an existing destination on every platform:
//! `std::fs::rename` uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` on Windows.
//!
//! # Residual risks
//!
//! - The fixed `.tmp` suffix in [`atomic_write_private`] is a symlink-race target on
//!   shared directories. Callers that open files in directories they do not own must
//!   use `tempfile::NamedTempFile::persist` instead.
//! - `SQLite` WAL/SHM sidecar files (`.db-wal`, `.db-shm`) are created by sqlx after the
//!   pool opens and inherit the process umask. There is no way to prevent this without
//!   upstream sqlx support; see `zeph-db` for best-effort post-open chmod.

use std::fs::File;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

/// Create or truncate `path` with owner-read/write-only permissions on Unix (0o600).
///
/// Returns a writable [`File`] handle. The caller is responsible for writing content
/// and flushing. Use [`write_private`] for a one-shot write convenience.
///
/// On Windows the file is created with an owner-only DACL; other platforms fall back to
/// standard `OpenOptions` without extra permissions.
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
    #[cfg(windows)]
    {
        windows::open_private(path, windows::Disposition::TruncateOrCreate)
    }
    #[cfg(not(any(unix, windows)))]
    {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
}

/// Open `path` in append mode, creating it with mode 0o600 on Unix (owner-only DACL on Windows) if it does not exist.
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
    #[cfg(windows)]
    {
        windows::open_private(path, windows::Disposition::AppendOrCreate)
    }
    #[cfg(not(any(unix, windows)))]
    {
        OpenOptions::new().create(true).append(true).open(path)
    }
}

/// Open `path` for append like [`append_private`], but refuse symlinks, non-regular files and
/// files owned by another user, with all checks made on the opened descriptor.
///
/// On Unix the file is opened once with `O_CREAT | O_NOFOLLOW | O_NONBLOCK | O_NOCTTY` and mode
/// 0o600, then `fstat`-checked (regular file, owned by the effective uid) and `fchmod`ed through
/// the same descriptor, so there is no window between check and use. A pre-existing file with
/// group or other bits is tightened. Other platforms delegate to [`append_private`].
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] when `path` is a symlink, is not a regular file, or is
/// owned by a different user; otherwise the underlying [`io::Error`].
///
/// # Examples
///
/// ```no_run
/// use zeph_common::fs_secure;
///
/// let _lock = fs_secure::append_private_nofollow(std::path::Path::new("/tmp/registry.lock"))?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn append_private_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use rustix::fs::{FileType, Mode, OFlags};

        let fd = rustix::fs::open(
            path,
            OFlags::WRONLY
                | OFlags::APPEND
                | OFlags::CREATE
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::NOCTTY
                | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|e| {
            if e == rustix::io::Errno::LOOP {
                io::Error::new(io::ErrorKind::InvalidInput, "not a regular file (symlink)")
            } else {
                io::Error::from(e)
            }
        })?;
        let stat = rustix::fs::fstat(&fd).map_err(io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        if stat.st_uid != rustix::process::geteuid().as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file is owned by another user",
            ));
        }
        let new_mode = Mode::from_raw_mode(stat.st_mode).difference(Mode::RWXG | Mode::RWXO);
        rustix::fs::fchmod(&fd, new_mode).map_err(io::Error::from)?;
        Ok(File::from(fd))
    }
    #[cfg(not(unix))]
    {
        append_private(path)
    }
}

/// Write `data` to `path`, creating or truncating the file with mode 0o600 on Unix (owner-only DACL on Windows).
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

/// Write `data` to `path` via a crash-safe replace: write to `<path>.tmp` (owner-only on
/// Unix and Windows), fsync the tmp file, rename it over the target, then fsync the parent directory.
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

/// Create `path` exclusively (`O_EXCL` / `create_new`) with 0o600 on Unix and an owner-only DACL
/// on Windows.
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
    #[cfg(windows)]
    {
        windows::open_private(path, windows::Disposition::CreateNew)
    }
    #[cfg(not(any(unix, windows)))]
    {
        OpenOptions::new().write(true).create_new(true).open(path)
    }
}

/// Clear the group and other permission bits of an existing regular file, without touching
/// its content.
///
/// The file is opened with `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY`, checked with `fstat` to be a
/// regular file, and then `fchmod`ed through the same descriptor, so a symlink or a FIFO/device
/// swapped in at `path` is refused instead of followed or opened blocking. Only the flagged bits
/// are cleared (`0o640` becomes `0o600`, `0o444` becomes `0o400`); owner and special bits stay.
///
/// Returns the new permission bits (without the file-type bits).
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file cannot be opened, or an
/// [`io::ErrorKind::InvalidInput`] error if `path` is not a regular file (symlinks included).
///
/// # Examples
///
/// ```no_run
/// use zeph_common::fs_secure;
///
/// let mode = fs_secure::restrict_to_owner(std::path::Path::new("/tmp/key.txt"))?;
/// assert_eq!(mode & 0o077, 0);
/// # Ok::<(), std::io::Error>(())
/// ```
#[cfg(unix)]
pub fn restrict_to_owner(path: &Path) -> io::Result<u32> {
    use rustix::fs::{FileType, Mode, OFlags};

    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        if e == rustix::io::Errno::LOOP {
            io::Error::new(io::ErrorKind::InvalidInput, "not a regular file (symlink)")
        } else {
            io::Error::from(e)
        }
    })?;
    let stat = rustix::fs::fstat(&fd).map_err(io::Error::from)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let new_mode = Mode::from_raw_mode(stat.st_mode).difference(Mode::RWXG | Mode::RWXO);
    rustix::fs::fchmod(&fd, new_mode).map_err(io::Error::from)?;
    // mode_t is u16 on macOS and u32 on Linux, so the widening is a no-op on Linux only.
    #[allow(clippy::useless_conversion)]
    let raw = u32::from(new_mode.as_raw_mode());
    Ok(raw)
}

/// Create `path` and any missing parent directories with mode 0o700 on Unix, or an owner-only
/// inheritable DACL on Windows.
///
/// Only components that do not exist yet are created; existing directories keep their mode or
/// ACL, so a user-chosen parent such as `~` or `/var/log` is never tightened by accident. On other
/// platforms this is [`std::fs::create_dir_all`].
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if a component cannot be created.
///
/// # Examples
///
/// ```no_run
/// use zeph_common::fs_secure;
///
/// fs_secure::create_private_dir_all(std::path::Path::new("/tmp/zeph/logs"))?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(windows)]
    {
        windows::create_private_dir_all(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Clear the group and other permission bits of an existing directory.
///
/// The directory is opened with `O_DIRECTORY | O_NOFOLLOW` and `fchmod`ed through that
/// descriptor, so a symlink swapped in at `path` is refused instead of followed. Owner and
/// special bits stay (`0o755` becomes `0o700`).
///
/// Returns the new permission bits (without the file-type bits). On Windows the directory's DACL
/// is replaced with a protected, inheritable one granting only `LocalSystem` and the current user
/// (the equivalent of `0o700`), and `0` is returned because ACLs have no mode bits. Other
/// platforms get a no-op returning `0`.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if `path` cannot be opened as a directory, or an
/// [`io::ErrorKind::InvalidInput`] error if it is a symlink (or, on Windows, any reparse point)
/// or not a directory.
///
/// # Examples
///
/// ```no_run
/// use zeph_common::fs_secure;
///
/// let mode = fs_secure::restrict_dir_to_owner(std::path::Path::new("/tmp/zeph/logs"))?;
/// # let _ = mode;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn restrict_dir_to_owner(path: &Path) -> io::Result<u32> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};

        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| {
            if e == rustix::io::Errno::LOOP {
                io::Error::new(io::ErrorKind::InvalidInput, "not a directory (symlink)")
            } else {
                io::Error::from(e)
            }
        })?;
        let stat = rustix::fs::fstat(&fd).map_err(io::Error::from)?;
        let new_mode = Mode::from_raw_mode(stat.st_mode).difference(Mode::RWXG | Mode::RWXO);
        rustix::fs::fchmod(&fd, new_mode).map_err(io::Error::from)?;
        // mode_t is u16 on macOS and u32 on Linux, so the widening is a no-op on Linux only.
        #[allow(clippy::useless_conversion)]
        let raw = u32::from(new_mode.as_raw_mode());
        Ok(raw & 0o7777)
    }
    #[cfg(windows)]
    {
        windows::restrict_dir_to_owner(path).map(|()| 0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(0)
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
    fn create_private_dir_all_creates_0700_and_leaves_existing_dirs_alone() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let leaf = root.path().join("a").join("b");
        create_private_dir_all(&leaf).unwrap();
        assert_eq!(mode(&root.path().join("a")), 0o700);
        assert_eq!(mode(&leaf), 0o700);
        assert_eq!(mode(root.path()), 0o755, "existing parent is not chmodded");
        create_private_dir_all(&leaf).unwrap();
    }

    #[test]
    fn restrict_dir_to_owner_tightens_an_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("logs");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(restrict_dir_to_owner(&dir).unwrap(), 0o700);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn restrict_dir_to_owner_refuses_a_symlink_and_a_file() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(restrict_dir_to_owner(&link).is_err());
        assert_eq!(mode(&target), 0o755, "symlink target must stay untouched");

        let file = root.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(restrict_dir_to_owner(&file).is_err());
    }

    #[test]
    fn append_private_nofollow_refuses_symlink_and_fifo_and_tightens_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = append_private_nofollow(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(append_private_nofollow(&fifo).is_err());

        let lock = dir.path().join("lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        append_private_nofollow(&lock).unwrap();
        assert_eq!(mode(&lock), 0o600);

        let fresh = dir.path().join("fresh");
        append_private_nofollow(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o600);
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

    fn file_with_mode(dir: &Path, name: &str, mode: u32) -> std::path::PathBuf {
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, b"content").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn restrict_to_owner_clears_only_group_and_other_bits() {
        let dir = tempfile::tempdir().unwrap();
        for (before, after) in [
            (0o640, 0o600),
            (0o644, 0o600),
            (0o444, 0o400),
            (0o755, 0o700),
        ] {
            let p = file_with_mode(dir.path(), "f", before);
            assert_eq!(restrict_to_owner(&p).unwrap(), after);
            assert_eq!(mode(&p), after);
        }
    }

    #[test]
    fn restrict_to_owner_keeps_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = file_with_mode(dir.path(), "f", 0o644);
        restrict_to_owner(&p).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"content");
    }

    #[test]
    fn restrict_to_owner_refuses_symlink_and_leaves_target_alone() {
        let dir = tempfile::tempdir().unwrap();
        let target = file_with_mode(dir.path(), "target", 0o644);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = restrict_to_owner(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(mode(&target), 0o644);
    }

    #[test]
    fn restrict_to_owner_refuses_directory_and_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = restrict_to_owner(&sub).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(mode(&sub), 0o755);

        let fifo = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::set_permissions(&fifo, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = restrict_to_owner(&fifo).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(mode(&fifo), 0o644);
    }

    #[test]
    fn restrict_to_owner_missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = restrict_to_owner(&dir.path().join("absent")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
