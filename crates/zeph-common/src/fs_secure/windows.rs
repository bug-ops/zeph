// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows owner-only file and directory creation.
//!
//! Files and directories are created with a protected DACL that grants `FILE_ALL_ACCESS` to
//! `LocalSystem` and to the current process token's user, and nothing else. The descriptor is
//! supplied to `CreateFileW` / `CreateDirectoryW` through `SECURITY_ATTRIBUTES`, so the
//! restriction is in effect from the first instant the object exists.
//!
//! This is a registered platform-FFI `unsafe` exception: see `specs/010-security` section
//! `unsafe`, row `zeph_common::fs_secure::windows`. No maintained safe crate covers it.
//!
//! Existing files and directories keep their ACL, matching the Unix `mode(0o600)` behavior
//! where the mode only applies at creation.

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
    FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
    OPEN_ALWAYS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

// The struct is a few words long, so the cast cannot truncate.
#[allow(clippy::cast_possible_truncation)]
const SECURITY_ATTRIBUTES_LEN: u32 = size_of::<SECURITY_ATTRIBUTES>() as u32;

/// How [`open_private`] opens or creates the file; mirrors the matching `OpenOptions` combinations.
#[derive(Debug, Clone, Copy)]
pub(super) enum Disposition {
    /// `write + create + truncate`.
    TruncateOrCreate,
    /// `append + create`.
    AppendOrCreate,
    /// `write + create_new`.
    CreateNew,
}

/// Open or create `path` with an owner-only DACL applied when the file is created.
pub(super) fn open_private(path: &Path, disposition: Disposition) -> io::Result<File> {
    let (access, creation) = match disposition {
        Disposition::TruncateOrCreate => (GENERIC_WRITE, CREATE_ALWAYS),
        Disposition::AppendOrCreate => (FILE_GENERIC_WRITE & !FILE_WRITE_DATA, OPEN_ALWAYS),
        Disposition::CreateNew => (GENERIC_WRITE, CREATE_NEW),
    };
    let wide = verbatim_wide(path)?;
    let descriptor = SecurityDescriptor::owner_only(false)?;
    let attributes = descriptor.attributes();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `attributes` points into
    // `descriptor`, which is alive until after the call; the template handle is null.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &raw const attributes,
            creation,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid, open file handle returned by `CreateFileW` that nothing
    // else owns; `File` takes over closing it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Create `path` and any missing parents with an owner-only DACL inherited by their children.
///
/// Existing components are left untouched. A concurrent creator winning the race
/// (`ERROR_ALREADY_EXISTS`) counts as success as long as the path is a directory.
pub(super) fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match path.parent() {
                Some(parent) => create_private_dir_all(parent)?,
                None => return Err(e),
            }
            match create_dir(path) {
                Ok(()) => Ok(()),
                Err(_) if path.is_dir() => Ok(()),
                Err(e) => Err(e),
            }
        }
        Err(_) if path.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

fn create_dir(path: &Path) -> io::Result<()> {
    let wide = verbatim_wide(path)?;
    let descriptor = SecurityDescriptor::owner_only(true)?;
    let attributes = descriptor.attributes();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `attributes` points into
    // `descriptor`, which is alive until after the call.
    let ok = unsafe { CreateDirectoryW(wide.as_ptr(), &raw const attributes) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Make `path` absolute and prefix it with `\\?\` (`\\?\UNC\` for network shares), NUL-terminated.
///
/// The verbatim form keeps paths longer than `MAX_PATH` working, as `std::fs` does.
fn verbatim_wide(path: &Path) -> io::Result<Vec<u16>> {
    let absolute = std::path::absolute(path)?;
    let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    let mut out = to_verbatim(&wide);
    out.push(0);
    Ok(out)
}

fn to_verbatim(wide: &[u16]) -> Vec<u16> {
    let starts_with = |prefix: &str| {
        wide.len() >= prefix.len()
            && wide
                .iter()
                .zip(prefix.bytes())
                .all(|(unit, byte)| *unit == u16::from(byte))
    };
    if starts_with(r"\\?\") || starts_with(r"\\.\") {
        return wide.to_vec();
    }
    let (prefix, rest) = if starts_with(r"\\") {
        (r"\\?\UNC\", &wide[2..])
    } else {
        (r"\\?\", wide)
    };
    let mut out: Vec<u16> = prefix.encode_utf16().collect();
    out.extend_from_slice(rest);
    out
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// SDDL for a protected DACL granting `FILE_ALL_ACCESS` to `LocalSystem` and `sid` only.
fn owner_only_sddl(sid: &str, inherit: bool) -> String {
    let flags = if inherit { "OICI" } else { "" };
    format!("D:P(A;{flags};FA;;;SY)(A;{flags};FA;;;{sid})")
}

/// Owner-only security descriptor released with `LocalFree` on drop.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    /// Protected DACL granting full access to `LocalSystem` and the token user only.
    ///
    /// With `inherit` the ACEs carry `OICI` so children of a directory get the same restriction.
    fn owner_only(inherit: bool) -> io::Result<Self> {
        let sid = user_sid()?;
        let wide = to_wide_nul(&owner_only_sddl(sid, inherit));
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `wide` is a NUL-terminated string that outlives the call; `descriptor` is a
        // valid out-pointer; the size out-parameter is optional and passed as null. On success
        // the returned buffer is LocalAlloc-ed and freed in `Drop`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: SECURITY_ATTRIBUTES_LEN,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `self.0` was allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`
        // (LocalAlloc) and is freed exactly once here.
        unsafe { LocalFree(self.0) };
    }
}

