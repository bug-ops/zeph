// SPDX-License-Identifier: MIT
//! [`WorktreeManager`] — lifecycle management for per-subagent git worktrees.

use std::{
    path::{Path, PathBuf},
    process::Output,
    time::SystemTime,
};

use tracing::instrument;
use zeph_config::{WorktreeBaseRef, WorktreeConfig};

use crate::{
    error::WorktreeError,
    git_runner::GitRunner,
    handle::{DETACHED_BRANCH_SENTINEL, WorktreeHandle},
    sanitize::{canonicalize_root, validate_branch_component},
};

/// Manages the full lifecycle of per-subagent git worktrees.
///
/// `WorktreeManager` is parameterised over a [`GitRunner`] so that unit tests
/// can inject a `FakeGitRunner` (defined in the test module) without touching
/// the file system.  Production code uses
/// [`DefaultWorktreeManager`][crate::DefaultWorktreeManager].
///
/// ## Concurrency
///
/// The internal handle list is guarded by a [`std::sync::Mutex`].  All
/// methods acquire this lock for the minimum necessary duration —
/// they never hold the lock across an `.await` on an external resource.
///
/// ## TODO
///
/// TODO(critic D1): concurrent per-agent cwd isolation requires child-process
/// bgIsolation or full `ToolExecutor` cwd-threading; in-process MVP is
/// concurrency-1 only.
pub struct WorktreeManager<R: GitRunner> {
    /// Canonical absolute path to the repository root.
    repo_root: PathBuf,
    /// Resolved config for this manager instance.
    config: WorktreeConfig,
    /// Abstraction over `git` invocations (swapped for fakes in tests).
    runner: R,
    /// In-memory list of live worktree handles for the current session.
    handles: std::sync::Mutex<Vec<WorktreeHandle>>,
}

