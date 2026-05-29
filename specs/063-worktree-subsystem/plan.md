---
aliases:
  - Worktree Subsystem Implementation Plan
  - plan-063
tags:
  - sdd
  - plan
  - worktree
created: 2026-05-29
status: approved
related:
  - "[[specs/063-worktree-subsystem/spec]]"
  - "[[specs/063-worktree-subsystem/tasks]]"
---

# Implementation Plan — Worktree Subsystem (#4655)

## Phase Overview

| Phase | Description | Crates Touched | LOC Estimate |
|---|---|---|---|
| P1 | Config types | `zeph-config` | ~80 |
| P2 | `zeph-worktree` crate (core logic) | `zeph-worktree` (new) | ~500 |
| P3 | Integration: subagent manager + CwdGuard | `zeph-subagent` | ~200 |
| P4 | Bootstrap, CLI, TUI, init/migrate | binary, `zeph-tui`, `zeph-config` | ~150 |
| P5 | Playbook, coverage-status, docs | `.local/testing/`, `docs/` | non-code |

Total estimated code: ~930 LOC (new + changed). These are net-new lines, not file sizes.

---

## Phase 1: Config Types (`zeph-config`)

**Goal:** Define `WorktreeConfig` and `WorktreeBaseRef` so all other phases can depend on them.

### Files

- `crates/zeph-config/src/worktree.rs` — new file
- `crates/zeph-config/src/root.rs` — add `pub worktree: WorktreeConfig` field
- `crates/zeph-config/src/migrate/steps.rs` — add step 53

### Key content

```rust
// worktree.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    pub enabled: bool,
    pub base_ref: WorktreeBaseRef,
    pub default_branch: String,
    pub root: String,
    pub branch_prefix: String,
    pub prune_branch_on_remove: bool,
    pub cleanup_on_completion: bool,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_ref: WorktreeBaseRef::default(),
            default_branch: "main".into(),
            root: ".claude/worktrees".into(),
            branch_prefix: "agent/".into(),
            prune_branch_on_remove: false,
            cleanup_on_completion: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorktreeBaseRef {
    #[default]
    Head,
    Fresh,
}
```

Migration step 53: insert `[worktree]` section with defaults if absent. Must be idempotent.

### Acceptance criteria
- `cargo test -p zeph-config` passes
- `cargo check -p zeph-config --all-features` clean
- `WorktreeConfig::default()` round-trips through `toml::to_string` / `toml::from_str`

---

## Phase 2: `zeph-worktree` Crate

**Goal:** Full crate with `WorktreeManager`, `WorktreeHandle`, `WorktreeError`, `GitRunner`.

### New files

```
crates/zeph-worktree/
  Cargo.toml
  src/
    lib.rs            -- pub re-exports + module declarations
    manager.rs        -- WorktreeManager impl
    handle.rs         -- WorktreeHandle struct
    error.rs          -- WorktreeError (thiserror)
    git_runner.rs     -- GitRunner trait + DefaultGitRunner impl
    sanitize.rs       -- branch_name_valid(), canonicalize_root()
```

### `Cargo.toml` dependencies

```toml
[dependencies]
zeph-config = { path = "../zeph-config" }
tokio = { workspace = true, features = ["process", "fs"] }
thiserror = { workspace = true }
tracing = { workspace = true }
serde = { workspace = true, features = ["derive"] }
async-trait = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true, features = ["test-util"] }
```

### Logical sub-components

**`sanitize.rs`**
- `fn validate_branch_component(s: &str) -> Result<(), WorktreeError>` — regex `^[A-Za-z0-9._-]+$`, no leading `-`/`.`, no `..`
- `fn canonicalize_root(root: &Path, repo_root: &Path) -> Result<PathBuf, WorktreeError>` — canonicalise, assert inside or sibling of repo root

