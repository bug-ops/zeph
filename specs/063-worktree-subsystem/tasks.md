---
aliases:
  - Worktree Subsystem Tasks
  - tasks-063
tags:
  - sdd
  - tasks
  - worktree
created: 2026-05-29
status: approved
related:
  - "[[specs/063-worktree-subsystem/plan]]"
  - "[[specs/063-worktree-subsystem/spec]]"
---

# Task Breakdown — Worktree Subsystem (#4655)

## T-01: Config types in `zeph-config`

**Crate:** `zeph-config`  
**Phase:** P1  
**LOC estimate:** ~80

**Description:**  
Create `crates/zeph-config/src/worktree.rs` with `WorktreeConfig` and `WorktreeBaseRef`.
Add `pub worktree: WorktreeConfig` to root config struct.
Add `SubAgentPermissions.worktree: bool` field (default `false`) in subagent permissions.

**Files:**
- `crates/zeph-config/src/worktree.rs` (new)
- `crates/zeph-config/src/root.rs`
- `crates/zeph-config/src/subagent.rs` (or `permissions.rs`)
- `crates/zeph-config/src/lib.rs` (re-export)

**Acceptance criteria:**
- `WorktreeConfig::default()` → `enabled=false`, `base_ref=Head`, `default_branch="main"`, `root=".claude/worktrees"`, `branch_prefix="agent/"`, `prune_branch_on_remove=false`, `cleanup_on_completion=true`
- `WorktreeBaseRef` is `#[non_exhaustive]` with `#[default] Head`
- `WorktreeConfig` and `WorktreeBaseRef` round-trip through TOML serialization
- `cargo test -p zeph-config` passes
- `cargo clippy -p zeph-config -- -D warnings` clean

---

## T-02: Config migration step 53

**Crate:** `zeph-config`  
**Phase:** P1  
**Depends on:** T-01  
**LOC estimate:** ~20

**Description:**  
Add migration step 53 to `crates/zeph-config/src/migrate/steps.rs` that inserts the `[worktree]`
section with default values into configs that lack it.

**Acceptance criteria:**
- Migration is idempotent: running twice on the same config produces no change on the second run
- `cargo nextest run -p zeph-config -- migrate` passes

---

## T-03: `zeph-worktree` crate scaffold

**Crate:** `zeph-worktree` (new)  
**Phase:** P2  
**Depends on:** T-01  
**LOC estimate:** ~100

**Description:**  
Create crate with `Cargo.toml`, module structure, and all type definitions:
`WorktreeHandle`, `WorktreeError`, `GitRunner` trait, `DefaultGitRunner`, `FakeGitRunner` (cfg(test)).

**Files:**
- `crates/zeph-worktree/Cargo.toml`
- `crates/zeph-worktree/src/lib.rs`
- `crates/zeph-worktree/src/handle.rs`
- `crates/zeph-worktree/src/error.rs`
- `crates/zeph-worktree/src/git_runner.rs`
- Root `Cargo.toml` workspace member

**Acceptance criteria:**
- `cargo build -p zeph-worktree` succeeds
- All `WorktreeError` variants match spec
- `GitRunner` trait has `async fn run(&self, args: &[&str], cwd: &Path) -> Result<Output, WorktreeError>` signature
- `DefaultGitRunner` applies timeout inside `run` — no call site responsible for timeout

---

## T-04: Branch name and path sanitisation

**Crate:** `zeph-worktree`  
**Phase:** P2  
**Depends on:** T-03  
**LOC estimate:** ~60

**Description:**  
Implement `sanitize.rs`:
- `validate_branch_component(s: &str) -> Result<(), WorktreeError>`
- `canonicalize_root(root: &Path, repo_root: &Path) -> Result<PathBuf, WorktreeError>`

**Acceptance criteria:**
- Valid UUIDs (current subagent ID format) pass `validate_branch_component`
- Inputs with `/`, `..`, leading `-`, leading `.` fail
- Inputs containing control characters fail
- `canonicalize_root` accepts paths inside repo root
- `canonicalize_root` rejects paths outside repo root (symlinked escape included)
- Unit tests cover all branches: `cargo test -p zeph-worktree sanitize`

---

## T-05: `WorktreeManager::create` — `head` mode

**Crate:** `zeph-worktree`  
**Phase:** P2  
**Depends on:** T-04  
**LOC estimate:** ~80

**Description:**  
Implement `WorktreeManager::create` for `base_ref = Head`:
- Validate branch component
- Emit dirty-tree warning (`git status --porcelain` non-empty)
- Call `git worktree add -b {branch} -- {path} HEAD`
- Store `WorktreeHandle` in session list
- Trace span: `worktree.create`

**Acceptance criteria:**
- `FakeGitRunner` captures confirm `--` separator present in add args
- `FakeGitRunner` captures confirm commitish is `HEAD`
- Dirty-tree warning emitted when status returns non-empty output
- No warning when status is empty
- `cargo test -p zeph-worktree create_head` passes

---

## T-06: `WorktreeManager::create` — `fresh` mode

**Crate:** `zeph-worktree`  
**Phase:** P2  
**Depends on:** T-04  
**LOC estimate:** ~80

