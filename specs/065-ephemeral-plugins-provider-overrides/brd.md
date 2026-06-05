---
aliases:
  - Ephemeral Plugins and Provider Overrides BRD
  - Parity BRD 3918
tags:
  - sdd
  - brd
  - parity
  - plugins
  - provider-persistence
created: 2026-05-29
status: approved
related:
  - "[[specs/065-ephemeral-plugins-provider-overrides/spec]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/srs]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/nfr]]"
  - "[[specs/058-plugins/spec]]"
  - "[[specs/003-llm-providers/spec]]"
---

# BRD: Ephemeral Plugin Loading and Provider Override Persistence (GitHub #3918)

## 1. Business Context

Zeph is an AI agent that tracks capability gaps identified through competitive analysis. GitHub issue #3918 covers a gap assessment for release v2.1.141–v2.1.143. This document defines the business case for the two actionable gaps identified in that assessment.

## 2. Problem Statement

The assessed release introduced:

1. **`--plugin-url` flag** — load a plugin from a URL for the duration of one session, with no permanent installation. Zeph has permanent plugin installation but no session-scoped ephemeral loading.
2. **Background session provider persistence** — model selection and reasoning effort are preserved when the agent wakes from an idle background state. Zeph persists the provider *name* but discards per-session parameter overrides (reasoning effort, temperature) on process restart.

Without these capabilities, Zeph users who prototype with third-party plugins must permanently install them (polluting their plugin store), and users who tune provider parameters lose those settings across restarts.

## 3. Business Goals

| ID | Goal | Priority |
|----|------|----------|
| BG-01 | Users can load a plugin from a URL for one session without permanent installation side-effects | P2 |
| BG-02 | Users' provider parameter overrides (reasoning effort, temperature) survive agent restarts | P2 |
| BG-03 | Ephemeral plugin loading is at least as secure as permanent installation (HTTPS-only, blocking scan) | P1 |

## 4. Stakeholders

| Role | Interest |
|------|----------|
| CLI user | Prototype plugins without polluting plugin store; retain provider settings across restarts |
| Plugin developer | Test plugin during development with a single flag, no install ceremony |
| Security administrator | Ephemeral URL plugins are not weaker than permanent plugins from an injection/MITM perspective |

## 5. Out of Scope

The following gaps from issue #3918 are **deferred** to follow-up work:

| Gap | Reason |
|-----|--------|
| `worktree.baseRef` config | Requires native worktree management subsystem (does not exist in Zeph) |
| `worktree.bgIsolation: none` | Logically depends on `worktree.baseRef` implementation |
| Ctrl+R cross-project history search | Zeph TUI has no prompt-history infrastructure; prerequisite work is out of scope here |

These deferrals are **explicit** (not silent omissions). They must be tracked as follow-up issues.

## 6. Success Criteria

| ID | Criterion | Measurable |
|----|-----------|-----------|
| SC-01 | `zeph --plugin-url <https-url>` loads and activates a plugin; skills/MCP servers from it are usable for that session only | Pass end-to-end test with ephemeral plugin fixture |
| SC-02 | After process restart, provider parameters (effort, temperature) previously set via `/provider` are restored | Verified by integration test: set overrides → restart → confirm restored |
| SC-03 | Plain HTTP `--plugin-url` is rejected with a clear error message | Unit test on `validate_url_scheme` |
| SC-04 | A plugin with injected SKILL.md entries (scan failure) blocks loading when loaded via `--plugin-url` | Unit test on ephemeral load path with scan failure fixture |
| SC-05 | The overrides JSON blob is capped at 1 KB and rejects unknown fields | Unit test on deserialization with oversized/unknown-field blob |

## 7. Constraints

- No new SQLite schema migration needed (use existing `channel_preferences` key-value table with a new `pref_key`)
- Implementation stays within `zeph-plugins`, `zeph-core`, `zeph-config`, `zeph-commands`, and the root binary crate
- MSRV remains Rust 1.95
- No new mandatory dependencies

## 8. Dependencies

| Dependency | Type | Notes |
|------------|------|-------|
| Existing `PluginManager::add_remote()` | Internal | Provides download + SHA-256 verification; ephemeral variant reuses this path |
| `channel_preferences` SQLite table | Internal | Key-value store; new `pref_key = "provider_overrides"` row requires no schema change |
| `validate_url_scheme()` in `zeph-plugins` | Internal | Must be tightened to HTTPS-only for the ephemeral path |
