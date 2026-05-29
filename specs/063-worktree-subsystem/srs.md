---
aliases:
  - Worktree Subsystem SRS
  - SRS-063
tags:
  - sdd
  - srs
  - worktree
created: 2026-05-29
status: approved
related:
  - "[[specs/063-worktree-subsystem/brd]]"
  - "[[specs/063-worktree-subsystem/nfr]]"
  - "[[specs/063-worktree-subsystem/spec]]"
---

# SRS-063: Worktree Subsystem + `worktree.base_ref`

ISO/IEC/IEEE 29148:2018 — Software Requirements Specification

## 1. Scope

This SRS specifies the functional requirements for the `zeph-worktree` crate and its integration points
into `zeph-config`, `zeph-subagent`, and the Zeph binary. It traces to BRD-063.

## 2. Definitions

| Term | Definition |
|---|---|
| **worktree** | A git linked working tree created by `git worktree add`. |
| **CwdGuard** | A process-global `tokio::sync::Mutex` that serialises all worktree-attached agent runs. |
| **base_ref** | Config field controlling the commit from which a worktree branch is created. |
| **worktree agent** | A subagent whose definition has `permissions.worktree = true`. |
| **capability probe** | Startup check validating git presence and in-repo status. |

## 3. Functional Requirements

### 3.1 Configuration (`zeph-config`)

**FR-CFG-01** [BG-2]: The config schema SHALL expose a `[worktree]` TOML section with the fields:
```toml
[worktree]
enabled = false
base_ref = "head"            # "fresh" | "head"
default_branch = "main"
root = ".claude/worktrees"
branch_prefix = "agent/"
prune_branch_on_remove = false
cleanup_on_completion = true
```

**FR-CFG-02** [BG-3]: `worktree.enabled` SHALL default to `false`. When `false`, all worktree
behaviour is bypassed and subagents continue to share the parent cwd as today.

**FR-CFG-03** [BG-2]: `worktree.base_ref` SHALL accept the values `"head"` and `"fresh"` (case-
insensitive, serde `rename_all = snake_case`). The default SHALL be `"head"`.

**FR-CFG-04**: `WorktreeBaseRef` SHALL be `#[non_exhaustive]` to allow future values without a
breaking change.

**FR-CFG-05**: Per-agent opt-in SHALL be expressed as `permissions.worktree: true` in subagent
definition frontmatter (a new `bool` field on `SubAgentPermissions`, default `false`).

### 3.2 Crate `zeph-worktree`

**FR-WT-01**: The crate SHALL expose `WorktreeManager::new(repo_root: PathBuf, config: WorktreeConfig) -> Result<Self, WorktreeError>` that validates the repo root and config.

**FR-WT-02** [BG-1]: `WorktreeManager::create(subagent_id: &str) -> Result<WorktreeHandle, WorktreeError>` SHALL create a git worktree for the given subagent at `<root>/<subagent_id>/`.

**FR-WT-03**: `create` SHALL resolve the base commit according to `config.base_ref`:
- `Head` → base commit = `HEAD` (`git worktree add -b {branch} -- {path} HEAD`)
- `Fresh` → `git fetch origin {default_branch}`, then base = `origin/{default_branch}` (`git worktree add -b {branch} -- {path} origin/{default_branch}`)

**FR-WT-04**: `create` SHALL emit a `tracing::warn!` when `base_ref = Head` and `git status --porcelain` is non-empty: "parent working tree has uncommitted changes; the worktree branches from the last commit and will NOT include them."

**FR-WT-05**: `WorktreeManager::remove(handle: &WorktreeHandle, prune_branch: bool) -> Result<(), WorktreeError>` SHALL run `git worktree remove {path}` and, when `prune_branch = true`, run `git branch -D {branch_name}`.

**FR-WT-06**: `WorktreeManager::list(&self) -> &[WorktreeHandle]` SHALL return handles for worktrees created in the current process session. This is live-session in-RAM only; it does NOT enumerate worktrees from prior sessions.