/// String form of the current process token's user SID, cached after the first success.
///
/// A failed lookup is not cached, so a transient error does not break every later create.
///
/// The token user, not the object owner, is used on purpose: an elevated process owns new
/// objects as `Administrators`, which would grant the whole group access.
fn user_sid() -> io::Result<&'static str> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(sid) = SID.get() {
        return Ok(sid);
    }
    let sid = query_user_sid()?;
    Ok(SID.get_or_init(|| sid))
}

fn query_user_sid() -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing; `token` is a
    // valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a valid handle just returned by `OpenProcessToken`, owned by us.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let raw = token.as_raw_handle();

    let mut needed: u32 = 0;
    // SAFETY: a null buffer with length 0 is the documented way to query the required size.
    unsafe { GetTokenInformation(raw, TokenUser, null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 elements keep the buffer 8-byte aligned, as `TOKEN_USER` requires.
    let mut buf = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buf` provides at least `needed` writable bytes; `needed` is a valid out-pointer.
    let ok = unsafe {
        GetTokenInformation(
            raw,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            needed,
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above filled `buf` with a `TOKEN_USER`; the buffer is suitably aligned
    // and lives through this function.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut wide: *mut u16 = null_mut();
    // SAFETY: `sid` points into `buf`, valid for the call; `wide` is a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `wide` is a NUL-terminated UTF-16 string allocated with LocalAlloc;
    // it is read once and freed immediately after.
    let result = unsafe {
        let mut len = 0;
        while *wide.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16(std::slice::from_raw_parts(wide, len));
        LocalFree(wide.cast());
        s
    };
    result.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::super::{
        append_private, atomic_write_private, create_private_dir_all, open_private_truncate,
        write_private,
    };
    use super::*;
    use std::io::Write as _;

    fn dacl_aces(path: &Path) -> Vec<String> {
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!(
                "(Get-Acl -LiteralPath '{}').Sddl",
                path.display().to_string().replace('\'', "''")
            ))
            .output()
            .expect("powershell runs");
        assert!(out.status.success(), "Get-Acl failed: {out:?}");
        let sddl = String::from_utf8(out.stdout).unwrap();
        let dacl = sddl
            .split("D:")
            .nth(1)
            .expect("SDDL has a DACL")
            .split("S:")
            .next()
            .unwrap()
            .trim();
        assert!(
            dacl.starts_with('P'),
            "DACL must be protected (no inherited ACEs): {sddl}"
        );
        dacl.split('(')
            .skip(1)
            .map(|ace| ace.trim_end_matches(')').to_owned())
            .collect()
    }

    fn assert_owner_only(path: &Path, inherit: bool) {
        let aces = dacl_aces(path);
        assert_eq!(aces.len(), 2, "exactly SYSTEM + owner ACEs, got {aces:?}");
        let flags = if inherit { "OICI" } else { "" };
        let prefix = format!("A;{flags};FA;;;");
        assert!(
            aces.iter().all(|a| a.starts_with(&prefix)),
            "unexpected ACE type, flags or rights: {aces:?}"
        );
        assert!(
            aces.iter().any(|a| a.ends_with("SY")),
            "SYSTEM ACE missing: {aces:?}"
        );
        for broad in ["WD", "BU", "AU", "BA", "IU"] {
            assert!(
                !aces.iter().any(|a| a.ends_with(broad)),
                "{broad} in {aces:?}"
            );
        }
    }

    #[test]
    fn owner_only_sddl_is_protected_and_grants_exactly_system_and_sid() {
        assert_eq!(
            owner_only_sddl("S-1-5-21-1", false),
            "D:P(A;;FA;;;SY)(A;;FA;;;S-1-5-21-1)"
        );
        assert_eq!(
            owner_only_sddl("S-1-5-21-1", true),
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;S-1-5-21-1)"
        );
    }

    #[test]
    fn open_private_truncate_on_existing_file_truncates_and_keeps_its_acl() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("existing.txt");
        std::fs::write(&p, b"long old content").unwrap();
        let before = dacl_aces(&p);
        let mut f = open_private_truncate(&p).unwrap();
        f.write_all(b"new").unwrap();
        drop(f);
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(dacl_aces(&p), before, "existing file keeps its ACL");
    }

    #[test]
    fn user_sid_looks_like_a_sid() {
        assert!(user_sid().unwrap().starts_with("S-1-"));
    }

    #[test]
    fn to_verbatim_handles_drive_unc_and_verbatim_paths() {
        let enc = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(to_verbatim(&enc(r"C:\a\b")), enc(r"\\?\C:\a\b"));
        assert_eq!(
            to_verbatim(&enc(r"\\srv\share\a")),
            enc(r"\\?\UNC\srv\share\a")
        );
        assert_eq!(to_verbatim(&enc(r"\\?\C:\a")), enc(r"\\?\C:\a"));
        assert_eq!(to_verbatim(&enc(r"\\.\pipe\x")), enc(r"\\.\pipe\x"));
    }

    #[test]
    fn open_private_truncate_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret.txt");
        write_private(&p, b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        assert_owner_only(&p, false);
        let mut f = open_private_truncate(&p).unwrap();
        f.write_all(b"x").unwrap();
        drop(f);
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
    }

    #[test]
    fn append_private_creates_owner_only_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        writeln!(append_private(&p).unwrap(), "line1").unwrap();
        writeln!(append_private(&p).unwrap(), "line2").unwrap();
        assert_owner_only(&p, false);
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.contains("line1") && content.contains("line2"));
    }

    #[test]
    fn atomic_write_private_replaces_existing_destination_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        std::fs::write(&p, b"old").unwrap();
        atomic_write_private(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert!(!p.with_added_extension("tmp").exists());
        assert_owner_only(&p, false);
        atomic_write_private(&p, b"newer").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"newer");
    }

    #[test]
    fn create_private_dir_all_applies_owner_only_acl_to_new_components_only() {
        let root = tempfile::tempdir().unwrap();
        let root_aces = dacl_aces(root.path());
        let leaf = root.path().join("a").join("b");
        create_private_dir_all(&leaf).unwrap();
        assert_owner_only(&root.path().join("a"), true);
        assert_owner_only(&leaf, true);
        assert_eq!(
            dacl_aces(root.path()),
            root_aces,
            "existing parent untouched"
        );
        create_private_dir_all(&leaf).unwrap();
    }

    #[test]
    fn create_private_dir_all_fails_when_a_file_is_in_the_way() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert!(create_private_dir_all(&file).is_err());
        assert!(create_private_dir_all(&file.join("sub")).is_err());
    }

    #[test]
    fn paths_longer_than_max_path_work() {
        let root = tempfile::tempdir().unwrap();
        let mut deep = root.path().to_path_buf();
        for _ in 0..10 {
            deep.push("d".repeat(30));
        }
        assert!(deep.as_os_str().len() > 260);
        create_private_dir_all(&deep).unwrap();
        let file = deep.join("secret.txt");
        write_private(&file, b"deep").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"deep");
    }

    #[test]
    fn concurrent_dir_creation_succeeds_for_every_thread() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("x").join("y").join("z");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        create_private_dir_all(&target)
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }
        });
        assert!(target.is_dir());
    }

    #[test]
    fn interior_nul_is_rejected() {
        let err = verbatim_wide(Path::new("a\0b")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
