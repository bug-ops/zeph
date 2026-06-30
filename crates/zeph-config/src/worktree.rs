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
    /// network links.  A value of `0` is treated as `1` at the call site.
    pub git_timeout_secs: u64,
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
}
