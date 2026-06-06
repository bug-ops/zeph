# zeph-tui Guide

The ratatui dashboard, UI state, event loop, and visual feedback live here.

- Start with crate-local checks: `cargo build -p zeph-tui`, `cargo nextest run -p zeph-tui`, `cargo clippy -p zeph-tui --all-targets -- -D warnings`.
- Any background or implicit operation must surface visible status/spinner feedback in the UI.
- Preserve keyboard flow, redraw behavior, and test coverage for regressions in event handling.
- If external behavior changes, update `crates/zeph-tui/README.md` and the relevant TUI docs.
