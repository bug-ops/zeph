# Root Binary Guide

This directory contains the top-level binary wiring for CLI commands, runtime startup, daemon entrypoints, and feature-gated integration points.

- Changes here usually coordinate existing crate APIs rather than introducing core logic from scratch.
- When adding or changing a feature, provide all integration points: config section, CLI subcommand/argument, TUI command palette entry, `--init` wizard update, `--migrate-config` migration step, live testing playbook in `.local/testing/playbooks/`, and coverage row in `.local/testing/coverage-status.md`.
- Secrets are never passed via environment variables or flags; all `ZEPH_*` keys are resolved from the age vault at startup.
- Keep command handling thin; prefer pushing reusable logic into the appropriate crate.
- Validate that CLI flags and subcommands stay aligned with docs and `config/default.toml`.
