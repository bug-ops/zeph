// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const TTL: Duration = Duration::from_secs(30);
/// Hard cap on indexed paths to prevent unbounded memory usage on repos with
/// large unignored directories. Applied during the walk itself (before the
/// recency sort), so on a repo past the cap the retained subset — and
/// therefore the set of files eligible for the git-modified/recency boost —
/// is in walk order, not necessarily the most-recently-touched files.
const MAX_INDEXED: usize = 50_000;
/// Wall-clock budget for each `git` subprocess call. A wedged `git status`
/// (hung credential helper, lock contention) would otherwise never return,
/// leaving `App::pending_file_index` `Some` forever and permanently
/// disabling index rebuilds for the rest of the session (see
/// `App::ensure_file_index`, which early-returns while a build is pending).
const GIT_TIMEOUT: Duration = Duration::from_secs(3);

/// A path collected by the walk, paired with its filesystem mtime so `build`
/// can order the empty-query head by recency (#6651) instead of the
/// alphabet.
struct IndexedFile {
    path: String,
    modified: SystemTime,
}

pub struct FileIndex {
    paths: Arc<Vec<String>>,
    built_at: Instant,
}

impl FileIndex {
    /// Builds the file index by walking `root` with `.gitignore` awareness.
    ///
    /// Paths are ordered so the empty-query picker head (#6651) surfaces the
    /// files a user is most likely to `@`-mention: uncommitted changes first
    /// (`git status --porcelain`, most recently modified first), then the
    /// remaining tracked files by mtime descending. A typed query re-ranks
    /// everything by fuzzy score (see `crate::widgets::mention_picker`), so
    /// this ordering only matters for the empty-query case. Outside a git
    /// repository — or if `git` fails for any reason — the modified-set is
    /// simply empty and every file falls back to plain mtime ordering; a
    /// missing mtime (unreadable metadata) sorts as the oldest possible time
    /// rather than erroring, so the picker never blocks or fails on a
    /// filesystem quirk.
    ///
    /// # Blocking I/O note
    ///
    /// This function performs synchronous directory traversal (one extra
    /// `metadata()` stat call per walked file, for its mtime) and, for git
    /// repos, spawns synchronous `git` subprocesses (each bounded by
    /// `GIT_TIMEOUT`) on the calling thread. For small to medium repos
    /// (< 5 000 files) the cost is negligible (< 20 ms). Callers run this via
    /// `spawn_blocking` under the task supervisor (see
    /// `crate::app::keys::App::ensure_file_index`) so it never blocks the
    /// render loop.
    #[must_use]
    pub fn build(root: &Path) -> Self {
        let mut files = Vec::new();
        let walker = ignore::WalkBuilder::new(root)
            .hidden(true) // exclude dotfiles (.env, .ssh/, etc.)
            .ignore(true)
            .git_ignore(true)
            .build();

        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(path);
                if let Some(s) = rel.to_str() {
                    // Normalize Windows backslashes to forward slashes
                    let path = s.replace('\\', "/");
                    let modified = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    files.push(IndexedFile { path, modified });
                }
                if files.len() >= MAX_INDEXED {
                    tracing::warn!(
                        max = MAX_INDEXED,
                        root = %root.display(),
                        "file index cap reached; some files will not be searchable"
                    );
                    break;
                }
            }
        }

        let modified = git_modified_set(root);
        files.sort_by(|a, b| {
            let a_modified = modified.contains(&a.path);
            let b_modified = modified.contains(&b.path);
            b_modified
                .cmp(&a_modified)
                .then_with(|| b.modified.cmp(&a.modified))
                .then_with(|| a.path.cmp(&b.path))
        });

        let paths = files.into_iter().map(|f| f.path).collect();
        Self {
            paths: Arc::new(paths),
            built_at: Instant::now(),
        }
    }

    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.built_at.elapsed() > TTL
    }

    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    #[must_use]
    pub fn paths_arc(&self) -> Arc<Vec<String>> {
        Arc::clone(&self.paths)
    }
}

