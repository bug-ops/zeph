// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! macOS Seatbelt sandbox backend using `sandbox-exec`.
//!
//! No FFI and no `sandbox_init` — uses only the `/usr/bin/sandbox-exec` CLI wrapper so
//! that no private Apple symbols are linked.
//!
//! # NFR-SB-2
//!
//! Apple has deprecated `sandbox-exec` as an API but the binary remains functional as of
//! macOS 14. If Apple removes the binary, [`MacosSandbox::wrap`] returns
//! [`SandboxError::Unavailable`] and strict-mode startup fails.

use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tempfile::NamedTempFile;

use super::{Sandbox, SandboxError, SandboxPolicy, SandboxProfile};

/// macOS sandbox backend wrapping commands with `sandbox-exec -f <profile>.sb`.
///
/// Holds a pool of `NamedTempFile` handles that are kept alive until [`MacosSandbox`] itself
/// drops. This ensures each profile file exists on disk from the moment `sandbox-exec` opens
/// it until all outstanding children have had a chance to exec. The pool is bounded by the
/// session lifetime; files are unlinked when `MacosSandbox` drops.
#[derive(Debug, Clone)]
pub struct MacosSandbox {
    // Kept-alive temp files: one per wrap() call, dropped when the sandbox itself drops.
    tmpfiles: Arc<Mutex<Vec<NamedTempFile>>>,
}

impl MacosSandbox {
    /// Create a new `MacosSandbox`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tmpfiles: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Default for MacosSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox for MacosSandbox {
    fn name(&self) -> &'static str {
        "macos-seatbelt"
    }

    fn supports(&self, _policy: &SandboxPolicy) -> Result<(), SandboxError> {
        // sandbox-exec is always present on macOS; profile generation never fails at the
        // supports() stage.
        Ok(())
    }

    /// Rewrites `cmd` to execute via `sandbox-exec -f <profile.sb> -- <original>`.
    ///
    /// # Errors
    ///
    /// - [`SandboxError::Unavailable`] when `sandbox-exec` is not found on `PATH`.
    /// - [`SandboxError::Policy`] when profile serialization fails.
    /// - [`SandboxError::Setup`] on temp-file I/O errors.
    fn wrap(
        &self,
        cmd: &mut tokio::process::Command,
        policy: &SandboxPolicy,
    ) -> Result<(), SandboxError> {
        if policy.profile == SandboxProfile::Off {
            return Ok(());
        }

        // Verify sandbox-exec is available.
        let sandbox_exec = locate_sandbox_exec()?;

        let profile_str = generate_sb_profile(policy);

        // Write profile to a NamedTempFile. We keep the `NamedTempFile` alive by storing
        // it in `self.tmpfiles` — it stays on disk until `MacosSandbox` itself drops.
        // This prevents the race where sandbox-exec opens the profile path after the file
        // was deleted. The pool accumulates one entry per shell invocation (typical session:
        // tens to low hundreds), all cleaned up when the sandbox instance drops at session end.
        let mut tmp = NamedTempFile::new().map_err(SandboxError::Setup)?;
        tmp.write_all(profile_str.as_bytes())
            .map_err(SandboxError::Setup)?;
        tmp.flush().map_err(SandboxError::Setup)?;
        let profile_path = tmp.path().to_path_buf();
        // Store before passing path to command so the file is never unlinked early.
        self.tmpfiles
            .lock()
            .map_err(|_| SandboxError::Policy("tmpfiles lock poisoned".into()))?
            .push(tmp);

        rewrite_command_with_sandbox_exec(cmd, &sandbox_exec, &profile_path);

        Ok(())
    }
}

fn locate_sandbox_exec() -> Result<std::path::PathBuf, SandboxError> {
    let path = std::path::PathBuf::from("/usr/bin/sandbox-exec");
    if path.exists() {
        return Ok(path);
    }
    // Fallback: search PATH.
    if let Ok(found) = which_sandbox_exec() {
        return Ok(found);
    }
    Err(SandboxError::Unavailable {
        reason: "sandbox-exec not found at /usr/bin/sandbox-exec or on PATH".into(),
    })
}

fn which_sandbox_exec() -> Result<std::path::PathBuf, SandboxError> {
    let output = std::process::Command::new("which")
        .arg("sandbox-exec")
        .output()
        .map_err(|e| SandboxError::Unavailable {
            reason: format!("which failed: {e}"),
        })?;
    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout);
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(std::path::PathBuf::from(trimmed));
        }
    }
    Err(SandboxError::Unavailable {
        reason: "sandbox-exec not on PATH".into(),
    })
}

