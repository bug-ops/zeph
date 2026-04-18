# zeph-plugins

[![Crates.io](https://img.shields.io/crates/v/zeph-plugins)](https://crates.io/crates/zeph-plugins)
[![docs.rs](https://img.shields.io/docsrs/zeph-plugins)](https://docs.rs/zeph-plugins)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

Plugin packaging, installation, and runtime config overlay for Zeph.

## Overview

Manages the full lifecycle of Zeph plugin packages: installing from a path or URL, removing, listing, and applying tighten-only config overlays at bootstrap and on hot-reload. Each plugin lives in a subdirectory under the managed plugins root and may bundle skills, MCP server definitions, and a `.plugin.toml` overlay fragment. Security invariants are enforced at both install time and load time.

## Key types

| Type | Description |
|------|-------------|
| `PluginManager` | Install, remove, and list plugins in the managed directory |
| `PluginMeta` | Parsed `.plugin.toml` manifest (name, version, description, overlay) |
| `ResolvedOverlay` | Result of merging all installed plugin overlays into the live config |
| `PluginError` | Typed error enum (Io, Parse, UnsafeOverlay, Conflict, PathTraversal) |

## Key modules

| Module | Description |
|--------|-------------|
| `manager` | `PluginManager` — install/remove/list with path-traversal defense (`canonicalize + starts_with(root)`), recursive `.bundled` marker stripping, symlink skip, and atomic install-then-verify |
| `overlay` | `apply_plugin_config_overlays` — scans installed plugins, validates overlays, and merges tighten-only keys into the live `Config` struct |
| `validate` | Install-time and load-time safelist validation for overlay keys |

## Plugin format

A plugin is a directory with the following layout:

```
my-plugin/
    .plugin.toml          # required: manifest and config overlay
    skills/               # optional: bundled SKILL.md files
    mcp.toml              # optional: additional MCP server definitions
```

Minimal `.plugin.toml`:

```toml
[plugin]
name        = "my-plugin"
version     = "1.0.0"
description = "Does something useful"

[overlay]
# All keys are optional. Only tighten-only operations are permitted.
# tools.blocked_commands is merged via union (plugin can only add to the blocklist)
# tools.allowed_commands is merged via intersection (plugin can only narrow the allowlist)
# skills.disambiguation_threshold is merged via max (plugin can only raise the threshold)
tools.blocked_commands         = ["curl", "wget"]
skills.disambiguation_threshold = 0.25
```

> [!IMPORTANT]
> Keys outside the safelist (`blocked_commands`, `allowed_commands`, `disambiguation_threshold`) are rejected at install time with `PluginError::UnsafeOverlay`. Plugins cannot widen the command allowlist — if the base allowlist is empty, `allowed_commands` intersection is a no-op.

## Install and manage plugins

### CLI

```bash
# Install a plugin from a local path or URL
zeph plugin add ./path/to/my-plugin
zeph plugin add https://example.com/my-plugin.tar.gz

# List installed plugins
zeph plugin list

# Remove a plugin
zeph plugin remove my-plugin
```

### TUI slash commands

```text
/plugins list            # list installed plugins
/plugins add <path|url>  # install a plugin
/plugins remove <name>   # uninstall a plugin
```

## Config overlay merge

At bootstrap (`AppBuilder::new`) and on hot-reload (`reload_config`), `apply_plugin_config_overlays` is called to merge all installed plugin overlays into the live `Config`. The merge is deterministic: plugins are processed in directory-sorted order to ensure reproducible results.

`ResolvedOverlay` carries diagnostic fields:

| Field | Description |
|-------|-------------|
| `source_plugins` | Names of plugins whose overlay was successfully applied |
| `skipped_plugins` | Names of plugins whose overlay was skipped (validation failure, I/O error) |
| `merged_blocked_commands` | Final union of all plugin `blocked_commands` contributions |
| `merged_disambiguation_threshold` | Final max of all `disambiguation_threshold` contributions |

> [!WARNING]
> Hot-reload applies `skills.disambiguation_threshold` immediately. Changes to `tools.blocked_commands` or `tools.allowed_commands` require an agent restart to take full effect — the live `ShellExecutor` is built once at startup. A banner is emitted in the status channel when a restart is required.

## Security model

- **Install-time validation** — overlay keys are checked against the safelist; unsafe keys abort the install with a clear error.
- **Load-time re-validation** — the safelist check is re-run at every bootstrap and hot-reload as defence-in-depth against post-install tampering.
- **`.bundled` marker stripping** — `.bundled` marker files in the installed package are stripped recursively to prevent trust escalation; if stripping fails, the partial install is cleaned up before propagating the error.
- **Symlink skip** — symlinks in the source package are never copied, preventing symlink-based path traversal.
- **Path traversal defense** — all install paths are canonicalized and verified to remain inside the managed root.

## Installation

```bash
cargo add zeph-plugins
```

Enabled automatically when the `zeph-plugins` crate is a dependency of the root `zeph` binary.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
