---
aliases:
  - Ephemeral Plugins and Provider Overrides SRS
  - Parity SRS 3918
tags:
  - sdd
  - srs
  - parity
  - plugins
  - provider-persistence
created: 2026-05-29
status: approved
related:
  - "[[specs/065-ephemeral-plugins-provider-overrides/brd]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/spec]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/nfr]]"
---

# SRS: Ephemeral Plugin Loading and Provider Override Persistence (GitHub #3918)

ISO/IEC/IEEE 29148:2018 compliant. Requirements use EARS notation.

## 1. Scope

This SRS covers two implementation gaps:

- **Feature A**: `--plugin-url` session-scoped ephemeral plugin loading
- **Feature B**: Provider parameter override persistence across restarts

Deferred gaps (worktree.baseRef, bgIsolation, Ctrl+R) are acknowledged in BRD §5 and excluded here.

---

## 2. Feature A: `--plugin-url` Ephemeral Plugin Loading

### FR-A-01: CLI Flag

**WHEN** the user starts Zeph with `--plugin-url <url>`,
**THE SYSTEM SHALL** treat `<url>` as a plugin archive to download and load for the current session only.

**WHEN** `--plugin-sha256 <hex>` is also provided,
**THE SYSTEM SHALL** verify the SHA-256 digest of the downloaded archive before loading.

### FR-A-02: HTTPS Enforcement

**WHEN** `--plugin-url` is specified,
**IF** the URL scheme is not `https`,
**THE SYSTEM SHALL** reject the request with a `PluginError::InsecureUrl` error and a human-readable message before any network I/O occurs.

> Rationale: existing `validate_url_scheme` accepts `http`. The ephemeral path requires HTTPS-only to prevent MITM (Critic Finding 1, P1). The general `add_remote` behavior is a separate concern.

### FR-A-03: Blocking Security Scan

**WHEN** an ephemeral plugin is downloaded,
**THE SYSTEM SHALL** run `scan_skill_entries` with strict mode (`strict_scan = true`).

**WHEN** `scan_skill_entries` finds an injection pattern in any SKILL.md entry,
**THE SYSTEM SHALL** abort the load and return an error to the user.

> Rationale: for permanent plugins the advisory-only scan is acceptable because the user explicitly chose to install. For ephemeral plugins from arbitrary URLs, silent advisory-only is too weak (Critic Finding 4, P2).

### FR-A-04: Ephemeral Lifetime

**WHEN** a plugin is loaded via `--plugin-url`,
**THE SYSTEM SHALL** extract it into a temporary directory whose lifetime is tied to the agent process.

**WHEN** the agent exits (normally or via panic unwind),
**THE SYSTEM SHALL** delete the temporary directory automatically.

No manual cleanup step may be required.

### FR-A-05: Skill and MCP Registration

**WHEN** an ephemeral plugin is loaded successfully,
**THE SYSTEM SHALL** register its skills in `SkillRegistry` and its MCP servers in the MCP lifecycle, identical to a permanently installed plugin.

**WHEN** the agent exits,
**THE SYSTEM SHALL NOT** leave any permanent entry in the skill registry, integrity registry, or MCP server configuration.

### FR-A-06: No Config Overlay Privileges

**WHEN** an ephemeral plugin is loaded,
**THE SYSTEM SHALL NOT** apply any config overlay from its `plugin.toml`.

> Rationale: session ends before any damage persists, but principle of least privilege.

### FR-A-07: `plugin list` Visibility

**WHEN** the user runs `zeph plugin list` during a session with an active ephemeral plugin,
**THE SYSTEM SHALL** include the ephemeral plugin in the listing with an `[ephemeral]` tag.

### FR-A-08: CLI-Only Scope

**WHEN** `--plugin-url` is specified in a non-CLI mode (Telegram, Discord, Slack, ACP),
**THE SYSTEM SHALL** ignore the flag silently (CLI-only convenience flag; other channels have no CLI argument parsing).

---

## 3. Feature B: Provider Parameter Override Persistence

### FR-B-01: Persist Overrides

**WHEN** the user sets a provider override (reasoning effort, temperature) in a session via `/provider` or `/effort`,
**THE SYSTEM SHALL** persist the override values alongside the provider name in `channel_preferences` using `pref_key = "provider_overrides"`.

### FR-B-02: Restore Overrides on Startup

**WHEN** `provider_persistence_enabled = true` and the agent starts,
**THE SYSTEM SHALL** restore provider overrides from `channel_preferences` (pref_key `"provider_overrides"`) for the active channel.

**WHEN** the stored overrides blob references a parameter not applicable to the restored provider (e.g., `reasoning_effort` on an Ollama provider),
**THE SYSTEM SHALL** silently skip that parameter with a `tracing::warn!` log.

### FR-B-03: Overrides Schema Validation

**WHEN** the overrides blob is deserialized,
**THE SYSTEM SHALL** reject blobs containing unknown fields (via `#[serde(deny_unknown_fields)]`).

**WHEN** the overrides blob exceeds 1 KB,
**THE SYSTEM SHALL** discard it and log a warning, then proceed without overrides.

> Rationale: prevents unbounded growth and unvalidated keys (Critic Finding 5, P3).

### FR-B-04: Zero Schema Migration

**THE SYSTEM SHALL NOT** require an `ALTER TABLE` migration to add the `provider_overrides` pref_key.

The existing `channel_preferences` key-value design (`pref_key` / `pref_value`) accommodates a new row without schema change.

> Rationale: eliminates critic Finding 3 (schema migration gap). A new `pref_key` row is append-only.

### FR-B-05: Config Gate

**WHEN** `[session] persist_provider_overrides = false` (default: `true`),
**THE SYSTEM SHALL NOT** persist or restore provider overrides.

---

## 4. Deferred Requirements (Acknowledged)

### FR-D-01: `worktree.baseRef`

Deferred (P3). Requires native worktree management subsystem. No implementation in this spec.

### FR-D-02: `bgIsolation: none`

Deferred (P3). Depends on FR-D-01. No implementation in this spec.

### FR-D-03: Ctrl+R Cross-Project History Search

Deferred (P3). Zeph TUI has no prompt-history infrastructure. No implementation in this spec.

---

## 5. Traceability Matrix

| Requirement | BRD Goal | Critic Finding |
|-------------|----------|----------------|
| FR-A-01 | BG-01 | — |
| FR-A-02 | BG-03 | Finding 1 (P1, HTTPS) |
| FR-A-03 | BG-03 | Finding 4 (P2, blocking scan) |
| FR-A-04 | BG-01 | — |
| FR-A-05 | BG-01 | — |
| FR-A-06 | BG-03 | — |
| FR-A-07 | BG-01 | — |
| FR-A-08 | BG-01 | — |
| FR-B-01 | BG-02 | — |
| FR-B-02 | BG-02 | — |
| FR-B-03 | BG-02 | Finding 5 (P3, validation) |
| FR-B-04 | BG-02 | Finding 3 (P2, no ALTER TABLE) |
| FR-B-05 | BG-02 | — |
| FR-D-01 | — | Finding 2 (P2, explicit defer) |
| FR-D-02 | — | Finding 2 (P2, explicit defer) |
| FR-D-03 | — | Finding 2 (P2, explicit defer) |
