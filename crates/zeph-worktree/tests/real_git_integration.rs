// SPDX-License-Identifier: MIT
//! Real-`git` integration tests for [`WorktreeManager`].
//!
//! Unlike the unit tests in `src/manager.rs` (which use `FakeGitRunner`),
//! these tests spawn the actual `git` binary against a temporary repository,
//! exercising the full `reconcile` -> `remove` -> `prune` pipeline (#5936,
//! #5937) and the `git_timeout_secs = 0` clamp (#5939) exactly as wired at
//! the real construction call sites in `src/commands/worktree.rs` and
//! `src/runner.rs`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use zeph_config::WorktreeConfig;
use zeph_worktree::{DETACHED_BRANCH_SENTINEL, DefaultGitRunner, WorktreeError, WorktreeManager};

fn git(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git must be on PATH for this integration test")
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    assert!(git(&["init", "-q"], path).status.success());
    git(&["config", "user.email", "test@example.com"], path);
    git(&["config", "user.name", "Test"], path);
    std::fs::write(path.join("README.md"), "test\n").unwrap();
    git(&["add", "."], path);
    assert!(git(&["commit", "-q", "-m", "init"], path).status.success());
    dir
}

fn config() -> WorktreeConfig {
    WorktreeConfig {
        enabled: true,
        root: "worktrees".to_string(),
        branch_prefix: "agent/".to_string(),
        ..WorktreeConfig::default()
    }
}

/// End-to-end regression test for #5937 (FR-CLEANUP-04): when a worktree
/// directory is deleted directly (`rm -rf`, simulating a crash or manual
/// cleanup) instead of via `git worktree remove`, `git` leaves a "prunable"
/// administrative entry behind. `WorktreeManager::prune()` — wired into
/// `zeph worktree clean` after the `reconcile()`/`remove()` loop — must
/// clear it.
///
/// This test isolates `prune()`'s own contribution by deliberately *not*
/// calling `remove()` on the discovered stale handle: on the git version
/// this suite runs against (2.50.1), `git worktree remove --force` on an
/// already-missing directory happens to also clear the entry by itself, so
/// a full-pipeline test alone would not prove `prune()` is load-bearing.
/// Calling `reconcile()` + `prune()` only shows `prune()` independently
/// clears the leftover entry, which is the FR-CLEANUP-04 guarantee.
///
/// The worktree is created by one `WorktreeManager` instance and discovered
/// by a *second*, freshly-constructed one over the same repo — mirroring
/// the real `zeph worktree clean` flow, which always runs in a brand-new
/// process whose in-memory handle list starts empty. Using the same
/// instance for both `create` and `reconcile` would make the handle
/// self-tracked and therefore invisible to `reconcile` (which only reports
/// worktrees *not* already in the current session's list).
#[tokio::test]
async fn prune_clears_manually_deleted_worktree_administrative_entry() {
    let repo = init_repo();
    let repo_root = repo.path().canonicalize().unwrap();

    let creator = WorktreeManager::new(repo_root.clone(), config(), DefaultGitRunner::new())
        .await
        .unwrap();
    let handle = creator
        .create("clean-test-agent")
        .await
        .expect("create worktree");
    assert!(handle.path.exists(), "worktree dir must exist after create");

    // Simulate external deletion (crash, manual `rm -rf`) instead of `mgr.remove()`.
    std::fs::remove_dir_all(&handle.path).unwrap();

    let list_before = git(&["worktree", "list", "--porcelain"], &repo_root);
    let before_str = String::from_utf8_lossy(&list_before.stdout);
    assert!(
        before_str.contains(handle.path.to_string_lossy().as_ref()),
        "git registry should still reference the deleted worktree before cleanup: {before_str}"
    );

    // Fresh manager instance — empty in-memory handle list, as in a real
    // `zeph worktree clean` invocation (new process per CLI call).
    let cleaner = WorktreeManager::new(repo_root.clone(), config(), DefaultGitRunner::new())
        .await
        .unwrap();

    let stale = cleaner.reconcile().await.expect("reconcile");
    assert_eq!(
        stale.len(),
        1,
        "expected exactly the deleted worktree to be discovered as stale"
    );
    assert_eq!(stale[0].handle.path, handle.path);
    assert!(
        stale[0].is_safe_to_force_remove(),
        "a worktree whose directory was deleted directly must be reported as \
         prunable by git, and therefore safe to force-remove"
    );

    // Deliberately skip `remove()` — see doc comment above.
    cleaner.prune().await.expect("prune");

    let list_after = git(&["worktree", "list", "--porcelain"], &repo_root);
    let after_str = String::from_utf8_lossy(&list_after.stdout);
    assert!(
        !after_str.contains(handle.path.to_string_lossy().as_ref()),
        "git registry must no longer reference the deleted worktree after prune: {after_str}"
    );
}