/// Runs `git -C root <args>`, bounded by `GIT_TIMEOUT`.
///
/// The subprocess is spawned on a helper thread so the blocking `.output()`
/// call (which itself concurrently drains stdout/stderr, avoiding
/// pipe-buffer deadlock) never has to be interrupted mid-flight. The common
/// case — `git` returns well within `GIT_TIMEOUT` — joins the helper thread
/// before returning, so nothing outlives this call (no leaked thread for
/// e.g. nextest's leak detector to flag). Only if `git` is genuinely wedged
/// does this function stop waiting after `GIT_TIMEOUT` and return `None`
/// without joining — the helper thread and the child process are abandoned
/// (not killed), an acceptable one-off leak for that rare pathological case
/// versus permanently stalling every future index rebuild for the session.
fn run_git(root: &Path, args: &[&str]) -> Option<std::process::Output> {
    let root = root.to_path_buf();
    let owned_args: Vec<String> = args.iter().map(|&s| s.to_owned()).collect();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let result = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(&owned_args)
            .stdin(Stdio::null())
            .output();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(GIT_TIMEOUT) {
        Ok(result) => {
            let _ = handle.join();
            result.ok()
        }
        Err(_) => None,
    }
}

/// Returns the path from the repository's top-level directory down to
/// `root` (e.g. `"crates/zeph-tui/"`, or `""` if `root` **is** the
/// top-level), via `git rev-parse --show-prefix`.
///
/// `git status` always reports paths relative to the repo **top-level**, not
/// to the walk root — needed so [`git_modified_set`] can strip that prefix
/// and match against `FileIndex`'s root-relative paths (#6651 fix: without
/// this, launching the TUI from any subdirectory of the repo made the
/// git-modified boost silently never match anything). `None` if `root` is
/// not inside a git repository, `git` is not on `PATH`, or the command
/// otherwise fails/times out.
fn git_repo_prefix(root: &Path) -> Option<String> {
    let output = run_git(root, &["rev-parse", "--show-prefix"])?;
    if !output.status.success() {
        return None;
    }
    let prefix = String::from_utf8(output.stdout).ok()?;
    Some(prefix.trim_end_matches(['\n', '\r']).replace('\\', "/"))
}

