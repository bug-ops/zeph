# Root Binary Guide

This directory contains the top-level binary wiring for CLI commands, runtime startup, daemon entrypoints, and feature-gated integration points.

- Changes here usually coordinate existing crate APIs rather than introducing core logic from scratch.
- When adding or changing a feature, provide all integration points: config section, CLI subcommand/argument, TUI command palette entry, `--init` wizard update, `--migrate-config` migration step, live testing playbook in `.local/testing/playbooks/`, and coverage row in `.local/testing/coverage-status.md`.
- This directory has multiple parallel session entry points (`runner.rs`, `daemon.rs`, `acp.rs`, `serve/`, plus gateway spawn paths) that each build their own agent/session wiring. Verify new functionality is wired into every entry point that constructs a session, not just the one you're testing — inconsistent cross-entry-point wiring has been the most common defect class in this directory (e.g. #6031/#6032, #5978, #5976, #6169, #6039, #6047, #6102, #6140).
- Secrets are never passed via environment variables or flags; all `ZEPH_*` keys are resolved from the age vault at startup.
- Keep command handling thin; prefer pushing reusable logic into the appropriate crate.
- Validate that CLI flags and subcommands stay aligned with docs and `config/default.toml`.
