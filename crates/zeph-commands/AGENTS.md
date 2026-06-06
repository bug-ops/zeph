# zeph-commands Guide

Slash command registry, `CommandHandler` trait, `ChannelSink` abstraction, and `CommandOutput` type live here.

- Start with crate-local checks: `cargo build -p zeph-commands`, `cargo nextest run -p zeph-commands`, `cargo clippy -p zeph-commands --all-targets -- -D warnings`.
- Keep command dispatch logic thin; push reusable business logic into the appropriate domain crate.
- When adding a new slash command, wire it in the root binary (`src/`), update the `--help` output, and add a TUI command palette entry where applicable.
- If the command surface changes, update `crates/zeph-commands/README.md` and the relevant docs.
