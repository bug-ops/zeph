// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! OS-level sandbox abstractions for subprocess tool execution.
//!
//! This module provides a portable [`Sandbox`] trait and platform-specific backends that
//! restrict filesystem, network, and syscall access for shell commands spawned by
//! `ShellExecutor`.
//!
//! # Scope (NFR-SB-1)
//!
//! The sandbox applies **only to subprocess executors** (`ShellExecutor`). In-process executors
//! (`WebScrapeExecutor`, `FileExecutor`) do not spawn a child process and are therefore not
//! subject to OS-level sandboxing. Application-layer controls (allowed hosts, path allowlists)
//! govern those executors instead.
//!
//! # Platform support
//!
//! | Platform | Backend | Compiled |
//! |----------|---------|----------|
//! | macOS | `sandbox-exec` (Seatbelt) | always |
//! | Linux + `sandbox` feature | `bwrap` + Landlock + seccomp | `#[cfg(all(target_os="linux", feature="sandbox"))]` |
//! | Other | `NoopSandbox` (logs WARN) | always |
//!
//! # Example
//!
//! ```rust,no_run
//! use zeph_tools::sandbox::{build_sandbox, SandboxPolicy, SandboxProfile};
//! use tokio::process::Command;
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let policy = SandboxPolicy {
//!     profile: SandboxProfile::Workspace,
//!     allow_read: vec![],
//!     allow_write: vec![std::env::current_dir()?],
//!     allow_network: false,
//!     allow_exec: vec![],
//!     env_inherit: vec![],
//! };
//! let sb = build_sandbox(false)?;
//! let mut cmd = Command::new("bash");
//! cmd.arg("-c").arg("echo hello");
//! sb.wrap(&mut cmd, &policy)?;
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod noop;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(all(target_os = "linux", feature = "sandbox"))]
pub mod linux;

pub use noop::NoopSandbox;

#[cfg(target_os = "macos")]
pub use macos::MacosSandbox;

#[cfg(all(target_os = "linux", feature = "sandbox"))]
pub use linux::LinuxSandbox;

/// Declarative sandbox policy evaluated at command launch.
///
/// Applied *after* blocklist, `PolicyGate`, and `TrustGate` have accepted the call.
/// The sandbox is the last hard boundary, not a replacement for application-level controls.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// The enforcement profile controlling which restrictions are active.
    pub profile: SandboxProfile,
    /// Paths granted read (and execute) access. Normalized to absolute paths at construction.
    ///
    /// Paths are resolved to their canonical (real) form by [`SandboxPolicy::canonicalized`]
    /// before being applied. If a path is a symlink, the resolved target is used for the allow
    /// rule. Deny rules for well-known secret paths are also generated for the canonical form,
    /// so the allow override works correctly even when the denied path is a symlink.
    pub allow_read: Vec<PathBuf>,
    /// Paths granted read and write access. Normalized to absolute paths at construction.
    pub allow_write: Vec<PathBuf>,
    /// Whether unrestricted network egress is permitted.
    pub allow_network: bool,
    /// Additional executables or directories granted execute permission.
    pub allow_exec: Vec<PathBuf>,
    /// Environment variable names or prefixes that are inherited by the sandboxed child.
    pub env_inherit: Vec<String>,
}

impl SandboxPolicy {
    /// Canonicalize all path fields so that symlinks and `..` components cannot bypass
    /// the policy. Paths that cannot be resolved (e.g., non-existent) are dropped
    /// silently — callers must ensure paths exist before adding them to the policy.
    #[must_use]
    pub fn canonicalized(mut self) -> Self {
        self.allow_read = canonicalize_paths(self.allow_read);
        self.allow_write = canonicalize_paths(self.allow_write);
        self.allow_exec = canonicalize_paths(self.allow_exec);
        self
    }
}

fn canonicalize_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter_map(|p| {
            let canonical = std::fs::canonicalize(&p).ok()?;
            if canonical != p {
                tracing::debug!(
                    "sandbox: resolved symlink {} → {}",
                    p.display(),
                    canonical.display()
                );
            }
            Some(canonical)
        })
        .collect()
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        let cwd =
            std::fs::canonicalize(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
                .unwrap_or_else(|_| PathBuf::from("/"));
        Self {
            profile: SandboxProfile::Workspace,
            allow_read: vec![cwd.clone()],
            allow_write: vec![cwd],
            allow_network: false,
            allow_exec: vec![],
            env_inherit: vec![],
        }
    }
}

