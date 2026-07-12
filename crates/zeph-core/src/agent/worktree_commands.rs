// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Channel-free `/worktree` command implementation for use via
//! [`zeph_commands::traits::agent::AgentAccess`].
//!
//! Operates on the same live [`zeph_worktree::DefaultWorktreeManager`] instance the running
//! agent's [`zeph_subagent::SubAgentManager`] uses to create per-subagent worktrees, so
//! `/worktree list` and `/worktree clean` reflect this session's actual state. Contrast with
//! the CLI's `zeph worktree list`/`clean` (`src/commands/worktree.rs`), which constructs a
//! fresh manager from a disk scan on every invocation.

use std::fmt::Write as _;

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Channel-free `/worktree list` — formats active and stale worktrees tracked by the
    /// live session's worktree manager.
    ///
    /// Returns `Ok(None)` when the worktree subsystem is disabled for this session.
    ///
    /// # Errors
    ///
    /// Returns `Err` when git reconciliation fails.
    pub(super) async fn handle_worktree_list_as_string(
        &mut self,
    ) -> Result<Option<String>, AgentError> {
        let Some(mgr) = &self.services.orchestration.subagent_manager else {
            return Ok(None);
        };
        let Some(wm) = mgr.worktree_manager() else {
            return Ok(None);
        };

        let stale = wm.reconcile().await?;
        let active = wm.list();

        if active.is_empty() && stale.is_empty() {
            return Ok(Some("No active worktrees.".to_owned()));
        }

        let mut out = String::new();
        if !active.is_empty() {
            let _ = writeln!(out, "{:<36}  PATH", "AGENT ID");
            for handle in &active {
                let _ = writeln!(out, "{:<36}  {}", handle.subagent_id, handle.path.display());
            }
        }
        if !stale.is_empty() {
            if !active.is_empty() {
                out.push('\n');
            }
            out.push_str("Stale (on disk but not tracked):\n");
            for stale_wt in &stale {
                match &stale_wt.prunable_reason {
                    Some(reason) => {
                        let _ = writeln!(
                            out,
                            "  {}  [prunable: {reason}]",
                            stale_wt.handle.path.display()
                        );
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "  {}  [in use — not marked prunable by git; may belong to \
                             another session]",
                            stale_wt.handle.path.display()
                        );
                    }
                }
            }
        }
        Ok(Some(out.trim_end().to_owned()))
    }

    /// Channel-free `/worktree clean [--force]` — removes stale worktrees tracked by the
    /// live session's worktree manager.
    ///
    /// `force` mirrors `zeph worktree clean --force`: also removes worktrees whose directory
    /// git does not report as prunable. Returns `Ok(None)` when the worktree subsystem is
    /// disabled for this session.
    ///
    /// Delegates the actual reconcile/remove/prune pipeline and outcome counting to
    /// [`WorktreeManager::clean`][zeph_worktree::WorktreeManager::clean], shared with the
    /// CLI's `zeph worktree clean` (`src/commands/worktree.rs`), so the two surfaces cannot
    /// silently diverge in behavior (#6142) — only the `--force` hint text differs.
    ///
    /// # Errors
    ///
    /// Returns `Err` only when the initial git reconciliation fails (nothing has been
    /// removed yet, so there is no summary to lose). Per-worktree removal failures and a
    /// failure of the final registry-prune step are both reported inline in the summary
    /// instead of aborting.
    pub(super) async fn handle_worktree_clean_as_string(
        &mut self,
        force: bool,
    ) -> Result<Option<String>, AgentError> {
        let Some(mgr) = &self.services.orchestration.subagent_manager else {
            return Ok(None);
        };
        let Some(wm) = mgr.worktree_manager() else {
            return Ok(None);
        };
        let prune_branch_on_remove = wm.prune_branch_on_remove();

        let outcome = wm
            .clean(force, prune_branch_on_remove, "`/worktree clean --force`")
            .await?;

        let mut out = String::new();
        for warning in &outcome.warnings {
            let _ = writeln!(out, "{warning}");
        }
        out.push_str(&zeph_worktree::format_clean_summary(&outcome));
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_config::WorktreeConfig;
    use zeph_worktree::{DefaultGitRunner, DefaultWorktreeManager};

    use super::*;
    use crate::testing::{MockChannel, MockToolExecutor, mock_provider};

    fn git(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git must be on PATH for this test")
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        assert!(git(&["init", "-q"], path).status.success());
        git(&["config", "user.email", "test@example.com"], path);
        git(&["config", "user.name", "Test"], path);
        std::fs::write(path.join("README.md"), "test\n").expect("write README");
        git(&["add", "."], path);
        assert!(git(&["commit", "-q", "-m", "init"], path).status.success());
        dir
    }

    fn worktree_config() -> WorktreeConfig {
        WorktreeConfig {
            enabled: true,
            root: "worktrees".to_string(),
            branch_prefix: "agent/".to_string(),
            ..Default::default()
        }
    }

    fn test_agent() -> Agent<MockChannel> {
        Agent::new(
            mock_provider(vec!["ignored".to_string()]),
            MockChannel::new(Vec::<String>::new()),
            zeph_skills::registry::SkillRegistry::load(&Vec::<std::path::PathBuf>::new()),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
    }

    /// Builds an `Agent` whose `SubAgentManager` is wired to a real, live
    /// `DefaultWorktreeManager` over `repo_root` — mirroring how bootstrap wires the
    /// two together in production (`src/agent_setup.rs`) — so
    /// `handle_worktree_list_as_string`/`handle_worktree_clean_as_string` exercise the
    /// actual `Some(wm)` branch instead of only the `None` (disabled-subsystem)
    /// short-circuit that `zeph-commands`' `NullAgent`-backed tests already cover.
    async fn agent_with_live_worktree_manager(repo_root: std::path::PathBuf) -> Agent<MockChannel> {
        let wm = Arc::new(
            DefaultWorktreeManager::new(repo_root, worktree_config(), DefaultGitRunner::new())
                .await
                .expect("construct live worktree manager"),
        );
        let mut sam = zeph_subagent::SubAgentManager::new(4);
        sam.set_worktree_manager(wm);

        let mut agent = test_agent();
        agent.services.orchestration.subagent_manager = Some(sam);
        agent
    }

    #[tokio::test]
    async fn list_reports_no_worktrees_when_none_exist() {
        let repo = init_repo();
        let repo_root = repo.path().canonicalize().expect("canonicalize");
        let mut agent = agent_with_live_worktree_manager(repo_root).await;

        let out = agent.handle_worktree_list_as_string().await.unwrap();
        assert_eq!(out.as_deref(), Some("No active worktrees."));
    }

    /// Covers the "active" (in-memory, non-stale) branch of `list` — a worktree created
    /// through the *same* live manager instance the agent holds shows up via
    /// `WorktreeManager::list()`, not `reconcile()`. This is genuinely new coverage: the
    /// CLI's `zeph worktree list` always constructs a fresh manager per invocation, so its
    /// own `list()` is trivially always empty and this branch is unreachable from there.
    #[tokio::test]
    async fn list_reports_active_worktrees_created_by_this_session() {
        let repo = init_repo();
        let repo_root = repo.path().canonicalize().expect("canonicalize");
        let mut agent = agent_with_live_worktree_manager(repo_root).await;

        {
            let mgr = agent
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap();
            let wm = mgr.worktree_manager().unwrap();
            wm.create("agent-1").await.expect("create worktree");
            wm.create("agent-2").await.expect("create worktree");
        }

        let out = agent
            .handle_worktree_list_as_string()
            .await
            .unwrap()
            .unwrap();
        assert!(out.contains("AGENT ID"), "got: {out}");
        assert!(out.contains("agent-1"), "got: {out}");
        assert!(out.contains("agent-2"), "got: {out}");
        assert!(!out.contains("Stale"), "got: {out}");
    }

    /// Covers the "stale" branch of `list` with both a prunable and a non-prunable entry
    /// in the same result — using a *separate* creator manager (over the same repo) so
    /// the agent's own live manager discovers them purely via `reconcile()`, exactly as a
    /// worktree left behind by a crashed or concurrently running session would appear.
    #[tokio::test]
    async fn list_reports_stale_worktrees_with_prunable_and_in_use_reasons() {
        let repo = init_repo();
        let repo_root = repo.path().canonicalize().expect("canonicalize");

        let creator = DefaultWorktreeManager::new(
            repo_root.clone(),
            worktree_config(),
            DefaultGitRunner::new(),
        )
        .await
        .expect("construct creator manager");
        let prunable = creator.create("prunable-1").await.expect("create");
        std::fs::remove_dir_all(&prunable.path).expect("remove prunable dir");
        let in_use = creator.create("in-use-1").await.expect("create");

        let mut agent = agent_with_live_worktree_manager(repo_root).await;
        let out = agent
            .handle_worktree_list_as_string()
            .await
            .unwrap()
            .unwrap();

        assert!(
            out.contains("Stale (on disk but not tracked):"),
            "got: {out}"
        );
        assert!(
            out.contains(&format!("{}  [prunable:", prunable.path.display())),
            "got: {out}"
        );
        assert!(
            out.contains(&format!(
                "{}  [in use — not marked prunable by git; may belong to another session]",
                in_use.path.display()
            )),
            "got: {out}"
        );
    }

    /// End-to-end fixture test for #6142 part A: a mixed prunable/non-prunable stale
    /// list through the real `handle_worktree_clean_as_string`, `force = false`. Confirms
    /// the live-manager `Some(wm)` branch actually reaches `WorktreeManager::clean` with
    /// the right semantics (prunable removed, non-prunable left alone) — not just the
    /// `None`-manager short-circuit `zeph-commands`' existing tests cover.
    #[tokio::test]
    async fn clean_removes_prunable_and_skips_in_use_without_force() {
        let repo = init_repo();
        let repo_root = repo.path().canonicalize().expect("canonicalize");

        let creator = DefaultWorktreeManager::new(
            repo_root.clone(),
            worktree_config(),
            DefaultGitRunner::new(),
        )
        .await
        .expect("construct creator manager");
        let prunable = creator.create("prunable-1").await.expect("create");
        std::fs::remove_dir_all(&prunable.path).expect("remove prunable dir");
        let in_use = creator.create("in-use-1").await.expect("create");

        let mut agent = agent_with_live_worktree_manager(repo_root.clone()).await;
        let out = agent
            .handle_worktree_clean_as_string(false)
            .await
            .unwrap()
            .unwrap();

        assert!(
            out.contains("Removed 1 stale worktree(s), skipped 1 in-use candidate(s), 0 error(s)."),
            "got: {out}"
        );
        assert!(
            in_use.path.exists(),
            "in-use worktree must survive without --force"
        );

        let list = git(&["worktree", "list", "--porcelain"], &repo_root);
        let list_str = String::from_utf8_lossy(&list.stdout);
        assert!(
            !list_str.contains(&*prunable.path.to_string_lossy()),
            "prunable worktree must be gone from the registry: {list_str}"
        );
        assert!(
            list_str.contains(&*in_use.path.to_string_lossy()),
            "in-use worktree must remain in the registry: {list_str}"
        );
    }

    /// `--force` variant reaching the live `WorktreeManager` with correct semantics: the
    /// same non-prunable entry that survives without `force` is actually removed once
    /// `force = true` is threaded through.
    #[tokio::test]
    async fn clean_with_force_removes_in_use_entry_too() {
        let repo = init_repo();
        let repo_root = repo.path().canonicalize().expect("canonicalize");

        let creator = DefaultWorktreeManager::new(
            repo_root.clone(),
            worktree_config(),
            DefaultGitRunner::new(),
        )
        .await
        .expect("construct creator manager");
        let in_use = creator.create("in-use-1").await.expect("create");

        let mut agent = agent_with_live_worktree_manager(repo_root.clone()).await;
        let out = agent
            .handle_worktree_clean_as_string(true)
            .await
            .unwrap()
            .unwrap();

        assert!(
            out.contains("Removed 1 stale worktree(s), skipped 0 in-use candidate(s), 0 error(s)."),
            "got: {out}"
        );
        assert!(
            !in_use.path.exists(),
            "in-use worktree must be removed once --force is passed"
        );
    }

    #[tokio::test]
    async fn list_and_clean_return_none_when_worktree_subsystem_disabled() {
        let mut agent = test_agent();
        assert!(agent.services.orchestration.subagent_manager.is_none());

        assert_eq!(agent.handle_worktree_list_as_string().await.unwrap(), None);
        assert_eq!(
            agent.handle_worktree_clean_as_string(false).await.unwrap(),
            None
        );
    }
}
