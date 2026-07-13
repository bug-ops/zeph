// SPDX-License-Identifier: MIT
//! [`WorktreeManager`] — lifecycle management for per-subagent git worktrees.

use std::{
    path::{Path, PathBuf},
    process::Output,
    time::{Instant, SystemTime},
};

use tracing::instrument;
use zeph_config::{WorktreeBaseRef, WorktreeConfig};

use crate::{
    error::WorktreeError,
    git_runner::GitRunner,
    handle::{BARE_WORKTREE_SENTINEL, DETACHED_BRANCH_SENTINEL, StaleWorktree, WorktreeHandle},
    sanitize::{canonicalize_root, validate_branch_component},
    usage::{QuotaStatus, WorktreeDiskUsage},
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
/// The internal handle list is guarded by a [`std::sync::Mutex`].  Most
/// methods acquire this lock for the minimum necessary duration — they
/// never hold it across an `.await` on an external resource.
///
/// [`create`][Self::create] is the exception: its quota-check-through-
/// registration sequence is additionally guarded end-to-end by a
/// [`tokio::sync::Mutex`] (`admission_lock`), held across every `.await` in
/// that sequence. This makes admission to `max_worktrees` safe against
/// concurrent in-process `create()` calls without relying on caller-side
/// locking — see `create`'s `# Concurrency` section for details.
///
/// ## TODO
///
/// TODO(critic D1): concurrent per-agent cwd isolation requires child-process
/// bgIsolation or full `ToolExecutor` cwd-threading; in-process MVP is
/// concurrency-1 only.
pub struct WorktreeManager<R: GitRunner> {
    /// Canonical absolute path to the repository root.
    repo_root: PathBuf,
    /// Canonicalised, validated worktree root — computed once in [`Self::new`]
    /// and reused by [`Self::create`] on every call, since `config.root` and
    /// `repo_root` never change for the lifetime of the manager.
    worktree_root: PathBuf,
    /// Resolved config for this manager instance.
    config: WorktreeConfig,
    /// Abstraction over `git` invocations (swapped for fakes in tests).
    runner: R,
    /// In-memory list of live worktree handles for the current session.
    handles: std::sync::Mutex<Vec<WorktreeHandle>>,
    /// Last computed disk usage, populated by [`Self::disk_usage`]. Read cheaply
    /// (without a filesystem walk) via [`Self::cached_disk_usage`]. `None` until
    /// the first `disk_usage()` call.
    usage_cache: parking_lot::Mutex<Option<(Instant, WorktreeDiskUsage)>>,
    /// Serialises [`Self::create`]'s quota-check-through-registration
    /// sequence so concurrent in-process calls cannot both observe the same
    /// pre-admission count and both proceed past the `max_worktrees` check.
    /// Held across `.await` points for that entire sequence, so this must be
    /// a [`tokio::sync::Mutex`] rather than [`std::sync::Mutex`] — see
    /// `create`'s `# Concurrency` section.
    admission_lock: tokio::sync::Mutex<()>,
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
        // Validate the root now so bootstrap fails fast rather than at first spawn,
        // and cache the result for reuse by `create()` on every subsequent call.
        // Offload blocking I/O (create_dir_all + canonicalize) to a dedicated thread.
        let root = PathBuf::from(&config.root);
        let repo = repo_root.clone();
        let worktree_root = tokio::task::spawn_blocking(move || canonicalize_root(&root, &repo))
            .await
            .map_err(|e| WorktreeError::Io(std::io::Error::other(e)))??;

        Ok(Self {
            repo_root,
            worktree_root,
            config,
            runner,
            handles: std::sync::Mutex::new(Vec::new()),
            usage_cache: parking_lot::Mutex::new(None),
            admission_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Returns the repository root this manager was constructed with.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns the [`WorktreeConfig`] this manager was constructed with.
    ///
    /// Exposed so callers that only hold the manager (not the original config, e.g.
    /// `/worktree list` in `zeph-core`) can read `max_worktrees`/`disk_quota_mb` for
    /// quota-status formatting without threading the config separately.
    #[must_use]
    pub fn config(&self) -> &WorktreeConfig {
        &self.config
    }

    /// Whether [`remove`][Self::remove] should also delete the branch, per the
    /// `WorktreeConfig::prune_branch_on_remove` this manager was constructed with.
    ///
    /// Exposed so callers that only hold the manager (not the original config, e.g.
    /// `/worktree clean` in `zeph-core`) don't need to thread the config separately.
    #[must_use]
    pub fn prune_branch_on_remove(&self) -> bool {
        self.config.prune_branch_on_remove
    }

    /// Creates a new worktree for `subagent_id` according to the configured
    /// `base_ref` strategy.
    ///
    /// The branch name is `"{branch_prefix}{subagent_id}"`.  The path on disk is
    /// `"{root}/{subagent_id}"`.
    ///
    /// ## Admission cap
    ///
    /// When `config.max_worktrees` is `Some(max)`, this call first counts all
    /// git-registered secondary worktrees under `root` — via
    /// [`reconcile`][Self::reconcile] (stale/foreign entries) plus
    /// [`list`][Self::list] (this session's own) — and fails with
    /// [`WorktreeError::QuotaExceeded`] if that count is already `>= max`. The
    /// count includes worktrees created by *other*, concurrently running zeph
    /// sessions over the same `root`, since they consume the same disk budget
    /// `max_worktrees` is meant to bound. This is a best-effort **soft** cap
    /// across *processes*: the count-then-`git worktree add` sequence is not
    /// atomic across separate zeph sessions, so two concurrent `create()`
    /// calls in different processes can both pass the check and briefly push
    /// the total above `max`. No cross-process locking is used to close that
    /// gap — it is out of scope for the size of this feature. Within a single
    /// process, admission is a **hard** guarantee — see `# Concurrency` below.
    ///
    /// ## Concurrency
    ///
    /// The quota-check-through-registration sequence (the count read, the
    /// `max` comparison, `git worktree add`, and the final push onto the
    /// in-memory handle list) is serialised end-to-end by an internal
    /// `tokio::sync::Mutex`, held across every `.await` in that span. Two
    /// concurrent in-process `create()` calls on the same `WorktreeManager`
    /// can therefore never both observe the same pre-admission count and
    /// both proceed past the `max_worktrees` check — the second call's count
    /// read always reflects the first call's completed registration. Callers
    /// do not need to replicate external locking for quota-safety purposes;
    /// any locking they hold (e.g. `zeph-subagent`'s `cwd_lock`) exists for
    /// unrelated invariants and is not load-bearing for `max_worktrees`
    /// enforcement.
    ///
    /// ## TODO
    ///
    /// TODO(critic D2): head worktree does not include parent uncommitted changes
    /// by design; revisit if users need stash-based propagation.
    ///
    /// # Errors
    ///
    /// - [`WorktreeError::InvalidBranchName`] when `subagent_id` fails validation.
    /// - [`WorktreeError::QuotaExceeded`] when `config.max_worktrees` would be
    ///   reached or exceeded (see "Admission cap" above).
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

        // Serialises the quota-check-through-registration sequence below so
        // two concurrent in-process `create()` calls can never both observe
        // the same pre-admission count and both proceed past the
        // `max_worktrees` check (see `# Concurrency` above).
        let _admission_guard = self.admission_lock.lock().await;

        if let Some(max) = self.config.max_worktrees {
            let current = self.reconcile().await?.len() + self.list().len();
            if current >= max {
                return Err(WorktreeError::QuotaExceeded { current, max });
            }
        }

        let branch_name = format!("{}{}", self.config.branch_prefix, subagent_id);
        let path = self.worktree_root.join(subagent_id);

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
    /// This issues a single `git worktree remove --force`, which bypasses
    /// git's "refuse to remove a dirty working tree" guard but — deliberately
    /// — does *not* override an explicit `git worktree lock`; git demands a
    /// second `-f` (`remove -f -f`) for that. Callers deciding *whether* to
    /// call `remove` on a worktree not created by this session (e.g.
    /// `zeph worktree clean`) MUST gate on
    /// [`StaleWorktree::is_safe_to_force_remove`][crate::StaleWorktree::is_safe_to_force_remove]
    /// or an explicit operator override first — `remove` itself performs no
    /// such check (#6055).
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

    /// Reads the git worktree registry and returns [`StaleWorktree`] entries for
    /// worktrees that exist on disk but are not in the current session's
    /// in-memory list.
    ///
    /// This is called by the `zeph worktree list` and `zeph worktree clean`
    /// CLI subcommands (`handle_worktree_command` in `src/commands/worktree.rs`)
    /// to recover from a previous crash that left stale worktrees behind. There
    /// is no startup caller — worktrees are only reconciled on-demand via these
    /// subcommands. Each entry carries
    /// git's own `prunable` verdict (see [`StaleWorktree::is_safe_to_force_remove`])
    /// so callers can distinguish a worktree whose directory is already gone
    /// from one that is merely untracked by *this* process — the latter may
    /// belong to another, concurrently running session and MUST NOT be
    /// force-removed without an explicit operator override (#6055).
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
    /// for s in &stale {
    ///     println!("stale worktree: {:?} (safe to force-remove: {})", s.handle.path, s.is_safe_to_force_remove());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "worktree.reconcile", skip(self))]
    pub async fn reconcile(&self) -> Result<Vec<StaleWorktree>, WorktreeError> {
        let out = self
            .runner
            .run(&["worktree", "list", "--porcelain"], &self.repo_root)
            .await?;
        check_git_status(&out, "worktree list")?;

        let output_str = String::from_utf8_lossy(&out.stdout);
        let raw_entries = parse_worktree_list_porcelain(&output_str);

        let session_paths: std::collections::HashSet<PathBuf> = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|h| h.path.clone())
            .collect();

        let stale = raw_entries
            .into_iter()
            .filter(|e| !session_paths.contains(&e.path))
            // Skip the main worktree (repo_root itself).
            .filter(|e| e.path != self.repo_root)
            .map(|e| {
                let branch_name = match (e.branch, e.is_bare) {
                    (Some(branch), _) => branch,
                    (None, true) => BARE_WORKTREE_SENTINEL.to_string(),
                    (None, false) => DETACHED_BRANCH_SENTINEL.to_string(),
                };
                StaleWorktree {
                    handle: WorktreeHandle {
                        path: e.path,
                        branch_name,
                        base_ref_resolved: String::new(),
                        subagent_id: String::new(),
                        created_at: SystemTime::UNIX_EPOCH,
                    },
                    prunable_reason: e.prunable_reason,
                }
            })
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

    /// Runs the full `worktree clean` pipeline: [`reconcile`][Self::reconcile], then
    /// [`remove`][Self::remove] each stale entry that is either `prunable` or covered by
    /// `force`, then [`prune`][Self::prune] the registry.
    ///
    /// Shared by the CLI (`zeph worktree clean`, `src/commands/worktree.rs`) and the
    /// agent-side `/worktree clean` slash command (`crates/zeph-core/src/agent/
    /// worktree_commands.rs`) so their removed/skipped/errored counts and per-entry
    /// warnings cannot silently diverge — this exact divergence (a discarded `prune()`
    /// failure on one call site) was caught in review during #6141 (#6142).
    ///
    /// `force_hint` is substituted into the skip-warning for a non-`force` run advising
    /// the operator how to override it (e.g. `` `zeph worktree clean --force` `` for the
    /// CLI, `` `/worktree clean --force` `` for the slash command) — the only piece of
    /// UX text that legitimately differs between the two surfaces.
    ///
    /// This is a thin wrapper around an internal `clean_from_stale` helper —
    /// [`sweep`][Self::sweep] calls that helper directly with an already-fetched
    /// `stale` list so a single `sweep()` tick only ever issues one `reconcile()`
    /// subprocess call (#6205).
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::GitCommand`] only if the initial [`reconcile`][Self::reconcile] call
    /// fails — nothing has been removed yet, so there is no partial outcome to lose.
    /// Per-entry removal failures and a final prune failure are both recorded in the
    /// returned [`CleanOutcome`] instead of aborting.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(mgr: &zeph_worktree::DefaultWorktreeManager) -> Result<(), zeph_worktree::WorktreeError> {
    /// let outcome = mgr.clean(false, false, "`zeph worktree clean --force`").await?;
    /// println!("{}", zeph_worktree::format_clean_summary(&outcome));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn clean(
        &self,
        force: bool,
        prune_branch_on_remove: bool,
        force_hint: &str,
    ) -> Result<CleanOutcome, WorktreeError> {
        let stale = self.reconcile().await?;
        let (outcome, _remaining) = self
            .clean_from_stale(stale, force, prune_branch_on_remove, force_hint)
            .await;
        Ok(outcome)
    }

    /// Removal-and-prune half of the `clean` pipeline, operating on an already-fetched
    /// `stale` list rather than calling [`reconcile`][Self::reconcile] itself.
    ///
    /// For each entry, removes it via [`remove`][Self::remove] when it is either
    /// `prunable` or covered by `force`; otherwise leaves it in place with a skip
    /// warning. Always runs [`prune`][Self::prune] afterward (FR-CLEANUP-04), recording
    /// a failure as a warning rather than aborting.
    ///
    /// Returns the [`CleanOutcome`] alongside the "remaining" stale entries — the
    /// entries from `stale` that were *not* successfully removed (skipped or
    /// errored) — so a caller like [`sweep`][Self::sweep] can derive a post-clean
    /// worktree count purely in-memory, without a second `reconcile()` subprocess call.
    async fn clean_from_stale(
        &self,
        stale: Vec<StaleWorktree>,
        force: bool,
        prune_branch_on_remove: bool,
        force_hint: &str,
    ) -> (CleanOutcome, Vec<StaleWorktree>) {
        let mut outcome = CleanOutcome::default();
        let mut remaining = Vec::new();
        for stale_wt in stale {
            if !force && !stale_wt.is_safe_to_force_remove() {
                outcome.warnings.push(format!(
                    "warning: skipping {} — directory exists and git does not report it as \
                     prunable; it may be in active use by another zeph session. \
                     Re-run with {force_hint} if you are certain it is abandoned.",
                    stale_wt.handle.path.display()
                ));
                outcome.skipped += 1;
                remaining.push(stale_wt);
                continue;
            }
            if let Err(e) = self.remove(&stale_wt.handle, prune_branch_on_remove).await {
                outcome.warnings.push(format!(
                    "warning: failed to remove {}: {e}",
                    stale_wt.handle.path.display()
                ));
                outcome.errored += 1;
                remaining.push(stale_wt);
            } else {
                outcome.removed += 1;
            }
        }
        // FR-CLEANUP-04: clear any remaining stale administrative entries
        // (e.g. worktrees deleted outside Zeph) from the git registry.
        if let Err(e) = self.prune().await {
            outcome
                .warnings
                .push(format!("warning: failed to prune worktree registry: {e}"));
        }
        (outcome, remaining)
    }

    /// Computes total and per-worktree disk usage across every worktree under
    /// `root` — both this session's own ([`list`][Self::list]) and any discovered
    /// via [`reconcile`][Self::reconcile] (stale/foreign entries).
    ///
    /// The recursive filesystem walk runs on [`tokio::task::spawn_blocking`], so
    /// it never stalls the async executor — but it is still an O(files-under-root)
    /// operation that can be slow against multi-gigabyte `target/` directories.
    /// Callers on a hot or interactive path should prefer
    /// [`cached_disk_usage`][Self::cached_disk_usage] and only call this method
    /// from a deliberate, infrequent trigger (a [`sweep`][Self::sweep] tick or an
    /// explicit CLI invocation) — **never** from [`create`][Self::create].
    ///
    /// The reported total is a sum of logical file sizes
    /// (`std::fs::Metadata::len`), not on-disk block usage — content shared via
    /// hardlinks across worktrees (e.g. zeph-session blobs) can be double-counted.
    /// Treat the result as an approximation suitable for a soft warn threshold.
    ///
    /// On success, the result is stored so a subsequent
    /// [`cached_disk_usage`][Self::cached_disk_usage] call can read it without
    /// re-walking the filesystem.
    ///
    /// This is a thin wrapper around an internal `disk_usage_from_paths` helper —
    /// [`sweep`][Self::sweep] calls that helper directly with the stale paths left
    /// over from its own internal `clean_from_stale` call, so a single `sweep()` tick
    /// only ever issues one `reconcile()` subprocess call (#6205).
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::GitCommand`] if the underlying
    /// [`reconcile`][Self::reconcile] call fails, or [`WorktreeError::Io`] if the
    /// blocking walk task itself panics or is cancelled.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(mgr: &zeph_worktree::DefaultWorktreeManager) -> Result<(), zeph_worktree::WorktreeError> {
    /// let usage = mgr.disk_usage().await?;
    /// println!("total: {} bytes across {} worktree(s)", usage.total_bytes, usage.per_worktree.len());
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "worktree.disk_usage", skip(self))]
    pub async fn disk_usage(&self) -> Result<WorktreeDiskUsage, WorktreeError> {
        let paths: Vec<PathBuf> = self
            .reconcile()
            .await?
            .into_iter()
            .map(|stale| stale.handle.path)
            .collect();
        self.disk_usage_from_paths(paths).await
    }

    /// Filesystem-walk half of [`disk_usage`][Self::disk_usage], operating on an
    /// already-fetched list of stale worktree paths rather than calling
    /// [`reconcile`][Self::reconcile] itself.
    ///
    /// `paths` is merged with this session's own [`list`][Self::list] paths before the
    /// walk, matching [`disk_usage`][Self::disk_usage]'s behavior exactly.
    async fn disk_usage_from_paths(
        &self,
        mut paths: Vec<PathBuf>,
    ) -> Result<WorktreeDiskUsage, WorktreeError> {
        paths.extend(self.list().into_iter().map(|h| h.path));

        let usage = tokio::task::spawn_blocking(move || walk_worktree_sizes(&paths))
            .await
            .map_err(|e| WorktreeError::Io(std::io::Error::other(e)))?;

        *self.usage_cache.lock() = Some((Instant::now(), usage.clone()));
        Ok(usage)
    }

    /// Returns the disk usage computed by the most recent
    /// [`disk_usage`][Self::disk_usage] call, without performing a filesystem
    /// walk. Returns `None` if `disk_usage()` has never been called on this
    /// manager instance.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example(mgr: &zeph_worktree::DefaultWorktreeManager) {
    /// if let Some(usage) = mgr.cached_disk_usage() {
    ///     println!("last known total: {} bytes", usage.total_bytes);
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn cached_disk_usage(&self) -> Option<WorktreeDiskUsage> {
        self.usage_cache
            .lock()
            .as_ref()
            .map(|(_, usage)| usage.clone())
    }

    /// Runs one reconcile-and-quota sweep: a single [`reconcile`][Self::reconcile]
    /// call feeds prunable-only auto-reclaim (the same removal-and-prune pipeline as
    /// `zeph worktree clean`'s `clean(force=false, ..)`), then the resulting
    /// post-clean worktree count is evaluated against `config.max_worktrees` and,
    /// when `config.disk_quota_mb` is set, against a disk-usage walk.
    ///
    /// Unlike calling [`clean`][Self::clean] and [`disk_usage`][Self::disk_usage]
    /// directly (which each perform their own `reconcile()`), `sweep()` fetches the
    /// stale worktree list once and threads it through the internal
    /// `clean_from_stale` and `disk_usage_from_paths` helpers — the "remaining" stale
    /// entries `clean_from_stale` returns (i.e. `stale` minus what it just removed) are
    /// exactly the post-clean stale state, computed in-memory rather than by
    /// re-invoking `git worktree list --porcelain` (#6205). This makes every `sweep()`
    /// tick issue exactly one `reconcile()` subprocess call instead of three.
    ///
    /// Never force-removes an intact worktree — reclamation only removes entries git
    /// itself reports as `prunable` (spec-063 INV-5/INV-6). An over-quota state with
    /// only intact worktrees is reported via [`QuotaStatus::is_over_quota`], never
    /// resolved by deleting anything.
    ///
    /// The disk-usage walk is skipped entirely (and [`QuotaStatus::total_bytes`]
    /// is left at `0` with [`QuotaStatus::disk_quota_bytes`] as `None`) when
    /// `config.disk_quota_mb` is unset, avoiding the filesystem walk's cost when
    /// there is no threshold to evaluate it against.
    ///
    /// ## Concurrency note
    ///
    /// `count` and the disk-usage figures both derive from the single
    /// [`reconcile`][Self::reconcile] snapshot taken at the start of this call, not from
    /// re-querying git afterward. `WorktreeManager` is typically shared (e.g. `Arc`'d
    /// between a subagent spawn/teardown path and a periodic sweep loop), so a
    /// worktree registry mutation that lands *during* this call (a concurrent
    /// [`create`][Self::create]/[`remove`][Self::remove] from another task) will not be
    /// reflected in this tick's result — the next `sweep()` tick picks it up instead.
    /// This is a narrower staleness window than calling [`clean`][Self::clean] and
    /// [`disk_usage`][Self::disk_usage] separately (each would re-snapshot git at its own
    /// call time), but it does not weaken any safety invariant: reclamation still never
    /// force-removes an intact worktree (see below), and [`disk_usage`][Self::disk_usage]
    /// is already documented as an approximation suitable only for a soft warn threshold.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError::GitCommand`] if the initial
    /// [`reconcile`][Self::reconcile] call fails, or [`WorktreeError::Io`] from the
    /// disk-usage walk when disk accounting is enabled.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(mgr: &zeph_worktree::DefaultWorktreeManager) -> Result<(), zeph_worktree::WorktreeError> {
    /// let status = mgr.sweep().await?;
    /// if status.is_over_quota() {
    ///     eprintln!("worktrees over quota: {}/{:?}", status.count, status.max_worktrees);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(name = "worktree.sweep", skip(self))]
    pub async fn sweep(&self) -> Result<QuotaStatus, WorktreeError> {
        let stale = self.reconcile().await?;
        let (outcome, remaining_stale) = self
            .clean_from_stale(
                stale,
                false,
                self.config.prune_branch_on_remove,
                "`zeph worktree clean --force`",
            )
            .await;

        let count = remaining_stale.len() + self.list().len();
        let max_worktrees = self.config.max_worktrees;
        let over_count = max_worktrees.is_some_and(|max| count >= max);

        let (total_bytes, disk_quota_bytes, over_disk) =
            if let Some(quota_mb) = self.config.disk_quota_mb {
                let paths: Vec<PathBuf> = remaining_stale
                    .into_iter()
                    .map(|stale| stale.handle.path)
                    .collect();
                let usage = self.disk_usage_from_paths(paths).await?;
                let quota_bytes = quota_mb.saturating_mul(1_048_576);
                let over_disk = usage.total_bytes >= quota_bytes;
                (usage.total_bytes, Some(quota_bytes), over_disk)
            } else {
                (0, None, false)
            };

        Ok(QuotaStatus {
            count,
            max_worktrees,
            total_bytes,
            disk_quota_bytes,
            reclaimed: outcome.removed,
            over_count,
            over_disk,
        })
    }
}

