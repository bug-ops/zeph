---
aliases:
  - Parity Plan 3918
  - Claude Code Parity Implementation Plan
tags:
  - sdd
  - plan
  - parity
  - plugins
  - provider-persistence
created: 2026-05-29
status: approved
related:
  - "[[specs/parity-claude-code-3918/spec]]"
  - "[[specs/parity-claude-code-3918/tasks]]"
---

# Implementation Plan: Claude Code v2.1.141–v2.1.143 Parity (GitHub #3918)

## Recommended Implementation Order

**Phase 1: Provider override persistence (Feature B)** — implement first.

Rationale (from architect and critic both agree):
- Self-contained: extends existing `provider_cmd.rs` and `channel_preferences` table
- No new network I/O or security surface
- No dependency on other in-flight work
- Estimated effort: ~150 LOC across 3 files

**Phase 2: `--plugin-url` ephemeral loading (Feature A)** — implement second.

Rationale:
- Builds on existing `add_remote` infrastructure
- Security surface (HTTPS gate, blocking scan) benefits from Phase 1 being merged and stable
- Estimated effort: ~200 LOC across 5 files (including new `PluginError::InsecureUrl`)

**Phase 3: Deferred gap issues** — file GitHub issues for deferred gaps after Phase 1+2 merge.

---

## Phase 1: Provider Override Persistence

### P1-1: Add `ProviderOverrides` struct

**File:** `crates/zeph-config/src/providers.rs`

Add:
```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderOverrides {
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
}

impl ProviderOverrides {
    pub fn is_empty(&self) -> bool {
        self.reasoning_effort.is_none() && self.temperature.is_none()
    }
}
```

### P1-2: Add config gate

**File:** `crates/zeph-config/src/session.rs` (or equivalent session config file)

Add `persist_provider_overrides: bool` field (default `true`) to `SessionConfig`.

### P1-3: Extend provider persistence

**File:** `crates/zeph-core/src/agent/provider_cmd.rs`

- `persist_channel_provider()`: after persisting provider name, if `persist_provider_overrides && !overrides.is_empty()`, serialize overrides to JSON (assert `len() <= 1024`), upsert `pref_key = "provider_overrides"`
- `restore_channel_provider()`: after restoring provider name, load `pref_key = "provider_overrides"`, validate size, deserialize with `deny_unknown_fields`, apply to provider (skip inapplicable params with `tracing::warn!`)

### P1-4: Wire `--init` wizard

**File:** `src/main.rs` or wizard module

Add prompt for `persist_provider_overrides` to the interactive configuration wizard (`--init`).

### P1-5: Update `--migrate-config`

No migration needed. Confirm in code that no schema change is made.

### P1-6: Tests

- `crates/zeph-core/src/agent/provider_cmd.rs` (inline `#[cfg(test)]`)
  - `test_persist_restore_overrides`
  - `test_oversized_blob_discarded`
  - `test_unknown_field_rejected`
  - `test_inapplicable_param_skipped`

### P1-7: Playbook + coverage-status

- Create `.local/testing/playbooks/provider-persistence.md`
- Add row to `.local/testing/coverage-status.md` (Untested)

---

## Phase 2: `--plugin-url` Ephemeral Plugin Loading

### P2-1: Add `PluginError::InsecureUrl`

**File:** `crates/zeph-plugins/src/error.rs`

Add variant:
```rust
#[error("plugin URL must use HTTPS scheme, got: {0}")]
InsecureUrl(String),
```

### P2-2: Update `validate_url_scheme`

**File:** `crates/zeph-plugins/src/manager.rs`

Add `ephemeral: bool` parameter. When `ephemeral = true`, reject `http://` with `PluginError::InsecureUrl`. Existing callers pass `ephemeral = false` (no behavior change).

Alternatively: create a separate `validate_url_scheme_ephemeral()` to avoid touching existing callers' signatures.

### P2-3: Extract shared download/extract helper

**File:** `crates/zeph-plugins/src/manager.rs`

Refactor: extract `download_and_extract(url, sha256, dest: &Path, strict_scan: bool) -> Result<AddResult>` as a private helper used by both `add_remote` and `add_remote_ephemeral`.

### P2-4: Add `add_remote_ephemeral`

**File:** `crates/zeph-plugins/src/manager.rs`

```rust
pub async fn add_remote_ephemeral(
    &self,
    url: &str,
    sha256: Option<&str>,
) -> Result<(AddResult, TempDir)>
```

Implementation:
1. Call `validate_url_scheme_ephemeral(url)` → error on non-HTTPS
2. Create `TempDir`
3. Call `download_and_extract(url, sha256, tempdir.path(), strict_scan=true)`
4. Return `(result, tempdir)` — caller holds ownership

### P2-5: Update CLI

**File:** `src/cli.rs`

Add to top-level `Args`:
```rust
#[arg(long)]
plugin_url: Option<String>,

#[arg(long)]
plugin_sha256: Option<String>,
```

### P2-6: Update `AgentBuilder` and `AgentRuntime`

**Files:**
- `crates/zeph-core/src/agent/builder.rs`: add `with_ephemeral_plugins(plugins: Vec<TempDir>) -> Self`
- `crates/zeph-core/src/agent/state/mod.rs`: add `ephemeral_plugins: Vec<TempDir>` to `AgentRuntime`

### P2-7: Bootstrap wiring

**File:** `src/main.rs`

After config load, if `args.plugin_url.is_some()`:
1. Call `plugin_manager.add_remote_ephemeral(url, sha256).await`
2. Register returned skills in `SkillRegistry`
3. Register returned MCP servers in MCP lifecycle
4. Pass `TempDir` to `AgentBuilder::with_ephemeral_plugins`

### P2-8: `plugin list` display

**File:** `crates/zeph-commands/src/plugin_list.rs` (or wherever `plugin list` is handled)

For each ephemeral plugin in `AgentRuntime::ephemeral_plugins`, display with `[ephemeral]` tag.

### P2-9: Tests

- `crates/zeph-plugins/src/manager.rs` (inline `#[cfg(test)]`)
  - `test_validate_url_scheme_ephemeral_rejects_http`
  - `test_add_remote_ephemeral_blocks_on_scan_failure`
  - `test_add_remote_ephemeral_succeeds_with_good_archive`
  - `test_tempdir_drops_on_agent_exit`

### P2-10: Playbook + coverage-status

- Create `.local/testing/playbooks/ephemeral-plugins.md`
- Add row to `.local/testing/coverage-status.md` (Untested)

---

## Phase 3: Deferred Gap Issues

File three GitHub issues (see `tasks.md`):
- `worktree.baseRef` config (P3)
- `worktree.bgIsolation: none` (P3)
- Ctrl+R cross-project history search (P3)

---

## Pre-Merge Checklist

- [ ] `cargo +nightly fmt --check`
- [ ] `cargo clippy --workspace --all-features -- -D warnings`
- [ ] `cargo nextest run --config-file .github/nextest.toml --workspace --all-features --lib --bins`
- [ ] `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-plugins -p zeph-core -p zeph-config`
- [ ] `cargo test --doc -p zeph-plugins -p zeph-core -p zeph-config`
- [ ] RUSTFLAGS="-D warnings" cargo check --workspace --all-targets
- [ ] Update `CHANGELOG.md` (`[Unreleased]` section)
- [ ] Feature B does NOT touch LLM serialization paths — no live API test required
- [ ] Feature A download path is network-bound — integration test uses mock HTTP server