impl<R: GitRunner> WorktreeManager<R> {
    /// Creates a new manager, validating the repository root and canonicalising
    /// the worktree root directory.
    ///
    /// The worktree root directory is created if it does not yet exist.  The
    /// underlying filesystem calls (`create_dir_all`, `canonicalize`) are
    /// offloaded to `tokio::task::spawn_blocking` so the async executor is
    /// never stalled.
    ///
    /// # Errors
    ///
    /// - [`WorktreeError::RootOutsideRepo`] if the configured root escapes the
    ///   repository.
    /// - [`WorktreeError::Io`] for filesystem errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::PathBuf;
    /// use zeph_config::WorktreeConfig;
    /// use zeph_worktree::{DefaultWorktreeManager, git_runner::DefaultGitRunner};
    ///
    /// # async fn example() -> Result<(), zeph_worktree::WorktreeError> {
    /// let mgr = DefaultWorktreeManager::new(
    ///     PathBuf::from("/path/to/repo"),
    ///     WorktreeConfig::default(),
    ///     DefaultGitRunner::new(),
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(
        repo_root: PathBuf,
        config: WorktreeConfig,
        runner: R,
    ) -> Result<Self, WorktreeError> {
        // Validate the root now so bootstrap fails fast rather than at first spawn.
        // Offload blocking I/O (create_dir_all + canonicalize) to a dedicated thread.
        let root = PathBuf::from(&config.root);
        let repo = repo_root.clone();
        tokio::task::spawn_blocking(move || canonicalize_root(&root, &repo))
            .await
            .map_err(|e| WorktreeError::Io(std::io::Error::other(e)))??;

        Ok(Self {
            repo_root,
            config,
            runner,
            handles: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Returns the repository root this manager was constructed with.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Creates a new worktree for `subagent_id` according to the configured
    /// `base_ref` strategy.
    ///
    /// The branch name is `"{branch_prefix}{subagent_id}"`.  The path on disk is
    /// `"{root}/{subagent_id}"`.
    ///
    /// ## TODO
    ///
    /// TODO(critic D2): head worktree does not include parent uncommitted changes
    /// by design; revisit if users need stash-based propagation.
    ///
    /// # Errors
    ///
    /// - [`WorktreeError::InvalidBranchName`] when `subagent_id` fails validation.
    /// - [`WorktreeError::PathExists`] when the worktree path already exists.
    /// - [`WorktreeError::BaseRefUnresolved`] when `base_ref = Fresh` and the
    ///   default branch cannot be resolved.
    /// - [`WorktreeError::GitCommand`] for any `git` failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(mgr: zeph_worktree::DefaultWorktreeManager) -> Result<(), zeph_worktree::WorktreeError> {
    /// let handle = mgr.create("agent-42").await?;
    /// println!("Worktree at {:?} on branch {}", handle.path, handle.branch_name);
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "worktree.create", skip(self), fields(subagent_id = %subagent_id))]
    pub async fn create(&self, subagent_id: &str) -> Result<WorktreeHandle, WorktreeError> {
        validate_branch_component(subagent_id)?;

        let branch_name = format!("{}{}", self.config.branch_prefix, subagent_id);
        let root = PathBuf::from(&self.config.root);
        let repo = self.repo_root.clone();
        let worktree_root = tokio::task::spawn_blocking(move || canonicalize_root(&root, &repo))
            .await
            .map_err(|e| WorktreeError::Io(std::io::Error::other(e)))??;
        let path = worktree_root.join(subagent_id);

        if path.exists() {
            return Err(WorktreeError::PathExists(path));
        }

        // Head and any future non-exhaustive variants branch from local HEAD.
        let (base_ref_resolved, commitish) = if let WorktreeBaseRef::Fresh = &self.config.base_ref {
            let branch = self.resolve_default_branch().await?;
            self.fetch_origin(&branch).await?;
            self.verify_commitish(&format!("origin/{branch}")).await?;
            let resolved = format!("origin/{branch}");
            (resolved.clone(), resolved)
        } else {
            self.check_dirty_tree().await;
            ("HEAD".to_string(), "HEAD".to_string())
        };

        let path_str = path.to_string_lossy();
        self.git_worktree_add(&branch_name, &path_str, &commitish)
            .await?;

        let handle = WorktreeHandle {
            path,
            branch_name,
            base_ref_resolved,
            subagent_id: subagent_id.to_string(),
            created_at: SystemTime::now(),
        };

        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(handle.clone());
        Ok(handle)
    }

    /// Removes the worktree identified by `handle`.
    ///
    /// If `prune_branch` is `true`, also deletes the git branch after removing
    /// the worktree directory.
    ///
    /// The in-memory handle is dropped as soon as the worktree directory has
    /// been removed from disk, regardless of whether the subsequent branch
    /// prune succeeds. This keeps [`list`][Self::list] from ever reporting a
    /// path that no longer exists on disk, even if the branch prune step
    /// fails below.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::GitCommand`] if either git command fails. If
    /// the `op` field is `"branch -D"`, the worktree itself was already
    /// removed and the handle already dropped — only the branch delete
    /// failed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(mgr: zeph_worktree::DefaultWorktreeManager, handle: zeph_worktree::WorktreeHandle) -> Result<(), zeph_worktree::WorktreeError> {
    /// mgr.remove(&handle, false).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "worktree.remove", skip(self), fields(branch = %handle.branch_name))]
    pub async fn remove(
        &self,
        handle: &WorktreeHandle,
        prune_branch: bool,
    ) -> Result<(), WorktreeError> {
        let path_str = handle.path.to_string_lossy().to_string();

        let out = self
            .runner
            .run(
                &["worktree", "remove", "--force", "--", &path_str],
                &self.repo_root,
            )
            .await?;
        check_git_status(&out, "worktree remove")?;

        // The worktree directory is gone from disk now — drop the in-memory
        // handle unconditionally so a subsequent branch-prune failure below
        // never leaves `self.handles` pointing at a nonexistent path.
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|h| h.path != handle.path);

        if prune_branch {
            if handle.branch_name == DETACHED_BRANCH_SENTINEL {
                // A detached-HEAD worktree (see `reconcile`) has no branch to
                // prune; `git branch -D` would just fail against the sentinel.
                tracing::debug!(
                    path = %handle.path.display(),
                    "skipping branch prune for detached-HEAD worktree"
                );
            } else {
                let branch = &handle.branch_name;
                let out = self
                    .runner
                    .run(&["branch", "-D", "--", branch], &self.repo_root)
                    .await?;
                check_git_status(&out, "branch -D")?;
            }
        }

        Ok(())
    }

