// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for the per-subagent git worktree isolation feature.
//!
//! The `[worktree]` section controls whether subagents execute inside an isolated
//! git worktree, how that worktree is branched, and how background agents behave.
//! All fields have sensible defaults — existing configs without a `[worktree]`
//! section parse as if the feature is disabled (`enabled = false`).
//!
//! # Example
//!
//! ```toml
//! [worktree]
//! enabled = true
//! base_ref = "head"
//! default_branch = "main"
//! root = ".claude/worktrees"
//! branch_prefix = "agent/"
//! prune_branch_on_remove = false
//! cleanup_on_completion = true
//! bg_isolation = "worktree"
//! ```

use serde::{Deserialize, Serialize};

/// Configuration for the per-subagent git worktree isolation feature.
///
/// When `enabled = true`, each subagent that opts in via
/// `SubAgentPermissions::worktree` receives a dedicated git worktree on a
/// fresh branch, ensuring that file edits from concurrent agents do not
/// interfere with each other or with the main working tree.
///
/// # Examples
///
/// ```
/// use zeph_config::WorktreeConfig;
///
/// let cfg = WorktreeConfig::default();
/// assert!(!cfg.enabled);
/// assert_eq!(cfg.root, ".claude/worktrees");
/// assert_eq!(cfg.branch_prefix, "agent/");
/// assert_eq!(cfg.git_timeout_secs, 30);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct WorktreeConfig {
    /// Enable per-subagent git worktrees. When `false`, no worktrees are created
    /// regardless of other settings.
    pub enabled: bool,
    /// Base commit strategy for new worktree branches.
    pub base_ref: WorktreeBaseRef,
    /// Default remote branch used when `base_ref = "fresh"`.
    ///
    /// Empty string triggers auto-detection of `origin/HEAD`.
    pub default_branch: String,
    /// Root directory for worktrees, relative to the repository root.
    ///
    /// Each worktree is placed in a subdirectory named after the subagent ID.
    pub root: String,
    /// Branch name prefix. The full branch name is `"{prefix}{subagent_id}"`.
    pub branch_prefix: String,
    /// Delete the worktree branch after the worktree is removed.
    ///
    /// When `false` (default), the branch persists so the agent's work can be
    /// reviewed, merged, or discarded manually.
    pub prune_branch_on_remove: bool,
    /// Remove the worktree when the agent completes or is cancelled.
    ///
    /// When `false`, worktrees persist until an explicit `worktree clean` command.
    pub cleanup_on_completion: bool,
    /// Background subagent isolation mode.
    ///
    /// Controls whether background subagents receive a dedicated worktree or
    /// edit the working copy directly.
    pub bg_isolation: BgIsolation,
    /// Per-command timeout for `git` invocations, in seconds.
    ///
    /// Applied to every `git` call issued by the worktree subsystem (e.g.
    /// `git worktree add`, `git fetch`, `git rev-parse`).  Increase this value
    /// on repositories that are slow to clone or when running over high-latency
    /// network links.  A value of `0` is clamped to `1` second by
    /// [`DefaultGitRunner`](https://docs.rs/zeph-worktree/latest/zeph_worktree/git_runner/struct.DefaultGitRunner.html),
    /// not by any call site.
    pub git_timeout_secs: u64,
    /// Maximum number of concurrent worktrees under `root`. `None` (default) means
    /// unlimited.
    ///
    /// Enforced as a creation-time admission cap: a `create()` call that would push
    /// the count of git-registered secondary worktrees to `max_worktrees` or beyond
    /// fails with `WorktreeError::QuotaExceeded` instead of silently growing disk
    /// usage. The count includes worktrees created by *other*, concurrently running
    /// zeph sessions over the same `root` — `max_worktrees` bounds total disk-safe
    /// usage under the repository, not just this session's own worktrees. The check
    /// is a best-effort soft cap: it is not atomic across processes, so two
    /// concurrent `create()` calls can both pass and briefly exceed the configured
    /// maximum by a small margin. Lowering this value below the current worktree
    /// count does not evict existing worktrees; it only blocks new admissions until
    /// an operator runs `zeph worktree clean` or raises the limit. A value of
    /// `Some(0)` is rejected at config-validation time (it would block all worktree
    /// creation).
    pub max_worktrees: Option<usize>,
    /// Soft total-disk-usage threshold, in megabytes, across all worktrees under
    /// `root`. `None` (default) disables disk accounting.
    ///
    /// When exceeded, the reconcile sweep (startup and/or periodic, see
    /// `auto_reconcile_secs` / `reconcile_on_startup`) emits a warning status
    /// indicator and auto-reclaims only git-`prunable` entries — an intact worktree
    /// is never force-removed to satisfy this threshold. The reported total is a sum
    /// of logical file sizes (`metadata().len()`), not on-disk block usage; content
    /// shared via hardlinks across worktrees (e.g. zeph-session blobs) can be
    /// double-counted, so treat the total as an approximation, not exact `du`
    /// output. A value of `Some(0)` is rejected at config-validation time (it would
    /// leave every non-empty worktree permanently over quota).
    pub disk_quota_mb: Option<u64>,
    /// Interval, in seconds, for the supervised background reconcile-and-quota
    /// sweep. `0` (default) disables the periodic sweep.
    ///
    /// When greater than zero, one task is registered with the session
    /// `TaskSupervisor` that, on this cadence, reconciles the git worktree
    /// registry, auto-reclaims `prunable` entries via the same path as
    /// `zeph worktree clean` (non-force), and evaluates `max_worktrees` /
    /// `disk_quota_mb`. Each tick may perform a filesystem walk of every worktree
    /// (potentially multi-gigabyte `target/` directories), so a short interval is
    /// wasteful; an hourly cadence (`3600`) is a reasonable default when enabling
    /// this. `Config::validate` rejects any value in `1..60` — a sub-minute interval
    /// would run a full filesystem walk in a tight loop.
    pub auto_reconcile_secs: u64,
    /// Run one reconcile-and-quota sweep at bootstrap, immediately after the
    /// worktree manager is constructed. Default `true`.
    ///
    /// Recovers from a crash that left `prunable` worktrees behind without waiting
    /// for the first periodic tick, and evaluates `disk_quota_mb` / `max_worktrees`
    /// once per launch even when `auto_reconcile_secs = 0` — without this, a
    /// `disk_quota_mb` set by itself would never be evaluated at all. `Config::validate`
    /// rejects `disk_quota_mb.is_some()` combined with both this field `false` and
    /// `auto_reconcile_secs == 0`, so that exact inert combination cannot reach a running
    /// session. Safe by construction: the startup sweep only ever removes entries git itself
    /// reports as `prunable` (directory or gitdir-link already gone), identical to
    /// `zeph worktree clean` without `--force`.
    pub reconcile_on_startup: bool,
}

