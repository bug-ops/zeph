# zeph-plugins

[![Crates.io](https://img.shields.io/crates/v/zeph-plugins)](https://crates.io/crates/zeph-plugins)
[![docs.rs](https://img.shields.io/docsrs/zeph-plugins)](https://docs.rs/zeph-plugins)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Plugin packaging, installation, and runtime config overlay for Zeph.

## Overview

Manages the full lifecycle of Zeph plugin packages: installing from a path or URL, removing, listing, and applying tighten-only config overlays at bootstrap and on hot-reload. Each plugin lives in a subdirectory under the managed plugins root and may bundle skills, MCP server definitions, and a `plugin.toml` config overlay. Security invariants are enforced at both install time and load time.

## Key types

| Type | Description |
|------|-------------|
| `PluginManager` | Install, remove, list, enable/disable, and auto-update plugins in the managed directory |
| `PluginManifest` | Parsed `plugin.toml` manifest (`[plugin]` metadata, `[[skills]]`, `[[mcp.servers]]`, `[config]` overlay) |
| `ResolvedOverlay` | Result of merging all installed plugin overlays into the live config |
| `PluginName` | Validated plugin identifier (`[a-z0-9][a-z0-9-]*`) |
| `PluginError` | Typed error enum (`InvalidManifest`, `InvalidName`, `UnsafeOverlay`, `SkillNameConflict*`, `NotFound`, `Io`) |
| `ReputationSource` | Trait for a pluggable install-time typosquat check; `check(name, known_names)` returns near-matches |
| `LocalTyposquatCheck` | Built-in `ReputationSource` — zero-network, Levenshtein-similarity check against bundled, managed, and other installed plugins' skill names |
| `ReputationWarning` / `ReputationEnforcement` | A single near-match result; enforcement posture (`Warn` default / `Block`) applied when at least one warning is returned |

## Reputation and typosquat scanning

`PluginManager::add` and `apply_staged_update` run an advisory, local-only name-similarity check against every incoming plugin/skill name before install or auto-update, guarding against typosquat-style names that closely resemble a bundled or already-installed skill:

```toml
[plugins.reputation]
enabled               = true    # zero-network Levenshtein check, on by default
similarity_threshold  = 0.65    # [0, 1]; higher = stricter = fewer warnings
min_name_len          = 3       # names shorter than this are skipped as noise
enforcement           = "warn"  # "warn" (advisory, default) | "block" (refuse install/update)
```

```bash
# Escalate to a hard block for a single invocation without changing the persisted config
zeph plugin add ./path/to/my-plugin --strict-reputation
```

> [!NOTE]
> The check is entirely local (no network calls): it compares names via Levenshtein similarity against `zeph_skills::bundled::bundled_skill_names()`, the managed skills directory, and other installed plugins' skill names. It never blocks by default — `enforcement = "warn"` surfaces the warning and lets the install proceed.

## Key modules

| Module | Description |
|--------|-------------|
| `manager` | `PluginManager` — install/remove/list with path-traversal defense (`canonicalize + starts_with(root)`), recursive `.bundled` marker stripping, symlink skip, and atomic install-then-verify |
| `manifest` | `plugin.toml` schema (`PluginManifest`, `PluginMeta`, `SkillEntry`, `McpSection`) |
| `overlay` | `apply_plugin_config_overlays` — scans installed plugins, validates overlays, and merges tighten-only keys into the live `Config` struct |
| `marketplace` | `RegistryClient` trait, `RegistryEntry`, `PackageArchive`, `RegistryError` — skill/plugin discovery-and-install marketplace backing `zeph plugin search`/`get`; always compiled, opt-in only via `[skills.registry] enabled` config |
| `error` | `PluginError` typed error enum |
| `types` | `PluginName` validated identifier |

## Plugin format

A plugin is a directory with the following layout:

```
my-plugin/
    plugin.toml           # required: manifest and config overlay
    skills/               # optional: bundled SKILL.md files
```

Minimal `plugin.toml`:

```toml
[plugin]
name        = "my-plugin"
version     = "1.0.0"
description = "Does something useful"
auto_update = false
# Optional: names of other installed plugins this one requires.
# Install validates the count (max 64) and each name; the graph itself
# is walked by enable/disable. See "Enable, disable, and dependencies".
dependencies = ["my-base-plugin"]

[[skills]]
path = "skills/my-skill"

[[mcp.servers]]
id      = "git"
command = "mcp-git"
args    = ["--repo", "."]

# Tighten-only config overlay. All keys are optional.
# config.tools.blocked_commands is merged via union (plugin can only add to the blocklist)
# config.tools.allowed_commands is merged via intersection (plugin can only narrow the allowlist)
# config.skills.disambiguation_threshold is merged via max (plugin can only raise the threshold)
[config.tools]
blocked_commands = ["curl", "wget"]

[config.skills]
disambiguation_threshold = 0.25
```

> [!IMPORTANT]
> Keys outside the safelist (`tools.blocked_commands`, `tools.allowed_commands`, `skills.disambiguation_threshold`) are rejected at install time with `PluginError::UnsafeOverlay`. Plugins cannot widen the command allowlist — if the base allowlist is empty, `allowed_commands` intersection is a no-op.

## Install and manage plugins

### CLI

```bash
# Install a plugin from a local directory (must contain plugin.toml)
zeph plugin add ./path/to/my-plugin

# List installed plugins
zeph plugin list

# List installed plugins plus the active config overlay (contributors and skip reasons)
zeph plugin list --overlay

# Remove a plugin
zeph plugin remove my-plugin
```

> [!NOTE]
> `zeph plugin add` takes a **local directory path only** — a URL is rejected with `PluginError::InvalidSource`. Remote installs go through the marketplace (`zeph plugin get <registry-id>`, below) or the auto-update path, both of which run the archive through `extract_archive_safe` (size cap, symlink rejection, tar-slip checks). The crate API exposes `PluginManager::add_remote` / `add_remote_ephemeral` for embedders that need remote install directly.

### TUI slash commands

```text
/plugins list            # list installed plugins (includes ephemeral ones)
/plugins list --overlay  # show the active config overlay
/plugins overlay         # same as above
/plugins add <path>      # install a plugin from a local directory
/plugins remove <name>   # uninstall a plugin
```

### Marketplace discovery (opt-in)

```bash
# Requires [skills.registry] enabled = true in config.toml
zeph plugin search <query>
zeph plugin get <registry-id>
```

Backed by the `marketplace` module's `RegistryClient` trait (default backend: `skills.sh`). Disabled
by default (`FR-004`): when `skills.registry.enabled = false`, both subcommands print an actionable
opt-in message and make zero network calls.

## Enable, disable, and dependencies

`PluginManager::enable` / `disable` toggle a `.disabled` marker file inside the installed plugin
directory, keeping the package on disk. Both are crate-API only — there is no `zeph plugin
enable`/`disable` subcommand or `/plugins` slash equivalent yet.

The optional `[plugin] dependencies` list is the graph these two walk:

- `enable(name)` recursively enables every declared dependency depth-first, rejecting an absent one
  with `PluginError::MissingDependency` and a cyclic graph with `PluginError::DependencyCycle` —
  both detected *before* any filesystem write. Enabling an already-enabled plugin is a no-op.
- `disable(name, force)` refuses with `PluginError::DependencyRequired` when some other *enabled*
  plugin still declares `name` as a dependency; `force = true` overrides that guard.

> [!NOTE]
> Dependencies are not resolved at install time — `zeph plugin add` never fetches a missing
> dependency for you. The manifest is only bounded at install (at most 64 entries, per
> `MAX_DEPENDENCIES`, to cap recursive `enable()` fan-out); the graph itself is walked on enable.

## Config overlay merge

At bootstrap (`AppBuilder::new`) and on hot-reload (`reload_config`), `apply_plugin_config_overlays` is called to merge all installed plugin overlays into the live `Config`. The merge is deterministic: plugins are processed in directory-sorted order to ensure reproducible results.

`ResolvedOverlay` carries diagnostic fields:

| Field | Description |
|-------|-------------|
| `source_plugins` | Names of plugins whose overlay contributed at least one safelisted value |
| `skipped_plugins` | Plugins skipped (validation failure, I/O error), each as `"<name>: <reason>"` |
| `blocked_commands_add` | Sorted, de-duplicated union of all plugin `blocked_commands` contributions |
| `allowed_commands_intersect_accum` | Accumulated intersection of `allowed_commands` (`None` if no plugin supplied it) |
| `disambiguation_threshold_max` | Max of all `disambiguation_threshold` contributions (`None` if none supplied) |

> [!WARNING]
> Hot-reload applies `skills.disambiguation_threshold` immediately. Changes to `tools.blocked_commands` or `tools.allowed_commands` require an agent restart to take full effect — the live `ShellExecutor` is built once at startup. A banner is emitted in the status channel when a restart is required.

## Security model

- **Install-time validation** — overlay keys are checked against the safelist; unsafe keys abort the install with a clear error.
- **Load-time re-validation** — the safelist check is re-run at every bootstrap and hot-reload as defence-in-depth against post-install tampering.
- **`.bundled` marker stripping** — `.bundled` marker files in the installed package are stripped recursively to prevent trust escalation; if stripping fails, the partial install is cleaned up before propagating the error.
- **Symlink skip** — symlinks in the source package are never copied, preventing symlink-based path traversal.
- **Path traversal defense** — all install paths are canonicalized and verified to remain inside the managed root.
- **Manifest integrity registry** — a sha256 digest of each installed `plugin.toml` is recorded in `<data_root>/.plugin-integrity.toml`; a mismatch at load time is a tamper-detection hint, not a cryptographic guarantee (an attacker with write access to `data_root` can modify both files).
- **Reputation/typosquat scanning** — see [Reputation and typosquat scanning](#reputation-and-typosquat-scanning) above.

## Installation

```bash
cargo add zeph-plugins
```

Enabled automatically when the `zeph-plugins` crate is a dependency of the root `zeph` binary.

## Feature flags

`zeph-skills`/`zeph-tools` (and transitively `zeph-db`) require a database backend to compile, so exactly one of `sqlite`/`postgres` must be enabled:

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend — the default, lets the crate build in isolation |
| `postgres` | no | PostgreSQL backend for PostgreSQL deployments (#4956) |
| `mock` | no | Exposes `marketplace::mock::MockRegistryClient` outside `#[cfg(test)]` for downstream crates' `dev-dependencies` |

The `marketplace` module body backing `zeph plugin search`/`get` (spec-045) compiles
unconditionally — the `registry` feature was removed in the 2026-08 feature-flag audit (it gated
no real optional dependency; `reqwest`'s `query` sub-feature is now unconditional on the
workspace `reqwest` dependency).

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
