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
  - "[[028-runtime-layer/spec]]"
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

## 12. Open Questions

None.

---

## 13. See Also

- [[001-system-invariants/spec]] — system-wide invariants
- [[005-skills/spec]] — skill registry and trust model
- [[008-mcp/spec]] — MCP server lifecycle and server commands
- [[010-security/spec]] — security model and authorization
- [[028-runtime-layer/spec]] — config overlay merge timing