/// Portable sandbox enforcement profile.
///
/// The profile sets the _baseline_ restrictions. `allow_read`, `allow_write`, and
/// `allow_network` in [`SandboxPolicy`] further refine what is permitted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxProfile {
    /// Read-only to `allow_read` paths, no writes, no network. Exec restricted to `allow_exec` + bash.
    ReadOnly,
    /// Read/write to configured paths; network egress blocked.
    #[default]
    Workspace,
    /// Workspace-level filesystem access plus unrestricted network egress.
    ///
    /// Does **not** curate host/port allowlists. Use application-layer controls for that.
    #[serde(rename = "network-allow-all", alias = "network")]
    NetworkAllowAll,
    /// Sandbox disabled. The subprocess inherits the parent's full capabilities.
    ///
    /// Config authors must set this explicitly to opt out.
    Off,
}

/// Error returned when sandbox setup or policy application fails.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// The OS backend binary or kernel API is unavailable on this system.
    #[error("sandbox backend unavailable: {reason}")]
    Unavailable { reason: String },
    /// The configured policy is not supported by the backend.
    #[error("policy not supported by {backend}: {reason}")]
    UnsupportedPolicy {
        /// Backend name for diagnostics.
        backend: &'static str,
        /// Human-readable explanation.
        reason: String,
    },
    /// I/O error during sandbox setup (e.g. temp file creation).
    #[error("sandbox setup failed: {0}")]
    Setup(#[from] std::io::Error),
    /// Policy string generation failed.
    #[error("policy generation failed: {0}")]
    Policy(String),
}

/// Operating-system sandbox backend.
///
/// `wrap` is the sole entry point. Implementations rewrite a [`tokio::process::Command`]
/// in place so that the next `.spawn()` launches inside the OS sandbox. Implementations
/// must be fork-safe: state installed via the command builder must survive `fork()+exec()`.
///
/// # Contract for implementors
///
/// - Must not spawn the child themselves — only rewrite `cmd`.
/// - Must not use `unsafe` code.
/// - When the profile is [`SandboxProfile::Off`], `wrap` MUST be a no-op.
pub trait Sandbox: Send + Sync + std::fmt::Debug {
    /// Short identifier for logging and diagnostics (e.g., `"macos-seatbelt"`, `"linux-bwrap"`).
    fn name(&self) -> &'static str;

    /// Verify that `policy` is expressible on this backend.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::UnsupportedPolicy`] when a required feature is missing.
    fn supports(&self, policy: &SandboxPolicy) -> Result<(), SandboxError>;

    /// Rewrite `cmd` to execute inside the OS sandbox described by `policy`.
    ///
    /// Called synchronously in the executor thread. Must not block on I/O for more than a few
    /// milliseconds (temp file writes are acceptable; network calls are not).
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if wrapping fails (binary missing, profile generation error, etc.).
    fn wrap(
        &self,
        cmd: &mut tokio::process::Command,
        policy: &SandboxPolicy,
    ) -> Result<(), SandboxError>;
}

/// Construct the best available [`Sandbox`] backend for the current platform.
///
/// Selection order:
/// 1. macOS → `MacosSandbox`
/// 2. Linux + `sandbox` feature → `LinuxSandbox`
/// 3. Fallback → [`NoopSandbox`]
///
/// # Errors
///
/// Returns [`SandboxError::Unavailable`] when `strict = true` and the preferred backend
/// is missing (e.g. `bwrap` not on `PATH`).
pub fn build_sandbox(strict: bool) -> Result<Box<dyn Sandbox>, SandboxError> {
    #[cfg(target_os = "macos")]
    {
        let _ = strict;
        Ok(Box::new(MacosSandbox::new()))
    }

    #[cfg(all(target_os = "linux", feature = "sandbox"))]
    {
        linux::LinuxSandbox::new(strict).map(|s| Box::new(s) as Box<dyn Sandbox>)
    }

    #[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "sandbox"))))]
    {
        if strict {
            return Err(SandboxError::Unavailable {
                reason: "OS sandbox not supported on this platform and strict=true".into(),
            });
        }
        tracing::warn!(
            "OS sandbox not supported on this platform — running without subprocess isolation"
        );
        Ok(Box::new(NoopSandbox))
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "sandbox"))))]
    fn build_sandbox_strict_fails_when_unsupported() {
        let err = build_sandbox(true).expect_err("strict must fail on unsupported platform");
        assert!(matches!(err, SandboxError::Unavailable { .. }));
    }

    #[test]
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "sandbox"))))]
    fn build_sandbox_nonstrict_falls_back_to_noop() {
        let sb = build_sandbox(false).expect("noop fallback ok");
        assert_eq!(sb.name(), "noop");
    }
}
