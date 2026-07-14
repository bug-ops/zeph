---
aliases:
  - CLI Init/Migrate-Config Flag Mismatch
  - Spec 076
  - --init/--migrate-config Documentation Bug
tags:
  - sdd
  - spec
  - core
  - config
  - cross-cutting
created: 2026-07-14
status: draft
related:
  - "[[001-system-invariants/spec]]"
  - "[[047-cli-modes/spec]]"
  - "[[MOC-specs]]"
issues:
  - "#587"
---

# Spec 076 — `--init` / `--migrate-config` Documented as Flags but Implemented as Subcommands

> [!info]
> `src/cli.rs` defines `init` and `migrate-config` exclusively as `clap` `Commands` subcommand
> variants (`zeph init`, `zeph migrate-config`). No top-level `--init` or `--migrate-config`
> boolean flag exists on the `Cli` struct. Every mandatory instruction file in this repository —
> including the user's own global `CLAUDE.md` and the project's `CLAUDE.md` — documents these as
> flags (`--init`, `--migrate-config`), so every session that follows the documented convention
> hits an immediate clap "unexpected argument" parse error before reaching the intended
> functionality. This spec documents the defect and the two viable remediation paths; it does not
> prescribe which one to take.

## Sources

### External
- None — this is an internal documentation/CLI-contract consistency defect, not derived from an
  external standard or reference implementation.

### Internal
| File | Contents |
|---|---|
| `src/cli.rs` | `clap`-derived `Cli` struct (top-level flags: `--tui`, `--theme`, `--daemon`, `--acp`, `--config`, etc.) and `Command` enum, where `Init { output: Option<PathBuf> }` (line 339) and `MigrateConfig { config, in_place, diff }` (line 459) are declared as subcommand variants only — never mirrored as `Cli`-level flags |
| `src/cli.rs:922-923, 962-963` | The CLI's own doc comments say `zeph --init` / `--migrate-config`, i.e. the flag-style mistake is baked into the source code itself, not only external docs |
| `/Users/rabax/.claude/CLAUDE.md` | User-global mandatory instructions: "Config migration (`--migrate-config`)" |
| `/Users/rabax/Dev/zeph/CLAUDE.md` | Project mandatory instructions, Development Rules points 4/5: "Interactive configuration wizard (`--init`)" and "Config migration (`--migrate-config`)" |
| `.zeph/zeph.md:212-213` | Same flag-style wording, duplicated from CLAUDE.md |
| `crates/zeph-config/AGENTS.md:7-8` | "add a `--migrate-config` migration step" / "Keep config structs ... and the `--init` wizard in sync" |
| `CHANGELOG.md` | 111 historical occurrences of `--migrate-config` / `--init` style wording across the project's lifetime (informational precedent only — not to be retroactively corrected, see §Out of Scope) |
| `.local/testing/playbooks/worktree-disk-quota.md` (Scenarios 6-7) | Test steps that themselves invoke the broken `--migrate-config` / `--init` flag syntax, confirmed to fail when followed literally during ci-1386 live testing |
| GitHub issue #587 (closed 2026-02-19) | "Restore `--vault`, `--vault-key`, `--vault-path` CLI flags after clap migration" — precedent showing a prior clap migration silently dropped top-level flags; those were explicitly restored, but `init`/`migrate-config` were never given the same treatment (or were simply always documented incorrectly) |

---

## 1. Overview

