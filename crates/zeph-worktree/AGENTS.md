# zeph-worktree Guide

Per-subagent git worktree lifecycle: `WorktreeManager` creates, removes, lists, and reconciles git worktrees; `WorktreeHandle` records one managed worktree; git invocation is abstracted behind `GitRunner` / `DefaultGitRunner`.

- Start with crate-local checks: `cargo build -p zeph-worktree`, `cargo nextest run -p zeph-worktree`, `cargo clippy -p zeph-worktree --all-targets -- -D warnings`.
- Read `specs/063-worktree-subsystem/spec.md` before any change; honor its `## Key Invariants` and `NEVER` sections.
- Dependency direction: `zeph-subagent → zeph-worktree → (zeph-config, tokio, thiserror, tracing)`. This crate MUST NOT depend on `zeph-core`, `zeph-subagent`, or `zeph-channels`.
- All git invocations go through the `GitRunner` abstraction so tests can mock them — never shell out to `git` directly elsewhere in the crate.
- `sanitize.rs` feeds agent ids and branch names into git CLI arguments; treat it as security-sensitive (argument/shell injection). Every new input shape needs a sanitization regression test before merge.
- Worktrees are never created inside this repository — managed worktrees live under the sibling `../worktrees/` directory. Path-construction and reconciliation logic must respect that convention.
- `max_worktrees` admission and `reconcile()`'s stale/quota accounting must scope strictly to entries under this manager's own canonicalized `worktree_root` — `git worktree list --porcelain` reports every worktree registered to the repo, including ones from unrelated tooling (e.g. `EnterWorktree`, a manual `git worktree add` elsewhere), which must never count toward quota or be removed by `clean` (#6257, #6283). `create()`'s quota check-through-registration sequence must stay atomic within a process (internal `admission_lock`) — do not reintroduce the check-then-act race fixed in #6252.