/// Generate a `TinyScheme` `.sb` profile string for the given policy.
fn generate_sb_profile(policy: &SandboxPolicy) -> String {
    let mut rules = vec![
        "(version 1)".to_owned(),
        "(deny default)".to_owned(),
        // Process operations for the child itself.
        "(allow process-exec*)".to_owned(),
        "(allow process-fork)".to_owned(),
        "(allow process-info*)".to_owned(),
        "(allow signal (target self))".to_owned(),
        // Baseline syscalls needed for dylib loading and libSystem initialisation.
        "(allow sysctl-read)".to_owned(),
        "(allow mach-lookup)".to_owned(),
        "(allow ipc-posix*)".to_owned(),
        // Unconditional read access.
        //
        // bash and every dylib-linked macOS binary mmap()s the DYLD shared cache
        // (/System/Volumes/Preboot/Cryptexes/OS/...), stat()s /.file, and reads
        // xattrs on SIP-protected libraries during startup. None of these are
        // reachable via (subpath ...) rules. Matches Apple's pure-computation.sb.
        // Writes, exec, ioctl-write and network remain strictly scoped below (#3077).
        "(allow file-read*)".to_owned(),
    ];

    // Per-path read allow rules are now subsumed by the global (allow file-read*)
    // grant but we keep them in the profile for two reasons:
    //   1. Symmetry with Linux Landlock which strictly requires per-path entries.
    //   2. Explicit documentation of caller intent — future-you may restrict the
    //      global grant and these entries will still carry semantic meaning.
    for path in &policy.allow_read {
        let p = escape_sb(&path.display().to_string());
        rules.push(format!("(allow file-read* (subpath \"{p}\"))"));
    }

    // Writes imply reads — explicit pair stays for documentation.
    for path in &policy.allow_write {
        let p = escape_sb(&path.display().to_string());
        rules.push(format!("(allow file-read* file-write* (subpath \"{p}\"))"));
    }

    if policy.allow_network || policy.profile == SandboxProfile::NetworkAllowAll {
        rules.push("(allow network*)".to_owned());
    }

    rules.join("\n")
}

fn escape_sb(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Rewrite `cmd` so it runs as: `sandbox-exec -f <profile> -- <original_program> [args...]`
fn rewrite_command_with_sandbox_exec(
    cmd: &mut tokio::process::Command,
    sandbox_exec: &Path,
    profile_path: &Path,
) {
    // tokio::process::Command does not expose the current program/args for mutation, so we
    // use a workaround: capture program + args via std::process::Command std-side methods
    // then rebuild as a new tokio command.
    //
    // We cannot read the args back from tokio::process::Command after construction.
    // The architecture spec calls for: wrap() only rewrites, caller does spawn().
    //
    // Strategy: the caller always constructs `Command::new("bash").arg("-c").arg(code)`.
    // We prepend sandbox-exec and keep bash as the sub-command.
    //
    // Replace the program in-place by building a fresh command structure and swapping via
    // the inner std command (tokio::process::Command wraps std::process::Command).
    // Since tokio 1.x does not expose set_program, we rebuild via the `as_std_mut` method.
    let std_cmd = cmd.as_std_mut();

    // Collect existing args before clearing.
    let original_program = std_cmd.get_program().to_os_string();
    let original_args: Vec<std::ffi::OsString> = std_cmd
        .get_args()
        .map(std::ffi::OsStr::to_os_string)
        .collect();

    // Replace program with sandbox-exec.
    *std_cmd = std::process::Command::new(sandbox_exec);
    std_cmd.arg("-f");
    std_cmd.arg(profile_path);
    std_cmd.arg("--");
    std_cmd.arg(original_program);
    for arg in original_args {
        std_cmd.arg(arg);
    }
    // stdout/stderr piping must be re-applied by the caller (execute_bash already does this
    // before calling wrap, so the Stdio handles are set on the freshly-built std_cmd above).
    // Actually: Stdio configuration is not preserved across Command replacement. The caller
    // (execute_bash) sets stdout/stderr AFTER wrap(), which is the correct order per the
    // architecture spec (wrap rewrites program+args, caller sets I/O after).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_workspace_denies_network_by_default() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            allow_network: false,
            ..Default::default()
        };
        let profile = generate_sb_profile(&policy);
        assert!(profile.contains("(deny default)"));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn profile_network_allow_all_permits_network() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::NetworkAllowAll,
            allow_network: true,
            ..Default::default()
        };
        let profile = generate_sb_profile(&policy);
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn profile_off_returns_early() {
        let sb = MacosSandbox::new();
        let policy = SandboxPolicy {
            profile: SandboxProfile::Off,
            ..Default::default()
        };
        let mut cmd = tokio::process::Command::new("bash");
        // Should be a no-op (Ok) even if sandbox-exec missing.
        assert!(sb.wrap(&mut cmd, &policy).is_ok());
    }

    #[test]
    fn escape_quotes_and_backslashes() {
        assert_eq!(escape_sb(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn profile_workspace_grants_global_file_read_wildcard() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile(&policy);
        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains("(allow process-info*)"));
    }

    #[test]
    fn profile_workspace_does_not_grant_global_writes() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile(&policy);
        for line in profile.lines() {
            let t = line.trim();
            assert!(
                !t.starts_with("(allow file-write"),
                "unexpected bare write grant: {t}"
            );
        }
    }

    #[test]
    fn profile_workspace_no_legacy_subpath_rules_for_system_dirs() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile(&policy);
        assert!(!profile.contains("(allow file-read* (subpath \"/usr\"))"));
        assert!(!profile.contains("(allow file-read* (subpath \"/bin\"))"));
    }
}
