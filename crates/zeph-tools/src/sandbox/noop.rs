// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! No-op sandbox backend used when no OS-native implementation is available.

use super::{Sandbox, SandboxError, SandboxPolicy};

/// Identity sandbox that passes the command through unchanged.
///
/// Used on platforms without native sandbox support (Windows, BSDs, etc.) or when
/// the `sandbox` feature is compiled without the Linux backend.
///
/// Emits a single `WARN` log on construction via [`build_sandbox`](super::build_sandbox) so
/// operators are aware that subprocess isolation is absent.
#[derive(Debug, Clone, Copy)]
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn supports(&self, _policy: &SandboxPolicy) -> Result<(), SandboxError> {
        Ok(())
    }

    fn wrap(
        &self,
        _cmd: &mut tokio::process::Command,
        _policy: &SandboxPolicy,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_supports_any_policy() {
        let sb = NoopSandbox;
        let policy = SandboxPolicy::default();
        assert!(sb.supports(&policy).is_ok());
    }

    #[test]
    fn noop_wrap_is_identity() {
        let sb = NoopSandbox;
        let policy = SandboxPolicy::default();
        let mut cmd = tokio::process::Command::new("bash");
        // wrap must not return an error
        assert!(sb.wrap(&mut cmd, &policy).is_ok());
    }
}