**Description:**  
Implement `WorktreeManager::create` for `base_ref = Fresh`:
- Resolve `default_branch`: `config.default_branch` → `git symbolic-ref refs/remotes/origin/HEAD` → error
- `git fetch origin {default_branch}` (single attempt, timeout inside `run`)
- `git rev-parse --verify origin/{default_branch}`
- `git worktree add -b {branch} -- {path} origin/{default_branch}`
- Trace span: `worktree.fetch`

**Acceptance criteria:**
- `FakeGitRunner` captures confirm fetch args correct
- `FakeGitRunner` captures confirm `rev-parse --verify` called before `worktree add`
- `default_branch` config overrides `symbolic-ref` result
- `symbolic-ref` failure with empty `default_branch` → `WorktreeError::BaseRefUnresolved`
- Fetch failure (non-zero exit) → `WorktreeError::GitCommand { op: "fetch", .. }`
- `cargo test -p zeph-worktree create_fresh` passes

---

## T-07: `WorktreeManager::remove` and `reconcile`

**Crate:** `zeph-worktree`  
**Phase:** P2  
**Depends on:** T-03  
**LOC estimate:** ~80

**Description:**  
Implement `WorktreeManager::remove` and `WorktreeManager::reconcile`.

`remove`:
- `git worktree remove {path}`
- If `prune_branch`: `git branch -D {branch}`
- Remove handle from session list

`reconcile`:
- `git worktree list --porcelain` → parse output
- Scan `config.root` directory
- Return `WorktreeHandle` entries present on disk but not in current session list

**Acceptance criteria:**
- `remove` with `prune_branch=true`: both git commands called, in correct order
- `remove` with `prune_branch=false`: only `git worktree remove` called
- `reconcile` correctly parses `git worktree list --porcelain` canned output in unit test
- Session list no longer contains handle after `remove`
- `cargo test -p zeph-worktree remove reconcile` passes

---

## T-08: Startup capability probe

**Crate:** `zeph-worktree` (or binary)  
**Phase:** P2  
**Depends on:** T-03  
**LOC estimate:** ~40

**Description:**  
Implement `WorktreeManager::probe_capabilities(runner: &dyn GitRunner) -> Result<(), WorktreeError>`:
- `git --version` → parse, fail if < 2.5
- `git rev-parse --is-inside-work-tree` from `repo_root` → fail if not a repo

Called at bootstrap when `worktree.enabled = true`, before any spawn.

**Acceptance criteria:**
- `FakeGitRunner` returning version `2.4.0` → error with message containing "2.5"
- `FakeGitRunner` returning non-zero for `rev-parse` → `WorktreeError::NotAGitRepo`
- Valid git ≥ 2.5 inside a repo → `Ok(())`
- `cargo test -p zeph-worktree probe` passes

---

## T-09: `CwdRestoreGuard` and `CwdGuard` in `zeph-subagent`

**Crate:** `zeph-subagent`  
**Phase:** P3  
**Depends on:** T-03  
**LOC estimate:** ~80

**Description:**  
Create `crates/zeph-subagent/src/cwd_guard.rs`:
- `CwdRestoreGuard<'a>`: captures `prev: PathBuf`, holds `MutexGuard<'a, ()>`
- `Drop` impl: calls `std::env::set_current_dir(&self.prev)`, logs error on failure (cannot propagate)
- `SubAgentManager` gains `cwd_guard: Arc<tokio::sync::Mutex<()>>` field

**Acceptance criteria (M4 — must be an explicit test):**

> **Concurrency serialisation test:** Spawn a worktree agent (holding `CwdGuard`) and, while it is
> active (before it releases the guard), attempt to execute a tool call from a second agent. Assert
> that the second agent's tool execution does NOT begin until the worktree agent's run is complete
> and the guard is released.

- `CwdRestoreGuard::drop` restores cwd even when the agent task panics (test with `std::panic::catch_unwind` or by poisoning the task)
- `cargo test -p zeph-subagent cwd_guard` passes

---

## T-10: `SubAgentManager::spawn` integration

**Crate:** `zeph-subagent`  
**Phase:** P3  
**Depends on:** T-09, T-05, T-06  
**LOC estimate:** ~120

**Description:**  
Wire `WorktreeManager` into `SubAgentManager::spawn`:
- Add `worktree_manager: Option<Arc<WorktreeManager>>` field
- In `spawn`: when worktree is applicable (manager present + `def.permissions.worktree`), acquire
  `CwdGuard`, call `create`, build `CwdRestoreGuard`, use worktree path as cwd for system prompt
- In `build_filtered_executor`: add `set_working_directory` to disallowed when `permissions.worktree`
- On completion/cancel: `remove` the worktree (after cwd restore via guard drop)
- INV-4: propagate `WorktreeError` as `SubAgentError`, do NOT fall back to shared cwd

**Acceptance criteria (also covers A2 — cancellation):**

> **Cancellation path test:** Create a worktree agent, cancel it mid-run. Assert that cwd is restored
> to the pre-spawn value and the `CwdGuard` is released.

