---
aliases:
  - Worktree Subsystem Spec
  - spec-063
tags:
  - sdd
  - spec
  - worktree
created: 2026-05-29
status: approved
related:
  - "[[specs/063-worktree-subsystem/brd]]"
  - "[[specs/063-worktree-subsystem/srs]]"
  - "[[specs/063-worktree-subsystem/nfr]]"
  - "[[specs/063-worktree-subsystem/plan]]"
  - "[[specs/parity-claude-code-3918/spec]]"
  - "[[specs/045-subagent-lifecycle/spec]]"
  - "[[constitution]]"
---

# Spec-063: Worktree Subsystem + `worktree.base_ref`

**GitHub:** #4655  
**Branch:** `feat/4655-worktree-baseref-config`  
**Crate:** `zeph-worktree` (new), `zeph-config` (extends), `zeph-subagent` (extends)  
**Parent parity spec:** `[[specs/parity-claude-code-3918/spec]]` rows 40–41 (deferred)

---

## Summary

This spec covers the `zeph-worktree` crate and the `worktree.base_ref: fresh | head` config knob.
Subagents that opt in via `permissions.worktree = true` receive an isolated git worktree for their run.
The MVP uses **concurrency-1 serialisation** (CwdGuard mutex) because in-process agents share the
process cwd; true concurrent isolation is deferred to the `bgIsolation` child-process work (#4656).

---

## Key Invariants

These invariants are hard constraints. Violating them silently re-introduces the race conditions that
the CwdGuard design is built to prevent.

**INV-1 — CwdGuard is run-scoped, not spawn-scoped.**  
When `worktree.enabled = true` and an agent has `permissions.worktree = true`, a single process-global
`tokio::sync::Mutex` (`CwdGuard`) MUST be acquired before `set_current_dir(worktree_path)` and MUST
remain held for the agent's ENTIRE multi-turn run, including all interleaved async tool turns, until
`set_current_dir(prev_cwd)` has completed and cleanup is done. A "spawn-admission" lock that is
released once the task is launched is NOT sufficient: the agent executes many tool turns after spawn,
and any other agent (worktree or plain) executing a tool during a gap would read/cross the mutated
process cwd.

All other agents — both worktree-opted and plain non-worktree agents — are quiesced (block on the mutex)
while a worktree agent holds the guard. They share the same process cwd that the worktree agent is
mutating.

**INV-2 — cwd save/restore with restore-before-remove ordering.**  
Before entering the worktree: `prev = std::env::current_dir()`, then `set_current_dir(worktree_path)`.
On exit — success, error, OR cancellation — `set_current_dir(prev)` MUST run in a guaranteed-run path
(RAII `Drop` or equivalent). The restore MUST occur BEFORE `git worktree remove`. Reversing the
order would `set_current_dir` into an already-deleted path.

Teardown sequence (normative):
1. `set_current_dir(prev_cwd)`
2. Release `CwdGuard`
3. `git worktree remove {path}` (if `cleanup_on_completion`)

**INV-3 — `set_working_directory` is disallowed for worktree agents.**  
For every agent with `permissions.worktree = true`, the tool `set_working_directory` MUST appear in
`disallowed_tools` when the filtered executor is built at `build_filtered_executor`. This is enforced
through the existing `FilteredToolExecutor::with_disallowed` machinery, not a new mechanism.
Rationale: `set_working_directory` calls process-global `std::env::set_current_dir` (`cwd.rs:54`);
allowing it defeats INV-2 and lets the agent escape its worktree mid-run.

**INV-4 — No silent shared-cwd fallback.**  
If worktree creation fails, the agent spawn MUST fail with a `SubAgentError` wrapping the
`WorktreeError`. It MUST NOT silently proceed with the parent's cwd. Opting into `worktree: true`
means the worktree is required. Best-effort fallback is forbidden because it would silently run the
agent in the wrong repository tree.

---

## NEVER

- **NEVER** hold `CwdGuard` for less than the full run scope (spawn to teardown, including all tool turns).
- **NEVER** remove the worktree directory before restoring the previous cwd.
- **NEVER** pass an unvalidated branch component or un-canonicalised `root` path to any `git` invocation.
- **NEVER** pass a subagent-id derived value as the first argument position in a `git` call without using `--` separator; git interprets leading `-` as flags.
- **NEVER** expose raw git stderr to the user; log it at `debug`, return a sanitised message.
- **NEVER** allow `base_ref = fresh` to silently fall back to HEAD when a fetch fails; fail with a clear error.
- **NEVER** add the `set_working_directory` tool to the allowed list for a worktree-opted agent, even if the caller explicitly requests it.
- **NEVER** skip the capability probe when `worktree.enabled = true`; a missing `git` must be caught at bootstrap, not at first spawn.

---

## Config Schema

```toml
[worktree]
enabled = false                    # opt-in; false = current behaviour, no worktrees created
base_ref = "head"                  # "fresh" | "head" — base commit for the worktree branch
default_branch = "main"            # used when base_ref = "fresh"; empty = auto-detect origin/HEAD
root = ".claude/worktrees"         # relative to repo root; canonicalised at bootstrap
branch_prefix = "agent/"           # branch = "{prefix}{subagent_id}"
prune_branch_on_remove = false     # delete the branch after removing the worktree
cleanup_on_completion = true       # remove worktree when agent completes or is cancelled
```

Per-agent opt-in in subagent definition frontmatter:
```yaml
permissions:
  worktree: true
```

---

## Architecture

### New Crate: `crates/zeph-worktree`

Dependency direction: `zeph-subagent → zeph-worktree → (zeph-config, tokio, thiserror, tracing, serde)`

`zeph-worktree` MUST NOT depend on `zeph-core`, `zeph-subagent`, or `zeph-channels`.

```
WorktreeManager          // owns base repo path + config; create/remove/list/reconcile
WorktreeHandle           // path, branch_name, base_ref_resolved, subagent_id, created_at (UTC)
WorktreeError            // thiserror enum — see variants below
GitRunner (trait)        // abstraction for git invocations (testability seam)
DefaultGitRunner         // impl: tokio::process::Command, timeout applied inside run()
```

### `WorktreeManager` API

```rust
impl WorktreeManager {
    pub fn new(repo_root: PathBuf, config: WorktreeConfig) -> Result<Self, WorktreeError>;
    pub async fn create(&self, subagent_id: &str) -> Result<WorktreeHandle, WorktreeError>;
    pub async fn remove(&self, handle: &WorktreeHandle, prune_branch: bool) -> Result<(), WorktreeError>;
    pub fn list(&self) -> &[WorktreeHandle];           // live-session in-RAM only
    pub async fn reconcile(&self) -> Result<Vec<WorktreeHandle>, WorktreeError>; // reads git registry
}
```

### `WorktreeError` Variants

```rust
pub enum WorktreeError {
    NotAGitRepo,
    GitCommand { op: String, stderr: String },   // stderr = debug-only; user sees sanitised message
    PathExists(PathBuf),
    BaseRefUnresolved { attempted: String },
    InvalidBranchName(String),
    RootOutsideRepo(PathBuf),
    Io(#[from] std::io::Error),
}
```

### `GitRunner` Trait

```rust
#[async_trait]
pub trait GitRunner: Send + Sync {
    async fn run(&self, args: &[&str], cwd: &Path) -> Result<std::process::Output, WorktreeError>;
}
```

`cwd` is an explicit parameter — this is the one place in the call stack where cwd threading is correct
by construction. The `DefaultGitRunner` implementation applies the await-discipline external-call timeout
*inside* `run`; no call site is responsible for applying it.

---

## `base_ref` → Git Operations

### `base_ref = head`

```
git worktree add -b {branch} -- {path} HEAD
```

Emits a `tracing::warn!` when `git status --porcelain` is non-empty (see FR-WT-04).

### `base_ref = fresh`

```
git fetch origin {default_branch}         # single attempt; fail fast on timeout
git rev-parse --verify origin/{default}   # validate commitish
git worktree add -b {branch} -- {path} origin/{default_branch}
```

`default_branch` resolution order:
1. `config.default_branch` (if non-empty)
2. `git symbolic-ref refs/remotes/origin/HEAD` → strip `refs/remotes/origin/`
3. Fail with `WorktreeError::BaseRefUnresolved`

### Default-Branch Auto-Detection

`git symbolic-ref refs/remotes/origin/HEAD` is commonly unset in CI or shallow clones. When this
command fails and `config.default_branch` is not set, the spawn fails immediately with a clear message:
> "Cannot resolve default branch: `origin/HEAD` is not set and `worktree.default_branch` is not
> configured. Set `default_branch` in the `[worktree]` config section."

---

## CwdGuard: Process-Level Serialisation

The `CwdGuard` is a `tokio::sync::Mutex<()>` held at process scope (one instance per
`WorktreeManager`). The guard lifecycle is:

```
acquire CwdGuard
  └─ prev = std::env::current_dir()
  └─ set_current_dir(worktree_path)
  └─ run agent (all tool turns, all LLM turns)
  └─ [on success / error / cancel]
     └─ RAII Drop: set_current_dir(prev)
     └─ release CwdGuard
  └─ git worktree remove {path}   ← AFTER guard released
```

The RAII guard (`CwdRestoreGuard`) captures `prev` at construction and calls `set_current_dir(prev)`
in its `Drop` impl. This ensures restore happens on panic and cancellation, not just normal completion.

### Acceptance Test (M4 — concurrency serialisation)

A test MUST verify: when a worktree agent is active (holds `CwdGuard`), a second agent's tool turn is
blocked until the first agent's run completes and the guard is released. Specifically:

> Spawn a worktree agent. While it is running (before it releases the guard), attempt to execute a
> tool call from a plain subagent. Assert that the plain agent's tool call does not begin until the
> worktree agent has completed its run and the guard is released.

This test ensures INV-1 holds for both worktree-opted and plain agents.

---

## Startup Capability Probe

Runs at bootstrap when `worktree.enabled = true` (before any spawn):

1. `git --version` → parse major.minor; fail if < 2.5 with: "git ≥ 2.5 is required for worktree support (found: {version}). Upgrade git or set `worktree.enabled = false`."
2. `git rev-parse --is-inside-work-tree` from `repo_root` → fail if returns non-zero: "Zeph's working directory is not inside a git repository. Worktree support requires a git repo. Set `worktree.enabled = false` to disable."

Neither probe runs when `worktree.enabled = false`.

---

## Branch and Path Sanitisation

**Branch component validation:**  
The `{subagent_id}` component of the branch name MUST match `^[A-Za-z0-9._-]+$`.  
It MUST NOT begin with `-` or `.`.  
It MUST NOT contain `..` or `/`.  
Violation: `WorktreeError::InvalidBranchName(id)`.

**Root path canonicalisation:**  
`config.root` is canonicalised via `std::fs::canonicalize` or equivalent at `WorktreeManager::new`.  
The canonical path MUST be a subdirectory of `repo_root` or a sibling of `repo_root`.  
Paths that resolve outside: `WorktreeError::RootOutsideRepo(canonical)`.

**git arg hygiene:**  
All git invocations that accept branch names or paths use `--` separator:  
```
git worktree add -b {branch} -- {path} {commitish}
```
Commitish is pre-validated with `git rev-parse --verify {commitish}` before use.

---

## Integration Points

### `zeph-config`

New file `crates/zeph-config/src/worktree.rs`:
- `WorktreeConfig` struct with `#[serde(default)]` on all fields
- `WorktreeBaseRef` enum: `#[non_exhaustive]`, `#[default] Head`, serde `rename_all = "snake_case"`
- Add `pub worktree: WorktreeConfig` to root config struct (alongside `agents: SubAgentConfig`)

### `zeph-subagent`

- `SubAgentPermissions` gains `pub worktree: bool` (default `false`)
- `SubAgentManager` gains `Option<Arc<WorktreeManager>>` (set at bootstrap when enabled)
- `spawn` path: if `worktree_manager.is_some()` AND `def.permissions.worktree` → acquire `CwdGuard`, create worktree, set cwd, build system prompt using worktree path, run agent, restore on teardown
- `build_filtered_executor`: when `def.permissions.worktree`, append `"set_working_directory"` to `disallowed_tools`

### Binary Bootstrap

- Construct `WorktreeManager` during agent setup from `config.worktree` + detected repo root
- Run capability probe when `worktree.enabled = true`
- Inject `WorktreeManager` into `SubAgentManager`

### CLI

- Add `WorktreeCommand { List, Clean }` under `zeph worktree` (or extend `zeph agents`)
- `--worktree-base-ref <fresh|head>` session override flag

### TUI

- Command palette: `/worktree list`, `/worktree clean`
- Status spinner during worktree create/remove: "Creating worktree for {agent_id}…"

---

## Error Classification

| Scenario | `WorktreeError` variant | User-facing message |
|---|---|---|
| Not in a git repo | `NotAGitRepo` | "Not inside a git repository" |
| `git fetch` fails | `GitCommand { op: "fetch", .. }` | "Failed to fetch origin/{branch}: {sanitised reason}" |
| `origin/HEAD` unset, no `default_branch` | `BaseRefUnresolved` | "Cannot resolve default branch; set `worktree.default_branch`" |
| Branch name invalid | `InvalidBranchName` | "Subagent ID contains invalid characters for a branch name: {id}" |
| Root outside repo | `RootOutsideRepo` | "Worktree root resolves outside the repository: {path}" |
| Worktree path exists | `PathExists` | "Worktree path already exists: {path}" |
| `git` not on PATH or too old | Detected in probe | "git ≥ 2.5 is required; found: {version}" |

---

## Implementation Notes (Deferred Items)

**TODO(critic D1): concurrent per-agent cwd isolation requires child-process bgIsolation or full
ToolExecutor cwd-threading; in-process MVP is concurrency-1 only.**

Place this TODO on `WorktreeManager` struct doc comment. It marks the entry point for the deferred
`bgIsolation` work (#4656) and the `ToolExecutor` cwd-threading track.

**TODO(critic D2): head worktree does not include parent uncommitted changes by design; revisit if
users need stash-based propagation.**

Place this TODO near the `base_ref = Head` resolution in `WorktreeManager::create`.

---

## Live-Testing Playbook

See `.local/testing/playbooks/worktree.md` for concrete scenarios:
- `enabled = false` (default): no worktrees created
- `base_ref = head` + clean working tree
- `base_ref = head` + dirty working tree (expect dirty-tree warning)
- `base_ref = fresh` + valid origin
- `base_ref = fresh` + no network (expect clear error)
- Cancellation mid-run (expect cwd restored, worktree removed)
- Crash simulation (worktree clean reconciles stale entries)
- Concurrent plain agent during active worktree agent (expect serialisation)