### Problem Statement
Zeph's CLI parser (`clap`-derived `Cli`/`Command` in `src/cli.rs`) only accepts `init` and
`migrate-config` as subcommands (no leading dashes: `zeph init`, `zeph migrate-config ...`).
Every place in the repository that documents these two operations — both mandatory
instruction files (`CLAUDE.md` user-global and project-level), `.zeph/zeph.md`,
`crates/zeph-config/AGENTS.md`, a live-testing playbook, and even doc comments inside
`src/cli.rs` itself — describes them using flag syntax (`--init`, `--migrate-config`). Because
`CLAUDE.md` is treated as binding instruction for every Claude Code / agent session working in
this repository, any session that follows the documented convention literally
(`cargo run --features full -- --migrate-config ...` or `-- --init`) fails immediately with a
clap "unexpected argument" error, before the config wizard or migration logic ever runs. This
was reproduced directly during ci-1386 live testing of the worktree disk-quota migration/wizard
scenarios (issue #5924, `worktree-disk-quota.md` Scenarios 6 and 7), whose own documented steps
use the incorrect flag-style invocation.

This is not a cosmetic nit: it silently blocks a documented, load-bearing workflow (config
migration and first-run setup) for every contributor and every automated agent session that
trusts the instructions it was given.

### Goal
Either the CLI accepts the documented flag syntax (so all existing documentation becomes
correct without further edits), or every documented reference to these two operations across
the repository is corrected to the subcommand form that actually works — chosen consistently,
so that copy-pasting any documented command succeeds on the first try.

### Out of Scope
- Retroactively correcting `CHANGELOG.md` historical entries — it is an immutable historical
  record; only new entries going forward must use whichever form is decided correct.
- Any other CLI subcommand/flag mismatches not explicitly named here (this spec covers only
  `init` and `migrate-config`); a broader CLI-surface audit is a separate concern.
- Re-litigating the outcome of issue #587 (`--vault*` flags) — cited only as precedent that this
  class of defect has occurred before and was fixed by adding the flags, not by changing docs.

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a future implementation session resolves this spec THE SYSTEM SHALL make exactly one invocation form (flag or subcommand) work for `init`, consistently across the CLI and all documentation | must |
| FR-002 | WHEN a future implementation session resolves this spec THE SYSTEM SHALL make exactly one invocation form (flag or subcommand) work for `migrate-config`, consistently across the CLI and all documentation | must |
| FR-003 | IF the remediation adds `--init`/`--migrate-config` as top-level flags THE SYSTEM SHALL route them to the same handler logic as the existing `init`/`migrate-config` subcommands (no duplicated business logic), analogous to how `--vault`/`--vault-key`/`--vault-path` were restored in #587 | should |
| FR-004 | IF the remediation instead standardizes on the subcommand form THE SYSTEM SHALL update every current documentation reference identified in this spec's Internal Sources table (excluding `CHANGELOG.md` historical entries) to use `zeph init` / `zeph migrate-config` with no leading dashes | should |
| FR-005 | WHEN the remediation is complete THE SYSTEM SHALL have `.local/testing/playbooks/worktree-disk-quota.md` Scenarios 6 and 7 updated to use the corrected, working invocation form | must |
| FR-006 | WHEN the remediation is complete THE SYSTEM SHALL have the doc comments inside `src/cli.rs` (lines ~922-923, ~962-963, and any other in-source references) match the corrected invocation form — the source code must not contradict itself | must |

---

## 3. Architecture / Remediation Paths

Two mutually exclusive remediation paths exist. This spec intentionally does not choose between
them — that decision belongs to a future implementation-planning (`/sdd plan`) session, informed
by whichever tradeoffs are judged more important at that time.

### Path A — Flag-Alias Restoration (matches documentation, follows #587 precedent)

Add `--init` and `--migrate-config` as top-level boolean/optional-value flags on the `Cli`
struct, routed in `main`/bootstrap dispatch to the exact same code paths currently reached only
via the `Command::Init { .. }` / `Command::MigrateConfig { .. }` subcommand arms. This preserves
every existing documented reference verbatim and mirrors the precedent set by issue #587, where
dropped top-level flags (`--vault`, `--vault-key`, `--vault-path`) were restored rather than
having their call sites rewritten to a different form.

Open design questions for the planning session:
- `[NEEDS CLARIFICATION: Should --init/--migrate-config coexist with the init/migrate-config subcommands (both forms valid), or fully replace the subcommand form? clap generally supports both simultaneously, but the two forms accept slightly different argument shapes today — Init has an `--output` option, MigrateConfig has `--config`/`--in-place`/`--diff` — so the flag form needs a way to carry those same sub-arguments (e.g., --migrate-config as a flag plus separate --config/--in-place/--diff top-level flags already partially exist for other purposes and may collide).]`
- `[NEEDS CLARIFICATION: Does a bare --init flag with no value fit clap's model cleanly alongside the existing --config <PATH> top-level flag, or does it need to be an Option<PathBuf> flag with a default, mirroring the subcommand's --output option?]`

### Path B — Documentation Correction (subcommand form is permanent)

Treat `zeph init` / `zeph migrate-config` (no leading dashes) as the intentional, permanent
interface. Correct every current-state documentation reference to match:
- `/Users/rabax/.claude/CLAUDE.md` (user-global — out of this repository's control; would need
  to be flagged to the user directly rather than edited by an agent, since it lives outside the
  project tree)
- `/Users/rabax/Dev/zeph/CLAUDE.md` (Development Rules points 4/5)
- `.zeph/zeph.md`
- `crates/zeph-config/AGENTS.md`
- `.local/testing/playbooks/worktree-disk-quota.md` (Scenarios 6-7)
- `src/cli.rs` doc comments (lines ~922-923, ~962-963)

Open design question for the planning session:
- `[NEEDS CLARIFICATION: The user-global ~/.claude/CLAUDE.md is outside the zeph repository and cannot be edited as part of a project PR — does correcting it require a separate out-of-band action by the user, or should the project-level CLAUDE.md simply be the authoritative source that supersedes the global one for this repo?]`

Path B requires no `src/cli.rs` behavior change, only documentation edits, and carries zero risk
of clap argument-parsing ambiguity — but it does not preserve any of the historically documented
invocations, meaning contributors who have memorized `--init`/`--migrate-config` must relearn
the subcommand form.

---

## 4. Key Invariants

### Always (without asking)
- Whichever path is chosen, the CLI and all current (non-CHANGELOG) documentation must agree on
  exactly one working invocation form per command by the time this spec is closed.
- The chosen remediation must not introduce duplicate business logic — flag and subcommand forms
  (if both exist) must delegate to a single shared handler.

### Ask First
- Choosing between Path A and Path B is itself an architectural decision requiring explicit
  sign-off in a future `/sdd plan` session — this spec deliberately does not make that call.
- Any change to the `Cli`/`Command` clap surface (Path A) requires review against the existing
  `--vault`/`--vault-key`/`--vault-path` restoration pattern from #587 to avoid reintroducing the
  same class of drift.

### Never
- Do not retroactively edit `CHANGELOG.md` historical entries to "fix" old wording — they are a
  historical record of what was written at the time, not a live reference.
- Do not silently pick one path and leave the other's documentation stale; every reference table
  entry in §Sources/Internal must be reconciled before this spec is considered resolved.

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Contributor runs `cargo run -- --migrate-config` today (pre-fix) | clap prints `error: unexpected argument '--migrate-config' found` and exits nonzero, before any config is read — reproduced directly in this spec's own live-testing session |
| Contributor runs `cargo run -- --init` today (pre-fix) | clap prints `error: unexpected argument '--init' found` and exits nonzero |
| Contributor runs `cargo run -- migrate-config --config <path> --diff` today | Works correctly — subcommand form is fully functional today, only the flag-style documentation is wrong |
| Contributor runs `cargo run -- init` today | Works correctly — interactive wizard launches |
| Path A chosen, both `--init` flag and `init` subcommand accepted simultaneously | Both forms must produce identical behavior; only one implementation path should exist behind them (no forked logic) |
| Path B chosen, user-global `~/.claude/CLAUDE.md` cannot be edited by an in-repo PR | Flag this explicitly to the user as an out-of-band correction needed, rather than silently leaving it stale |

---

## 6. Success Criteria

- [ ] A future `/sdd plan` session has explicitly chosen Path A or Path B and recorded the
      decision
- [ ] `cargo run --features full -- --init` (or `zeph init`, per the chosen path) succeeds and
      matches every current documentation reference
- [ ] `cargo run --features full -- --migrate-config --config <path> --diff` (or
      `zeph migrate-config ...`, per the chosen path) succeeds and matches every current
      documentation reference
- [ ] `.local/testing/playbooks/worktree-disk-quota.md` Scenarios 6 and 7 use the corrected,
      working invocation form and pass when re-run live
- [ ] `src/cli.rs` doc comments no longer contradict the actual accepted CLI surface
- [ ] All non-`CHANGELOG.md` documentation references listed in this spec's Internal Sources
      table are mutually consistent with the CLI's real behavior

---

## 7. See Also

- [[MOC-specs]] — Map of all specifications
- [[047-cli-modes/spec]] — CLI execution modes (`--bare`, `--json`, `-y`, `/loop`, `/recap`) —
  related top-level `Cli` flag surface
- [[001-system-invariants/spec]] — project-wide invariants
- GitHub issue #587 — precedent for restoring dropped top-level clap flags
