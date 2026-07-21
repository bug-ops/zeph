---
aliases:
  - Plugin Management
  - Plugin Installation
  - Plugin Lifecycle
  - Plugin Configuration Overlays
tags:
  - sdd
  - spec
  - plugins
  - extensibility
  - skills
  - mcp
created: 2026-05-17
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[005-skills/spec]]"
  - "[[008-mcp/spec]]"
  - "[[010-security/spec]]"
  - "[[027-runtime-layer/spec]]"
---

# Spec: Plugin Management System (`zeph-plugins`)

> [!info]
> Plugin discovery, installation, manifest parsing, config overlay application, and lifecycle
> management for Zeph. Enables users to extend Zeph with third-party skills and MCP servers
> while enforcing security invariants (tighten-only overlays, path traversal prevention,
> skill name conflict detection).

## 1. Overview

### Problem Statement

Zeph needs a way for users to extend the agent with third-party skills and MCP servers. Without
a plugin system, users would need to fork the repository or manually copy files. A plugin system
must:

1. **Validate and install plugins** — with path traversal protection and manifest validation.
2. **Manage skill registration** — prevent name conflicts with bundled and other plugin skills.
3. **Apply tighten-only config overlays** — plugins cannot widen security constraints (e.g., add
   commands to an allowlist); they can only narrow them.
4. **Track plugin integrity** — record SHA-256 digests at install time and verify them at load time.
5. **Provide discovery and listing** — users can inspect installed plugins and their skills.

### Goal

Provide `zeph-plugins` as the authoritative source for plugin lifecycle:
- **Discovery**: scan `~/.local/share/zeph/plugins/` for installed plugins.
- **Installation**: validate manifests, copy trees, register skills, apply overlays.
- **Removal**: unregister skills and delete plugin directory.
- **Listing**: enumerate installed plugins with metadata.
- **Integrity**: verify plugins have not been tampered with since install.

### Out of Scope

- Skill registry and hot-reload (owned by `zeph-skills`)
- MCP server lifecycle and communication (owned by `zeph-mcp`)
- Agent bootstrap and config loading (owned by `zeph-config`)
- TUI plugin management UI (owned by `zeph-tui`)

---

## 2. User Stories

### US-001: Install a Local Plugin

AS A user
I WANT to install a plugin from a local directory
SO THAT I can extend Zeph with third-party skills and MCP servers.

**Acceptance criteria:**

```
GIVEN a local directory containing plugin.toml and skill directories
WHEN plugin add /path/to/plugin is called
THEN the plugin is validated, copied to ~/.local/share/zeph/plugins/<name>/
AND all skills are registered with the skill registry
AND MCP server declarations are recorded
AND config overlay is applied at next startup
AND warnings are emitted if the overlay will have no effect (empty base allowlist)
```

### US-002: Prevent Path Traversal Attacks

AS A security administrator
I WANT plugins to not escape their installation directory
SO THAT a malicious plugin cannot read/modify files outside its scope.

**Acceptance criteria:**

```
GIVEN a plugin manifest with skill path = "../../../etc/passwd"
WHEN plugin add is called
THEN the path is canonicalized
AND if the resolved path escapes the source root, PluginError::InvalidSource is returned
AND the plugin is not installed
```

### US-003: Enforce Tighten-Only Config Overlays

AS A security system
I WANT plugin config overlays to only narrow constraints, never loosen them
SO THAT a malicious plugin cannot re-enable blocked commands or lower detection thresholds.

**Acceptance criteria:**

```
GIVEN a plugin manifest with config overlay keys
WHEN plugin add is called
THEN only safelisted keys are allowed: [tools.blocked_commands, tools.allowed_commands, skills.disambiguation_threshold]
AND tools.blocked_commands overlays grow (union across plugins)
AND tools.allowed_commands overlays shrink (intersection, never wider than base)
AND skills.disambiguation_threshold overlays rise (max across plugins)
AND any non-safelisted key causes PluginError::UnsafeOverlay
```

### US-004: Detect Skill Name Conflicts

AS A plugin manager
I WANT to reject plugins with conflicting skill names
SO THAT the skill registry remains unambiguous.

**Acceptance criteria:**

```
GIVEN a plugin declaring skills with names already in {managed, bundled, other-plugin} sets
WHEN plugin add is called
THEN PluginError::SkillConflict is returned for each conflict
AND the plugin is not installed
```

