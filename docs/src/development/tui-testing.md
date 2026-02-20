# TUI Testing

This document covers the test automation infrastructure for `zeph-tui`.

## EventSource Trait

All terminal event reading is abstracted behind the `EventSource` trait:

```rust
pub trait EventSource: Send + 'static {
    fn next_event(&self) -> Result<TuiEvent>;
}
```

Two implementations exist:

- **`CrosstermEventSource`** — production implementation, reads from the real terminal via `crossterm::event::read()` on a dedicated OS thread.
- **`MockEventSource`** — test implementation, replays a pre-defined `Vec<TuiEvent>` sequence. Allows deterministic simulation of user input without a terminal.

## Widget Snapshot Tests

Widget rendering is verified using `insta` snapshots against a ratatui `TestBackend`.

The `render_to_string` helper creates a `TestBackend` of a given size, renders a widget into it, and converts the buffer contents to a plain string for snapshot comparison:

```rust
fn render_to_string(widget: &impl Widget, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| f.render_widget(widget, f.area())).unwrap();
    terminal.backend().to_string()
}
```

Snapshot tests live alongside widget code in `#[cfg(test)]` modules. Each test renders a widget with known state and asserts via `insta::assert_snapshot!`.

## Integration Tests

Integration tests combine `MockEventSource` with `TestBackend` to drive the full TUI application loop:

1. Construct `MockEventSource` with a sequence of key events (e.g., type text, press Enter, press `q`).
2. Build the `App` with the mock source and a `TestBackend`.
3. Run the event loop until the mock sequence is exhausted.
4. Assert on final application state or capture terminal buffer snapshots.

This validates keybinding dispatch, mode transitions, scrolling, and message queueing without a real terminal.

## Property-Based Tests

`proptest` is used to fuzz `AppLayout::compute` with arbitrary terminal dimensions:

- Width and height are drawn from reasonable ranges (10..500).
- Properties verified: panel widths sum to total width, no panel has zero width when visible, side panels are hidden below the 80-column threshold.

## E2E Terminal Tests

End-to-end tests use `expectrl` to spawn the actual `zeph --tui` binary in a pseudo-terminal and interact with it as a user would:

- Send keystrokes, wait for expected screen content.
- Validate splash screen rendering, mode switching, quit behavior.

These tests are marked `#[ignore]` because they require a built binary and are slow. Run them explicitly:

```bash
cargo nextest run -p zeph-tui -- --ignored
```

## Config and Filter Snapshot Tests

Beyond widget rendering, `insta` snapshots also cover:

- **Config serialization** (`zeph-core`): snapshot tests verify that `Config` round-trips correctly through TOML serialization/deserialization, catching unintended field changes or serde attribute regressions.
- **Output filters** (`zeph-tools`): each filter's output is snapshot-tested against known command outputs (e.g., `cargo test`, `cargo clippy`, `git diff`), ensuring filter logic changes are reviewed explicitly via snapshot diffs.

These snapshots follow the same `cargo insta test` / `cargo insta review` workflow described below.

## Snapshot Workflow

Snapshot management uses `cargo-insta`:

```bash
# Run tests and generate/update snapshots
cargo insta test -p zeph-tui

# Review pending snapshot changes interactively
cargo insta review

# CI mode: fail if snapshots are out of date
cargo insta test -p zeph-tui --check
```

CI runs with `--check` to ensure all snapshots are committed and up to date.

## Commands Reference

| Command | Purpose |
|---------|---------|
| `cargo nextest run -p zeph-tui --lib` | Run unit and snapshot tests |
| `cargo nextest run -p zeph-tui -- --ignored` | Run E2E terminal tests |
| `cargo insta test -p zeph-tui` | Run tests and update snapshots |
| `cargo insta review` | Interactively review pending snapshots |
| `cargo insta test -p zeph-tui --check` | CI snapshot verification |
| `cargo nextest run -p zeph-tui -E 'test(widget)'` | Run only widget tests |
