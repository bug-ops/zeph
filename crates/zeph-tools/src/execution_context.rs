// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-turn execution environment for tool calls.
//!
//! An [`ExecutionContext`] is attached to a [`crate::ToolCall`] to specify the working
//! directory and environment variable overrides for that specific call. When absent,
//! `ShellExecutor` uses the process CWD and inherited process environment — identical to
//! the behaviour before this module existed.
//!
//! # Trust model
//!
//! Contexts are either *untrusted* (the default, built via the public API) or *trusted*
//! (only constructible inside `zeph-tools` / `zeph-config` via [`ExecutionContext::trusted_from_parts`]).
//!
//! Untrusted contexts have their env overrides re-filtered through the executor's
//! `env_blocklist` after every merge step, so LLM-controlled callers cannot reintroduce
//! a blocked variable.  Trusted contexts bypass that final filter — the operator who
//! authored the TOML `[[execution.environments]]` table is the trust root.
//!
//! # Example
//!
//! ```rust
//! use zeph_tools::ExecutionContext;
//!
//! let ctx = ExecutionContext::new()
//!     .with_name("repo")
//!     .with_cwd("/workspace/myproject")
//!     .with_env("CARGO_TARGET_DIR", "/tmp/cargo-target");
//!
//! assert_eq!(ctx.name(), Some("repo"));
//! assert!(ctx.cwd().is_some());
//! assert_eq!(ctx.env_overrides().get("CARGO_TARGET_DIR").map(String::as_str), Some("/tmp/cargo-target"));
//! assert!(!ctx.is_trusted());
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Per-turn execution environment for a tool call.
///
/// When attached to a [`crate::ToolCall`], executors that honour it (currently
/// [`crate::ShellExecutor`]) use these values instead of the process-level CWD and
/// skill env.  `None` on `ToolCall::context` means "use the executor default" —
/// identical to today's behaviour.
///
/// Cheaply clonable (single `Arc`) so the same context can be shared across
/// parallel tool calls in one DAG layer without copying the underlying data.
///
/// # Precedence
///
/// When the `ShellExecutor` resolves the effective `(cwd, env)` for a call, the
/// highest-priority source wins for each dimension:
///
/// | Source | CWD priority | Env priority |
/// |---|---|---|
/// | `ToolCall.context.cwd` / `env_overrides` | 1 (highest) | 1 (highest) |
/// | Named registry entry (looked up by `name`) | 2 | 2 |
/// | Skill env (`set_skill_env`) | — | 3 |
/// | `default_env` registry entry (when set) | 3 | 4 |
/// | Process CWD | 4 | — |
/// | Inherited process env (minus blocklist) | — | 5 (lowest) |
///
/// Attaching a context for telemetry tagging via `env_overrides` never silently
/// disables `default_env` — a more specific source simply overrides a less specific one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionContext {
    inner: Arc<ExecutionContextInner>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExecutionContextInner {
    /// Logical name matching `[[execution.environments]]` in config. Used for audit/log
    /// lines and to look up unspecified fields from the named registry entry.
    name: Option<String>,
    /// Working directory override. May be relative — `resolve_context` joins it with the
    /// process CWD before sandbox validation.
    cwd: Option<PathBuf>,
    /// Extra environment variables to inject. `BTreeMap` for deterministic audit output
    /// and stable hashing.
    env_overrides: BTreeMap<String, String>,
    /// `true` when built via [`ExecutionContext::trusted_from_parts`]. Trusted contexts
    /// bypass the executor's final `env_blocklist` filter pass (step 6 of the env merge).
    trusted: bool,
}

// ── Public untrusted builder API ──────────────────────────────────────────────