**FR-WT-07**: `WorktreeManager::reconcile() -> Result<Vec<WorktreeHandle>, WorktreeError>` SHALL enumerate worktrees from `git worktree list --porcelain` combined with a filesystem scan of `config.root`, and return stale entries (present on disk but not in the current session). This is the source of truth for the CLI `worktree clean` command and startup reconciliation.

**FR-WT-08**: `WorktreeError` SHALL be a `thiserror`-derived enum with variants:
- `NotAGitRepo`
- `GitCommand { op: String, stderr: String }` — stderr logged at `debug`, sanitised message returned to user
- `PathExists(PathBuf)`
- `BaseRefUnresolved { attempted: String }`
- `InvalidBranchName(String)`
- `RootOutsideRepo(PathBuf)`
- `Io(std::io::Error)`

**FR-WT-09**: `GitRunner` trait SHALL have a single method:
```rust
async fn run(&self, args: &[&str], cwd: &Path) -> Result<std::process::Output, WorktreeError>;
```
The default implementation SHALL invoke `tokio::process::Command` with the `await-discipline` external-call timeout applied *inside* `run`. No call site shall be responsible for applying the timeout.

**FR-WT-10**: Branch names SHALL follow the pattern `{branch_prefix}{subagent_id}`. The `{subagent_id}` component SHALL be validated to match `^[A-Za-z0-9._-]+$`, MUST NOT begin with `-` or `.`, MUST NOT contain `..`. Violation returns `WorktreeError::InvalidBranchName`.

**FR-WT-11**: `config.root` SHALL be canonicalised at `WorktreeManager::new`. The canonical path MUST resolve to a path inside the repo root or a sibling of the repo root. Paths that canonicalise outside this boundary SHALL return `WorktreeError::RootOutsideRepo`.

**FR-WT-12**: All `git` invocations that accept branch names or paths SHALL use `--` separators (`git worktree add -b {branch} -- {path} {commitish}`) to prevent git from misinterpreting arguments as flags.

**FR-WT-13**: The `{commitish}` argument passed to `git worktree add` SHALL be validated with `git rev-parse --verify {commitish}` before use.

### 3.3 Startup Capability Probe

**FR-PROBE-01** [R6]: When `worktree.enabled = true`, the Zeph bootstrap SHALL run `git --version` and verify it reports ≥ 2.5. Failure SHALL abort startup with a clear, actionable error message.

**FR-PROBE-02**: When `worktree.enabled = true`, the bootstrap SHALL run `git rev-parse --is-inside-work-tree` from the repo root. Failure (not a git repo) SHALL abort startup with a clear message.

**FR-PROBE-03**: The capability probe SHALL NOT run when `worktree.enabled = false`.

### 3.4 CwdGuard — Process-Level Serialisation

**FR-CWD-01** [INV-1]: A single process-global `tokio::sync::Mutex` named `CwdGuard` SHALL be held from the moment `set_current_dir(worktree_path)` is called until `set_current_dir(prev_cwd)` has completed and the guard is dropped, spanning the agent's full multi-turn run.

**FR-CWD-02**: Acquiring `CwdGuard` SHALL block all other agents (both worktree-opted and plain agents) from executing tool calls until the worktree agent's run and cwd restoration are complete.

**FR-CWD-03** [INV-2]: Before entering the worktree, the previous cwd SHALL be captured: `prev = std::env::current_dir()`. After the agent completes (success, error, or cancellation), `set_current_dir(prev)` SHALL run in a guaranteed-run path (RAII `Drop` implementation or equivalent). This restore SHALL occur BEFORE `git worktree remove`.

**FR-CWD-04** [INV-3]: For every worktree-opted agent, `set_working_directory` (`TOOL_NAME = "set_working_directory"`) SHALL be added to the effective denylist via `FilteredToolExecutor::with_disallowed`. This is enforced at `build_filtered_executor` time in `zeph-subagent`.

**FR-CWD-05** [INV-4]: If worktree creation fails, the agent spawn SHALL fail with a `SubAgentError` wrapping the `WorktreeError`. The agent SHALL NOT proceed with the parent's shared cwd as fallback.

**FR-CWD-06**: Non-worktree subagents (those with `permissions.worktree = false`) SHALL NOT be restricted by the `CwdGuard` in isolation; they only become serialised while a worktree agent holds the guard.