### US-005: Download and Verify Remote Plugins

AS A user
I WANT to install plugins from HTTP URLs with optional SHA-256 integrity checks
SO THAT I can use plugins from trusted remote sources.

**Acceptance criteria:**

```
GIVEN a plugin URL and optional expected SHA-256 digest
WHEN plugin add_remote(url, Some(digest)) is called
THEN the archive is downloaded
AND the SHA-256 digest is computed and compared
AND if digests mismatch, PluginError::IntegrityCheckFailed is returned
AND if SHA-256 matches, the archive is extracted and installed locally
```

### US-006: Remove an Installed Plugin

AS A user
I WANT to uninstall a plugin and clean up its skills
SO THAT the plugin no longer contributes to the agent.

**Acceptance criteria:**

```
GIVEN an installed plugin name
WHEN plugin remove <name> is called
THEN the plugin directory is deleted from ~/.local/share/zeph/plugins/
AND skills from that plugin are unregistered
AND MCP server declarations are removed
AND the integrity registry entry is cleared
```

### US-007: List Installed Plugins

AS A user
I WANT to see all installed plugins with metadata
SO THAT I know what's available and what features each provides.

**Acceptance criteria:**

```
GIVEN the plugins directory
WHEN plugin list is called
THEN each installed plugin is enumerated
AND name, version, description, path, and skill list are returned
AND plugins are sorted deterministically (by directory name)
AND malformed or unverified plugins are logged but do not block listing
```

### US-008: Prevent .bundled Marker Carryover

AS A security system
I WANT all plugin skills to be treated as non-bundled
SO THAT a plugin's SKILL.md files are not subject to bundled-skill trust hardening.

**Acceptance criteria:**

