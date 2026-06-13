---
aliases:
  - TUI Reducer Tasks
  - Action Decomposition Tasks
tags:
  - sdd
  - tasks
  - tui
  - config
created: 2026-06-13
status: approved
related:
  - "[[tui-reducer/spec]]"
  - "[[tui-reducer/plan]]"
  - "[[MOC-specs]]"
---

# Tasks: TUI Reducer / Action Decomposition + Opt-in Mouse Mode

> [!info]
> Granular implementation tasks derived from `specs/tui-reducer/plan.md`.
> Each task is atomic (compiles + tests green after it lands).
> Ordered; do not start a task until the one above it is green.

## Step 1 — Action/Effect Skeleton

- [ ] **T1.1** Create `crates/zeph-tui/src/app/action.rs` with `Action` (scroll/toggle
  slice only), `ScrollDir`, `VertDir` enums. Add `Effect` enum (all variants) in
  `app/reducer.rs`. Register modules in `app/mod.rs`.
- [ ] **T1.2** Implement `reduce()` and `run_effects()` stubs in `app/reducer.rs` handling
  `ScrollLines`, `ScrollPage`, `ScrollToTop/Bottom`, `ToggleToolExpanded`, `CycleToolDensity`,
  `ToggleSidePanels`, `ToggleHelp`, `SetHelp`.
- [ ] **T1.3** Convert matching leaves in `app/keys.rs` `handle_normal_key` to return
  `Option<Action>`; introduce `handle_key` wrapper calling decode→reduce→run_effects.
- [ ] **T1.4** Add reducer unit tests for each Step 1 Action (targeted field changes;
  unrelated fields unchanged; Effects correct). Confirm `cargo nextest run -p zeph-tui`
  green and insta snapshots unchanged.

## Step 2 — Normal-mode + Command Dispatch

- [ ] **T2.1** Convert remaining `handle_normal_key` leaves: `ClearTranscript`,
  `TogglePlanView`, `CopyLastAssistant`, `CopyLastCodeBlock`, `Quit`, view-target/panel
  variants, `SetMouse` placeholder (no-op in run_effects for now).
- [ ] **T2.2** Convert `execute_command`: agent pass-through commands →
  `Action::Dispatch(cmd)` → `Effect::SendCommand` or `Effect::SendUserInput` in run_effects.
  Wire `Effect::CopyToClipboard`.
- [ ] **T2.3** Add unit tests: `Dispatch(cmd)` → `[Effect::SendCommand(cmd)]`;
  `CopyLastAssistant` returns `[Effect::CopyToClipboard(_)]`. Green suite.

## Step 3 — Insert-mode + Modals

- [ ] **T3a** Add `PaletteEdit { PushChar(char), PopChar }` and
  `ElicitationEdit { PushChar(char), PopChar, NextField, PrevField, ToggleBool, EnumNext, EnumPrev }`
  to `app/action.rs`. (C4 — complete enum before modal code lands.)
- [ ] **T3b** Convert `handle_insert_key` and input-composer leaves: `InsertChar`,
  `InsertNewline`, `InsertText`, `DeleteCharBackward`, `DeleteCharForward`,
  `DeleteWordBackward`, `MoveCursor`, `ClearInput`, `SubmitInput`, `SetInputMode`,
  `HistoryPrev`, `HistoryNext`. Verify zero mutation via helpers (C5).
- [ ] **T3c** Convert command palette handler: `OpenCommandPalette`, `CloseCommandPalette`,
  `PaletteMove`, `PaletteInput(PaletteEdit)`, `PaletteAccept`.
- [ ] **T3d** Convert file picker handler: `OpenFilePicker`, `CloseFilePicker`,
  `FilePickerMove`, `FilePickerInput`, `FilePickerAccept` →
  `Effect::StartFileIndex` where applicable.
- [ ] **T3e** Convert slash autocomplete handler: `SlashAutocompleteMove`,
  `SlashAutocompleteInput`, `SlashAutocompleteAccept`, `CloseSlashAutocomplete`.