- `set_working_directory` not in executor tool list when `permissions.worktree = true`
- `set_working_directory` present in executor tool list when `permissions.worktree = false`
- Worktree creation failure → spawn returns error, agent does NOT run
- `cargo nextest run -p zeph-subagent -- worktree` passes

---

## T-11: Bootstrap wiring

**Crate:** binary (`src/`)  
**Phase:** P4  
**Depends on:** T-08, T-10  
**LOC estimate:** ~30

**Description:**  
In `src/agent_setup.rs` or `src/bootstrap.rs`:
- Detect repo root (walk up to find `.git/`)
- When `config.worktree.enabled`: run `WorktreeManager::probe_capabilities`; fail fast with actionable error if probe fails
- Construct `WorktreeManager::new(repo_root, config.worktree.clone())`
- Inject into `SubAgentManager`

**Acceptance criteria:**
- Binary starts successfully in a git repo with `worktree.enabled = true` and `git` ≥ 2.5 present
- Binary prints clear error and exits non-zero when not in a git repo and `worktree.enabled = true`
- `worktree.enabled = false` produces no git invocations at startup

---

## T-12: CLI commands and session override

**Crate:** binary (`src/`)  
**Phase:** P4  
**Depends on:** T-07, T-11  
**LOC estimate:** ~60

**Description:**  
- Add `Worktree { command: WorktreeCommand }` subcommand
- `WorktreeCommand::List`: calls `reconcile()`, prints table
- `WorktreeCommand::Clean`: calls `reconcile()`, removes stale entries, prints count
- Add `--worktree-base-ref <fresh|head>` flag to root `Cli` struct; applies as session override

**Acceptance criteria:**
- `zeph worktree list` exits 0, prints header even with empty result
- `zeph worktree clean` exits 0, reports "0 stale worktrees removed" when clean
- `--worktree-base-ref fresh` overrides config value for the session

---

## T-13: TUI entries and spinners

**Crate:** `zeph-tui`  
**Phase:** P4  
**Depends on:** T-07  
**LOC estimate:** ~40

**Description:**  
- Add `/worktree list` and `/worktree clean` to TUI command palette
- During `WorktreeManager::create`: emit `SystemStatus` event with message "Creating worktree for {agent_id}…"
- During `WorktreeManager::remove`: emit "Removing worktree for {agent_id}…"

**Acceptance criteria:**
- Both commands appear in TUI autocomplete
- Spinner appears during create/remove (manual TUI test; document in playbook)

---

## T-14: `--init` wizard update

**Crate:** binary (`src/`)  
**Phase:** P4  
**Depends on:** T-01  
**LOC estimate:** ~30

**Description:**  
Add "Subagent isolation" section to `--init` interactive wizard:
- "Enable per-subagent git worktrees? [y/N]"
- If yes: "Branch from local HEAD or fetch from remote? [head/fresh]"
- If fresh: "Default remote branch? [main]"

**Acceptance criteria:**
- Wizard writes correct `[worktree]` TOML on confirmed answers
- Answering 'N' → `enabled = false` written, no follow-up questions asked

---

## T-15: Live-testing playbook and coverage-status

**Target:** `.local/testing/`  
**Phase:** P5  
**LOC estimate:** non-code

**Description:**  
Create `.local/testing/playbooks/worktree.md` with the scenarios listed in `spec.md`.
Add rows to `.local/testing/coverage-status.md` for `zeph-worktree` and the worktree integration
in `zeph-subagent` — initial status: `Untested`.

**Scenarios to cover (from spec.md):**
1. `enabled = false` (default): no worktrees created, no git invocations
2. `base_ref = head` + clean working tree: worktree created at HEAD
3. `base_ref = head` + dirty working tree: dirty-tree warning emitted
4. `base_ref = fresh` + valid origin: worktree created at `origin/<default>`
5. `base_ref = fresh` + no network: clear error, agent does not run
6. Cancellation mid-run: cwd restored, worktree removed
7. Crash simulation: `worktree clean` reconciles stale entries on next startup
8. Concurrent plain agent during active worktree agent: plain agent tool turn serialised

**Acceptance criteria:**
- `.local/testing/playbooks/worktree.md` exists and covers all 8 scenarios with expected outcomes
- `.local/testing/coverage-status.md` has rows for `zeph-worktree` (Untested) and `worktree-subagent-integration` (Untested)

---

## T-16: Pre-PR checklist

**Phase:** P5

**Description:**  
Before opening the PR, run all pre-commit checks required by `branching.md`:

```bash
cargo +nightly fmt --check
cargo clippy --all-targets --all-features --workspace -- -D warnings
cargo nextest run --config-file .github/nextest.toml --workspace --lib --bins
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --features desktop,ide,server,chat,pdf,scheduler --locked
RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --all-features -p zeph-worktree
cargo test --doc -p zeph-worktree
RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --all-features -p zeph-config
RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --all-features -p zeph-subagent
```

Update `CHANGELOG.md` with the entry template from `plan.md`.

**Acceptance criteria:**
- All checks pass with zero warnings
- `CHANGELOG.md` updated
- `docs/src/worktree.md` created or updated if user-facing behaviour changed
