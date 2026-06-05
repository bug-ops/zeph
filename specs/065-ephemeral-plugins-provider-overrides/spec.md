---
aliases:
  - Ephemeral Plugins and Provider Overrides
  - Parity Spec 3918
tags:
  - sdd
  - spec
  - parity
  - plugins
  - provider-persistence
created: 2026-05-29
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/brd]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/srs]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/nfr]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/plan]]"
  - "[[specs/058-plugins/spec]]"
  - "[[specs/003-llm-providers/spec]]"
  - "[[specs/010-security/spec]]"
---

# Spec: Ephemeral Plugin Loading and Provider Override Persistence (GitHub #3918)

> This spec is the authoritative implementation contract for the two actionable parity gaps
> identified in GitHub issue #3918. It is derived from the architect plan
> (`.local/handoff/2026-05-29T18-24-25-architect.md`) and the critic review
> (`.local/handoff/2026-05-29T18-30-12-critic.md`). All critic findings are incorporated.

---

## 1. Gap Assessment Summary

| Gap | Verdict | Priority | Rationale |
|-----|---------|----------|-----------|
| `--plugin-url` session-scoped loading | **Implement** | P2 | Download infra exists; missing ephemeral variant + HTTPS gate |
| Session provider override persistence | **Implement** | P2 | Persistence infra exists; missing overrides blob per channel |
| `worktree.baseRef` config | **Implemented** | P3 | `worktree.base_ref: fresh\|head` in spec-063; `--init` wizard via `step_worktree()` (#4847) |
| `worktree.bgIsolation: none` | Partially deferred | P3 | `bg_isolation` field added to `WorktreeConfig` via `step_worktree()` (#4847); full child-process isolation still deferred |
| Ctrl+R cross-project history | **Implemented (single-session)** | P3 | `ReverseSearchState` widget with Ctrl+R keybinding added in TUI (#4678); cross-session scope deferred |

Deferred gaps **must** have follow-up GitHub issues filed. See `tasks.md`.

---

## 2. Feature A: `--plugin-url` Ephemeral Plugin Loading

### 2.1 Sources (Architect Plan §Gap 3 + Critic Findings 1, 4)

**Architect plan:** CLI flag `--plugin-url`, reuse `add_remote`, store `TempDir` in agent runtime.

**Critic amendments incorporated:**
- Finding 1 (P1): `validate_url_scheme` currently accepts `http`. For the ephemeral path, must enforce HTTPS-only. A new `PluginError::InsecureUrl` variant is the correct return.
- Finding 4 (P2): `scan_skill_entries` is advisory-only. For ephemeral plugins from arbitrary URLs, failures must be blocking. The `add_remote_ephemeral` function takes a `strict_scan: bool` parameter defaulting to `true`.

### 2.2 Crate Impact

| Crate | File | Change |
|-------|------|--------|
| `src/cli.rs` | — | Add `--plugin-url: Option<String>` and `--plugin-sha256: Option<String>` top-level args |
| `crates/zeph-plugins/src/manager.rs` | `add_remote_ephemeral()` | New function: download + verify + extract to `TempDir`; calls shared helper with `strict_scan = true` |
| `crates/zeph-plugins/src/manager.rs` | `validate_url_scheme()` | Add `ephemeral: bool` parameter; when `true`, reject `http` (return `PluginError::InsecureUrl`) |
| `crates/zeph-plugins/src/error.rs` | `PluginError` | Add `InsecureUrl(String)` variant |
| `crates/zeph-core/src/agent/builder.rs` | `AgentBuilder` | Add `with_ephemeral_plugins(Vec<TempDir>)` to hold ownership |
| `crates/zeph-core/src/agent/state/mod.rs` | `AgentRuntime` | Add `ephemeral_plugins: Vec<TempDir>` field |
| `src/main.rs` | bootstrap | If `--plugin-url` is set: call `add_remote_ephemeral`, register skills + MCP, pass `TempDir` to builder |

### 2.3 Shared Helper Pattern

```
download_and_extract(url, sha256, dest_dir)  ← shared by add_remote and add_remote_ephemeral
add_remote(url, sha256) → installs to plugins_dir, strict_scan=false
add_remote_ephemeral(url, sha256) → TempDir, strict_scan=true, validate_url_scheme(ephemeral=true)
```

This avoids duplication (NFR-MA-01).

### 2.4 Security Invariants

- HTTPS-only for ephemeral path (SRS FR-A-02, NFR-SE-01)
- Path traversal protection reused from existing extraction guard (NFR-SE-02)
- `scan_skill_entries` failures block load (SRS FR-A-03, NFR-SE-03)
- No write to `plugins_dir` (NFR-SE-04)
- No config overlay applied (SRS FR-A-06)

### 2.5 Key Invariants

- **NEVER** accept a plain `http://` URL for `--plugin-url`
- **NEVER** install to permanent plugin store
- **NEVER** apply config overlays from ephemeral plugins
- **ALWAYS** run blocking scan before registering skills

---

## 3. Feature B: Provider Parameter Override Persistence

### 3.1 Sources (Architect Plan §Gap 4 + Critic Findings 3, 5)

**Architect plan:** Extend `channel_preferences` with `overrides_json` column via `ALTER TABLE`.

**Critic amendments incorporated:**
- Finding 3 (P2): No `ALTER TABLE` needed. The `channel_preferences` table is already key-value (`pref_key` / `pref_value`). Store overrides as a new row with `pref_key = "provider_overrides"`. Zero migration required.
- Finding 5 (P3): `ProviderOverrides` must be a typed struct with `#[serde(deny_unknown_fields)]` and a 1 KB blob size cap.

### 3.2 `ProviderOverrides` Struct

```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderOverrides {
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
}
```

Constraints:
- Serialized form must not exceed 1024 bytes (validated before persist and after load)
- `Option` fields: omitted when `None` (compact JSON)
- Unknown fields on deserialize: return `serde_json::Error`, discard blob + warn (SRS FR-B-03)

### 3.3 Crate Impact

| Crate | File | Change |
|-------|------|--------|
| `crates/zeph-config/src/providers.rs` | — | Add `ProviderOverrides` struct |
| `crates/zeph-core/src/agent/provider_cmd.rs` | `persist_channel_provider()` | Also upsert `pref_key = "provider_overrides"` with serialized overrides JSON |
| `crates/zeph-core/src/agent/provider_cmd.rs` | `restore_channel_provider()` | Also load `pref_key = "provider_overrides"`, deserialize, apply to active provider |
| `crates/zeph-config/src/session.rs` | `SessionConfig` | Add `persist_provider_overrides: bool` (default: `true`) |

### 3.4 Persistence Flow

```
On /provider switch:
  persist_channel_provider(channel_id, provider_name)
  if persist_provider_overrides:
    overrides_json = serde_json::to_string(&current_overrides)?
    assert overrides_json.len() <= 1024
    upsert channel_preferences(channel_id, "provider_overrides", overrides_json)

On session start:
  restore_channel_provider(channel_id) → provider_name
  if persist_provider_overrides:
    load channel_preferences(channel_id, "provider_overrides") → blob
    if blob.len() > 1024: warn, skip
    ProviderOverrides::deserialize(blob) → overrides (discard on error, warn)
    apply overrides to restored provider (skip inapplicable params with warn)
```

### 3.5 Key Invariants

- **NEVER** require a DB schema migration for this feature (use existing key-value row)
- **ALWAYS** validate blob size before persist (prevent unbounded growth)
- **ALWAYS** use `#[serde(deny_unknown_fields)]` on `ProviderOverrides`
- **NEVER** crash on corrupted overrides blob — log and proceed without overrides

---

## 4. Integration Points

Per project rules, all new functionality must wire up:

| Integration Point | Feature A | Feature B |
|-------------------|-----------|-----------|
| CLI arg | `--plugin-url`, `--plugin-sha256` | `--reset-overrides` (optional, low priority) |
| Config section | — | `[session] persist_provider_overrides = true` |
| TUI command palette | `plugin list` shows `[ephemeral]` tag | `/effort` slash command (Phase 2, separate issue) |
| `--init` wizard | N/A (URL is session flag, not persistent config) | Prompt for `persist_provider_overrides` |
| `--migrate-config` | N/A | N/A (no config schema change, only new key) |
| Test playbook | `.local/testing/playbooks/ephemeral-plugins.md` | `.local/testing/playbooks/provider-persistence.md` |
| Coverage status | Add row `ephemeral-plugins` (Untested) | Add row `provider-overrides` (Untested) |

---

## 5. Testing Requirements

### Feature A

- Unit: `validate_url_scheme` with `ephemeral=true` rejects `http://`
- Unit: `add_remote_ephemeral` with fixture archive containing injected SKILL.md → returns error
- Unit: `add_remote_ephemeral` with good archive → `TempDir` returned, skills registered
- Unit: `TempDir` drop cleans up (implicit via `tempfile` crate behavior)
- Integration: full session with `--plugin-url` fixture (HTTPS mock) → skills active

### Feature B

- Unit: `persist_channel_provider` serializes `ProviderOverrides` to correct `pref_key`
- Unit: `restore_channel_provider` restores overrides and applies to provider
- Unit: oversized blob (> 1024 bytes) is discarded without panic
- Unit: blob with unknown field `{"foo": 1}` is rejected by `deny_unknown_fields`
- Unit: inapplicable param (effort on Ollama) logs warn and skips
- Integration: set overrides → `persist` → `restore` → overrides applied

---

## 6. Relationship to Existing Specs

| This spec | Existing spec | Relationship |
|-----------|---------------|-------------|
| Feature A | `[[specs/058-plugins/spec]]` | Extends plugin system with ephemeral variant |
| Feature A | `[[specs/010-security/spec]]` | Must comply with SSRF + path traversal guards |
| Feature B | `[[specs/003-llm-providers/spec]]` | Extends provider persistence; `ProviderOverrides` is a new type in the provider config layer |
| Feature B | `[[specs/021-config-loading/spec]]` | New `persist_provider_overrides` key follows existing config resolution rules |