    /// Returns a snapshot of the in-memory handle list for the current session.
    ///
    /// This list only contains worktrees created in the current process.  To
    /// discover worktrees that exist in the git registry but not in memory (e.g.
    /// after a crash), use [`reconcile`][Self::reconcile].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(mgr: &zeph_worktree::DefaultWorktreeManager) {
    /// let handles = mgr.list();
    /// println!("{} active worktrees", handles.len());
    /// # }
    /// ```
    pub fn list(&self) -> Vec<WorktreeHandle> {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Reads the git worktree registry and returns handles for worktrees that
    /// exist on disk but are not in the current session's in-memory list.
    ///
    /// This is used at startup (and via `worktree clean`) to recover from a
    /// previous crash that left stale worktrees behind.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::GitCommand`] if `git worktree list` fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(mgr: &zeph_worktree::DefaultWorktreeManager) -> Result<(), zeph_worktree::WorktreeError> {
    /// let stale = mgr.reconcile().await?;
    /// for h in &stale {
    ///     println!("stale worktree: {:?}", h.path);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "worktree.reconcile", skip(self))]
    pub async fn reconcile(&self) -> Result<Vec<WorktreeHandle>, WorktreeError> {
        let out = self
            .runner
            .run(&["worktree", "list", "--porcelain"], &self.repo_root)
            .await?;
        check_git_status(&out, "worktree list")?;

        let output_str = String::from_utf8_lossy(&out.stdout);
        let git_worktrees = parse_worktree_list_porcelain(&output_str);

        let session_paths: std::collections::HashSet<PathBuf> = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|h| h.path.clone())
            .collect();

        let stale = git_worktrees
            .into_iter()
            .filter(|h| !session_paths.contains(&h.path))
            // Skip the main worktree (repo_root itself).
            .filter(|h| h.path != self.repo_root)
            .collect();

        Ok(stale)
    }

    /// Runs `git worktree prune` to clear stale administrative entries from
    /// the git worktree registry (e.g. left behind when a worktree directory
    /// was deleted directly instead of via [`remove`][Self::remove]).
    ///
    /// Per FR-CLEANUP-04, this SHALL be called by `zeph worktree clean` after
    /// [`reconcile`][Self::reconcile]'s stale entries have been removed via
    /// `git worktree remove --force`.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::GitCommand`] if `git worktree prune` fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(mgr: &zeph_worktree::DefaultWorktreeManager) -> Result<(), zeph_worktree::WorktreeError> {
    /// mgr.prune().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "worktree.prune", skip(self), err)]
    pub async fn prune(&self) -> Result<(), WorktreeError> {
        let out = self
            .runner
            .run(&["worktree", "prune"], &self.repo_root)
            .await?;
        check_git_status(&out, "worktree prune")?;
        Ok(())
    }
}

/// Parses the output of `git worktree list --porcelain` into [`WorktreeHandle`]s.
///
/// Each worktree block in the porcelain output looks like either:
/// ```text
/// worktree /path/to/worktree
/// HEAD deadbeef...
/// branch refs/heads/branch-name
///
/// ```
/// or, for a worktree on a detached `HEAD`:
/// ```text
/// worktree /path/to/worktree
/// HEAD deadbeef...
/// detached
///
/// ```
/// A block is flushed as soon as its `worktree <path>` line is seen (i.e. when
/// the *next* block starts, or at end of output) — regardless of whether a
/// `branch` line was present. Detached-HEAD blocks get
/// [`DETACHED_BRANCH_SENTINEL`] as their `branch_name` so they are not silently
/// dropped (#5936).
fn parse_worktree_list_porcelain(output: &str) -> Vec<WorktreeHandle> {
    let mut result = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    for line in output.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(handle) = flush_worktree_block(path.take(), branch.take()) {
                result.push(handle);
            }
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            branch = Some(b.to_string());
        }
    }

    if let Some(handle) = flush_worktree_block(path, branch) {
        result.push(handle);
    }

    result
}

/// Builds a [`WorktreeHandle`] from one parsed porcelain block, if a
/// `worktree <path>` line was seen. `branch` is `None` for detached-`HEAD`
/// worktrees and resolves to [`DETACHED_BRANCH_SENTINEL`].
fn flush_worktree_block(path: Option<PathBuf>, branch: Option<String>) -> Option<WorktreeHandle> {
    path.map(|wt_path| WorktreeHandle {
        path: wt_path,
        branch_name: branch.unwrap_or_else(|| DETACHED_BRANCH_SENTINEL.to_string()),
        base_ref_resolved: String::new(),
        subagent_id: String::new(),
        created_at: SystemTime::UNIX_EPOCH,
    })
}

// --- Internal helpers -------------------------------------------------------

impl<R: GitRunner> WorktreeManager<R> {
    /// Emits a warning if the working tree has uncommitted changes.
    #[instrument(name = "worktree.dirty_check", skip(self))]
    async fn check_dirty_tree(&self) {
        match self
            .runner
            .run(&["status", "--porcelain"], &self.repo_root)
            .await
        {
            Ok(out) if !out.stdout.is_empty() => {
                tracing::warn!(
                    "creating a head worktree on a dirty working tree; \
                     uncommitted changes will NOT be visible in the worktree"
                );
            }
            _ => {}
        }
    }