```
GIVEN an installed plugin directory with residual .bundled markers
WHEN the plugin is loaded
THEN all .bundled markers are stripped recursively during installation
AND the registry treats all plugin skills as hub-installed (non-bundled)
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `plugin add /path` is called THEN `PluginManager::add()` SHALL read `plugin.toml`, parse it as TOML, and validate the manifest structure | must |
| FR-002 | WHEN a plugin manifest is parsed THEN name, version, description, skills[], mcp.servers[], and config are extracted and validated | must |
| FR-003 | WHEN a plugin name is validated THEN it SHALL match the regex `^[a-z0-9][a-z0-9-]*$` (lowercase alphanumeric and hyphens only) | must |
| FR-004 | WHEN a skill path in the manifest is validated THEN its canonical resolved path SHALL be inside the source root (no escapes via ../) | must |
| FR-005 | WHEN a skill path is validated THEN the skill directory SHALL contain a `SKILL.md` file or validation fails with `PluginError::SkillEntryMissing` | must |
| FR-006 | WHEN a config overlay is validated THEN only keys in the safelist `[tools.blocked_commands, tools.allowed_commands, skills.disambiguation_threshold]` are allowed | must |
| FR-007 | WHEN a skill name conflict is detected THEN the name must match against bundled skills (from `bundled_skill_names()`), managed skills, and other installed plugins | must |
| FR-008 | WHEN `PluginManager::check_skill_conflicts()` runs THEN if any conflict exists, `PluginError::SkillConflict` is returned with all conflicting names | must |
| FR-009 | WHEN a plugin is installed THEN its tree is copied to `~/.local/share/zeph/plugins/<name>/` atomically (copy source → copy dest → record registry entry) | must |
| FR-010 | WHEN a plugin directory is copied THEN all `.bundled` markers are recursively stripped via `strip_bundled_markers()` | must |
| FR-011 | WHEN `.plugin.toml` is written THEN it is a copy of the parsed and validated manifest; future overlay loads can read it without re-parsing the source | must |
| FR-012 | WHEN a plugin is installed THEN its SHA-256 digest is computed and stored in the integrity registry as `.plugin-integrity.toml` under `default_runtime_data_root()` (e.g. `~/.local/share/zeph/` on Linux) | must |
| FR-013 | WHEN a plugin config overlay is loaded at agent startup THEN `apply_plugin_config_overlays()` is called with the plugins directory | must |
| FR-014 | WHEN overlays are applied THEN all plugins in deterministic order (sorted by dirname) contribute their safelisted keys to a `ResolvedOverlay` | must |
| FR-015 | WHEN `tools.blocked_commands` overlays are applied THEN they form a union (monotonic growth); the agent's base blocked commands cannot shrink | must |
| FR-016 | WHEN `tools.allowed_commands` overlays are applied THEN they form an intersection with the base; if the base is empty, no plugin can widen it | must |
| FR-017 | WHEN `skills.disambiguation_threshold` overlays are applied THEN the maximum value across all plugins is used; thresholds never decrease | must |
| FR-018 | WHEN plugin integrity is verified at load time THEN the SHA-256 digest stored at install is compared against the current manifest; if mismatch, `VerifyResult::Mismatch` is returned | must |
| FR-019 | WHEN overlay resolution encounters a malformed `.plugin.toml` THEN the plugin is logged at WARN and skipped; the merge continues with remaining plugins | must |
| FR-020 | WHEN `add_remote(url, expected_sha256)` is called THEN the archive is downloaded, SHA-256 is computed and compared, and if match the archive is extracted to a temp dir | must |
| FR-021 | WHEN the archive SHA-256 does not match `expected_sha256` THEN `PluginError::IntegrityCheckFailed` is returned immediately; the archive is never extracted | must |
| FR-022 | WHEN `allowed_commands` overlay will have no effect (base is empty) THEN a non-fatal warning is recorded in `AddResult::warnings` for the user to see | must |
| FR-023 | WHEN skill regex scanning is performed THEN `scan_skill_body()` is called on each SKILL.md; warnings are advisory only and do not block installation | must |
| FR-024 | WHEN `plugin remove <name>` is called THEN the plugin directory is deleted from disk and the integrity registry entry is cleared | must |
| FR-025 | WHEN `plugin list` is called THEN all subdirectories under `plugins_dir` are enumerated; each `.plugin.toml` is parsed to collect metadata | must |
| FR-026 | WHEN `plugin list` enumerates plugins THEN entries are sorted deterministically by directory name (not inode order) | must |
| FR-027 | WHEN `plugin disable <name>` is called AND no enabled plugin depends on it THEN the plugin is disabled and `DisableResult { forced_over_dependents: [] }` is returned | must |
| FR-028 | WHEN `plugin disable <name>` is called AND enabled plugins depend on it AND `force = false` THEN `PluginError::HasDependents` is returned and the plugin is NOT disabled | must |
| FR-029 | WHEN `plugin disable <name> --force` is called THEN the plugin is disabled regardless of dependents and `DisableResult { forced_over_dependents }` lists impacted plugins | must |
| FR-030 | WHEN `add_remote` is called THEN all I/O operations use `tokio::fs` (not `std::fs`) to avoid blocking the async runtime | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | Path traversal checks must canonicalize both source and destination paths and verify inclusion |
| NFR-002 | Security | All plugin manifests are validated against a safelist before being applied to agent config |
| NFR-003 | Security | `.bundled` markers are stripped from all plugin skill trees to prevent bypass of plugin trust model |
| NFR-004 | Security | Skill name conflicts are detected and reported before installation; the operation fails cleanly |
| NFR-005 | Integrity | Plugin archives are verified via SHA-256 before extraction; digests are stored at install time |
| NFR-006 | Integrity | Installed manifests are copied to `.plugin.toml` for future integrity verification without re-reading source |
| NFR-007 | Atomicity | Plugin install operations are atomic at the manifest/registry level: all-or-nothing copying and integrity recording |
| NFR-008 | Observability | All install/remove/list operations emit tracing spans with plugin name and skill/MCP counts |
| NFR-009 | Determinism | Plugin enumeration order is deterministic (sorted by filesystem entry name, not inode order) |
| NFR-010 | Minimalism | The crate has no dependencies on `zeph-core`; it depends only on `zeph-config`, `zeph-skills`, `zeph-common`, and standard I/O libraries |

---

## 5. Data Model

### Core Structures

| Entity | Module | Description |
|--------|--------|-------------|
| `PluginManager` | `manager.rs` | Lifecycle manager: add, remove, list, conflict detection |
| `PluginManifest` | `manifest.rs` | TOML schema: plugin metadata, skills, MCP servers, config overlay |
| `PluginMeta` | `manifest.rs` | Plugin-level metadata: name, version, description |
| `SkillEntry` | `manifest.rs` | Skill declaration: relative path to skill directory |
| `McpSection` | `manifest.rs` | MCP server declarations: ID, command, args |
| `PluginMcpServer` | `manifest.rs` | Single MCP server entry |
| `AddResult` | `manager.rs` | Output of successful installation: name, root path, skills, MCP IDs, warnings |
| `RemoveResult` | `manager.rs` | Output of successful removal: removed skills and MCP IDs |
| `InstalledPlugin` | `manager.rs` | Plugin metadata as returned by `list`: name, version, description, path, skill_names |
| `ResolvedOverlay` | `overlay.rs` | Aggregated config overlay from all plugins: blocked_add, allowed_intersect, threshold_max, source/skipped plugins |
| `IntegrityRegistry` | `integrity.rs` | SHA-256 digest storage: maps plugin name → manifest SHA-256 hash |

### Plugin Directory Layout

```
~/.local/share/zeph/plugins/
├── plugin-a/
│   ├── plugin.toml          (source manifest, user-provided)
│   ├── .plugin.toml         (installed manifest copy, for integrity verification)
│   ├── skills/
│   │   └── skill-name/
│   │       ├── SKILL.md
│   │       └── ...
│   └── ...
├── plugin-b/
│   └── ...
```

### Config Overlay Merge Rules

| Field | Merge Strategy | Rationale |
|-------|---|-----------|
| `tools.blocked_commands` | Union (monotonic growth) | Only add restrictions; never remove existing blocks |
| `tools.allowed_commands` | Intersection with base; base-gated | Narrowing only; if base is empty, remains empty |
| `skills.disambiguation_threshold` | Max across plugins | Raise threshold to prevent ambiguity; never lower |

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Plugin name contains uppercase letters | Rejected; `PluginError::InvalidName` |
| Plugin manifest has syntax error (invalid TOML) | `PluginError::InvalidManifest` with parse error |
| Skill directory does not contain SKILL.md | `PluginError::SkillEntryMissing` |
| Skill path uses `../..` to escape source root | Canonicalization detects escape; `PluginError::InvalidSource` |
| Config overlay key not in safelist | `PluginError::UnsafeOverlay` |
| Skill name conflicts with bundled skill | `PluginError::SkillConflict` |
| Plugin directory already installed | Overwrite with fresh copy (idempotent re-install) |
| Remote download returns 404 | `PluginError::DownloadFailed` |
| Archive SHA-256 mismatch | `PluginError::IntegrityCheckFailed`; archive not extracted |
| `.plugin.toml` write fails after copy | Integrity registry not updated; plugin loads unverified next time (M4 fault tolerance) |
| Overlay resolve encounters missing `.plugin.toml` | Plugin is silently skipped; overlay merge continues with others |
| Overlay resolve encounters unreadable directory | Logged at DEBUG; plugin skipped |
| List enumerates symlink to plugin directory | Symlink is ignored (only real directories from `PluginManager::add` are valid) |
| User modifies `.plugin.toml` after install | Integrity verification fails; logged at WARN; overlay still applied (assume user intention) |
| Empty `skills` or `mcp.servers` list | Valid; plugin may provide only config overlays |
| Large plugin (>1 GB) | Copied as-is (no size limit at plugin level) |
| Plugin removes itself during load | Graceful; next agent restart will not find it |

---

## 7. Integration Points

### Agent Bootstrap (`zeph-core` startup)

1. Config is loaded from `config.toml`.
2. `apply_plugin_config_overlays()` is called with plugins directory.
3. Resolved overlays are applied to the config in-memory.
4. MCP servers and skills are registered based on plugin declarations.

### Skill Registry (`zeph-skills`)

- `bundled_skill_names()` is called to detect conflicts.
- `SkillRegistry::register_hub_dir()` is called for each plugin skill directory.
- Plugin skills are marked as non-bundled (full-trust model for user-installed extensions).

### MCP Server Registration (`zeph-mcp`)

- Plugin-declared MCP servers are validated against `mcp.allowed_commands` from config.
- Server IDs are recorded for lifecycle management (restart required on plugin add/remove).

### CLI Integration (`zeph` binary)

- `/plugin add <source>` — installs a local plugin.
- `/plugin add-remote <url> [sha256]` — installs a remote plugin.
- `/plugin remove <name>` — uninstalls a plugin.
- `/plugin list` — enumerates installed plugins.
- `/plugin disable <name>` — disables a plugin without removing it.
- `/plugin disable <name> --force` — disables a plugin even when other enabled plugins declare it as a dependency. Returns `DisableResult { forced_over_dependents: Vec<String> }` listing the impacted dependents.
- `/plugin enable <name>` — re-enables a previously disabled plugin.

### TUI Integration (`zeph-tui`)

- Plugin management commands in the command palette.
- Plugin listing in a sidebar or status panel.
- Inline warnings when overlay will have no effect.

---

## 8. Key Invariants

### Tighten-Only Invariant

Plugin config overlays can **never widen** security constraints. The safelisted keys enforce this:
- `tools.blocked_commands` — only add to the blocked set.
- `tools.allowed_commands` — only intersect with the base; if base is empty, union is empty.
- `skills.disambiguation_threshold` — only raise, never lower.

**Guarantee**: A plugin cannot re-enable blocked commands or lower security thresholds.

### Skill Conflict Invariant

No two plugins (including bundled skills and managed skills) may declare the same skill name.
Conflicts are detected at install time and cause the installation to fail.

**Guarantee**: The skill registry is unambiguous; each skill name maps to exactly one provider.

### Path Traversal Invariant

All plugin-relative paths (skill directories, file references) must stay within the plugin root.
Canonicalization and inclusion checks prevent `../` escapes.

**Guarantee**: A plugin cannot read or modify files outside its installation directory.

### Integrity Invariant

Every installed plugin has a SHA-256 digest recorded in the integrity registry. At load time,
the manifest's digest is verified to detect tampering.

**Guarantee**: Tampering with plugin manifests is detected (logged at WARN; overlay still applied).

### Deterministic Enumeration Invariant

Plugin directories are enumerated in a deterministic order (sorted by filesystem entry name),
not by inode or creation order. This ensures reproducible overlay merge results across platforms.

**Guarantee**: Config overlay results are identical regardless of filesystem mount order or recovery state.

---

## 9. NEVER Constraints

- **NEVER** install a plugin whose manifest is not in the safelist — all config keys must match
  `["tools.blocked_commands", "tools.allowed_commands", "skills.disambiguation_threshold"]`.
- **NEVER** allow a plugin skill name to conflict with bundled or managed skills — conflicts must
  be detected and reported before installation.
- **NEVER** skip path traversal checks — canonicalize both paths and verify inclusion before copying.
- **NEVER** widen `tools.allowed_commands` beyond the base allowlist — plugins can only narrow it.
- **NEVER** lower `skills.disambiguation_threshold` — only raise it.
- **NEVER** leave `.bundled` markers in installed plugin directories — strip them recursively
  during installation.
- **NEVER** panic on malformed plugin manifests — log and skip the plugin; continue with others.
- **NEVER** write plugin files to disk before manifest validation — all checks happen before copying.
- **NEVER** extract a remote plugin archive if SHA-256 verification fails — compare first, then extract.
- **NEVER** allow symlinks in the plugins directory to contribute to overlay merge — only real
  directories installed by `PluginManager::add` are valid sources.

---

## 10. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Path traversal protection | Unit tests verify that `../` escapes are rejected for all skill paths |
| SC-002 | Conflict detection | Unit tests confirm that bundled, managed, and plugin skill conflicts are all detected |
| SC-003 | Overlay tightening | Integration test verifies union, intersection, and max semantics for overlay merge |
| SC-004 | Determinism | `plugin list` returns plugins in sorted order; overlay merge is reproducible across runs |
| SC-005 | Integrity verification | Live test installs a plugin, modifies its manifest, and confirms detection at load time |
| SC-006 | Error handling | All error types are handled; no panics on invalid manifests or missing directories |
| SC-007 | .bundled stripping | Unit test confirms all `.bundled` markers are removed from installed plugin trees |

---

## 11. Agent Boundaries

### Always (without asking)
- Validate plugin names, skill paths, and config keys before copying any files
- Emit tracing spans for install/remove/list with plugin names and counts
- Deterministically sort plugin directories during enumeration
- Strip all `.bundled` markers during installation

### Ask First
- Changing the safelist of allowed config overlay keys
- Modifying overlay merge semantics (union/intersection/max rules)
- Adding new plugin manifest fields or requirements

### Never
- Import from `zeph-core`
- Allow symlinks in the plugins directory to contribute to merges
- Panic on malformed manifests — log and skip
- Copy files before manifest validation
- Extract remote archives before SHA-256 verification

---

## 12. Auto-Update Policy (#3902, #4252, #4289)

`PluginMeta::auto_update: bool` (default `false`) is an opt-in field declared in the
plugin manifest. When `true`, `PluginManager::enforce_auto_update()` checks for updates
on startup and schedules periodic refresh.

### Behavior

- `auto_update = false` (default): plugin is never auto-updated; manual update only
- `auto_update = true`: `PluginManager` checks for a newer version on every startup
  and updates in the background via `spawn_blocking`
- Update check is non-blocking — startup proceeds regardless of update outcome

### `add_remote()` Fix (#4318)

The blocking `add()` call inside `add_remote()` is now dispatched via
`tokio::task::spawn_blocking` to prevent stalling the async executor on disk I/O during
remote plugin installation.

### Key Invariants

- `auto_update` is declared in the plugin manifest, not in agent config — plugin author controls this
- Auto-update MUST NOT overwrite a locally-modified plugin without re-verifying the integrity hash
- NEVER perform `add_remote()` synchronously on the async executor thread

---

## 13. Plugin Dependency Enforcement (#4312)

Plugins can declare `requires` dependencies in their manifest. `PluginManager` enforces
these before activation.

### Dependency Types

- `skills`: list of skill names that must be active
- `mcp_servers`: list of MCP server names that must be configured
- `features`: list of compile-time feature flags that must be enabled

### Enforcement

`PluginManager::activate()` checks all declared dependencies before injecting the plugin's
config overlay and skills. Missing dependencies result in `PluginError::DependencyNotMet`
with a human-readable message listing which dependencies failed.

### Key Invariants

- Dependency check runs BEFORE config overlay merge — a plugin with unsatisfied deps never modifies agent config
- NEVER activate a plugin with unresolvable skill or MCP dependencies silently

---

## 14. Projected Context Cost in TUI (#4312)

The TUI Plugin panel displays a **projected context cost** for each active plugin:
estimated tokens the plugin's skills and config will consume per turn.

| Column | Description |
|--------|-------------|
| Name | Plugin name |
| Skills | Count of active skills from this plugin |
| Projected Cost | Estimated token cost per turn |
| Status | `active`, `inactive`, `error` |

This helps operators identify token-heavy plugins before they cause context pressure.

---

## 15. Multiple `--plugin-url` Values and `PluginName` Newtype (#4675, #4674, #4680)

### Multiple `--plugin-url` Values

The `--plugin-url` CLI flag now accepts multiple values in a single invocation:

```bash
zeph --plugin-url https://example.com/plugin-a.tar.gz \
     --plugin-url https://example.com/plugin-b.tar.gz
