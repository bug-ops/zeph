# zeph-tui Guide

The ratatui dashboard, UI state, event loop, and visual feedback live here.

- Start with crate-local checks: `cargo build -p zeph-tui`, `cargo nextest run -p zeph-tui`, `cargo clippy -p zeph-tui --all-targets -- -D warnings`.
- Read `specs/011-tui/spec.md` before changing panel state, the spinner rule, or app-event wiring; honor its `## Key Invariants` sections.
- Any background or implicit operation must surface visible status/spinner feedback in the UI.
- Preserve keyboard flow, redraw behavior, and test coverage for regressions in event handling.
- New `App` state fed via `AgentEvent` (cancel signal, metrics receiver, `TaskSupervisor`, etc.) must be wired into BOTH TUI startup paths — the phase-2/early-start path (`run_tui_agent` in `src/tui_bridge.rs`) and the legacy path — or the feature silently degrades on whichever path was missed (#6276/#6281).
- Views that render provider/server/agent config (e.g. the Settings panel) must build their display structs via explicit whitelist field-copy, never by deriving `Serialize`/`Debug` on config types that may carry secret fields (#6246).
- If external behavior changes, update `crates/zeph-tui/README.md` and the relevant TUI docs.
