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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::NamedTempFile;
use tracing::warn;

use super::{Sandbox, SandboxError, SandboxPolicy, SandboxProfile};

/// Directories under `$HOME` whose entire subtrees are denied for file-read.
///
/// Rules use `(subpath ...)` — every file inside these directories is blocked.
const SECRET_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".password-store",
    ".config/gh",
    ".config/op",
    ".config/gcloud",
    ".config/hub",
    ".config/glab-cli",
    ".config/lab",
    ".config/rclone",
    ".docker",
    ".kube",
    ".anthropic",
    ".config/anthropic",
    ".claude",
    ".config/claude",
    ".codex",
    ".config/codex",
    ".openai",
    ".subversion/auth",
    "Library/Keychains",
    "Library/Cookies",
    "Library/Application Support/sops",
    ".config/zeph",
];

/// Individual files under `$HOME` denied for file-read via `(literal ...)`.
const SECRET_FILES: &[&str] = &[
    ".git-credentials",
    ".gitconfig",
    ".config/git/credentials",
    ".netrc",
    ".zsh_history",
    ".bash_history",
    ".cargo/credentials.toml",
    ".npmrc",
    ".pypirc",
    ".vault-token",
    "Library/Application Support/sops/age/keys.txt",
];

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
    /// - [`SandboxError::Policy`] when profile serialization or home-dir resolution fails.
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

        let profile_str = generate_sb_profile(policy)?;

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
///
/// Returns `Err(SandboxError::Policy)` when the user home directory cannot be resolved.
/// Failing open (allowing all reads without the deny-first rules) would silently expose
/// secrets, so we fail closed instead.
fn generate_sb_profile(policy: &SandboxPolicy) -> Result<String, SandboxError> {
    let Some(home) = dirs::home_dir() else {
        warn!("sandbox: home_dir() returned None — cannot generate deny-first secret rules");
        return Err(SandboxError::Policy(
            "home_dir() returned None; sandbox profile generation requires a resolvable home \
             directory"
                .into(),
        ));
    };
    Ok(generate_sb_profile_for_home(policy, &home))
}

/// Pure profile-string builder given an explicit `home` path.
///
/// Extracted so that unit tests can call it with a deterministic fake home directory
/// and exercise the real production logic without touching `dirs::home_dir()`.
fn generate_sb_profile_for_home(policy: &SandboxPolicy, home: &Path) -> String {
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

    // Deny well-known secret paths AFTER the global (allow file-read*).
    // Seatbelt uses last-rule-wins semantics, so deny rules placed here override the
    // global allow above and are themselves overridden by any subsequent (allow ...) entries
    // from the user-provided allow_read list below.
    push_secret_deny_rules_for_home(&mut rules, home);

    // Per-path read allow rules are now subsumed by the global (allow file-read*)
    // grant but we keep them in the profile for two reasons:
    //   1. Symmetry with Linux Landlock which strictly requires per-path entries.
    //   2. Explicit documentation of caller intent — future-you may restrict the
    //      global grant and these entries will still carry semantic meaning.
    //   3. User-provided allow_read paths appearing here override deny-first rules
    //      above (last-rule-wins), giving callers an explicit opt-in escape hatch.
    //
    for path in &policy.allow_read {
        let p = escape_sb(&path.to_string_lossy());
        rules.push(format!("(allow file-read* (subpath \"{p}\"))"));
    }

    // Writes imply reads — explicit pair stays for documentation.
    for path in &policy.allow_write {
        let p = escape_sb(&path.to_string_lossy());
        rules.push(format!("(allow file-read* file-write* (subpath \"{p}\"))"));
    }

    if policy.allow_network || policy.profile == SandboxProfile::NetworkAllowAll {
        rules.push("(allow network*)".to_owned());
    }

    // Egress deny rules — appended AFTER any (allow network*) so Seatbelt's
    // last-rule-wins semantics block the listed hosts even when network is allowed.
    // When network is not allowed at all, these deny rules are redundant but harmless.
    //
    // Wildcard patterns `*.example.com` expand to two rules:
    //   - `(remote host "example.com")` — the apex hostname itself
    //   - `(remote host-suffix ".example.com")` — all subdomains
    //
    // Note: `(remote host "X")` does not match IP literals. Bypass via IP address is
    // possible. This is acceptable for the MVP threat model (advisory egress filter).
    for domain in &policy.denied_domains {
        if let Some(base) = domain.strip_prefix("*.") {
            let escaped_base = escape_sb(base);
            let escaped_suffix = escape_sb(domain.strip_prefix('*').unwrap_or(domain));
            rules.push(format!("(deny network* (remote host \"{escaped_base}\"))"));
            rules.push(format!(
                "(deny network* (remote host-suffix \"{escaped_suffix}\"))"
            ));
        } else {
            let escaped = escape_sb(domain);
            rules.push(format!("(deny network* (remote host \"{escaped}\"))"));
        }
    }

    rules.join("\n")
}