```

Each URL is validated and downloaded in sequence. Validation enforces HTTPS-only (HTTP is
rejected at parse time). Download, extraction, and installation follow the same
`add_remote_ephemeral` path as single-URL invocation.

### `PluginName` Newtype

`PluginName(Arc<str>)` replaces `String` as the canonical type for plugin names throughout the
crate. The newtype:
- Derives `Clone`, `Eq`, `Hash`, `Display`, `Debug`, `Serialize`, `Deserialize`
- Validates the plugin name regex `^[a-z0-9][a-z0-9-]*$` in its constructor
- Provides `as_str() -> &str` for low-overhead string access

All internal maps keyed on plugin names use `PluginName` instead of `String`.

### `add_remote_ephemeral` Async Fix (#4845)

`add_remote_ephemeral` previously used `std::fs` reads inside an async function. All file
I/O in `add_remote_ephemeral` is now `tokio::fs` to prevent blocking the async executor.

### `.bundled` Marker Stripping in Ephemeral Plugins (#4676)

`extract_archive_safe` now also strips `.bundled` marker files from ephemeral plugin archives
during extraction, before any skill loading. This prevents a scenario where an ephemeral plugin
bundle includes `.bundled` markers that would incorrectly grant `Trusted` trust to its skills.

### Key Invariants

- `--plugin-url` MUST reject non-HTTPS URLs — `http://` is a hard block, not a warning
- `PluginName` constructor MUST validate the regex — NEVER create a `PluginName` from a string that has not been validated
- `add_remote_ephemeral` MUST use `tokio::fs` — NEVER use `std::fs` inside an async fn
- `.bundled` markers MUST be stripped during ephemeral extraction — NEVER load ephemeral plugins with bundled trust