**`git_runner.rs`**
- `GitRunner` trait: single `async fn run(&self, args: &[&str], cwd: &Path) -> Result<Output, WorktreeError>`
- `DefaultGitRunner`: invokes `tokio::process::Command::new("git")` with a configurable timeout (default 30s for fetch, 5s for other ops); non-zero exit → `WorktreeError::GitCommand`
- `FakeGitRunner` (cfg(test)): record args, return pre-configured outputs — used by unit tests

**`manager.rs`** (core logic, ~300 LOC)
- `WorktreeManager::new` — validate repo root + config, run no git calls here (probe is at bootstrap)
- `create` — validate branch component, resolve base_ref, call git ops, emit dirty-tree warning, store handle
- `remove` — `git worktree remove`, optionally `git branch -D`
- `list` — return `&[WorktreeHandle]` (in-RAM only)
- `reconcile` — `git worktree list --porcelain` + scan `config.root` dir; return stale handles

### Test surface

| Test | Mechanism |
|---|---|
| Branch name validation (good/bad inputs) | Unit test, `FakeGitRunner` |
| Root canonicalization (inside/outside/symlink) | Unit test with `tempfile` |
| `create` → git args sent correctly for `head` | `FakeGitRunner`, assert args |
| `create` → git args sent correctly for `fresh` | `FakeGitRunner`, assert args |
| `create` fails → `InvalidBranchName` | `FakeGitRunner` |
| `create` fails → `BaseRefUnresolved` | `FakeGitRunner` (fake `symbolic-ref` fails, `default_branch` empty) |
| `remove` → `git worktree remove` called | `FakeGitRunner` |
| `remove` with `prune_branch` → `git branch -D` called | `FakeGitRunner` |
| `reconcile` → parses `git worktree list --porcelain` output | `FakeGitRunner` with canned output |
| Dirty-tree warning emitted | `FakeGitRunner`, capture tracing output |
| Git `--` separator present in all add calls | Assert arg slice in `FakeGitRunner` captures |

### Acceptance criteria
- `cargo test -p zeph-worktree` passes
- `cargo clippy -p zeph-worktree -- -D warnings` clean
- `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc -p zeph-worktree --no-deps` clean
- All doc-tests pass: `cargo test --doc -p zeph-worktree`

---

## Phase 3: Integration — `zeph-subagent` + CwdGuard

**Goal:** Wire `WorktreeManager` into `SubAgentManager`; implement `CwdGuard` + `CwdRestoreGuard`.

### Files changed

- `crates/zeph-subagent/src/permissions.rs` — add `pub worktree: bool` to `SubAgentPermissions`/`RawPermissions`
- `crates/zeph-subagent/src/manager.rs` — add `worktree_manager: Option<Arc<WorktreeManager>>`, `cwd_guard: Arc<tokio::sync::Mutex<()>>` fields; integrate in `spawn`
- `crates/zeph-subagent/src/cwd_guard.rs` — new: `CwdRestoreGuard` (RAII restore + guard holder)

### `CwdRestoreGuard` (RAII)

```rust
struct CwdRestoreGuard<'a> {
    prev: PathBuf,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

impl Drop for CwdRestoreGuard<'_> {
    fn drop(&mut self) {
        // Attempt restore; log error if it fails (cannot propagate from Drop)
        if let Err(e) = std::env::set_current_dir(&self.prev) {
            tracing::error!(path = ?self.prev, err = ?e, "Failed to restore process cwd");
        }
    }
}
```

### `spawn` integration (pseudocode)

```rust
// In SubAgentManager::spawn, before building system prompt:
if let (Some(wm), true) = (&self.worktree_manager, def.permissions.worktree) {
    // Acquire process-global guard (blocks all other agents)
    let guard = self.cwd_guard.lock().await;
    let handle = wm.create(&task_id).await?;   // INV-4: failure = fatal to this spawn
    let prev = std::env::current_dir()?;
    std::env::set_current_dir(handle.path())?;
    let _cwd_guard = CwdRestoreGuard { prev, _guard: guard };
    // _cwd_guard held for agent's entire run via task-local drop on task completion
    // ... build system prompt using handle.path() as cwd
    // ... run agent
    // _cwd_guard dropped here: restores cwd, releases mutex
    // THEN: wm.remove(&handle, config.prune_branch_on_remove).await
}
```