### 3.5 Teardown and Cleanup

**FR-CLEANUP-01**: On agent completion or cancellation, if `cleanup_on_completion = true`, `WorktreeManager::remove` SHALL be called as a tracked background task (not fire-and-forget). The `CwdGuard` SHALL be released after `set_current_dir(prev_cwd)` completes and before `git worktree remove` is called, per the normative teardown sequence in FR-CLEANUP-02.

**FR-CLEANUP-02** [A1]: The teardown sequence SHALL be: (1) `set_current_dir(prev_cwd)`, (2) release `CwdGuard`, (3) `git worktree remove`.

**FR-CLEANUP-03** [A2]: The cancellation path SHALL follow the same sequence as normal completion — cwd is restored and the guard is released before any worktree removal.

**FR-CLEANUP-04**: The CLI command `zeph worktree clean` SHALL call `WorktreeManager::reconcile()`, enumerate stale entries, and remove each via `git worktree remove --force` followed by `git worktree prune`.

### 3.6 `base_ref = fresh` Network Behaviour

**FR-FRESH-01**: For `fresh` mode, the fetch SHALL be a single-attempt `git fetch origin {default_branch}` with the await-discipline timeout. There is no retry in the MVP.

**FR-FRESH-02**: If the fetch fails (non-zero exit or timeout), the spawn SHALL fail with `WorktreeError::GitCommand`. The error message SHALL state that the fetch failed and suggest checking network connectivity and `default_branch` config.

**FR-FRESH-03**: `config.default_branch` SHALL override the auto-detected default branch. When `default_branch` is empty and `git symbolic-ref refs/remotes/origin/HEAD` fails, the spawn SHALL fail with `WorktreeError::BaseRefUnresolved`.

### 3.7 CLI Integration

**FR-CLI-01**: The Zeph CLI SHALL expose `zeph worktree list` — prints active worktrees (session + stale).

**FR-CLI-02**: The Zeph CLI SHALL expose `zeph worktree clean` — removes stale/leaked worktrees.

**FR-CLI-03**: A session-scoped override `--worktree-base-ref <fresh|head>` SHALL override `config.worktree.base_ref` for the session, analogous to `--permission-mode`.

### 3.8 TUI and `--init` / `--migrate-config`

**FR-TUI-01**: The TUI command palette SHALL include a `worktree` section with entries:
- `/worktree list` — shows active worktrees
- `/worktree clean` — triggers reconciliation and removal of stale entries

**FR-INIT-01**: The `--init` wizard SHALL prompt for `worktree.enabled`, `worktree.base_ref`, and `worktree.default_branch` under a new "Subagent isolation" section.

**FR-MIGRATE-01**: Config migration step 53 SHALL add the `[worktree]` section with all defaults to existing configs that lack it.

### 3.9 Tracing and Observability

**FR-TRACE-01**: Every `async fn` in `WorktreeManager` that awaits an external resource (git command, filesystem operation) SHALL be wrapped in a `tracing::info_span!` using the naming convention `worktree.<operation>` (e.g. `worktree.create`, `worktree.remove`, `worktree.fetch`).

**FR-TRACE-02**: Span naming SHALL follow the project convention `<crate_short>.<subsystem>.<operation>`.

## 4. Traceability Matrix

| FR | BRD Goal | Architect Plan Section |
|---|---|---|
| FR-CFG-01..05 | BG-2, BG-3 | §3, §6 |
| FR-WT-01..13 | BG-1, BG-2, BG-4 | §2, §3, §5 |
| FR-PROBE-01..03 | BG-3, R6 | S3 |
| FR-CWD-01..06 | BG-1 | INV-1..INV-4, C1 Option 1 |
| FR-CLEANUP-01..04 | BG-1 | A1, A2, §7 |
| FR-FRESH-01..03 | BG-2 | §5, R4 |
| FR-CLI-01..03 | BG-2 | §4 |
| FR-TUI-01, FR-INIT-01, FR-MIGRATE-01 | BG-3 | §4 |
| FR-TRACE-01..02 | — | §9 (continuous-improvement.md) |