impl ExecutionContext {
    /// Construct an empty, untrusted context.
    ///
    /// Env overrides supplied via [`with_env`](Self::with_env) are subject to the
    /// executor's blocklist filter before reaching the subprocess.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the logical environment name.
    ///
    /// The name is matched against `[[execution.environments]]` in the agent config.
    /// An unknown name produces a [`crate::ToolError::Execution`] at dispatch time.
    #[must_use]
    pub fn with_name(self, name: impl Into<String>) -> Self {
        let mut inner = Arc::unwrap_or_clone(self.inner);
        inner.name = Some(name.into());
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Set the working directory override.
    ///
    /// Relative paths are joined with the process CWD inside `resolve_context` before
    /// sandbox validation. Non-existent paths are a hard error — no fallback to the
    /// process CWD.
    #[must_use]
    pub fn with_cwd(self, cwd: impl Into<PathBuf>) -> Self {
        let mut inner = Arc::unwrap_or_clone(self.inner);
        inner.cwd = Some(cwd.into());
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Add a single environment variable override.
    ///
    /// Overwrites any prior value for the same key.  Untrusted contexts have the final
    /// env re-filtered through the executor's `env_blocklist` — blocklisted keys are
    /// stripped regardless of their source.
    #[must_use]
    pub fn with_env(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let mut inner = Arc::unwrap_or_clone(self.inner);
        inner.env_overrides.insert(key.into(), value.into());
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Add multiple environment variable overrides from an iterator.
    ///
    /// Equivalent to calling [`with_env`](Self::with_env) for each pair.
    #[must_use]
    pub fn with_envs<K, V, I>(self, iter: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        let mut inner = Arc::unwrap_or_clone(self.inner);
        for (k, v) in iter {
            inner.env_overrides.insert(k.into(), v.into());
        }
        Self {
            inner: Arc::new(inner),
        }
    }
}

// ── Accessors ─────────────────────────────────────────────────────────────────

impl ExecutionContext {
    /// The logical environment name, if set.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    /// The working directory override, if set.
    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.inner.cwd.as_deref()
    }

    /// The environment variable overrides.
    #[must_use]
    pub fn env_overrides(&self) -> &BTreeMap<String, String> {
        &self.inner.env_overrides
    }

    /// Whether this context was built via the trusted constructor.
    ///
    /// Trusted contexts bypass the executor's final `env_blocklist` pass.
    /// Only contexts built from operator-authored TOML (via `build_registry`) are trusted.
    ///
    /// # Trust downgrade
    ///
    /// When a call-site context wraps a trusted registry entry by name (via
    /// [`ExecutionContext::with_name`]) but the call-site context itself is untrusted,
    /// `resolve_context` downgrades the effective trust flag to `false`. This prevents
    /// LLM-authored wrappers from escalating privilege by naming a trusted registry entry.
    #[must_use]
    pub fn is_trusted(&self) -> bool {
        self.inner.trusted
    }
}

// ── Trusted constructor (pub(crate) inside zeph-tools) ────────────────────────

impl ExecutionContext {
    /// Construct a trusted context from raw parts.
    ///
    /// Env overrides in trusted contexts bypass the executor's `env_blocklist` final pass.
    ///
    /// **Trust contract**: callers must ensure the values do not originate from LLM output,
    /// plugin code, or any user-controllable source.  The only in-tree producers are:
    /// - `ExecutionConfig::build_registry` (operator-authored TOML via `[[execution.environments]]`).
    /// - Tests that explicitly opt in.
    ///
    /// Marked `pub(crate)` inside `zeph-tools`; re-exported as `pub(crate)` via
    /// `zeph_tools::execution_context` so `zeph-config` can build registry entries without
    /// exposing the constructor to plugins or external crates.
    pub(crate) fn trusted_from_parts(
        name: Option<String>,
        cwd: Option<PathBuf>,
        env_overrides: BTreeMap<String, String>,
    ) -> Self {
        Self {
            inner: Arc::new(ExecutionContextInner {
                name,
                cwd,
                env_overrides,
                trusted: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_untrusted_and_empty() {
        let ctx = ExecutionContext::new();
        assert!(ctx.name().is_none());
        assert!(ctx.cwd().is_none());
        assert!(ctx.env_overrides().is_empty());
        assert!(!ctx.is_trusted());
    }

    #[test]
    fn builder_methods_chain() {
        let ctx = ExecutionContext::new()
            .with_name("test")
            .with_cwd("/tmp")
            .with_env("FOO", "bar");
        assert_eq!(ctx.name(), Some("test"));
        assert_eq!(ctx.cwd(), Some(Path::new("/tmp")));
        assert_eq!(
            ctx.env_overrides().get("FOO").map(String::as_str),
            Some("bar")
        );
        assert!(!ctx.is_trusted());
    }

    #[test]
    fn with_envs_adds_multiple() {
        let ctx = ExecutionContext::new().with_envs([("A", "1"), ("B", "2")]);
        assert_eq!(ctx.env_overrides().len(), 2);
        assert_eq!(ctx.env_overrides()["A"], "1");
        assert_eq!(ctx.env_overrides()["B"], "2");
    }

    #[test]
    fn trusted_from_parts_is_trusted() {
        let ctx = ExecutionContext::trusted_from_parts(
            Some("ops".to_owned()),
            Some(PathBuf::from("/workspace")),
            [("SECRET_KEY".to_owned(), "val".to_owned())]
                .into_iter()
                .collect(),
        );
        assert!(ctx.is_trusted());
        assert_eq!(ctx.name(), Some("ops"));
    }

    #[test]
    fn clone_shares_arc() {
        let ctx = ExecutionContext::new().with_name("shared");
        let cloned = ctx.clone();
        assert_eq!(ctx, cloned);
        assert!(Arc::ptr_eq(&ctx.inner, &cloned.inner));
    }

    #[test]
    fn with_env_overwrites_existing() {
        let ctx = ExecutionContext::new()
            .with_env("KEY", "first")
            .with_env("KEY", "second");
        assert_eq!(ctx.env_overrides()["KEY"], "second");
    }
}