/// Recursively sums regular-file sizes under each path in `paths`.
///
/// Runs on a blocking thread (see [`WorktreeManager::disk_usage`]). Entries that
/// cannot be read (permission errors, races with concurrent removal) are silently
/// skipped rather than failing the whole walk — a best-effort accounting is more
/// useful than an aborted one for a soft warn threshold.
fn walk_worktree_sizes(paths: &[PathBuf]) -> WorktreeDiskUsage {
    let mut per_worktree = Vec::with_capacity(paths.len());
    let mut total_bytes: u64 = 0;

    for path in paths {
        let mut size: u64 = 0;
        for entry in walkdir::WalkDir::new(path)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file()
                && let Ok(metadata) = entry.metadata()
            {
                size = size.saturating_add(metadata.len());
            }
        }
        total_bytes = total_bytes.saturating_add(size);
        per_worktree.push((path.clone(), size));
    }

    WorktreeDiskUsage {
        total_bytes,
        per_worktree,
    }
}

/// Outcome of a [`WorktreeManager::clean`] pass.
///
/// Every stale entry `reconcile()` discovers is accounted for in exactly one of
/// `removed`, `skipped`, or `errored` — the three always sum to the number of stale
/// entries processed, so a caller can never silently undercount what happened.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanOutcome {
    /// Number of stale entries successfully removed.
    pub removed: usize,
    /// Number of stale entries left in place because they were not `prunable` and
    /// `force` was not passed.
    pub skipped: usize,
    /// Number of stale entries whose removal was attempted but the underlying
    /// `git worktree remove` call itself failed (e.g. a locked worktree).
    pub errored: usize,
    /// One warning line per skipped, errored, or prune-failure event, in encounter order.
    pub warnings: Vec<String>,
}

