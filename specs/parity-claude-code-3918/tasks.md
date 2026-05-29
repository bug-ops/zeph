---
aliases:
  - Parity Tasks 3918
  - Claude Code Parity Task Breakdown
tags:
  - sdd
  - tasks
  - parity
  - plugins
  - provider-persistence
created: 2026-05-29
status: approved
related:
  - "[[specs/parity-claude-code-3918/plan]]"
  - "[[specs/parity-claude-code-3918/spec]]"
---

# Task Breakdown: Claude Code v2.1.141–v2.1.143 Parity (GitHub #3918)

All tasks reference the implementation plan in `plan.md`.

---

## Phase 1: Provider Override Persistence

| # | Task | Plan Step | Crate | Est. LOC |
|---|------|-----------|-------|----------|
| T1.1 | Add `ProviderOverrides` struct with `deny_unknown_fields` and `is_empty()` | P1-1 | `zeph-config` | ~20 |
| T1.2 | Add `persist_provider_overrides: bool` to `SessionConfig` | P1-2 | `zeph-config` | ~5 |
| T1.3 | Extend `persist_channel_provider` to upsert overrides blob with size assertion | P1-3 | `zeph-core` | ~30 |
| T1.4 | Extend `restore_channel_provider` to load, validate, and apply overrides | P1-3 | `zeph-core` | ~40 |
| T1.5 | Wire `persist_provider_overrides` prompt into `--init` wizard | P1-4 | binary | ~10 |
| T1.6 | Write unit tests (4 cases) for persist/restore/validation | P1-6 | `zeph-core` | ~60 |
| T1.7 | Create playbook `provider-persistence.md`, add coverage row | P1-7 | `.local/` | — |
| T1.8 | Update `CHANGELOG.md` `[Unreleased]` | — | root | — |

**Phase 1 total estimated LOC: ~165**

---

## Phase 2: `--plugin-url` Ephemeral Plugin Loading

| # | Task | Plan Step | Crate | Est. LOC |
|---|------|-----------|-------|----------|
| T2.1 | Add `PluginError::InsecureUrl` variant | P2-1 | `zeph-plugins` | ~5 |
| T2.2 | Add `validate_url_scheme_ephemeral()` (HTTPS-only) | P2-2 | `zeph-plugins` | ~15 |
| T2.3 | Extract `download_and_extract()` shared helper | P2-3 | `zeph-plugins` | ~40 refactor |
| T2.4 | Add `add_remote_ephemeral()` using helper with `strict_scan=true` | P2-4 | `zeph-plugins` | ~40 |
| T2.5 | Add `--plugin-url` and `--plugin-sha256` top-level CLI args | P2-5 | binary | ~15 |
| T2.6 | Add `with_ephemeral_plugins(Vec<TempDir>)` to `AgentBuilder` | P2-6 | `zeph-core` | ~15 |
| T2.7 | Add `ephemeral_plugins: Vec<TempDir>` to `AgentRuntime` | P2-6 | `zeph-core` | ~10 |
| T2.8 | Bootstrap wiring in `main.rs`: call ephemeral load, register skills + MCP, pass TempDir | P2-7 | binary | ~40 |
| T2.9 | Update `plugin list` output to show `[ephemeral]` tag | P2-8 | `zeph-commands` | ~15 |
| T2.10 | Write unit tests (4 cases) for URL validation + scan blocking + happy path | P2-9 | `zeph-plugins` | ~70 |
| T2.11 | Create playbook `ephemeral-plugins.md`, add coverage row | P2-10 | `.local/` | — |
| T2.12 | Update `CHANGELOG.md` `[Unreleased]` | — | root | — |

**Phase 2 total estimated LOC: ~265 (including refactor)**

---

## Phase 3: Deferred Gap Issues

| # | Task | GitHub Labels |
|---|------|---------------|
| T3.1 | File issue: `worktree.baseRef` config support (P3) | `enhancement`, `P3` |
| T3.2 | File issue: `worktree.bgIsolation: none` support (P3, blocks on T3.1) | `enhancement`, `P3` |
| T3.3 | File issue: Ctrl+R cross-project prompt history search in TUI (P3) | `enhancement`, `P3` |

---

## Acceptance Criteria (for PR merge)

- [ ] All Phase 1 and Phase 2 unit tests pass (`cargo nextest run --workspace --all-features --lib --bins`)
- [ ] No new clippy warnings (`cargo clippy --workspace --all-features -- -D warnings`)
- [ ] Docs build without broken links (`RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-plugins -p zeph-core -p zeph-config`)
- [ ] Doc-tests pass for all touched crates
- [ ] `CHANGELOG.md` updated
- [ ] Three deferred gap issues filed on GitHub (T3.1–T3.3)
- [ ] Coverage rows added to `.local/testing/coverage-status.md`
- [ ] Test playbooks created at `.local/testing/playbooks/`
