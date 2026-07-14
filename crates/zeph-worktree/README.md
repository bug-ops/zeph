# zeph-worktree

[![Crates.io](https://img.shields.io/crates/v/zeph-worktree)](https://crates.io/crates/zeph-worktree)
[![docs.rs](https://img.shields.io/docsrs/zeph-worktree)](https://docs.rs/zeph-worktree)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Git worktree lifecycle management for Zeph subagents.

## Overview

`zeph-worktree` creates, removes, lists, and reconciles per-subagent git worktrees. Each background subagent that opts into filesystem isolation gets a dedicated worktree cloned from the host repository, preventing concurrent agents from clobbering each other's working trees.

The crate is intentionally narrow in scope — it wraps `git worktree` subprocess calls with full path sanitization, capability probing, and a configurable timeout. It has no dependency on `zeph-core`, `zeph-subagent`, or `zeph-channels`.

## Key types

| Type | Description |
|------|-------------|
| `DefaultWorktreeManager` | Production `WorktreeManager<DefaultGitRunner>` — the type stored by `SubAgentManager` |
| `WorktreeManager<R>` | Generic manager parameterised over `GitRunner`; injectable for testing |
| `WorktreeHandle` | Live record of one managed worktree (path, branch, subagent ID, creation time) |
| `DefaultGitRunner` | Production git invocation backend with configurable timeout |
| `GitRunner` | Trait for abstracting git subprocess calls |
| `WorktreeError` | All errors this crate can produce |

## Usage

```toml
[dependencies]
zeph-worktree = { path = "crates/zeph-worktree" }
```

```rust
use std::path::PathBuf;
use zeph_config::WorktreeConfig;
use zeph_worktree::{DefaultWorktreeManager, git_runner::DefaultGitRunner, manager::probe_capabilities};

#[tokio::main]
async fn main() -> Result<(), zeph_worktree::WorktreeError> {
    let repo = PathBuf::from("/path/to/repo");
    let runner = DefaultGitRunner::new();

    // Verify git ≥ 2.5 is available and the path is a repository.
    probe_capabilities(&runner, &repo).await?;

    let mgr = DefaultWorktreeManager::new(repo, WorktreeConfig::default(), runner).await?;

    // Create a worktree for a subagent.
    let handle = mgr.create("agent-42").await?;
    println!("Worktree at {:?}", handle.path);

    // List all tracked worktrees.
    let all = mgr.list();
    println!("{} active worktrees", all.len());

    // Remove the worktree (force = false).
    mgr.remove(&handle, false).await?;

    Ok(())
}
```

> [!IMPORTANT]
> Call `probe_capabilities` once at bootstrap. It checks that `git` ≥ 2.5 is in `PATH` and that the target path is a git repository. A missing git binary is caught here, not at first spawn.

## Configuration

`WorktreeManager` is driven by `WorktreeConfig` from `zeph-config`:

```toml
[worktree]
enabled = true
bg_isolation = "worktree"   # "none" | "worktree"
base_ref = "head"           # "head" | "fresh"
git_timeout_secs = 30       # clamped to max(1, value)
cleanup_on_completion = true
# max_worktrees = 20        # optional admission cap; None = unlimited
# disk_quota_mb = 5000      # optional soft disk-usage threshold; None = disabled
auto_reconcile_secs = 0     # periodic reconcile-and-quota sweep; 0 disables it
reconcile_on_startup = true
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable worktree isolation for background subagents |
| `bg_isolation` | `"none"` | `"worktree"` creates a dedicated worktree; `"none"` only holds the CWD lock |
| `base_ref` | `"head"` | `"head"` branches off current HEAD; `"fresh"` fetches and branches off `origin/<default>` |
| `git_timeout_secs` | `30` | Per-command timeout for all git subprocess calls |
| `cleanup_on_completion` | `true` | Remove the worktree when the subagent finishes |
| `max_worktrees` | `None` (unlimited) | Creation-time admission cap on concurrent git-registered worktrees under `root`. Counts worktrees from other concurrently running Zeph sessions over the same `root`, not just this session's own. `Some(0)` is rejected at config-validation time |
| `disk_quota_mb` | `None` (disabled) | Soft total-disk-usage threshold (sum of logical file sizes) across all worktrees under `root`. When exceeded, the reconcile sweep auto-reclaims only git-`prunable` entries — an intact worktree is never force-removed to satisfy this threshold |
| `auto_reconcile_secs` | `0` (disabled) | Interval for the supervised background reconcile-and-quota sweep. `Config::validate` rejects values in `1..60` |
| `reconcile_on_startup` | `true` | Run one reconcile-and-quota sweep at bootstrap, recovering from crash-left `prunable` worktrees without waiting for the first periodic tick |

## Invariants

- Path sanitization rejects absolute paths, `..` components, and names starting with `-` before any git call.
- `base_ref = "fresh"` never silently falls back to HEAD on fetch failure — it returns an error.
- `git_timeout_secs = 0` is clamped to `1` by `DefaultGitRunner`.
- `create()` admission (quota check + worktree add) closes the check-then-act race under concurrent callers within the same process.
- Lowering `max_worktrees` below the current worktree count does not evict existing worktrees; it only blocks new admissions until `zeph worktree clean` runs or the limit is raised.

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
