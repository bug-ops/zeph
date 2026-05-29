---
aliases:
  - Worktree Subsystem NFR
  - NFR-063
tags:
  - sdd
  - nfr
  - worktree
created: 2026-05-29
status: approved
related:
  - "[[specs/063-worktree-subsystem/brd]]"
  - "[[specs/063-worktree-subsystem/srs]]"
  - "[[specs/063-worktree-subsystem/spec]]"
---

# NFR-063: Worktree Subsystem Non-Functional Requirements

ISO/IEC 25010:2011 — Software Product Quality

## 1. Performance Efficiency (ISO 25010 §4.1)

**NFR-PERF-01 — Worktree creation latency (head mode):**  
`WorktreeManager::create` with `base_ref = head` SHALL complete within **2 seconds** on a local SSD
under normal conditions (no network I/O). `git worktree add` is the dominant operation.

**NFR-PERF-02 — Worktree creation latency (fresh mode):**  
`WorktreeManager::create` with `base_ref = fresh` SHOULD complete within **30 seconds** when network
connectivity is available and the repo has a recent fetch cache. The await-discipline timeout SHALL
abort the `git fetch` and fail fast if this bound is exceeded.

**NFR-PERF-03 — Worktree removal latency:**  
`WorktreeManager::remove` SHALL complete within **1 second** on a local SSD.

**NFR-PERF-04 — Zero overhead when disabled:**  
When `worktree.enabled = false`, the worktree subsystem SHALL contribute zero runtime overhead to the
agent spawn path. No git invocations, no mutex contention, no cwd manipulation.

## 2. Reliability (ISO 25010 §4.2)

**NFR-REL-01 — Cwd restore on panic:**  
The `CwdGuard` RAII guard SHALL restore the previous cwd even if the worktree agent task panics.
The process cwd MUST NOT be left pointing to a removed or foreign directory after any code path.

**NFR-REL-02 — Cleanup on cancellation:**  
A cancelled worktree agent SHALL trigger cwd restoration and worktree removal (if `cleanup_on_completion
= true`). The guard SHALL be released before the worktree is removed (see FR-CLEANUP-02/03).

**NFR-REL-03 — Stale worktree detection:**  
After a crash that leaves worktrees on disk, the next startup SHALL detect them via
`WorktreeManager::reconcile()` (reading `git worktree list --porcelain`) and offer removal via
`zeph worktree clean`.

## 3. Security (ISO 25010 §4.3)

**NFR-SEC-01 — Branch name sanitisation:**  
Branch name components derived from subagent IDs SHALL match `^[A-Za-z0-9._-]+$`. Any component not
matching SHALL be rejected with `WorktreeError::InvalidBranchName` before any git call.

**NFR-SEC-02 — Path traversal prevention:**  
`config.root` SHALL be canonicalised at construction. The canonical path MUST be inside the repo root
or an explicitly-allowed sibling. Paths outside SHALL be rejected with `WorktreeError::RootOutsideRepo`.

**NFR-SEC-03 — Git arg hygiene:**  
All `git` invocations that accept user-derived values SHALL use `--` separators. Commitish arguments
SHALL be pre-validated with `git rev-parse --verify` before being passed to `git worktree add`.

**NFR-SEC-04 — `set_working_directory` disabled:**  
Worktree-opted agents SHALL have `set_working_directory` in their effective denylist, enforced through
the existing `FilteredToolExecutor` machinery. This prevents in-session cwd escape.

**NFR-SEC-05 — Sanitised error messages:**  
Raw git stderr SHALL be logged at `tracing::debug` level only. User-facing error messages SHALL be
locale-independent, sanitised strings that do not leak internal paths or credentials.

## 4. Maintainability (ISO 25010 §4.4)

**NFR-MAIN-01 — Testability:**  
`GitRunner` trait SHALL be the sole seam for unit testing path/branch-name logic without a real git
repo. All `WorktreeManager` logic that depends on git output SHALL route through `GitRunner`.

**NFR-MAIN-02 — Crate isolation:**  
`zeph-worktree` SHALL NOT depend on `zeph-core`, `zeph-subagent`, or `zeph-channels`. Dependency
direction is `zeph-subagent → zeph-worktree → (zeph-config, tokio, thiserror, tracing, serde)`.

**NFR-MAIN-03 — TODO markers for deferred work:**  
The `WorktreeManager` doc comment SHALL contain:
```
// TODO(critic D1): concurrent per-agent cwd isolation requires child-process bgIsolation or
// full ToolExecutor cwd-threading; in-process MVP is concurrency-1 only.
```
The `base_ref` resolution logic SHALL contain:
```
// TODO(critic D2): head worktree does not include parent uncommitted changes by design;
// revisit if users need stash-based propagation.
```

**NFR-MAIN-04 — Documentation:**  
All `pub` types, traits, functions, and methods SHALL have `///` doc comments explaining what the item
does and why a caller would use it, per the project documentation policy.

## 5. Portability (ISO 25010 §4.5)

**NFR-PORT-01 — Git on PATH:**  
The subsystem requires `git` ≥ 2.5 on PATH. This is detected at startup (FR-PROBE-01) when enabled.
No other external binaries are required.

**NFR-PORT-02 — macOS, Linux:**  
The subsystem SHALL work on macOS and Linux. Windows is not a supported target for Zeph.

## 6. Usability (ISO 25010 §4.6)

**NFR-USE-01 — Clear startup error:**  
When the capability probe (FR-PROBE-01/02) fails, the error message SHALL name the missing capability,
state the minimum version or configuration required, and suggest a corrective action (e.g. "Install git
≥ 2.5 or set `worktree.enabled = false`").

**NFR-USE-02 — Dirty-tree warning:**  
The runtime warning for uncommitted changes (FR-WT-04) SHALL include the list of changed paths
(truncated at 5 entries with "…and N more" if longer) to help users understand what is excluded.

## 7. Compatibility (ISO 25010 §4.7)

**NFR-COMPAT-01 — Existing configs:**  
Adding the `[worktree]` section to an existing config via `--migrate-config` step 53 SHALL be
idempotent: re-running migration on a config that already has `[worktree]` SHALL be a no-op.

**NFR-COMPAT-02 — Default-off:**  
All existing behaviour when `worktree.enabled = false` SHALL be identical to behaviour before this
feature was introduced (no code paths touched in the hot agent loop).