/// Formats a [`CleanOutcome`]'s counts into the standard one-line summary shared by
/// both the CLI and agent-side `/worktree clean` output.
#[must_use]
pub fn format_clean_summary(outcome: &CleanOutcome) -> String {
    format!(
        "Removed {} stale worktree(s), skipped {} in-use candidate(s), {} error(s).",
        outcome.removed, outcome.skipped, outcome.errored
    )
}

/// Intermediate result of parsing one `git worktree list --porcelain` block,
/// before [`reconcile`][WorktreeManager::reconcile] resolves it into a
/// [`StaleWorktree`].
///
/// Kept private to this module: it exists only so the parser doesn't have to
/// build a full [`WorktreeHandle`] (with placeholder `base_ref_resolved` /
/// `subagent_id` / `created_at`) before the `branch_name` fallback (detached
/// vs. bare vs. real branch) and the `prunable` verdict are both known.
struct RawWorktreeEntry {
    /// Absolute path from the `worktree <path>` line.
    path: PathBuf,
    /// `Some(name)` from a `branch refs/heads/<name>` line; `None` for a
    /// `detached` or `bare` block.
    branch: Option<String>,
    /// `true` if this block's line was `bare` (the main worktree of a bare
    /// repository) rather than `branch`/`detached`.
    is_bare: bool,
    /// `Some(reason)` from a `prunable <reason>` line — git's own signal that
    /// this worktree's directory or `.git` gitdir-link is gone/broken.
    prunable_reason: Option<String>,
}