/// Appends `(deny file-read* ...)` rules for well-known credential paths under `home`.
///
/// Iterates [`SECRET_DIRS`] (subpath deny) and [`SECRET_FILES`] (literal deny).
/// Placed after the global `(allow file-read*)` so they take effect via last-rule-wins.
///
/// When a path is a symlink, both the canonical (real) path and the symlink path receive
/// deny rules. This ensures that a user-provided `allow_read` entry pointing at the canonical
/// path (as produced by `SandboxPolicy::canonicalized()`) can override the correct deny rule.
fn push_secret_deny_rules_for_home(rules: &mut Vec<String>, home: &Path) {
    for rel in SECRET_DIRS {
        let path: PathBuf = home.join(rel);
        let canonical = std::fs::canonicalize(&path).ok();
        let deny_path = canonical.as_deref().unwrap_or(&path);
        rules.push(format!(
            "(deny file-read* (subpath {}))",
            escape_sb_quoted(&deny_path.to_string_lossy())
        ));
        if let Some(ref c) = canonical
            && c != &path
        {
            rules.push(format!(
                "(deny file-read* (subpath {}))",
                escape_sb_quoted(&path.to_string_lossy())
            ));
        }
    }
    for rel in SECRET_FILES {
        let path: PathBuf = home.join(rel);
        let canonical = std::fs::canonicalize(&path).ok();
        let deny_path = canonical.as_deref().unwrap_or(&path);
        rules.push(format!(
            "(deny file-read* (literal {}))",
            escape_sb_quoted(&deny_path.to_string_lossy())
        ));
        if let Some(ref c) = canonical
            && c != &path
        {
            rules.push(format!(
                "(deny file-read* (literal {}))",
                escape_sb_quoted(&path.to_string_lossy())
            ));
        }
    }
}