/// End-to-end regression test for #5937 covering the exact sequence
/// `WorktreeCommand::Clean` runs in `src/commands/worktree.rs`: `reconcile()`
/// -> `remove()` each stale entry -> `prune()`. Confirms the full pipeline
/// leaves `git worktree list` with no trace of the manually-deleted worktree.
///
/// As in `prune_clears_manually_deleted_worktree_administrative_entry`, the
/// worktree is created by one manager instance and cleaned by a second,
/// freshly-constructed one — matching the real `zeph worktree clean`
/// process boundary — so `reconcile()`'s stale list is non-empty and the
/// `remove()` step is actually exercised (not skipped as a no-op over an
/// empty list).
#[tokio::test]
async fn clean_pipeline_end_to_end_clears_manually_deleted_worktree() {
    let repo = init_repo();
    let repo_root = repo.path().canonicalize().unwrap();

    let creator = WorktreeManager::new(repo_root.clone(), config(), DefaultGitRunner::new())
        .await
        .unwrap();
    let handle = creator
        .create("clean-pipeline-agent")
        .await
        .expect("create worktree");
    std::fs::remove_dir_all(&handle.path).unwrap();

    let cleaner = WorktreeManager::new(repo_root.clone(), config(), DefaultGitRunner::new())
        .await
        .unwrap();

    let stale = cleaner.reconcile().await.expect("reconcile");
    assert_eq!(
        stale.len(),
        1,
        "expected exactly the deleted worktree to be discovered as stale"
    );
    for s in &stale {
        assert!(
            s.is_safe_to_force_remove(),
            "deleted worktree must be prunable"
        );
        if let Err(e) = cleaner.remove(&s.handle, false).await {
            // Mirrors `handle_worktree_command`'s non-fatal per-entry handling.
            eprintln!("warning: failed to remove {}: {e}", s.handle.path.display());
        }
    }
    cleaner.prune().await.expect("prune");

    let list_after = git(&["worktree", "list", "--porcelain"], &repo_root);
    let after_str = String::from_utf8_lossy(&list_after.stdout);
    assert!(
        !after_str.contains(handle.path.to_string_lossy().as_ref()),
        "git registry must no longer reference the deleted worktree after clean: {after_str}"
    );
}

/// End-to-end regression test for #5939: `git_timeout_secs = 0` from config,
/// constructed exactly as `handle_worktree_command`
/// (`src/commands/worktree.rs`) and the bootstrap call site (`src/runner.rs`)
/// build it — `DefaultGitRunner::with_timeout(Duration::from_secs(0))` — must
/// still allow a real `git` invocation to complete rather than racing an
/// instantly-expiring `tokio::time::timeout`.
#[tokio::test]
async fn zero_configured_timeout_still_allows_real_git_command_to_complete() {
    let repo = init_repo();
    let repo_root = repo.path().canonicalize().unwrap();

    let git_timeout_secs: u64 = 0;
    let runner = DefaultGitRunner::with_timeout(Duration::from_secs(git_timeout_secs));

    let mgr = WorktreeManager::new(repo_root.clone(), config(), runner)
        .await
        .unwrap();

    // Real process spawn racing the real `tokio::time::timeout`. Without the
    // internal `.max(MIN_TIMEOUT)` clamp this times out immediately.
    let result = mgr.reconcile().await;
    assert!(
        result.is_ok(),
        "git_timeout_secs = 0 must not cause every git call to time out: {result:?}"
    );
}

/// Regression test for the #5936 review finding: `DETACHED_BRANCH_SENTINEL`
/// must be an invalid git ref name, otherwise a real branch on a worktree
/// foreign to zeph could collide with it (see the constant's doc comment in
/// `crates/zeph-worktree/src/handle.rs`). This shells out to the real `git
/// check-ref-format` command — the authoritative ref-name validator — rather
/// than re-implementing its rules, so the invariant cannot silently regress
/// if someone changes the constant without re-checking this property.
#[test]
fn detached_branch_sentinel_is_not_a_valid_git_ref_name() {
    let status = Command::new("git")
        .args(["check-ref-format", "--branch", DETACHED_BRANCH_SENTINEL])
        .status()
        .expect("git must be on PATH for this integration test");
    assert!(
        !status.success(),
        "DETACHED_BRANCH_SENTINEL ({DETACHED_BRANCH_SENTINEL:?}) must be REJECTED by \
         `git check-ref-format --branch` — otherwise a real branch could be given this exact \
         name and collide with the sentinel (see #5936 review finding)"
    );
}

