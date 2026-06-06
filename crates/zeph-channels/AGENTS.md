# zeph-channels Guide

CLI and chat-channel adapters live here.

- Start with crate-local checks: `cargo build -p zeph-channels`, `cargo nextest run -p zeph-channels`, `cargo clippy -p zeph-channels --all-targets -- -D warnings`.
- Keep changes isolated to channel adapters, rendering, and streaming behavior unless shared channel traits require coordinated edits.
- Secrets (Telegram bot token, webhook secrets) are resolved exclusively from the age vault at startup — never from env vars, config files, or hardcoded values.
- Validate formatting and rendering changes against existing markdown and channel tests.
- If external behavior changes, update `crates/zeph-channels/README.md` and the relevant channel docs.