---

## 16. Install-Time Reputation and Typosquat Scanning (spec-043, #5864)

### Problem Statement

None of the existing install-time checks (§§1–15) evaluate a plugin's **name or source
identity** against any reputation signal — all are structural (path/archive safety) or
content-pattern-based (regex injection scan on skill body text). There was no check for
whether an incoming plugin's declared name is a likely typosquat of a bundled or
already-installed plugin/skill name (competitive parity gap vs. IronClaw's "Community
Scanner"; originally scoped in the draft spec at
`.local/specs/043-plugin-install-reputation-scanning/spec.md`, CI-1267).

### `ReputationSource` Trait Contract

```rust
pub trait ReputationSource: Send + Sync {
    fn check(&self, name: &str, known_names: &[(String, MatchedSource)]) -> Vec<ReputationWarning>;
}
```

**What implementors must guarantee:**
- `check` is advisory-only: it reports near-matches and never decides whether an install is
  blocked. Enforcement (`warn` vs `block`) is a caller-side policy applied by
  `PluginManager`, not the source.
- `check` is synchronous and must complete within the install-time budget (NFR-003-equivalent:
  sub-millisecond against a small local name set). No network I/O for the default/only
  built-in implementation (`LocalTyposquatCheck`); an opt-in future external source could add
  network calls, but that requires explicit user configuration (Ask-First boundary).