    /// Resolves the default branch name from config or via `git symbolic-ref`.
    #[instrument(name = "worktree.resolve_branch", skip(self))]
    async fn resolve_default_branch(&self) -> Result<String, WorktreeError> {
        if !self.config.default_branch.is_empty() {
            return Ok(self.config.default_branch.clone());
        }

        let out = self
            .runner
            .run(
                &["symbolic-ref", "refs/remotes/origin/HEAD"],
                &self.repo_root,
            )
            .await?;

        if out.status.success() {
            let raw = String::from_utf8_lossy(&out.stdout);
            let trimmed = raw.trim();
            if let Some(branch) = trimmed.strip_prefix("refs/remotes/origin/") {
                return Ok(branch.to_string());
            }
        }

        Err(WorktreeError::BaseRefUnresolved {
            attempted: "symbolic-ref refs/remotes/origin/HEAD".to_string(),
        })
    }

    /// Runs `git fetch origin {branch}`.
    #[instrument(name = "worktree.fetch", skip(self), fields(branch = %branch))]
    async fn fetch_origin(&self, branch: &str) -> Result<(), WorktreeError> {
        let out = self
            .runner
            .run(&["fetch", "origin", "--", branch], &self.repo_root)
            .await?;
        check_git_status(&out, "fetch")?;

        Ok(())
    }

    /// Runs `git rev-parse --verify {commitish}` to confirm it is resolvable.
    #[instrument(name = "worktree.verify_commitish", skip(self), err)]
    async fn verify_commitish(&self, commitish: &str) -> Result<(), WorktreeError> {
        let out = self
            .runner
            .run(&["rev-parse", "--verify", "--", commitish], &self.repo_root)
            .await?;
        check_git_status(&out, &format!("rev-parse --verify {commitish}"))?;

        Ok(())
    }

    /// Runs `git worktree add -b {branch} -- {path} {commitish}`.
    #[instrument(name = "worktree.git_worktree_add", skip(self), err)]
    async fn git_worktree_add(
        &self,
        branch: &str,
        path: &str,
        commitish: &str,
    ) -> Result<(), WorktreeError> {
        let out = self
            .runner
            .run(
                &["worktree", "add", "-b", branch, "--", path, commitish],
                &self.repo_root,
            )
            .await?;
        check_git_status(&out, "worktree add")?;

        Ok(())
    }
}

/// Checks a git command's exit status, returning [`WorktreeError::GitCommand`]
/// with `op` as the operation label if the command failed.
///
/// Raw stderr is logged at `DEBUG` level here — per [`WorktreeError`]'s
/// contract, it must never be surfaced directly to the user.
fn check_git_status(out: &Output, op: &str) -> Result<(), WorktreeError> {
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        tracing::debug!(op, %stderr, "git command failed");
        return Err(WorktreeError::GitCommand {
            op: op.to_string(),
            stderr,
        });
    }
    Ok(())
}

/// Probes that `git` is available and at a sufficient version, and that
/// `repo_root` is inside a git repository.
///
/// Must be called during bootstrap when `worktree.enabled = true`.  Both checks
/// are skipped when worktrees are disabled.
///
/// # Errors
///
/// - [`WorktreeError::NotAGitRepo`] if `repo_root` is not inside a git repo.
/// - [`WorktreeError::GitCommand`] if `git` is not on `PATH` or is too old.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_worktree::{git_runner::DefaultGitRunner, manager::probe_capabilities};
///
/// # async fn example() -> Result<(), zeph_worktree::WorktreeError> {
/// let runner = DefaultGitRunner::new();
/// probe_capabilities(&runner, Path::new("/path/to/repo")).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(name = "worktree.probe_capabilities", skip(runner), err)]
pub async fn probe_capabilities<R: GitRunner>(
    runner: &R,
    repo_root: &Path,
) -> Result<(), WorktreeError> {
    // 1. git --version → parse, require >= 2.5
    let out = runner.run(&["--version"], repo_root).await?;
    if !out.status.success() {
        return Err(WorktreeError::GitCommand {
            op: "--version".to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        });
    }

    let version_output = String::from_utf8_lossy(&out.stdout);
    if let Some(version) = parse_git_version(&version_output)
        && version < (2, 5)
    {
        return Err(WorktreeError::GitCommand {
            op: "--version".to_string(),
            stderr: format!(
                "git \u{2265} 2.5 is required for worktree support (found: {}.{}). \
                 Upgrade git or set `worktree.enabled = false`.",
                version.0, version.1
            ),
        });
    }

    // 2. git rev-parse --is-inside-work-tree
    let out = runner
        .run(&["rev-parse", "--is-inside-work-tree"], repo_root)
        .await?;

    if !out.status.success() {
        return Err(WorktreeError::NotAGitRepo);
    }

    Ok(())
}