/// Regression test for #6077 (tester gap A): `WorktreeManager::remove()`'s
/// single `git worktree remove --force` is a *second* safety layer, alongside
/// the `prunable`-gating callers perform before ever calling `remove` (#6055)
/// — git itself refuses to remove a `git worktree lock`-ed worktree unless
/// given a second `--force` (`remove -f -f`), which `remove()` deliberately
/// never passes (see its doc comment). Before this test, that refusal was
/// only confirmed manually against real git 2.50.1; if `remove()` were ever
/// changed to pass `-f -f`, this protection would silently regress with
/// nothing catching it.
#[tokio::test]
async fn remove_refuses_locked_worktree_with_single_force() {
    let repo = init_repo();
    let repo_root = repo.path().canonicalize().unwrap();

    let mgr = WorktreeManager::new(repo_root.clone(), config(), DefaultGitRunner::new())
        .await
        .unwrap();
    let handle = mgr.create("locked-agent").await.expect("create worktree");

    let lock_out = git(
        &["worktree", "lock", "--", &handle.path.to_string_lossy()],
        &repo_root,
    );
    assert!(
        lock_out.status.success(),
        "git worktree lock must succeed: {lock_out:?}"
    );

    let err = mgr.remove(&handle, false).await.unwrap_err();
    assert!(
        matches!(err, WorktreeError::GitCommand { ref op, .. } if op == "worktree remove"),
        "expected a `worktree remove` GitCommand failure, got: {err:?}"
    );
    assert!(
        handle.path.exists(),
        "a locked worktree's directory must survive `remove()`'s single --force"
    );

    // Unlock so the underlying tempdir can be cleaned up without a dangling
    // lock file confusing later `git worktree` invocations against it.
    git(
        &["worktree", "unlock", "--", &handle.path.to_string_lossy()],
        &repo_root,
    );
}

/// End-to-end regression test for #6055: `zeph worktree clean` must not
/// force-remove a worktree whose directory is intact and not reported as
/// `prunable` by git — such a worktree may belong to another, concurrently
/// running zeph session. Mirrors the issue's own repro: one `WorktreeManager`
/// creates a worktree and leaves it with uncommitted changes (simulating a
/// live session); the `clean` gating logic then runs via a second, freshly
/// constructed manager — matching the real cross-process `zeph worktree
/// clean` invocation, whose in-memory `handles` always starts empty.
#[tokio::test]
async fn clean_without_force_preserves_intact_worktree_with_uncommitted_changes() {
    let repo = init_repo();
    let repo_root = repo.path().canonicalize().unwrap();

    let live_session = WorktreeManager::new(repo_root.clone(), config(), DefaultGitRunner::new())
        .await
        .unwrap();
    let handle = live_session
        .create("live-session-agent")
        .await
        .expect("create worktree");

    // Simulate uncommitted work belonging to the "other" live session.
    let dirty_file = handle.path.join("dirty.txt");
    std::fs::write(&dirty_file, "uncommitted work\n").unwrap();

    let cleaner = WorktreeManager::new(repo_root.clone(), config(), DefaultGitRunner::new())
        .await
        .unwrap();

    let stale = cleaner.reconcile().await.expect("reconcile");
    assert_eq!(
        stale.len(),
        1,
        "the live session's worktree is untracked by this process's handles"
    );
    assert!(
        !stale[0].is_safe_to_force_remove(),
        "an intact, non-prunable worktree must never be force-removable by default"
    );

    // Mirror `handle_worktree_command`'s `Clean` handler: skip unless the
    // operator passed `--force` or git itself reports the entry as prunable.
    let force = false;
    for s in &stale {
        if !force && !s.is_safe_to_force_remove() {
            continue;
        }
        cleaner.remove(&s.handle, false).await.expect("remove");
    }

    assert!(
        handle.path.exists(),
        "worktree directory must survive a non-force clean"
    );
    assert!(
        dirty_file.exists(),
        "uncommitted work must survive a non-force clean"
    );

    // Re-run with the operator's explicit `--force` override.
    let stale = cleaner.reconcile().await.expect("reconcile");
    assert_eq!(stale.len(), 1);
    let force = true;
    for s in &stale {
        if !force && !s.is_safe_to_force_remove() {
            continue;
        }
        cleaner.remove(&s.handle, false).await.expect("remove");
    }

    assert!(
        !handle.path.exists(),
        "worktree directory must be removed once the operator passes --force"
    );
}