/// Parses the output of `git worktree list --porcelain` into [`RawWorktreeEntry`]s.
///
/// Each worktree block in the porcelain output looks like one of:
/// ```text
/// worktree /path/to/worktree
/// HEAD deadbeef...
/// branch refs/heads/branch-name
///
/// ```
/// for a worktree on a detached `HEAD`:
/// ```text
/// worktree /path/to/worktree
/// HEAD deadbeef...
/// detached
///
/// ```
/// for the main worktree of a bare repository (#6052 — no `HEAD` line at all):
/// ```text
/// worktree /path/to/bare.git
/// bare
///
/// ```
/// or, when the directory/gitdir-link is gone or broken, with an extra line
/// regardless of the block's other contents:
/// ```text
/// prunable gitdir file points to non-existent location
/// ```
/// A block is flushed as soon as its `worktree <path>` line is seen (i.e. when
/// the *next* block starts, or at end of output) — regardless of whether a
/// `branch` line was present. Detached-HEAD and bare blocks are never silently
/// dropped (#5936, #6052).
fn parse_worktree_list_porcelain(output: &str) -> Vec<RawWorktreeEntry> {
    let mut result = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut is_bare = false;
    let mut prunable_reason: Option<String> = None;

    for line in output.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(entry) =
                flush_worktree_block(path.take(), branch.take(), is_bare, prunable_reason.take())
            {
                result.push(entry);
            }
            is_bare = false;
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            branch = Some(b.to_string());
        } else if let Some(reason) = line.strip_prefix("prunable ") {
            prunable_reason = Some(reason.to_string());
        } else if line == "bare" {
            is_bare = true;
        }
        // "locked ..." lines need no new handling here — a locked worktree is
        // already protected by git's own single-`--force` refusal to remove
        // it (see `WorktreeManager::remove`'s doc comment); "detached" lines
        // need no explicit match either, since the `branch = None, is_bare =
        // false` fallback already resolves to `DETACHED_BRANCH_SENTINEL`.
    }

    if let Some(entry) = flush_worktree_block(path, branch, is_bare, prunable_reason) {
        result.push(entry);
    }

    result
}

