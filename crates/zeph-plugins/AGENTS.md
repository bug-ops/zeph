# zeph-plugins Guide

Plugin packaging, installation, and management: a plugin is a directory (local or remote git) with a `plugin.toml` manifest, skill directories, optional MCP server declarations, and a config overlay. Plugins install to `~/.local/share/zeph/plugins/<name>/` and load at agent startup.

- Start with crate-local checks: `cargo build -p zeph-plugins`, `cargo nextest run -p zeph-plugins`, `cargo clippy -p zeph-plugins --all-targets -- -D warnings`.
- Read `specs/058-plugins/spec.md` before changing the manifest format, install flow, or overlay resolution.
- The security model is non-negotiable — every guarantee below needs regression coverage when its code path changes:
  - Config overlays are **tighten-only**: they may add to `blocked_commands`, narrow `allowed_commands`, or raise `disambiguation_threshold` — never loosen a constraint (`overlay.rs`).
  - Plugin MCP entries are validated against `mcp.allowed_commands` at install time.
  - `.bundled` markers are stripped recursively from all plugin skill trees.
  - Skill-name conflicts with managed, bundled, or other plugin skills are hard errors at install.
- `integrity.rs` and `manager/security.rs` are security-sensitive; do not relax integrity checks or install-time validation without an explicit security review.
- If install, overlay, or manifest behavior changes, update `crates/zeph-plugins/README.md` and the relevant plugin docs.