/// Wraps a path string in double quotes with internal backslash/quote escaping.
fn escape_sb_quoted(s: &str) -> String {
    format!("\"{}\"", escape_sb(s))
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

    // Collect existing program, args, env overrides, and cwd before clearing.
    // The Command struct is fully replaced below to swap the program to sandbox-exec; without
    // capturing these, caller-configured per-spawn state (skill secret env, cwd, etc.) is
    // silently dropped on the floor — see #3871.
    let original_program = std_cmd.get_program().to_os_string();
    let original_args: Vec<std::ffi::OsString> = std_cmd
        .get_args()
        .map(std::ffi::OsStr::to_os_string)
        .collect();
    let original_envs: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = std_cmd
        .get_envs()
        .map(|(k, v)| (k.to_os_string(), v.map(std::ffi::OsStr::to_os_string)))
        .collect();
    let original_cwd: Option<PathBuf> = std_cmd.get_current_dir().map(Path::to_path_buf);

    // Replace program with sandbox-exec.
    *std_cmd = std::process::Command::new(sandbox_exec);
    std_cmd.arg("-f");
    std_cmd.arg(profile_path);
    std_cmd.arg("--");
    std_cmd.arg(original_program);
    for arg in original_args {
        std_cmd.arg(arg);
    }

    // Restore env overrides and cwd captured above. `Some(v)` = explicit set, `None` = remove.
    for (key, value) in original_envs {
        match value {
            Some(val) => {
                std_cmd.env(key, val);
            }
            None => {
                std_cmd.env_remove(key);
            }
        }
    }
    if let Some(cwd) = original_cwd {
        std_cmd.current_dir(cwd);
    }
    // stdout/stderr piping must be re-applied by the caller (execute_bash already does this
    // before calling wrap, so the Stdio handles are set on the freshly-built std_cmd above).
    // Actually: Stdio configuration is not preserved across Command replacement. The caller
    // (execute_bash) sets stdout/stderr AFTER wrap(), which is the correct order per the
    // architecture spec (wrap rewrites program+args, caller sets I/O after).
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Fixed fake home used across all tests — avoids calling `dirs::home_dir()`.
    const FAKE_HOME: &str = "/tmp/fake-home-test";

    fn fake_home() -> PathBuf {
        PathBuf::from(FAKE_HOME)
    }

    // -- Original baseline tests, now calling the real production function -------------

    #[test]
    fn profile_workspace_denies_network_by_default() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            allow_network: false,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
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
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
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
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains("(allow process-info*)"));
    }

    #[test]
    fn profile_workspace_does_not_grant_global_writes() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
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
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(!profile.contains("(allow file-read* (subpath \"/usr\"))"));
        assert!(!profile.contains("(allow file-read* (subpath \"/bin\"))"));
    }

    // -- Deny-first rules tests (#3086) -----------------------------------------------

    #[test]
    fn test_deny_rules_present() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(
            profile.contains(&format!("(deny file-read* (subpath \"{FAKE_HOME}/.ssh\"))")),
            ".ssh deny rule missing"
        );
        assert!(
            profile.contains(&format!(
                "(deny file-read* (subpath \"{FAKE_HOME}/.config/zeph\"))"
            )),
            ".config/zeph deny rule missing"
        );
        assert!(
            profile.contains(&format!(
                "(deny file-read* (literal \"{FAKE_HOME}/.netrc\"))"
            )),
            ".netrc deny rule missing"
        );
    }

    #[test]
    fn test_deny_ordering() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        let allow_pos = profile
            .find("(allow file-read*)")
            .expect("global allow missing");
        let deny_pos = profile
            .find(&format!("(deny file-read* (subpath \"{FAKE_HOME}/.ssh\"))"))
            .expect("deny rule for .ssh missing");
        assert!(
            deny_pos > allow_pos,
            "deny rule must appear after global (allow file-read*)"
        );
    }

    #[test]
    fn test_readonly_has_deny_rules() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::ReadOnly,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(
            profile.contains(&format!("(deny file-read* (subpath \"{FAKE_HOME}/.ssh\"))")),
            "ReadOnly profile must have deny rules"
        );
    }

    #[test]
    fn test_network_allow_all_has_deny_rules() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::NetworkAllowAll,
            allow_network: true,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(
            profile.contains(&format!("(deny file-read* (subpath \"{FAKE_HOME}/.ssh\"))")),
            "NetworkAllowAll profile must have deny rules"
        );
    }

    #[test]
    fn test_allow_read_override_after_deny() {
        let ssh_path = PathBuf::from(format!("{FAKE_HOME}/.ssh"));
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            allow_read: vec![ssh_path],
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        let deny_rule = format!("(deny file-read* (subpath \"{FAKE_HOME}/.ssh\"))");
        let allow_rule = format!("(allow file-read* (subpath \"{FAKE_HOME}/.ssh\"))");
        let deny_pos = profile.find(&deny_rule).expect("deny rule missing");
        let allow_pos = profile.find(&allow_rule).expect("allow override missing");
        // Last-rule-wins: user allow must appear after deny.
        assert!(
            allow_pos > deny_pos,
            "user allow_read override must appear after deny rule"
        );
    }

    #[test]
    fn home_path_with_quotes_is_escaped() {
        // A home path containing a double-quote must not produce bare unescaped quotes
        // in the Seatbelt profile, which would break the TinyScheme parser.
        let quoted_home = PathBuf::from("/tmp/a\"b-home");
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &quoted_home);
        // Every deny rule line must contain the escaped form \" — never a raw bare "
        // inside the path portion. We check the .ssh rule as the representative case.
        let ssh_rule_line = profile
            .lines()
            .find(|l| l.contains(".ssh") && l.contains("deny"))
            .expect("deny rule for .ssh must be present");
        // The escaped path segment must appear.
        assert!(
            ssh_rule_line.contains(r#"/tmp/a\"b-home"#),
            "quote in home path must be escaped with backslash, got: {ssh_rule_line}"
        );
        // And the raw unescaped sequence (space between /tmp/ and b-home without backslash)
        // must NOT appear.
        assert!(
            !ssh_rule_line.contains("/tmp/a\"b-home/.ssh"),
            "bare unescaped quote must not appear in rule, got: {ssh_rule_line}"
        );
    }

    #[test]
    fn all_37_deny_rules_emitted() {
        // Uses FAKE_HOME (/tmp/fake-home-test) which does not exist on disk, so
        // fs::canonicalize will fail and fall back to the raw path — one rule per entry,
        // same as before. When symlinks are present, additional rules may be emitted
        // (covered by allow_read_overrides_deny_when_ssh_is_symlink).
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        let subpath_denies = profile
            .lines()
            .filter(|l| l.contains("(deny file-read* (subpath"))
            .count();
        let literal_denies = profile
            .lines()
            .filter(|l| l.contains("(deny file-read* (literal"))
            .count();
        assert!(
            subpath_denies >= SECRET_DIRS.len(),
            "expected at least {} subpath deny rules, got {subpath_denies}",
            SECRET_DIRS.len()
        );
        assert!(
            literal_denies >= SECRET_FILES.len(),
            "expected at least {} literal deny rules, got {literal_denies}",
            SECRET_FILES.len()
        );
    }

    // -- denied_domains tests (#3294) -------------------------------------------------

    #[test]
    fn denied_domains_empty_produces_no_deny_network_rule() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::NetworkAllowAll,
            allow_network: true,
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(
            !profile.contains("(deny network* (remote host"),
            "empty denied_domains must produce no egress deny rules"
        );
    }

    #[test]
    fn denied_domains_exact_appended_after_network_allow() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::NetworkAllowAll,
            allow_network: true,
            denied_domains: vec!["pastebin.com".to_owned()],
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        let allow_pos = profile
            .find("(allow network*)")
            .expect("allow network* missing");
        let deny_rule = "(deny network* (remote host \"pastebin.com\"))";
        let deny_pos = profile.find(deny_rule).expect("deny rule missing");
        assert!(
            deny_pos > allow_pos,
            "deny-domain rule must appear after (allow network*)"
        );
    }

    #[test]
    fn denied_domains_wildcard_emits_host_and_host_suffix_rules() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::NetworkAllowAll,
            allow_network: true,
            denied_domains: vec!["*.pastebin.com".to_owned()],
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(
            profile.contains("(deny network* (remote host \"pastebin.com\"))"),
            "wildcard must produce apex host rule"
        );
        assert!(
            profile.contains("(deny network* (remote host-suffix \".pastebin.com\"))"),
            "wildcard must produce host-suffix rule"
        );
    }

    #[test]
    fn denied_domains_hostile_input_escaped() {
        let policy = SandboxPolicy {
            profile: SandboxProfile::NetworkAllowAll,
            allow_network: true,
            denied_domains: vec!["host\"with\"quotes.com".to_owned()],
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(
            profile.contains(r#"host\"with\"quotes.com"#),
            "quotes in domain names must be escaped"
        );
        // The profile must not contain bare (unescaped) quotes inside the host name.
        // We check that the profile does not have the raw domain string with literal quotes.
        let raw = "host\"with\"quotes.com";
        assert!(
            !profile
                .lines()
                .any(|l| l.contains("remote host") && l.contains(raw)),
            "bare unescaped quote must not appear inside a remote host rule"
        );
    }

    #[test]
    fn denied_domains_without_network_allow_still_emitted() {
        // Deny rules are emitted even when network is not allowed — they are
        // redundant but harmless (deny-default already blocks all traffic).
        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            allow_network: false,
            denied_domains: vec!["example.com".to_owned()],
            ..Default::default()
        };
        let profile = generate_sb_profile_for_home(&policy, &fake_home());
        assert!(
            profile.contains("(deny network* (remote host \"example.com\"))"),
            "deny rule must be present even without (allow network*)"
        );
    }

    #[test]
    fn allow_read_overrides_deny_when_ssh_is_symlink() {
        let real_dir = tempfile::tempdir().unwrap();
        let fake_home_dir = tempfile::tempdir().unwrap();
        let symlink_path = fake_home_dir.path().join(".ssh");
        std::os::unix::fs::symlink(real_dir.path(), &symlink_path).unwrap();

        let policy = SandboxPolicy {
            profile: SandboxProfile::Workspace,
            allow_read: vec![symlink_path],
            ..Default::default()
        }
        .canonicalized();

        let profile = generate_sb_profile_for_home(&policy, fake_home_dir.path());

        // On macOS /tmp is a symlink to /private/tmp; canonicalize real_dir to get the
        // resolved path that Seatbelt rules will use.
        let real = std::fs::canonicalize(real_dir.path()).unwrap();
        let real = real.to_string_lossy();
        let deny_real = format!("(deny file-read* (subpath \"{real}\"))");
        let allow_real = format!("(allow file-read* (subpath \"{real}\"))");
        let deny_pos = profile
            .find(&deny_real)
            .expect("deny rule on canonical path must exist");
        let allow_pos = profile
            .find(&allow_real)
            .expect("allow override on canonical path must exist");
        assert!(
            allow_pos > deny_pos,
            "allow must appear after deny (last-rule-wins)"
        );
    }

    /// Regression for #3871: env overrides and cwd set on the original Command must survive
    /// the sandbox-exec rewrite. Before the fix, the inner `*std_cmd = Command::new(...)`
    /// reset replaced the entire struct, dropping `.env(...)` / `.env_remove(...)` / `.current_dir(...)`
    /// entries — breaking skill secret env injection (`x-requires-secrets`) on macOS.
    #[test]
    fn rewrite_preserves_env_overrides_and_cwd() {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg("echo hi");
        cmd.env("GITHUB_TOKEN", "tok-xyz");
        cmd.env("FOO", "bar");
        cmd.env_remove("SECRET_TO_DROP");
        cmd.current_dir("/tmp");

        let sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
        let profile_path = PathBuf::from("/tmp/fake-profile.sb");
        rewrite_command_with_sandbox_exec(&mut cmd, &sandbox_exec, &profile_path);

        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "/usr/bin/sandbox-exec");

        let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                std::ffi::OsStr::new("-f"),
                std::ffi::OsStr::new("/tmp/fake-profile.sb"),
                std::ffi::OsStr::new("--"),
                std::ffi::OsStr::new("bash"),
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("echo hi"),
            ]
        );

        let envs: std::collections::HashMap<std::ffi::OsString, Option<std::ffi::OsString>> =
            std_cmd
                .get_envs()
                .map(|(k, v)| (k.to_os_string(), v.map(std::ffi::OsStr::to_os_string)))
                .collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("GITHUB_TOKEN")),
            Some(&Some(std::ffi::OsString::from("tok-xyz"))),
            "GITHUB_TOKEN must survive sandbox rewrite (#3871)"
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("FOO")),
            Some(&Some(std::ffi::OsString::from("bar")))
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("SECRET_TO_DROP")),
            Some(&None),
            "env_remove entries must also be preserved as removals"
        );

        assert_eq!(
            std_cmd.get_current_dir(),
            Some(std::path::Path::new("/tmp")),
            "current_dir must survive sandbox rewrite"
        );
    }
}