- `check` must be side-effect-free and safe to call with an empty `known_names` slice
  (returns an empty `Vec`, never panics).

**What callers may assume:**
- `PluginManager::add`, `add_remote_ephemeral`, and the auto-update path
  (`apply_staged_update`) all invoke the *same* configured `ReputationSource` instance
  (threaded via `Arc<dyn ReputationSource>`), preserving the #5401 call-site-parity invariant.
- When `PluginManager.reputation` is `None` (default), the corpus is never assembled and
  `check` is never called — zero cost when disabled.
- `MatchedSource` is `#[non_exhaustive]`; callers formatting warnings must handle unknown
  future variants (e.g., a later `External(String)` source-provenance variant) without a
  hard match.

### Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a plugin manifest is validated for install (`add`, `add_remote_ephemeral`) or staged for auto-update (`apply_staged_update`) THE SYSTEM SHALL compare the plugin's declared name and each declared skill name against a corpus of bundled skill names (`bundled_skill_names()`), managed skill names, and other installed plugins' names/skill names using a Levenshtein-based similarity ratio | must |
| FR-002 | WHEN a near-match is found at or above `similarity_threshold` THE SYSTEM SHALL surface a `ReputationWarning` identifying the matched name, its `MatchedSource`, and the similarity ratio, without failing the install by default (`enforcement = "warn"`) | must |
| FR-003 | WHEN `[plugins.reputation].enabled = false` or no `ReputationSource` is attached THE SYSTEM SHALL skip the check entirely — the corpus is not assembled and the install proceeds using only the existing content-based checks (NFR-002: zero cost when disabled) | must |
| FR-004 | THE SYSTEM SHALL expose the check as the `ReputationSource` trait so a future external source can be attached via `PluginManager::with_reputation` without changing any of the three call sites | should |
| FR-005 | THE default (and only built-in) `ReputationSource`, `LocalTyposquatCheck`, SHALL make zero network calls; an external source requires explicit user configuration to enable network access (not implemented — trait boundary only) | must |
| FR-006 | THE SYSTEM SHALL NOT hard-block an install/update based on a reputation warning unless `[plugins.reputation].enforcement = "block"` is configured, or `--strict-reputation` is passed to `zeph plugin add` for that invocation | must |
| FR-007 | WHEN `enforcement = "block"` and at least one warning is produced THE SYSTEM SHALL return `PluginError::ReputationBlocked` before any file is written (`add`) or staged content is swapped (`apply_staged_update`), and log every warning at `WARN` regardless of enforcement mode | must |
| FR-008 | Raw byte-identical names (`checked_name == known_name`) SHALL NOT warn — this is treated as a legitimate re-install/update, not a typosquat; the exact-match guard is applied on the original strings before the hyphen-stripped similarity pass, so a hyphen-stripped pair that reaches `1.0` similarity without being raw-equal still warns (catches separator-removal squats like `gitpr` vs `git-pr`) | must |
| FR-009 | Comparisons where the shorter of the two compared names has fewer than `min_name_len` characters SHALL be skipped as noise | must |