fn default_git_timeout_secs() -> u64 {
    30
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_ref: WorktreeBaseRef::default(),
            default_branch: "main".to_owned(),
            root: ".claude/worktrees".to_owned(),
            branch_prefix: "agent/".to_owned(),
            prune_branch_on_remove: false,
            cleanup_on_completion: true,
            bg_isolation: BgIsolation::default(),
            git_timeout_secs: default_git_timeout_secs(),
            max_worktrees: None,
            disk_quota_mb: None,
            auto_reconcile_secs: 0,
            reconcile_on_startup: true,
        }
    }
}

/// Base commit strategy for worktree branches.
///
/// Determines where the new branch for an agent's worktree is forked from.
///
/// # Examples
///
/// ```
/// use zeph_config::WorktreeBaseRef;
///
/// // Default is Head — no network access needed.
/// let base = WorktreeBaseRef::default();
/// assert!(matches!(base, WorktreeBaseRef::Head));
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorktreeBaseRef {
    /// Branch from the local `HEAD` commit. No network access required.
    #[default]
    Head,
    /// Fetch `origin/<default_branch>` and branch from that commit.
    ///
    /// Ensures the agent starts from the latest remote state, at the cost of
    /// a `git fetch` on every spawn.
    Fresh,
}

/// Background subagent isolation mode.
///
/// Controls whether background subagents (spawned implicitly, not by an explicit
/// user command) receive an isolated git worktree or edit the shared working copy.
///
/// # Examples
///
/// ```
/// use zeph_config::BgIsolation;
///
/// // Default is Worktree — background agents are fully isolated.
/// let iso = BgIsolation::default();
/// assert!(matches!(iso, BgIsolation::Worktree));
/// ```
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BgIsolation {
    /// Background subagents receive an isolated git worktree (default).
    ///
    /// This is the recommended setting — it prevents background agents from
    /// accidentally editing files that the user is working on.
    #[default]
    Worktree,
    /// Background subagents edit the working copy directly, without a worktree.
    ///
    /// Use only when worktrees are impractical for the repository (e.g., bare
    /// clones or repos with hooks that break under worktrees).
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn worktree_config_default_values() {
        let cfg = WorktreeConfig::default();
        assert!(!cfg.enabled);
        assert_matches!(cfg.base_ref, WorktreeBaseRef::Head);
        assert_eq!(cfg.default_branch, "main");
        assert_eq!(cfg.root, ".claude/worktrees");
        assert_eq!(cfg.branch_prefix, "agent/");
        assert!(!cfg.prune_branch_on_remove);
        assert!(cfg.cleanup_on_completion);
        assert_eq!(cfg.bg_isolation, BgIsolation::Worktree);
        assert_eq!(cfg.git_timeout_secs, 30);
        assert_eq!(cfg.max_worktrees, None);
        assert_eq!(cfg.disk_quota_mb, None);
        assert_eq!(cfg.auto_reconcile_secs, 0);
        assert!(cfg.reconcile_on_startup);
    }

    #[test]
    fn worktree_config_roundtrip_toml() {
        let cfg = WorktreeConfig::default();
        let serialized = toml::to_string(&cfg).expect("serialize");
        let deserialized: WorktreeConfig = toml::from_str(&serialized).expect("deserialize");
        assert!(!deserialized.enabled);
        assert_eq!(deserialized.root, cfg.root);
        assert_eq!(deserialized.branch_prefix, cfg.branch_prefix);
        assert_eq!(deserialized.bg_isolation, cfg.bg_isolation);
        assert_eq!(deserialized.git_timeout_secs, 30);
        assert_eq!(deserialized.max_worktrees, cfg.max_worktrees);
        assert_eq!(deserialized.disk_quota_mb, cfg.disk_quota_mb);
        assert_eq!(deserialized.auto_reconcile_secs, cfg.auto_reconcile_secs);
        assert_eq!(deserialized.reconcile_on_startup, cfg.reconcile_on_startup);
    }

    #[test]
    fn worktree_base_ref_roundtrip_toml() {
        #[derive(Serialize, Deserialize, Debug)]
        struct Wrapper {
            base_ref: WorktreeBaseRef,
        }
        let head = Wrapper {
            base_ref: WorktreeBaseRef::Head,
        };
        let s = toml::to_string(&head).expect("serialize Head");
        assert!(s.contains("head"), "expected 'head' in: {s}");
        let rt: Wrapper = toml::from_str(&s).expect("deserialize Head");
        assert_matches!(rt.base_ref, WorktreeBaseRef::Head);

        let fresh = Wrapper {
            base_ref: WorktreeBaseRef::Fresh,
        };
        let s = toml::to_string(&fresh).expect("serialize Fresh");
        assert!(s.contains("fresh"), "expected 'fresh' in: {s}");
        let rt: Wrapper = toml::from_str(&s).expect("deserialize Fresh");
        assert_matches!(rt.base_ref, WorktreeBaseRef::Fresh);
    }

    #[test]
    fn bg_isolation_roundtrip_toml() {
        #[derive(Serialize, Deserialize, Debug)]
        struct Wrapper {
            bg_isolation: BgIsolation,
        }
        let iso = Wrapper {
            bg_isolation: BgIsolation::Worktree,
        };
        let s = toml::to_string(&iso).expect("serialize Worktree");
        assert!(s.contains("worktree"), "expected 'worktree' in: {s}");
        let rt: Wrapper = toml::from_str(&s).expect("deserialize Worktree");
        assert_eq!(rt.bg_isolation, BgIsolation::Worktree);

        let none = Wrapper {
            bg_isolation: BgIsolation::None,
        };
        let s = toml::to_string(&none).expect("serialize None");
        assert!(s.contains("none"), "expected 'none' in: {s}");
        let rt: Wrapper = toml::from_str(&s).expect("deserialize None");
        assert_eq!(rt.bg_isolation, BgIsolation::None);
    }

    #[test]
    fn worktree_config_enabled_roundtrip() {
        let toml_src = r#"
enabled = true
base_ref = "fresh"
default_branch = "develop"
root = ".worktrees"
branch_prefix = "bot/"
prune_branch_on_remove = true
cleanup_on_completion = false
bg_isolation = "none"
"#;
        let cfg: WorktreeConfig = toml::from_str(toml_src).expect("deserialize custom");
        assert!(cfg.enabled);
        assert_matches!(cfg.base_ref, WorktreeBaseRef::Fresh);
        assert_eq!(cfg.default_branch, "develop");
        assert_eq!(cfg.root, ".worktrees");
        assert_eq!(cfg.branch_prefix, "bot/");
        assert!(cfg.prune_branch_on_remove);
        assert!(!cfg.cleanup_on_completion);
        assert_eq!(cfg.bg_isolation, BgIsolation::None);
        // git_timeout_secs not set → must fall back to default
        assert_eq!(cfg.git_timeout_secs, 30);
    }

    #[test]
    fn worktree_config_git_timeout_secs_custom() {
        let toml_src = "enabled = true\ngit_timeout_secs = 120\n";
        let cfg: WorktreeConfig = toml::from_str(toml_src).expect("deserialize");
        assert_eq!(cfg.git_timeout_secs, 120);
    }

    #[test]
    fn worktree_config_git_timeout_secs_defaults_when_absent() {
        // Configs written before this field was added must parse without error
        // and resolve to the 30-second default.
        let toml_src = "enabled = false\n";
        let cfg: WorktreeConfig = toml::from_str(toml_src).expect("deserialize");
        assert_eq!(cfg.git_timeout_secs, 30);
    }

    #[test]
    fn worktree_config_quota_fields_default_when_absent() {
        // Configs written before max_worktrees/disk_quota_mb/auto_reconcile_secs/
        // reconcile_on_startup were added must parse without error and resolve to
        // their defaults (unlimited, no accounting, no periodic sweep, startup
        // sweep on).
        let toml_src = "enabled = true\n";
        let cfg: WorktreeConfig = toml::from_str(toml_src).expect("deserialize");
        assert_eq!(cfg.max_worktrees, None);
        assert_eq!(cfg.disk_quota_mb, None);
        assert_eq!(cfg.auto_reconcile_secs, 0);
        assert!(cfg.reconcile_on_startup);
    }

    #[test]
    fn worktree_config_quota_fields_custom_values_roundtrip() {
        let toml_src = "enabled = true\n\
             max_worktrees = 5\n\
             disk_quota_mb = 2048\n\
             auto_reconcile_secs = 3600\n\
             reconcile_on_startup = false\n";
        let cfg: WorktreeConfig = toml::from_str(toml_src).expect("deserialize");
        assert_eq!(cfg.max_worktrees, Some(5));
        assert_eq!(cfg.disk_quota_mb, Some(2048));
        assert_eq!(cfg.auto_reconcile_secs, 3600);
        assert!(!cfg.reconcile_on_startup);
    }
}
