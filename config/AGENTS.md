# Config Guide

This directory holds runtime defaults and configuration templates.

- Treat `config/default.toml` as the source of truth for documented defaults unless a feature explicitly overrides them at runtime.
- Keep config keys, comments, and docs in sync. Config supports `ZEPH_*` env var overrides for non-secret values only — secrets must go through the age vault, never env vars.
- When adding or changing config keys, update the relevant docs and any setup/instruction materials that reference them.
- Preserve readability: keep sections coherent and comments directly attached to the settings they describe.