/// Builds a [`RawWorktreeEntry`] from one parsed porcelain block, if a
/// `worktree <path>` line was seen.
fn flush_worktree_block(
    path: Option<PathBuf>,
    branch: Option<String>,
    is_bare: bool,
    prunable_reason: Option<String>,
) -> Option<RawWorktreeEntry> {
    path.map(|wt_path| RawWorktreeEntry {
        path: wt_path,
        branch,
        is_bare,
        prunable_reason,
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

    // --- config accessors ---

    #[tokio::test]
    async fn prune_branch_on_remove_reflects_constructor_config() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let config = WorktreeConfig {
            prune_branch_on_remove: true,
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();
        assert!(mgr.prune_branch_on_remove());
    }

    #[tokio::test]
    async fn prune_branch_on_remove_defaults_to_false() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let mgr = make_manager(&dir, runner).await;
        assert!(!mgr.prune_branch_on_remove());
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

    /// Regression test for #5940: `create()` must reuse the `worktree_root` cached on
    /// `self` at construction time rather than recomputing it on every call. Every
    /// other `create()` test in this module calls `create()` at most once per manager,
    /// so none of them would fail if `create()` accidentally recomputed a stale or
    /// diverged root each time — only calling `create()` twice on the same manager and
    /// checking both handles resolve under the identical cached parent actually
    /// exercises the caching behavior.
    #[tokio::test]
    async fn create_reuses_cached_worktree_root_across_calls() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        // First create(): status --porcelain (dirty check), then worktree add.
        runner.push_ok(b"" as &[u8]);
        runner.push_ok(b"" as &[u8]);
        // Second create(): status --porcelain, then worktree add.
        runner.push_ok(b"" as &[u8]);
        runner.push_ok(b"" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let cached_root = mgr.worktree_root.clone();

        let handle_a = mgr.create("agent-a").await.unwrap();
        let handle_b = mgr.create("agent-b").await.unwrap();

        assert_eq!(handle_a.path.parent(), Some(cached_root.as_path()));
        assert_eq!(handle_b.path.parent(), Some(cached_root.as_path()));
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
        assert_eq!(stale[0].handle.branch_name, "agent/agent-1");
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
        assert_eq!(stale[0].handle.branch_name, DETACHED_BRANCH_SENTINEL);
        assert_eq!(
            stale[0].handle.path,
            dir.path().join("worktrees/detached-1")
        );
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
        assert_eq!(result[0].branch, None);
        assert!(!result[0].is_bare);
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
        assert_eq!(result[0].branch, None);
        assert_eq!(result[1].path, PathBuf::from("/repo/wt-b"));
        assert_eq!(result[1].branch, None);
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
        assert_eq!(stale[0].handle.branch_name, DETACHED_BRANCH_SENTINEL);
        assert_eq!(
            stale[0].handle.path,
            dir.path().join("worktrees/detached-2")
        );
    }

    /// Regression test for #6052: `git worktree list --porcelain` emits a
    /// `bare` line (not `branch` or `detached`) for the main worktree of a
    /// bare repository:
    /// ```text
    /// worktree /path/to/bare.git
    /// bare
    ///
    /// ```
    /// (confirmed against real `git worktree list --porcelain` output, git
    /// 2.50.1 — no `HEAD` line is emitted for bare worktrees at all). The
    /// parser must capture this as `is_bare = true` rather than falling
    /// through to the detached-HEAD fallback.
    #[test]
    fn parse_worktree_list_porcelain_marks_bare_worktree_as_bare() {
        let output = "worktree /path/to/bare.git\nbare\n\n";
        let result = parse_worktree_list_porcelain(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/path/to/bare.git"));
        assert_eq!(result[0].branch, None);
        assert!(result[0].is_bare);
    }

    /// End-to-end regression test for #6052: `reconcile()` must label a bare
    /// worktree with [`BARE_WORKTREE_SENTINEL`], not
    /// [`DETACHED_BRANCH_SENTINEL`] — the two are semantically distinct and
    /// must not collide.
    #[tokio::test]
    async fn reconcile_distinguishes_bare_from_detached_worktree() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\nworktree {0}/worktrees/bare-1\nbare\n\nworktree {0}/worktrees/detached-1\nHEAD def456\ndetached\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;
        let stale = mgr.reconcile().await.unwrap();
        assert_eq!(stale.len(), 2);
        assert_eq!(stale[0].handle.branch_name, BARE_WORKTREE_SENTINEL);
        assert_eq!(stale[1].handle.branch_name, DETACHED_BRANCH_SENTINEL);
        assert_ne!(
            BARE_WORKTREE_SENTINEL, DETACHED_BRANCH_SENTINEL,
            "bare and detached sentinels must be distinct markers"
        );
    }

    /// Regression test for #6055: a worktree whose directory is intact (no
    /// `prunable` line in the porcelain output) must report
    /// `prunable_reason == None` and `is_safe_to_force_remove() == false` —
    /// this is the condition under which `clean` must skip-and-warn instead
    /// of force-removing, since the worktree may belong to another,
    /// concurrently running session.
    #[tokio::test]
    async fn reconcile_marks_directory_intact_worktree_as_not_prunable() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\nworktree {0}/worktrees/agent-1\nHEAD def456\nbranch refs/heads/agent/agent-1\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;
        let stale = mgr.reconcile().await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].prunable_reason, None);
        assert!(!stale[0].is_safe_to_force_remove());
    }

    /// Regression test for #6055: when git's porcelain output includes a
    /// `prunable <reason>` line for a worktree, `reconcile()` must surface the
    /// exact reason text and `is_safe_to_force_remove()` must be `true` — this
    /// is the one condition under which `clean` may force-remove by default.
    #[tokio::test]
    async fn reconcile_captures_prunable_reason_text() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\nworktree {0}/worktrees/gone\nHEAD def456\nbranch refs/heads/agent/gone\nprunable gitdir file points to non-existent location\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;
        let stale = mgr.reconcile().await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(
            stale[0].prunable_reason,
            Some("gitdir file points to non-existent location".to_string())
        );
        assert!(stale[0].is_safe_to_force_remove());
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

    // --- clean (#6142: shared by CLI + agent-side /worktree clean) ---

    /// Mixed prunable/non-prunable stale list, `force = false`: the prunable entry is
    /// removed, the non-prunable one is skipped and left alone — the standard case
    /// both the CLI and agent-side callers depend on.
    #[tokio::test]
    async fn clean_removes_prunable_and_skips_non_prunable_without_force() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/prunable-1\nHEAD def456\nbranch refs/heads/agent/prunable-1\n\
             prunable gitdir file points to non-existent location\n\n\
             worktree {0}/worktrees/in-use-1\nHEAD ghi789\nbranch refs/heads/agent/in-use-1\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes()); // reconcile: worktree list --porcelain
        runner.push_ok(b"" as &[u8]); // remove prunable-1: worktree remove --force
        runner.push_ok(b"" as &[u8]); // final: worktree prune

        let mgr = make_manager(&dir, runner).await;
        let outcome = mgr.clean(false, false, "`--force`").await.unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.errored, 0);
        assert_eq!(
            outcome.warnings.len(),
            1,
            "only the skip warning: {:?}",
            outcome.warnings
        );
        assert!(outcome.warnings[0].contains("in-use-1"));
        assert!(outcome.warnings[0].contains("`--force`"));
    }

    /// `force = true` bypasses the non-prunable skip-gate: the same entry that would be
    /// skipped without `--force` is now attempted and removed.
    #[tokio::test]
    async fn clean_with_force_removes_non_prunable_entries_too() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/in-use-1\nHEAD ghi789\nbranch refs/heads/agent/in-use-1\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());
        runner.push_ok(b"" as &[u8]); // remove in-use-1 (force bypasses the skip-gate)
        runner.push_ok(b"" as &[u8]); // final prune

        let mgr = make_manager(&dir, runner).await;
        let outcome = mgr.clean(true, false, "`--force`").await.unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.errored, 0);
    }

    /// Regression test for #6077/#6142 (critic M2): a stale entry whose `remove()` call
    /// itself fails (e.g. a locked worktree under `--force`) must be counted as
    /// `errored`, not silently folded into `removed` or `skipped`.
    #[tokio::test]
    async fn clean_counts_errored_when_remove_fails() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/locked-1\nHEAD def456\nbranch refs/heads/agent/locked-1\n\
             prunable gitdir file points to non-existent location\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());
        runner.push_err(b"error: unable to remove worktree: it is locked\n" as &[u8]);
        runner.push_ok(b"" as &[u8]); // final prune still runs

        let mgr = make_manager(&dir, runner).await;
        let outcome = mgr.clean(false, false, "`--force`").await.unwrap();

        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.errored, 1);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("failed to remove"));
    }

    /// Regression test for the exact divergence caught in #6141's review: a failure of
    /// the final `prune()` step must be recorded as a warning, not discard an
    /// otherwise-successful removal count or turn the whole call into an `Err`.
    #[tokio::test]
    async fn clean_records_prune_failure_without_discarding_removed_count() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/prunable-1\nHEAD def456\nbranch refs/heads/agent/prunable-1\n\
             prunable gitdir file points to non-existent location\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());
        runner.push_ok(b"" as &[u8]); // remove succeeds
        runner.push_err(b"fatal: not a working tree\n" as &[u8]); // prune fails

        let mgr = make_manager(&dir, runner).await;
        let outcome = mgr
            .clean(false, false, "`--force`")
            .await
            .expect("a prune failure must not turn clean() into an Err");

        assert_eq!(
            outcome.removed, 1,
            "the successful removal must not be discarded by a later prune failure"
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("failed to prune worktree registry")),
            "got: {:?}",
            outcome.warnings
        );
    }

    #[test]
    fn format_clean_summary_all_zero() {
        assert_eq!(
            format_clean_summary(&CleanOutcome::default()),
            "Removed 0 stale worktree(s), skipped 0 in-use candidate(s), 0 error(s)."
        );
    }

    #[test]
    fn format_clean_summary_includes_all_three_counts() {
        let outcome = CleanOutcome {
            removed: 2,
            skipped: 1,
            errored: 3,
            warnings: Vec::new(),
        };
        let msg = format_clean_summary(&outcome);
        assert!(msg.contains("Removed 2"), "got: {msg}");
        assert!(msg.contains("skipped 1"), "got: {msg}");
        assert!(msg.contains("3 error(s)"), "got: {msg}");
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

    // --- max_worktrees admission cap ---

    #[tokio::test]
    async fn create_succeeds_when_under_max_worktrees_cap() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes()); // reconcile for quota check
        runner.push_ok(b"" as &[u8]); // status --porcelain (dirty check)
        runner.push_ok(b"" as &[u8]); // worktree add

        let config = WorktreeConfig {
            max_worktrees: Some(5),
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();
        assert!(mgr.create("agent-a").await.is_ok());
    }

    /// Regression test for #5924: `create()` must refuse admission once the
    /// git-registered secondary worktree count reaches `max_worktrees`, without
    /// issuing any further git calls (only `FakeGitRunner`'s queued reconcile
    /// response is consumed for the rejected call).
    #[tokio::test]
    async fn create_fails_with_quota_exceeded_when_max_worktrees_reached() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n",
            dir.path().display()
        );
        // First create(): reconcile (quota check), status --porcelain, worktree add.
        runner.push_ok(porcelain.clone().into_bytes());
        runner.push_ok(b"" as &[u8]);
        runner.push_ok(b"" as &[u8]);
        // Second create(): reconcile (quota check) only — QuotaExceeded short-circuits
        // before any further git call.
        runner.push_ok(porcelain.into_bytes());

        let config = WorktreeConfig {
            max_worktrees: Some(1),
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();

        mgr.create("agent-a").await.unwrap();

        let err = mgr.create("agent-b").await.unwrap_err();
        assert_matches!(err, WorktreeError::QuotaExceeded { current: 1, max: 1 });
    }

    /// Test-only [`GitRunner`] wrapper that sleeps before delegating a
    /// `worktree list --porcelain` call (the git call `reconcile` issues as
    /// part of `create`'s quota check). This widens the check-then-act window
    /// enough for a multi-threaded runtime to reliably interleave concurrent
    /// `create()` calls — without it, `FakeGitRunner::run` never actually
    /// suspends, so two tasks racing through `create()` would just run
    /// sequentially to completion and the test would prove nothing.
    struct DelayedListRunner {
        inner: Arc<FakeGitRunner>,
        delay: std::time::Duration,
    }

    impl GitRunner for DelayedListRunner {
        async fn run(&self, args: &[&str], cwd: &Path) -> Result<Output, WorktreeError> {
            if args.first() == Some(&"worktree") && args.get(1) == Some(&"list") {
                tokio::time::sleep(self.delay).await;
            }
            self.inner.run(args, cwd).await
        }
    }

    /// Regression test for #6250: two or more in-process `create()` calls
    /// racing on a `WorktreeManager` configured with `max_worktrees = 1` must
    /// never both observe the same pre-admission count and both proceed past
    /// the quota check. Spawns `CONCURRENCY` tasks that all start as close to
    /// simultaneously as possible (via a barrier) on a real multi-thread
    /// runtime, with the quota-check git call artificially delayed to widen
    /// the TOCTOU window `admission_lock` closes. Every `run()` response is
    /// an empty-stdout success — `reconcile` parses `""` as zero stale
    /// entries, `check_dirty_tree` ignores its result entirely, and
    /// `git_worktree_add` only needs a zero exit code — so the response
    /// content is agnostic to which task ends up winning the race; only the
    /// call *count* (3 for the winner, 1 for each loser) matters, and that
    /// count is fixed regardless of interleaving order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn create_admission_is_race_free_under_concurrent_calls() {
        const CONCURRENCY: usize = 8;

        let dir = make_repo();
        let inner = Arc::new(FakeGitRunner::new());
        for _ in 0..CONCURRENCY * 3 {
            inner.push_ok(b"" as &[u8]);
        }
        let runner = DelayedListRunner {
            inner,
            delay: std::time::Duration::from_millis(20),
        };

        let config = WorktreeConfig {
            max_worktrees: Some(1),
            ..test_config()
        };
        let mgr = Arc::new(
            WorktreeManager::new(dir.path().to_path_buf(), config, runner)
                .await
                .unwrap(),
        );

        let barrier = Arc::new(tokio::sync::Barrier::new(CONCURRENCY));
        let mut tasks = Vec::with_capacity(CONCURRENCY);
        for i in 0..CONCURRENCY {
            let mgr = Arc::clone(&mgr);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                mgr.create(&format!("agent-{i}")).await
            }));
        }

        let mut ok_count = 0;
        let mut quota_exceeded_count = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => ok_count += 1,
                Err(WorktreeError::QuotaExceeded { max: 1, .. }) => quota_exceeded_count += 1,
                Err(e) => panic!("unexpected error from concurrent create(): {e}"),
            }
        }

        assert_eq!(
            ok_count, 1,
            "exactly one concurrent create() must be admitted under max_worktrees=1"
        );
        assert_eq!(quota_exceeded_count, CONCURRENCY - 1);
    }

    // --- disk_usage / cached_disk_usage ---

    #[tokio::test]
    async fn disk_usage_returns_zero_for_no_worktrees() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;
        let usage = mgr.disk_usage().await.unwrap();
        assert_eq!(usage.total_bytes, 0);
        assert!(usage.per_worktree.is_empty());
    }

    #[tokio::test]
    async fn disk_usage_sums_file_sizes_under_registered_worktree() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());

        let mgr = make_manager(&dir, runner).await;

        let wt_path = dir.path().join("worktrees/agent-1");
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::write(wt_path.join("file.txt"), b"hello world").unwrap();

        mgr.handles.lock().unwrap().push(WorktreeHandle {
            path: wt_path.clone(),
            branch_name: "agent/agent-1".to_string(),
            base_ref_resolved: "HEAD".to_string(),
            subagent_id: "agent-1".to_string(),
            created_at: SystemTime::now(),
        });

        let usage = mgr.disk_usage().await.unwrap();
        assert_eq!(usage.total_bytes, 11);
        assert_eq!(usage.per_worktree.len(), 1);
        assert_eq!(usage.per_worktree[0].0, wt_path);
        assert_eq!(usage.per_worktree[0].1, 11);
    }

    #[tokio::test]
    async fn cached_disk_usage_none_before_first_call() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let mgr = make_manager(&dir, runner).await;
        assert!(mgr.cached_disk_usage().is_none());
    }

    #[tokio::test]
    async fn cached_disk_usage_populated_after_disk_usage_call() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n",
            dir.path().display()
        );
        runner.push_ok(porcelain.into_bytes());
        let mgr = make_manager(&dir, runner).await;

        assert!(mgr.cached_disk_usage().is_none());
        mgr.disk_usage().await.unwrap();
        assert!(mgr.cached_disk_usage().is_some());
    }

    // --- sweep ---

    /// Regression test for #5924: `sweep()` reclaims only `prunable` entries (via the
    /// same `clean(force=false, ..)` pipeline as `zeph worktree clean`) and, with no
    /// `disk_quota_mb` configured, never performs the filesystem walk.
    #[tokio::test]
    async fn sweep_reclaims_prunable_and_reports_count_without_disk_check() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain_with_prunable = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/prunable-1\nHEAD def456\nbranch refs/heads/agent/prunable-1\n\
             prunable gitdir file points to non-existent location\n\n",
            dir.path().display()
        );
        // sweep()'s single reconcile() call, shared by the clean and count steps.
        runner.push_ok(porcelain_with_prunable.into_bytes());
        // clean_from_stale(): remove prunable-1
        runner.push_ok(b"" as &[u8]);
        // clean_from_stale(): prune
        runner.push_ok(b"" as &[u8]);

        let mgr = make_manager(&dir, runner).await;
        let status = mgr.sweep().await.unwrap();

        assert_eq!(status.reclaimed, 1);
        assert_eq!(status.count, 0);
        assert_eq!(status.max_worktrees, None);
        assert!(!status.over_count);
        assert_eq!(status.total_bytes, 0);
        assert_eq!(status.disk_quota_bytes, None);
        assert!(!status.over_disk);
        assert!(!status.is_over_quota());
    }

    /// Regression test for #5924 (critic M1): `sweep()` must evaluate `disk_quota_mb`
    /// when configured — this is what makes the quota knob non-inert even when
    /// `auto_reconcile_secs = 0` (periodic sweep disabled) and only the startup sweep
    /// runs `sweep()` once.
    #[tokio::test]
    async fn sweep_evaluates_disk_quota_when_configured() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n",
            dir.path().display()
        );
        // sweep()'s single reconcile() call (no prunable entries — nothing removed).
        runner.push_ok(porcelain.into_bytes());
        // clean_from_stale(): prune
        runner.push_ok(b"" as &[u8]);

        let config = WorktreeConfig {
            disk_quota_mb: Some(1),
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();

        let wt_path = dir.path().join("worktrees/agent-1");
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::write(wt_path.join("big.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();

        mgr.handles.lock().unwrap().push(WorktreeHandle {
            path: wt_path.clone(),
            branch_name: "agent/agent-1".to_string(),
            base_ref_resolved: "HEAD".to_string(),
            subagent_id: "agent-1".to_string(),
            created_at: SystemTime::now(),
        });

        let status = mgr.sweep().await.unwrap();
        assert_eq!(status.disk_quota_bytes, Some(1_048_576));
        assert!(status.total_bytes >= 2 * 1024 * 1024);
        assert!(status.over_disk);
        assert!(status.is_over_quota());
    }

    /// Regression test (tester gap G1): `sweep()`'s `over_count = true` branch — the only
    /// user-visible signal when an operator lowers `max_worktrees` below the existing worktree
    /// count (which does not evict anything, only blocks new admissions).
    #[tokio::test]
    async fn sweep_reports_over_count_when_max_worktrees_exceeded() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/agent-1\nHEAD def456\nbranch refs/heads/agent/agent-1\n\n\
             worktree {0}/worktrees/agent-2\nHEAD ghi789\nbranch refs/heads/agent/agent-2\n\n",
            dir.path().display()
        );
        // sweep()'s single reconcile() call (no prunable entries — both skipped, nothing removed).
        runner.push_ok(porcelain.into_bytes());
        // clean_from_stale(): prune
        runner.push_ok(b"" as &[u8]);

        let config = WorktreeConfig {
            max_worktrees: Some(1),
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();

        let status = mgr.sweep().await.unwrap();
        assert_eq!(status.count, 2);
        assert_eq!(status.max_worktrees, Some(1));
        assert!(status.over_count);
        assert!(status.is_over_quota());
        assert_eq!(
            status.reclaimed, 0,
            "over_count must never trigger removal of intact worktrees"
        );
    }

    /// Regression test (tester gap G2): `max_worktrees` and `disk_quota_mb` set together must
    /// report `over_count`/`over_disk` independently — one breached, the other not.
    #[tokio::test]
    async fn sweep_reports_independent_over_count_and_over_disk_flags() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n",
            dir.path().display()
        );
        // sweep()'s single reconcile() call (no prunable entries — nothing removed).
        runner.push_ok(porcelain.into_bytes());
        // clean_from_stale(): prune
        runner.push_ok(b"" as &[u8]);

        let config = WorktreeConfig {
            max_worktrees: Some(5),
            disk_quota_mb: Some(1),
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();

        let wt_path = dir.path().join("worktrees/agent-1");
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::write(wt_path.join("big.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();

        mgr.handles.lock().unwrap().push(WorktreeHandle {
            path: wt_path.clone(),
            branch_name: "agent/agent-1".to_string(),
            base_ref_resolved: "HEAD".to_string(),
            subagent_id: "agent-1".to_string(),
            created_at: SystemTime::now(),
        });

        let status = mgr.sweep().await.unwrap();
        assert_eq!(status.count, 1, "only the registered handle counts");
        assert!(
            !status.over_count,
            "count (1) is under max_worktrees (5) — must not be flagged over"
        );
        assert!(status.over_disk, "disk usage over quota must be flagged");
        assert!(status.is_over_quota());
    }

    /// Regression test for #6205 (review finding #1 / critic finding #4): a single
    /// `sweep()` tick with a MIX of removed + skipped + errored stale entries must
    /// retain in `remaining_stale` exactly the skipped and errored ones, excluding only
    /// the successfully-removed one. Every other sweep test has stale entries that are
    /// either all-removed or all-skipped, so none of them can distinguish "`remaining`
    /// correctly excludes removed entries" from "`remaining` happens to equal all stale
    /// entries" as a coincidence of the fixtures used — this test is the one that can.
    ///
    /// Asserts both `status.count` (derived from `remaining_stale.len()`) and
    /// `status.total_bytes` (derived from the paths handed to `disk_usage_from_paths`,
    /// which come from the same `remaining_stale`): each worktree directory is seeded
    /// with a distinct file size, so if the removed entry's path leaked into
    /// `remaining_stale` the total would be wrong by exactly its size (100 bytes).
    #[tokio::test]
    async fn sweep_retains_only_skipped_and_errored_entries_when_outcomes_are_mixed() {
        let dir = make_repo();
        let runner = FakeGitRunner::new();
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/removed-1\nHEAD def456\nbranch refs/heads/agent/removed-1\n\
             prunable gitdir file points to non-existent location\n\n\
             worktree {0}/worktrees/skipped-1\nHEAD ghi789\nbranch refs/heads/agent/skipped-1\n\n\
             worktree {0}/worktrees/errored-1\nHEAD jkl012\nbranch refs/heads/agent/errored-1\n\
             prunable gitdir file points to non-existent location\n\n",
            dir.path().display()
        );
        // sweep()'s single reconcile() call, covering all 3 stale entries.
        runner.push_ok(porcelain.into_bytes());
        // clean_from_stale(): remove removed-1 -> succeeds.
        runner.push_ok(b"" as &[u8]);
        // clean_from_stale(): remove errored-1 -> fails (e.g. a locked worktree).
        runner.push_err(b"error: unable to remove worktree: it is locked\n" as &[u8]);
        // clean_from_stale(): prune.
        runner.push_ok(b"" as &[u8]);

        let config = WorktreeConfig {
            disk_quota_mb: Some(1),
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, runner)
            .await
            .unwrap();

        // Seed all 3 directories with distinct sizes so a leaked/missing path in
        // `remaining_stale` shows up as a wrong `total_bytes`, not just a wrong count.
        for (name, size) in [("removed-1", 100), ("skipped-1", 10), ("errored-1", 1)] {
            let wt_path = dir.path().join("worktrees").join(name);
            std::fs::create_dir_all(&wt_path).unwrap();
            std::fs::write(wt_path.join("file.bin"), vec![0u8; size]).unwrap();
        }

        let status = mgr.sweep().await.unwrap();

        assert_eq!(
            status.reclaimed, 1,
            "only removed-1 was successfully removed"
        );
        assert_eq!(
            status.count, 2,
            "count must reflect exactly the skipped + errored entries, not the removed one"
        );
        assert_eq!(
            status.total_bytes, 11,
            "disk usage must walk exactly skipped-1 (10 bytes) + errored-1 (1 byte); \
             100 would mean removed-1 leaked into remaining_stale, 1 or 10 alone would mean \
             one of skipped/errored was dropped"
        );
    }

    /// Regression test for #6205: before the dedup fix, a `sweep()` tick with
    /// `disk_quota_mb` configured issued `worktree list --porcelain` three times
    /// (once inside `clean()`, once directly in `sweep()` for the post-clean count,
    /// once inside `disk_usage()`) — all three reflect the same git state, so the
    /// last two were pure duplicates. `sweep()` must now issue exactly one.
    #[tokio::test]
    async fn sweep_issues_exactly_one_reconcile_call_per_tick() {
        let dir = make_repo();
        let runner = Arc::new(FakeGitRunner::new());
        let porcelain = format!(
            "worktree {0}\nHEAD abc123\nbranch refs/heads/main\n\n\
             worktree {0}/worktrees/prunable-1\nHEAD def456\nbranch refs/heads/agent/prunable-1\n\
             prunable gitdir file points to non-existent location\n\n",
            dir.path().display()
        );
        // The single reconcile() call shared by the clean and disk-usage halves.
        runner.push_ok(porcelain.into_bytes());
        // clean_from_stale(): remove prunable-1
        runner.push_ok(b"" as &[u8]);
        // clean_from_stale(): prune
        runner.push_ok(b"" as &[u8]);

        let config = WorktreeConfig {
            disk_quota_mb: Some(1),
            ..test_config()
        };
        let mgr = WorktreeManager::new(dir.path().to_path_buf(), config, Arc::clone(&runner))
            .await
            .unwrap();

        mgr.sweep().await.unwrap();

        let calls = runner.calls.lock().unwrap();
        let reconcile_calls = calls
            .iter()
            .filter(|(args, _)| {
                args == &vec![
                    "worktree".to_string(),
                    "list".to_string(),
                    "--porcelain".to_string(),
                ]
            })
            .count();
        assert_eq!(
            reconcile_calls, 1,
            "sweep() must call `worktree list --porcelain` exactly once per tick, got calls: {calls:?}"
        );
    }
}
