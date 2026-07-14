# zeph-commands Guide

Slash command registry, `CommandHandler` trait, `ChannelSink` abstraction, and `CommandOutput` type live here.

- Start with crate-local checks: `cargo build -p zeph-commands`, `cargo nextest run -p zeph-commands`, `cargo clippy -p zeph-commands --all-targets -- -D warnings`.
- Keep command dispatch logic thin; push reusable business logic into the appropriate domain crate.
- `CommandHandler::requires_auth()` defaults to `true` (fail-closed, since #6203): a new handler only reaches untrusted remote channels (Telegram/Discord/Slack) if it explicitly overrides this to `false`. Only do so for read-only or already self-gated commands — this was a 4x recurring fail-open defect class (#5967, #5997, #6003/#6033, #6034) before the default flipped.
- When adding a new slash command, wire it in the root binary (`src/`), update the `--help` output, and add a TUI command palette entry where applicable. A regression test asserts every registered handler has a matching `zeph_commands::COMMANDS` entry (#6172) — do not hand-maintain `COMMANDS` separately from the registry.
- If the command surface changes, update `crates/zeph-commands/README.md` and the relevant docs.