### Config Surface (`[plugins.reputation]`)

```toml
[plugins.reputation]
enabled = true               # advisory typosquat check at install (local, zero network)
similarity_threshold = 0.65  # [0,1]; higher = require closer match to warn = fewer warnings
min_name_len = 3             # skip comparisons where the shorter name has fewer chars
enforcement = "warn"         # "warn" (advisory, default) | "block" (opt-in hard gate)
```

- `enabled` (bool, default `true`) — advisory-only and zero-network, so on-by-default is
  consistent with the existing `scan_skill_entries` posture.
- `similarity_threshold` (f32, default `0.65`) — the loosest value that still catches the
  motivating `github-pr`/`git-pr` example (similarity 0.667) with margin, while producing zero
  self-collisions among Zeph's own bundled skill names.
- `min_name_len` (usize, default `3`) — covers the bundled `git` skill; names of 1–2 characters
  are always skipped regardless of this setting.
- `enforcement` (`"warn"` | `"block"`, default `"warn"`) — `"block"` refuses the install/update
  before any file is written or swapped. `zeph plugin add --strict-reputation` forces `block`
  for a single invocation without persisting a config change.

**Integration points:** CLI (`--strict-reputation` flag on `plugin add`), TUI (`AddResult.warnings`
surfaced + `Checking plugin reputation…` spinner status, per TUI Rules), `--init` wizard prompt,
`--migrate-config` Step 98 (commented advisory block), playbook
(`.local/testing/playbooks/plugins.md` Scenario G, 9 sub-scenarios including G9 for the
`--plugin-url` path), `coverage-status.md` row.