### `build_filtered_executor` change

```rust
let mut disallowed = def.disallowed_tools.clone();
if def.permissions.worktree {
    disallowed.push(ToolName::from("set_working_directory"));
}
FilteredToolExecutor::with_disallowed(base, def.tools.as_deref(), &disallowed)
```

### Tests

- Concurrency serialisation test (M4 acceptance test):
  Spawn a fake worktree agent that holds `CwdGuard` for a known duration; assert a second agent's tool
  execution is queued and does not proceed until the guard is released.
- Cancellation path: cancel the worktree agent mid-run; assert cwd is restored via `CwdRestoreGuard`
  `Drop` and the guard is released.
- `set_working_directory` absent from executor tool list when `permissions.worktree = true`.

### Acceptance criteria
- Both tests pass
- `cargo test -p zeph-subagent -- worktree` passes
- Integration: `cargo nextest run --workspace --lib --bins` passes

---

## Phase 4: Bootstrap, CLI, TUI, Init/Migrate

**Goal:** Wire everything from Phase 1–3 into the binary entry points.

### Bootstrap (binary)

- In `src/agent_setup.rs` or `src/bootstrap.rs`: when `config.worktree.enabled`, run capability probe,
  construct `WorktreeManager::new(repo_root, config.worktree.clone())`, inject into `SubAgentManager`.

### CLI (`src/cli.rs`)

- Add `Worktree { command: WorktreeCommand }` to top-level enum (or extend `Agents`)
- `WorktreeCommand::List` → prints table from `reconcile()`
- `WorktreeCommand::Clean` → calls `reconcile()`, removes stale entries, prints summary
- Add `--worktree-base-ref` session override to root `Cli` struct

### TUI (`crates/zeph-tui`)

- Add `/worktree list` and `/worktree clean` to command palette
- During `WorktreeManager::create` / `remove`: emit `SystemStatus` with spinner message
  "Creating worktree for {agent_id}…" / "Removing worktree for {agent_id}…"

### `--init` wizard

- Add a "Subagent isolation" section after the existing subagent config questions:
  - "Enable per-subagent git worktrees? [y/N]" → `worktree.enabled`
  - If yes: "Branch from local HEAD or fetch from remote? [head/fresh]" → `worktree.base_ref`
  - If fresh: "Default remote branch?" → `worktree.default_branch`

### `--migrate-config` step 53

- Check if `[worktree]` section exists; if not, append with all defaults.
- Idempotent: skip if already present.

### Acceptance criteria
- `zeph worktree list` and `zeph worktree clean` exit 0 with expected output on a test repo
- `--worktree-base-ref fresh` overrides `config.worktree.base_ref` for the session
- `--migrate-config --in-place` adds `[worktree]` section to a config without one; re-running is a no-op
- TUI shows spinner during worktree create/remove

---

## Phase 5: Documentation and Testing Artifacts

### Files

- `.local/testing/playbooks/worktree.md` — live testing scenarios (mandatory per CLAUDE.md)
- `.local/testing/coverage-status.md` — add rows for `zeph-worktree` and `worktree` integration (status: Untested initially)
- `docs/src/worktree.md` — user-facing docs (mdbook)
- `CHANGELOG.md` — entry under `[Unreleased]`

### Playbook scenarios

See `spec.md` "Live-Testing Playbook" section for the required scenario list.

### CHANGELOG entry template

```markdown
### Added
- `zeph-worktree` crate: git worktree lifecycle management for subagents (#4655)
- `worktree.base_ref` config (`fresh | head`): controls base commit for subagent worktrees
- `worktree.enabled` master switch (default `false`): opt-in, zero impact on existing deployments
- `CwdGuard` process-level serialisation for worktree agents
- `zeph worktree list` / `zeph worktree clean` CLI commands
- Startup capability probe for `git` ≥ 2.5 when worktrees enabled
- Config migration step 53: adds `[worktree]` defaults to existing configs
```