- [ ] **T3f** Convert reverse search handler: `OpenReverseSearch`, `ReverseSearchInput`,
  `ReverseSearchNext`, `ReverseSearchPrev`, `ReverseSearchAccept`, `CloseReverseSearch`.
- [ ] **T3g** Convert confirm dialog: `ConfirmRespond(bool)` → reducer `take()`s sender
  from `confirm_state` → `Effect::ResolveConfirm(bool)`. INV-R3 test: `ConfirmRespond(true)`
  → exactly `[ResolveConfirm(true)]` + `confirm_state = None`.
- [ ] **T3h** Convert elicitation dialog: `ElicitationField(ElicitationEdit)`,
  `ElicitationSubmit`, `ElicitationCancel`. INV-R3 test for `ElicitationSubmit`.
- [ ] **T3i** Verify insta snapshots unchanged across T3b–T3h. Green suite.

## Step 4 — Cache Layout + OSC8 Accessor

- [ ] **T4.1** Add `#[derive(Clone, Copy)]` to `AppLayout` in `layout.rs`.
- [ ] **T4.2** Add `pub(crate) last_layout: Option<AppLayout>` to `App` (default `None`).
  At end of `draw()` in `draw.rs`, set `self.last_layout = Some(layout)`.
- [ ] **T4.3** Add `pub(crate) fn hyperlinks(&self) -> &[HyperlinkSpan]` to hyperlink
  holder. Switch `tui_loop`'s OSC8 write from `take_hyperlinks()` to a borrow.
  Keep `set_hyperlinks()` as the replace-on-draw call.
- [ ] **T4.4** Verify OSC8 link sequences still emit on redraw (manual smoke or integration
  test). Document result in PR description. Green suite.

## Step 5 — Mouse Plumbing

- [ ] **T5.1** Add `Mouse(crossterm::event::MouseEvent)` to `AppEvent` in `event.rs`.
  In `CrosstermEventSource::next_event`: map `CrosstermEvent::Mouse(m)` for
  `ScrollUp/Down`, `Down(_)`, `Up(_)` → `Some(AppEvent::Mouse(m))`;
  map `Moved` and `Drag(_)` → `Tick` (C6).
- [ ] **T5.2** Add `AppEvent::Mouse(m) => self.handle_mouse(m)` arm in `app/events.rs`.
- [ ] **T5.3** Create `app/mouse.rs` with:
  - `fn rect_contains(r: Rect, col: u16, row: u16) -> bool`
  - `pub(crate) fn decode_mouse(&self, m: MouseEvent) -> Option<Action>` — guard
    `last_layout.is_none()` → return `None`; then hit-test table from spec §3.5.
  - `pub(crate) fn handle_mouse(&mut self, m: MouseEvent)` (guard `mouse_enabled`).
- [ ] **T5.4** Unit tests for `decode_mouse`: wheel-in-chat → `ScrollLines`, click-in-skills
  → `SetActivePanel(Skills)`, drag → `None`, `last_layout = None` → `None`.
- [ ] **T5.5** Cross-mode scroll equivalence test: same `Action::ScrollLines(-3)` from
  Down-arrow key and from chat wheel event → identical `App` state. Green suite.

## Step 6 — Config + Runtime Toggle

- [ ] **T6.1** Add `#[serde(default)] pub mouse: bool` to `TuiConfig`
  in `crates/zeph-config/src/ui.rs`.
- [ ] **T6.2** Add `pending_mouse_capture: Option<bool>` to `App`.
  Add `pub(crate) fn take_mouse_capture_request(&mut self) -> Option<bool>`.
  Add `pub(crate) fn with_mouse(mut self, enabled: bool) -> Self` builder.
- [ ] **T6.3** In `reducer.rs`: implement `Action::SetMouse(b)` — set `mouse_enabled = b`,
  push status message, return `[Effect::SetMouseCapture(b)]`.
  In `run_effects`: `Effect::SetMouseCapture(b)` → `pending_mouse_capture = Some(b)`.