### Call Sites (all three, per #5401 parity invariant)

1. **`PluginManager::add`** (`install.rs`) — after `validate_manifest_for_install` and
   `collect_skill_names`, before `copy_dir_all`. Warnings pushed to `AddResult.warnings`.
2. **`PluginManager::add_remote_ephemeral`** (`registry.rs`, `--plugin-url` path) — after
   manifest parse and skill-name collection, before the injection scan. Mirrors `add`'s pattern
   exactly; wired at `src/runner.rs` via `.with_reputation_config(&config.plugins.reputation, false)`.
3. **`apply_staged_update`** (`registry.rs`, auto-update path) — a free function; the
   `ReputationSource` and enforcement posture are threaded in as explicit parameters (cloned
   `Arc`/`Copy` enum) from `check_auto_updates`'s `spawn_blocking` closure, rather than relying
   on the throwaway `tmp_mgr` built for the conflict check (which would otherwise silently never
   run the check — the same call-site-divergence class as #5401).

### Key Invariants

- **Zero cost when disabled**: `PluginManager.reputation == None` (the default) means the
  known-names corpus is never assembled and `ReputationSource::check` is never called on any
  of the three call sites.
- **Call-site parity**: all three install/update entry points run the check through the *same*
  configured `ReputationSource` instance and the *same* `reputation_enforcement` value — no
  entry point may silently use a different or absent source.
- **Advisory by default**: `enforcement = "warn"` is the shipped default; a warning never blocks
  an install unless the operator has explicitly opted into `"block"` (config or
  `--strict-reputation`).
- **No persistent storage**: `ReputationWarning` is a transient install-time result, never
  written to the integrity registry or any other persistent store.

### NEVER Constraints

- **NEVER** make a network call from the default (`LocalTyposquatCheck`) reputation check.
- **NEVER** hard-block an install/update on a reputation warning without explicit
  `enforcement = "block"` configuration or `--strict-reputation`.
- **NEVER** assemble the known-names corpus or invoke `ReputationSource::check` when
  `[plugins.reputation].enabled = false` or no source is attached.
- **NEVER** run the check on only a subset of the three install/update call sites — all three
  must share the same configured source and enforcement posture.

### Out of Scope (unchanged from the draft spec)

- A live/hosted community reputation registry or database — only the local heuristic ships;
  the `ReputationSource` trait boundary exists for a future opt-in external source.
- Dependency-graph reputation scanning (IronClaw's other "Community Scanner" input) — deferred
  to a separate follow-up spec.
- Homoglyph/Unicode-confusable detection — plugin names are ASCII-only by
  `validate_plugin_name`, structurally precluding this for the plugin-name axis; skill names
  are a documented future refinement.

## 17. Open Questions

None.

---

## 18. See Also

- [[001-system-invariants/spec]] — system-wide invariants
- [[005-skills/spec]] — skill registry and trust model
- [[008-mcp/spec]] — MCP server lifecycle and server commands
- [[010-security/spec]] — security model and authorization
- [[027-runtime-layer/spec]] — config overlay merge timing