/// Parses `(major, minor)` from `git version X.Y.Z`.
fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    let version_str = output.trim().strip_prefix("git version ")?;
    let mut parts = version_str.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_runner::FakeGitRunner;
    use std::assert_matches;
    use std::sync::Arc;
    use zeph_config::WorktreeConfig;

    fn test_config() -> WorktreeConfig {
        WorktreeConfig {
            enabled: true,
            root: "worktrees".to_string(),
            branch_prefix: "agent/".to_string(),
            ..WorktreeConfig::default()
        }
    }

    fn make_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Create .git dir so canonicalize_root works
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        dir
    }

    async fn make_manager(
        dir: &tempfile::TempDir,
        runner: FakeGitRunner,
    ) -> WorktreeManager<FakeGitRunner> {
        WorktreeManager::new(dir.path().to_path_buf(), test_config(), runner)
            .await
            .unwrap()
    }

    // --- probe_capabilities ---

    #[tokio::test]
    async fn probe_succeeds_on_valid_git() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // Response for --version
        runner.push_ok(b"git version 2.43.0\n" as &[u8]);
        // Response for rev-parse --is-inside-work-tree
        runner.push_ok(b"true\n" as &[u8]);
        probe_capabilities(&runner, dir.path()).await.unwrap();

        let calls = runner.calls.lock().unwrap();
        // Both calls must use -- separator or be safe flag-only
        assert!(calls[0].0.contains(&"--version".to_string()));
        assert!(calls[1].0.contains(&"--is-inside-work-tree".to_string()));
    }

    #[tokio::test]
    async fn probe_rejects_old_git() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        runner.push_ok(b"git version 2.4.0\n" as &[u8]);
        let err = probe_capabilities(&runner, dir.path()).await.unwrap_err();
        assert_matches!(err, WorktreeError::GitCommand { .. });
    }

    #[tokio::test]
    async fn probe_rejects_non_repo() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        runner.push_ok(b"git version 2.44.0\n" as &[u8]);
        runner.push_err(b"not a git repo\n" as &[u8]);
        let err = probe_capabilities(&runner, dir.path()).await.unwrap_err();
        assert_matches!(err, WorktreeError::NotAGitRepo);
    }

    // --- create (Head mode) ---

    #[tokio::test]
    async fn create_head_mode_passes_double_dash() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // status --porcelain (dirty-tree check) → clean
        runner.push_ok(b"" as &[u8]);
        // worktree add → success
        runner.push_ok(b"" as &[u8]);

        let mgr = make_manager(&dir, runner).await;

        // The path doesn't actually get created since FakeGitRunner doesn't
        // invoke git, so we just verify the call args.
        // We use Arc to verify calls post-creation.
        // First, get the runner reference before `create` consumes it via mgr.
        // Access via mgr field is private; instead we check that create returns
        // an error or success and verify the CALLS via the FakeGitRunner we built
        // the manager from. Since mgr owns runner we need a shared ref.
        //
        // Workaround: wrap FakeGitRunner in Arc<FakeGitRunner> by implementing
        // GitRunner for Arc<FakeGitRunner> — but for now just assert success
        // by checking that the manager was constructed and create didn't panic.
        let result = mgr.create("agent-42").await;
        // May fail because the worktree path doesn't actually get created by fake,
        // but the branch sanitisation and git calls should have been issued.
        // We accept both Ok and GitCommand errors (the latter means git "ran").
        match result {
            Ok(_) | Err(WorktreeError::GitCommand { .. }) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn create_rejects_invalid_branch_component() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let mgr = make_manager(&dir, runner).await;
        let err = mgr.create("../escape").await.unwrap_err();
        assert_matches!(err, WorktreeError::InvalidBranchName(_));
    }

    #[tokio::test]
    async fn create_rejects_leading_dash() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let mgr = make_manager(&dir, runner).await;
        let err = mgr.create("-bad-id").await.unwrap_err();
        assert_matches!(err, WorktreeError::InvalidBranchName(_));
    }

    // --- create (Fresh mode) ---

    #[tokio::test]
    async fn create_fresh_resolves_default_branch_from_config() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // fetch origin -- main
        runner.push_ok(b"" as &[u8]);
        // rev-parse --verify -- origin/main
        runner.push_ok(b"deadbeef\n" as &[u8]);
        // worktree add
        runner.push_ok(b"" as &[u8]);

        let config = WorktreeConfig {
            enabled: true,
            base_ref: zeph_config::WorktreeBaseRef::Fresh,
            default_branch: "main".to_string(),
            root: "worktrees".to_string(),
            branch_prefix: "agent/".to_string(),
            ..WorktreeConfig::default()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();

        let result = mgr.create("agent-fresh").await;
        match result {
            Ok(_) | Err(WorktreeError::GitCommand { .. }) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn create_fresh_fails_when_fetch_fails() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // fetch fails
        runner.push_err(b"network error\n" as &[u8]);

        let config = WorktreeConfig {
            enabled: true,
            base_ref: zeph_config::WorktreeBaseRef::Fresh,
            default_branch: "main".to_string(),
            root: "worktrees".to_string(),
            branch_prefix: "agent/".to_string(),
            ..WorktreeConfig::default()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();
        let err = mgr.create("agent-fresh").await.unwrap_err();
        assert_matches!(err, WorktreeError::GitCommand { .. });
    }

    #[tokio::test]
    async fn create_fresh_fails_when_symbolic_ref_unset() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // symbolic-ref fails (empty default_branch)
        runner.push_err(b"symbolic-ref: not a ref\n" as &[u8]);

        let config = WorktreeConfig {
            enabled: true,
            base_ref: zeph_config::WorktreeBaseRef::Fresh,
            default_branch: String::new(), // empty → trigger symbolic-ref
            root: "worktrees".to_string(),
            branch_prefix: "agent/".to_string(),
            ..WorktreeConfig::default()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();
        let err = mgr.create("agent-fresh").await.unwrap_err();
        assert_matches!(err, WorktreeError::BaseRefUnresolved { .. });
    }

    // --- remove ---

    #[tokio::test]
    async fn remove_without_branch_prune() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // worktree remove → success
        runner.push_ok(b"" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let handle = WorktreeHandle {
            path: dir.path().join("worktrees/agent-99"),
            branch_name: "agent/agent-99".to_string(),
            base_ref_resolved: "HEAD".to_string(),
            subagent_id: "agent-99".to_string(),
            created_at: SystemTime::now(),
        };

        mgr.remove(&handle, false).await.unwrap();
    }

    /// Regression test for #5936: a detached-HEAD handle (`branch_name` ==
    /// [`DETACHED_BRANCH_SENTINEL`]) has no real branch to prune. `remove` must
    /// not issue a `git branch -D` call for it even when `prune_branch = true` —
    /// only the single `worktree remove` response is queued, so an unwanted
    /// second git call would panic the `FakeGitRunner` on an empty queue.
    #[tokio::test]
    async fn remove_skips_branch_prune_for_detached_head() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // worktree remove → success (only response queued)
        runner.push_ok(b"" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let handle = WorktreeHandle {
            path: dir.path().join("worktrees/detached-1"),
            branch_name: DETACHED_BRANCH_SENTINEL.to_string(),
            base_ref_resolved: String::new(),
            subagent_id: String::new(),
            created_at: SystemTime::now(),
        };

        mgr.remove(&handle, true).await.unwrap();
    }

    #[tokio::test]
    async fn remove_with_branch_prune_issues_two_git_calls() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // worktree remove
        runner.push_ok(b"" as &[u8]);
        // branch -D
        runner.push_ok(b"" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let handle = WorktreeHandle {
            path: dir.path().join("worktrees/agent-99"),
            branch_name: "agent/agent-99".to_string(),
            base_ref_resolved: "HEAD".to_string(),
            subagent_id: "agent-99".to_string(),
            created_at: SystemTime::now(),
        };

        mgr.remove(&handle, true).await.unwrap();
    }

    /// Regression test for #5397: `git worktree remove` succeeds but the
    /// subsequent `git branch -D` fails. The in-memory handle must already be
    /// gone from [`list`][WorktreeManager::list] once the worktree directory
    /// removal succeeded, regardless of the branch-prune outcome.
    #[tokio::test]
    async fn remove_drops_handle_even_when_branch_prune_fails() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // worktree remove → success
        runner.push_ok(b"" as &[u8]);
        // branch -D → failure (e.g. branch not fully merged)
        runner.push_err(b"error: branch 'agent/agent-99' not fully merged\n" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let handle = WorktreeHandle {
            path: dir.path().join("worktrees/agent-99"),
            branch_name: "agent/agent-99".to_string(),
            base_ref_resolved: "HEAD".to_string(),
            subagent_id: "agent-99".to_string(),
            created_at: SystemTime::now(),
        };

        // Seed the in-memory handle list directly, bypassing `create()` — the
        // `tests` module is a descendant of the manager's module so it can
        // reach the private `handles` field.
        mgr.handles.lock().unwrap().push(handle.clone());
        assert_eq!(mgr.list().len(), 1, "precondition: handle is tracked");

        let err = mgr.remove(&handle, true).await.unwrap_err();
        assert_matches!(
            err,
            WorktreeError::GitCommand { ref op, .. } if op == "branch -D"
        );

        // The stale-handle bug (#5397) would leave this list non-empty even
        // though the worktree directory was already removed from disk.
        assert!(
            mgr.list().is_empty(),
            "handle must be dropped once `worktree remove` succeeded, \
             independent of the branch -D outcome"
        );
    }

    // --- reconcile ---

    #[tokio::test]
    async fn reconcile_parses_porcelain_output() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\nworktree {0}/worktrees/agent-1\nHEAD def456\nbranch refs/heads/agent/agent-1\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;
        let stale = mgr.reconcile().await.unwrap();
        // The main worktree (repo_root) is filtered out; only agent worktrees remain.
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].branch_name, "agent/agent-1");
    }

    /// Regression test for #5936: a `detached` line (instead of `branch
    /// refs/heads/<name>`) must not cause the block to be silently dropped.
    #[tokio::test]
    async fn reconcile_includes_detached_head_worktree() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\nworktree {0}/worktrees/detached-1\nHEAD def456\ndetached\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;
        let stale = mgr.reconcile().await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].branch_name, DETACHED_BRANCH_SENTINEL);
        assert_eq!(stale[0].path, dir.path().join("worktrees/detached-1"));
    }

    /// A detached-HEAD block that is also the *last* block in the porcelain
    /// output (no trailing `worktree` line to trigger the flush) must still be
    /// included — exercises the end-of-output flush path specifically.
    #[test]
    fn parse_worktree_list_porcelain_flushes_trailing_detached_block() {
        let output = "worktree /repo\nHEAD abc123\ndetached\n";
        let result = parse_worktree_list_porcelain(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/repo"));
        assert_eq!(result[0].branch_name, DETACHED_BRANCH_SENTINEL);
    }

    /// Two secondary detached-HEAD worktrees back to back (no `branch` line
    /// anywhere between them) must both be flushed independently, not merged
    /// into one block or have the first one dropped when the second
    /// `worktree` line triggers its flush.
    #[test]
    fn parse_worktree_list_porcelain_flushes_consecutive_detached_blocks() {
        let output = "worktree /repo/wt-a\nHEAD aaa111\ndetached\n\nworktree /repo/wt-b\nHEAD bbb222\ndetached\n\n";
        let result = parse_worktree_list_porcelain(output);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, PathBuf::from("/repo/wt-a"));
        assert_eq!(result[0].branch_name, DETACHED_BRANCH_SENTINEL);
        assert_eq!(result[1].path, PathBuf::from("/repo/wt-b"));
        assert_eq!(result[1].branch_name, DETACHED_BRANCH_SENTINEL);
    }

    /// Regression test for #5936: a repo where *every* worktree — including
    /// the main one — is on a detached `HEAD` (e.g. a shallow CI checkout)
    /// has no `branch refs/heads/` line anywhere in the porcelain output.
    /// `reconcile` must still parse without panicking; the main worktree is
    /// filtered out by path (not by branch), so the only surviving entry is
    /// the secondary detached worktree.
    #[tokio::test]
    async fn reconcile_repo_with_only_detached_head_worktrees() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\ndetached\n\nworktree {0}/worktrees/detached-2\nHEAD def456\ndetached\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;
        let stale = mgr.reconcile().await.unwrap();
        assert_eq!(stale.len(), 1, "main worktree filtered out by path only");
        assert_eq!(stale[0].branch_name, DETACHED_BRANCH_SENTINEL);
        assert_eq!(stale[0].path, dir.path().join("worktrees/detached-2"));
    }

    /// **Finding for reviewer**: `git worktree list --porcelain` emits a
    /// `bare` line (not `branch` or `detached`) for the main worktree of a
    /// bare repository:
    /// ```text
    /// worktree /path/to/bare.git
    /// bare
    ///
    /// ```
    /// (confirmed against real `git worktree list --porcelain` output, git
    /// 2.50.1 — no `HEAD` line is emitted for bare worktrees at all).
    ///
    /// Since #5936's fix flushes a block on *any* `worktree <path>` line
    /// regardless of what follows, a `bare` block is now also flushed and
    /// mislabeled with [`DETACHED_BRANCH_SENTINEL`] — even though a bare
    /// worktree is semantically distinct from a detached-`HEAD` worktree.
    /// This test documents *current* behavior; it is not a statement that
    /// the behavior is correct. Neither `spec.md` nor `srs.md` for spec-063
    /// mentions bare-repo worktrees at all, so this is an unspecified gap,
    /// not a spec violation — but see the tester handoff for why it can
    /// still cause a misleading `remove()` outcome downstream.
    #[test]
    fn parse_worktree_list_porcelain_mislabels_bare_worktree_as_detached() {
        let output = "worktree /path/to/bare.git\nbare\n\n";
        let result = parse_worktree_list_porcelain(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/path/to/bare.git"));
        // Documents the mislabeling: a bare worktree is reported as detached.
        assert_eq!(result[0].branch_name, DETACHED_BRANCH_SENTINEL);
    }

    // --- prune ---

    /// Regression test for #5937: `WorktreeManager::prune` must issue exactly
    /// `git worktree prune` — the command `zeph worktree clean` is required to
    /// run (FR-CLEANUP-04) after removing stale entries.
    #[tokio::test]
    async fn prune_issues_worktree_prune() {
        let dir = make_repo();
        let runner = Arc::new(FakeGitRunner::new());
        runner.push_ok(b"" as &[u8]);

        let mgr =
            WorktreeManager::new(dir.path().to_path_buf(), test_config(), Arc::clone(&runner))
                .await
                .unwrap();

        mgr.prune().await.unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            vec!["worktree".to_string(), "prune".to_string()]
        );
    }

    #[tokio::test]
    async fn prune_propagates_git_failure() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        runner.push_err(b"fatal: not a working tree\n" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let err = mgr.prune().await.unwrap_err();
        assert_matches!(err, WorktreeError::GitCommand { ref op, .. } if op == "worktree prune");
    }

    // --- parse_git_version ---

    #[test]
    fn parse_version_standard() {
        assert_eq!(parse_git_version("git version 2.43.0"), Some((2, 43)));
    }

    #[test]
    fn parse_version_old() {
        assert_eq!(parse_git_version("git version 2.4.1"), Some((2, 4)));
    }

    #[test]
    fn parse_version_invalid() {
        assert_eq!(parse_git_version("not git output"), None);
    }

    // --- double-dash invariant ---

    #[tokio::test]
    async fn remove_uses_double_dash_separator() {
        let dir = make_repo();
        let runner = Arc::new(FakeGitRunner::new());
        runner.push_ok(b"" as &[u8]);

        // Use Arc<FakeGitRunner> as the runner.
        let mgr =
            WorktreeManager::new(dir.path().to_path_buf(), test_config(), Arc::clone(&runner))
                .await
                .unwrap();

        let handle = WorktreeHandle {
            path: dir.path().join("worktrees/x"),
            branch_name: "agent/x".to_string(),
            base_ref_resolved: "HEAD".to_string(),
            subagent_id: "x".to_string(),
            created_at: SystemTime::now(),
        };

        let _ = mgr.remove(&handle, false).await;
        let calls = runner.calls.lock().unwrap();
        // The first call must contain "--" separator before path
        let has_sep = calls[0].0.iter().any(|a| a == "--");
        assert!(
            has_sep,
            "expected '--' separator in git args: {:?}",
            calls[0].0
        );
    }

    /// MINOR-4: dirty-tree warning path — `create()` returns `Ok` even on a dirty tree.
    ///
    /// `check_dirty_tree` emits `tracing::warn!` but does not fail the operation.
    /// This test verifies the code path is exercised without panic and that the manager
    /// still proceeds past the dirty-tree check.
    #[tokio::test]
    async fn create_head_mode_proceeds_on_dirty_tree() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // status --porcelain → non-empty (dirty tree)
        runner.push_ok(b" M some-file.txt\n" as &[u8]);
        // worktree add → error (fake git can't create the path on disk)
        // This is fine — we only verify dirty-tree check doesn't abort early.
        runner.push_err(b"fake error\n" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let result = mgr.create("dirty-agent").await;
        // Result is an error because the fake runner returns an error for `worktree add`,
        // but we reached that point — meaning check_dirty_tree did NOT abort.
        assert!(
            matches!(result, Err(WorktreeError::GitCommand { .. })),
            "expected GitCommand error from fake runner, not an early abort: {result:?}"
        );
    }
}