/// Returns the set of paths (relative to `root`, forward-slash normalized)
/// with uncommitted changes: staged, unstaged, and untracked-but-not-ignored
/// (`git status --porcelain=v1 -z --untracked-files=all`, with
/// `--no-optional-locks` so this read never contends with the user's own
/// concurrent git operations for the index lock).
///
/// `--untracked-files=all` (not `normal`) is required so a wholly new,
/// unstaged directory is reported as its individual files rather than
/// collapsed into one `newdir/`-style entry that would never match any
/// individual file path.
///
/// Returns an empty set — never an error — if `root` is not inside a git
/// repository, `git` is not on `PATH`, or any command fails or times out
/// (see [`run_git`]); the empty-query ordering then degrades silently to
/// plain mtime ordering (never blocks or errors the picker). `-z`
/// NUL-terminates entries and never quotes paths, so unicode filenames
/// round-trip without needing `core.quotePath` tweaks.
fn git_modified_set(root: &Path) -> HashSet<String> {
    let Some(prefix) = git_repo_prefix(root) else {
        return HashSet::new();
    };
    let Some(output) = run_git(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
    ) else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }

    let mut set = HashSet::new();
    let mut tokens = output.stdout.split(|&b| b == 0).filter(|t| !t.is_empty());
    while let Some(token) = tokens.next() {
        if token.len() < 3 {
            continue;
        }
        let status = &token[..2];
        if let Ok(path) = std::str::from_utf8(&token[3..]) {
            let normalized = path.replace('\\', "/");
            // Paths outside `root`'s subtree (repo-relative but not
            // root-relative) can't match anything `FileIndex` walked; drop
            // them rather than inserting a prefix that will never hit.
            if let Some(rel) = normalized.strip_prefix(&prefix) {
                set.insert(rel.to_owned());
            }
        }
        // Rename/copy entries carry a NUL-separated "from" path immediately
        // after the "to" path (see `git status --help`, `-z` section) — skip
        // it so it isn't misread as its own status line. R/C can appear in
        // either the index (X) or worktree (Y) status column.
        if status.contains(&b'R') || status.contains(&b'C') {
            tokens.next();
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn make_index(files: &[&str]) -> FileIndex {
        let dir = tempfile::tempdir().unwrap();
        for &f in files {
            let path = dir.path().join(f);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, "").unwrap();
        }
        FileIndex::build(dir.path())
    }

    #[test]
    fn build_collects_files() {
        let idx = make_index(&["src/main.rs", "src/lib.rs", "README.md"]);
        assert_eq!(idx.paths().len(), 3);
        assert!(idx.paths().iter().any(|p| p.ends_with("main.rs")));
    }

    #[test]
    fn is_stale_false_when_fresh() {
        let idx = make_index(&["a.rs"]);
        assert!(!idx.is_stale());
    }

    #[test]
    fn unicode_paths_are_indexed_and_searchable() {
        let idx = make_index(&["src/данные.rs", "データ/main.rs", "normal.rs"]);
        assert!(idx.paths().iter().any(|p| p.contains("данные")));
        assert!(idx.paths().iter().any(|p| p.contains("main")));
    }

    #[test]
    fn arc_paths_shared_not_cloned() {
        let idx = make_index(&["a.rs", "b.rs"]);
        let arc1 = idx.paths_arc();
        let arc2 = idx.paths_arc();
        // Both should point to the same allocation
        assert!(Arc::ptr_eq(&arc1, &arc2));
    }

    fn set_mtime(path: &Path, secs_ago: u64) {
        let time = SystemTime::now() - Duration::from_secs(secs_ago);
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    /// Runs a git command for test setup (`init`/`config`/`add`/`commit`),
    /// isolated from the developer machine's global/system git config
    /// (`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` → the null device) so an
    /// ambient `commit.gpgsign = true` or `core.hooksPath` pre-commit hook
    /// can't hang or fail this test. Distinct from the production
    /// [`super::run_git`] (which is read-only and timeout-bounded); this one
    /// asserts success since setup failures should fail the test loudly.
    fn run_git_setup(root: &Path, args: &[&str]) {
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .env("GIT_CONFIG_GLOBAL", null_device)
            .env("GIT_CONFIG_SYSTEM", null_device)
            .args(args)
            .status()
            .expect("git must be on PATH for this test");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn empty_query_orders_by_mtime_desc_without_git() {
        // Outside a git repo the modified-set is empty, so ordering degrades
        // to plain mtime descending (#6651).
        let dir = tempfile::tempdir().unwrap();
        for f in ["old.rs", "mid.rs", "new.rs"] {
            fs::write(dir.path().join(f), "").unwrap();
        }
        set_mtime(&dir.path().join("old.rs"), 300);
        set_mtime(&dir.path().join("mid.rs"), 150);
        set_mtime(&dir.path().join("new.rs"), 10);

        let idx = FileIndex::build(dir.path());
        assert_eq!(idx.paths(), &["new.rs", "mid.rs", "old.rs"]);
    }

    #[test]
    fn git_modified_set_empty_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git_modified_set(dir.path()).is_empty());
    }

    #[test]
    fn empty_query_ranks_git_modified_files_first() {
        // Mtimes are deliberately inverted relative to git-modified status —
        // `committed_recent.rs` (clean) is the most recently touched file of the
        // three, while the two boosted files are mtime-older — so a plain
        // mtime-only sort (i.e. the git-boost silently doing nothing) would rank
        // `committed_recent.rs` *first*, the opposite of what's asserted below.
        // Passing therefore actually exercises the git-modified boost, not just
        // mtime ordering it happens to agree with (testing-round finding on S1).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git_setup(root, &["init", "-q"]);
        run_git_setup(root, &["config", "user.email", "test@example.com"]);
        run_git_setup(root, &["config", "user.name", "Test"]);

        fs::write(root.join("committed_old.rs"), "").unwrap();
        fs::write(root.join("committed_recent.rs"), "").unwrap();
        run_git_setup(root, &["add", "-A"]);
        run_git_setup(root, &["commit", "-q", "-m", "init"]);

        // Modify a committed file and add an untracked one — both should
        // outrank the unmodified `committed_recent.rs` despite being mtime-older.
        fs::write(root.join("committed_old.rs"), "changed").unwrap();
        fs::write(root.join("untracked.rs"), "").unwrap();
        set_mtime(&root.join("committed_recent.rs"), 10);
        set_mtime(&root.join("committed_old.rs"), 500);
        set_mtime(&root.join("untracked.rs"), 800);

        let idx = FileIndex::build(root);
        let paths = idx.paths();
        let pos = |name: &str| paths.iter().position(|p| p == name).unwrap();
        assert!(
            pos("untracked.rs") < pos("committed_recent.rs"),
            "untracked.rs is git-modified (mtime 800s-ago) and must still \
             outrank clean committed_recent.rs (mtime 10s-ago): {paths:?}"
        );
        assert!(
            pos("committed_old.rs") < pos("committed_recent.rs"),
            "committed_old.rs is git-modified (mtime 500s-ago) and must still \
             outrank clean committed_recent.rs (mtime 10s-ago): {paths:?}"
        );
        // Within the modified group, mtime desc still applies (500 is more
        // recent than 800).
        assert!(pos("committed_old.rs") < pos("untracked.rs"));
    }

    #[test]
    fn empty_query_ranks_git_modified_files_first_from_subdirectory_root() {
        // Regression for the S1 finding: `git status` reports paths relative
        // to the repo top-level, but the walk root (and `FileIndex`'s paths)
        // is `std::env::current_dir()`, which for a real TUI session is
        // often a subdirectory of the repo (#6651 fix — see
        // `git_repo_prefix`). Build the index rooted at a subdirectory and
        // confirm the git-modified boost still matches.
        //
        // Mtimes are deliberately inverted relative to git-modified status (see
        // `empty_query_ranks_git_modified_files_first` above for why) so this
        // actually exercises `git_repo_prefix`'s stripping, not just mtime
        // ordering it happens to agree with: if prefix-stripping were broken
        // (paths never match, e.g. reverted to repo-top-level-relative), the
        // modified-set lookup would fail for both files and the sort would fall
        // through to mtime alone — flipping the order asserted below.
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        run_git_setup(repo_root, &["init", "-q"]);
        run_git_setup(repo_root, &["config", "user.email", "test@example.com"]);
        run_git_setup(repo_root, &["config", "user.name", "Test"]);

        let sub = repo_root.join("crates").join("zeph-tui");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("committed_old.rs"), "").unwrap();
        fs::write(sub.join("committed_recent.rs"), "").unwrap();
        run_git_setup(repo_root, &["add", "-A"]);
        run_git_setup(repo_root, &["commit", "-q", "-m", "init"]);

        fs::write(sub.join("committed_old.rs"), "changed").unwrap();
        set_mtime(&sub.join("committed_recent.rs"), 10);
        set_mtime(&sub.join("committed_old.rs"), 500);

        // Index is built rooted at the *subdirectory*, not the repo top-level.
        let idx = FileIndex::build(&sub);
        let paths = idx.paths();
        let pos = |name: &str| paths.iter().position(|p| p == name).unwrap();
        assert!(
            pos("committed_old.rs") < pos("committed_recent.rs"),
            "committed_old.rs is git-modified (mtime 500s-ago) and must still \
             outrank clean committed_recent.rs (mtime 10s-ago) even though the \
             walk root is a repo subdirectory: {paths:?}"
        );
    }

    #[test]
    fn empty_query_boosts_new_files_inside_an_untracked_directory() {
        // Regression for the S2 finding: `--untracked-files=normal` collapses
        // a wholly new directory into one `newdir/`-style entry, which never
        // matches any individual walked file path.
        //
        // Mtimes are deliberately inverted relative to git-modified status (see
        // `empty_query_ranks_git_modified_files_first` above for why): `fresh.rs`
        // is mtime-*older* than the clean `committed.rs`, so a plain mtime-only
        // sort (i.e. `--untracked-files=all` reverted back to `normal`, silently
        // dropping the boost) would rank `committed.rs` first — the opposite of
        // what's asserted below.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git_setup(root, &["init", "-q"]);
        run_git_setup(root, &["config", "user.email", "test@example.com"]);
        run_git_setup(root, &["config", "user.name", "Test"]);

        fs::write(root.join("committed.rs"), "").unwrap();
        run_git_setup(root, &["add", "-A"]);
        run_git_setup(root, &["commit", "-q", "-m", "init"]);
        set_mtime(&root.join("committed.rs"), 10);

        let newdir = root.join("newdir");
        fs::create_dir_all(&newdir).unwrap();
        fs::write(newdir.join("fresh.rs"), "").unwrap();
        set_mtime(&newdir.join("fresh.rs"), 500);

        let idx = FileIndex::build(root);
        let paths = idx.paths();
        let pos = |name: &str| paths.iter().position(|p| p == name).unwrap();
        assert!(
            pos("newdir/fresh.rs") < pos("committed.rs"),
            "fresh.rs is a new file inside a wholly untracked directory (mtime \
             500s-ago) and must still outrank clean committed.rs (mtime \
             10s-ago): {paths:?}"
        );
    }
}