- [ ] **T6.4** In `lib.rs` post-select block: drain `take_mouse_capture_request()`;
  run `crossterm::execute!(terminal.backend_mut(), DisableAlternateScroll, EnableMouseCapture)`
  or inverse. (C2: shared block, not inside an event arm.)
- [ ] **T6.5** Startup capture: if `app.mouse_enabled` after first `terminal.draw`,
  set `pending_mouse_capture = Some(true)` and drain immediately. (C3)
- [ ] **T6.6** `restore_terminal`: add `DisableMouseCapture` to teardown sequence.
- [ ] **T6.7** Panic hook: confirm existing hook calls `restore_terminal`, or add
  `std::panic::set_hook` wrapping `restore_terminal`. Document in PR. (C7 / INV-M4)
- [ ] **T6.8** `src/tui_bridge.rs`: add `.with_mouse(config.tui.mouse)` to builder chain.
- [ ] **T6.9** Unit tests: `SetMouse(true)` → `mouse_enabled = true` + `[Effect::SetMouseCapture(true)]`;
  `SetMouse(false)` → inverse. Green suite.

## Step 7 — Command + UX

- [ ] **T7.1** `command.rs`: add `TuiCommand::SetMouse(bool)`, `TuiCommand::ToggleMouse`.
  Add palette entry `{ id: "app:mouse", label: "Toggle mouse mode (wheel scroll, click focus)", category: "app" }`.
  `PaletteAccept` arm reads `mouse_enabled` → `Action::SetMouse(!current)`.
- [ ] **T7.2** `keys.rs` `parse_session_slash`: add `/mouse` arm (bare → `ToggleMouse`;
  with `on|off` → `SetMouse(bool)`).
- [ ] **T7.3** `keys.rs` `execute_command`: `SetMouse(b)` → `Action::SetMouse(b)`;
  `ToggleMouse` → `Action::SetMouse(!app.mouse_enabled)`.
- [ ] **T7.4** `widgets/status.rs`: render `mouse on — text selection via Shift+drag`
  when `mouse_enabled` (truncate on narrow widths).
- [ ] **T7.5** `src/wizard.rs`: add mouse yes/no prompt in `[tui]` section.
- [ ] **T7.6** `src/migration.rs`: add step 67 inserting `mouse = false` under `[tui]`
  if absent.
- [ ] **T7.7** Green suite; snapshot tests unchanged.

## Step 8 — Docs / Playbook / Coverage

- [ ] **T8.1** Author `.local/testing/playbooks/tui-reducer-mouse.md` with scenarios:
  default mode, `/mouse on`, `/mouse off`, panic recovery, OSC8 links, config startup.
- [ ] **T8.2** Add rows to `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md`:
  `TUI Reducer` and `TUI Mouse Mode` → `Untested`.
- [ ] **T8.3** Update `docs/src/` pages for TUI mouse mode if user-facing docs exist.
- [ ] **T8.4** Run full pre-PR check suite:
  ```bash
  cargo +nightly fmt --check
  cargo clippy --profile ci --workspace --all-targets --features "desktop,ide,server,chat,pdf,scheduler,testing" -- -D warnings
  cargo nextest run --config-file .github/nextest.toml --workspace --features "desktop,ide,server,chat,pdf,scheduler" --lib --bins
  RUSTFLAGS="-D warnings" RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --workspace --features "desktop,ide,server,chat,pdf,scheduler"
  cargo insta test --workspace --features full --check --lib --bins
  ```
  All must pass.

## Critic-finding traceability

| Finding | Task(s) | Nature |
|---------|---------|--------|
| C1 — OSC8 drain semantics | T4.3, T4.4 | Changes what code is written |
| C2 — drain placement ordering | T6.4 | Changes where drain is placed |
| C3 — startup before first draw | T6.5 | Changes timing of capture enable |
| C4 — ElicitationEdit completeness | T3a | Changes enum definition |
| C5 — helpers count as mutation | T3b–T3h | Strengthens review gate |
| C6 — Moved/Drag flood filter | T5.1 | Changes event source filtering |
| C7 — panic hook for INV-M4 | T6.7 | Adds or confirms teardown guard |
