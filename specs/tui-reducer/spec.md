---
aliases:
  - TUI Reducer
  - Action Decomposition
  - Mouse Mode
  - TUI Reducer Spec
tags:
  - sdd
  - spec
  - tui
  - config
  - contract
created: 2026-06-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
  - "[[011-tui/spec]]"
  - "[[027-tui-subagent-management/spec]]"
  - "[[031-tui-slash-autocomplete/spec]]"
---

# Spec: TUI Reducer / Action Decomposition + Opt-in Mouse Mode

> [!info]
> Defines the `Action` + `Effect` + `reduce` architecture for `zeph-tui` (#5076) and the
> opt-in mouse capture mode (#5103). This document is the prerequisite spec referenced by
> the TODO at `crates/zeph-tui/src/app/mod.rs:4-6`.

## Sources

### External
- [crossterm 0.29 mouse API](https://docs.rs/crossterm/0.29.0/crossterm/event/struct.MouseEvent.html)
- [ratatui `Rect` geometry](https://docs.rs/ratatui/latest/ratatui/layout/struct.Rect.html)

### Internal

| File | Contents |
|---|---|
| `crates/zeph-tui/src/app/mod.rs` | `App` struct (globals + session registry access) |
| `crates/zeph-tui/src/app/keys.rs` | Keyboard handler tree — the primary migration target |
| `crates/zeph-tui/src/app/events.rs` | Agent-event handlers (out of scope for this PR) |
| `crates/zeph-tui/src/event.rs` | `AppEvent` enum + `CrosstermEventSource::next_event` |
| `crates/zeph-tui/src/lib.rs` | `tui_loop`, `init_terminal`, `restore_terminal` |
| `crates/zeph-tui/src/app/draw.rs` | `draw()` — computes and (currently) discards `AppLayout` |
| `crates/zeph-tui/src/layout.rs` | `AppLayout` (ten `Rect`s, derives neither Clone nor Copy today) |
| `crates/zeph-tui/src/hyperlink.rs` | `HyperlinkSpan { url, row, start_col, end_col }` |
| `crates/zeph-tui/src/session.rs` | `SessionSlot` — per-session state |
| `crates/zeph-tui/src/command.rs` | `TuiCommand` enum + `build_app_commands()` |
| `crates/zeph-config/src/ui.rs` | `TuiConfig` — config entry point |
| `src/tui_bridge.rs` | Builder chain wiring (`with_motion`, `with_delights`, etc.) |

---

## 1. Overview

### Problem Statement

State mutation in the TUI is scattered across ~30 handler functions in `app/keys.rs`
(1749 lines) and `app/events.rs` (799 lines). Each handler mixes three concerns:
(1) decoding input, (2) mutating app state, and (3) triggering side-effects. This makes
the codebase untestable at the unit level and blocks the addition of mouse input — because
mouse events need to share the exact same state-mutation logic as keyboard events.

### Goal

- A single `reduce(&mut App, Action) -> Vec<Effect>` function is the **only** site that
  mutates keyboard/mouse-derived state.
- Handlers become pure decoders: `key/mouse event → Option<Action>`.
- Mouse events (wheel, click) produce `Action`s and flow through `reduce`, making
  keyboard and mouse paths provably consistent.
- `[tui] mouse: bool` (default `false`) enables an opt-in mouse capture mode with
  runtime toggling via `/mouse [on|off]`.

### Out of Scope

- Rewriting `app/events.rs` agent-event handlers — they are already a self-contained reducer
  over a disjoint state path. Migrating them risks a 2500-line big-bang refactor.
- Per-tool-group expand state (today `tool_expanded` is a single global bool).
- Phase-2 multi-session tab-bar mouse clicks (only one session slot exists today).
- Inventing a browser-spawn / URL-open effect for OSC8 mouse clicks — security and
  cross-platform surface; rely on terminal-native OSC8 handling.

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN a key or mouse event is handled THE SYSTEM SHALL route it through `reduce` as the sole state-mutation site | must |
| FR-002 | WHEN `reduce` is called THE SYSTEM SHALL perform no I/O (no channel sends, no clipboard, no spawn, no terminal escapes) | must |
| FR-003 | WHEN `reduce` returns effects containing a `oneshot::Sender` THE SYSTEM SHALL transfer it out of modal state inside the reducer so the sender is never left dangling | must |
| FR-004 | WHEN `[tui] mouse = false` (default) THE SYSTEM SHALL behave byte-for-byte identically to pre-PR (alternate-scroll enabled, no mouse capture, native selection works) | must |
| FR-005 | WHEN `[tui] mouse = true` or `/mouse on` is issued THE SYSTEM SHALL emit `DisableAlternateScroll` then `EnableMouseCapture` as a single atomic terminal write | must |
| FR-006 | WHEN `mouse = true`, wheel events in the chat region THE SYSTEM SHALL translate to `Action::ScrollLines(±3)` | must |
| FR-007 | WHEN `mouse = true`, a left-click in any panel THE SYSTEM SHALL focus that panel | must |
| FR-008 | WHEN `mouse = true`, a left-click on a tool-group header row THE SYSTEM SHALL toggle tool expansion | must |
| FR-009 | WHEN `restore_terminal` is called THE SYSTEM SHALL always emit `DisableMouseCapture` regardless of current state | must |
| FR-010 | WHEN `decode_mouse` is called with `last_layout = None` THE SYSTEM SHALL return `None` without panicking | must |
| FR-011 | WHEN `MouseEventKind::Moved` or `Drag(_)` arrives THE SYSTEM SHALL NOT forward it to the dirty-mark path (see §3.5, C6) | must |
| FR-012 | WHEN `/mouse on\|off` or the palette "Toggle mouse mode" entry is activated THE SYSTEM SHALL update `mouse_enabled` and display the status hint | must |
| FR-013 | WHEN `[tui] mouse = true` in config THE SYSTEM SHALL enable capture only AFTER the first successful `terminal.draw` | should |
| FR-014 | WHEN writing OSC8 sequences THE SYSTEM SHALL read hyperlink spans from a non-draining accessor (borrow, not drain) so spans persist until the next draw replaces them | must |
| FR-015 | THE SYSTEM SHALL expose `ElicitationEdit` and `PaletteEdit` variants that cover all sub-mutations (PushChar, PopChar, NextField, PrevField, ToggleBool, EnumNext, EnumPrev) or explicitly delegate to an opaque sub-reducer | must |
| FR-016 | THE SYSTEM SHALL provide a panic hook (or equivalent teardown guard) that calls `restore_terminal` so a crash mid-session cannot leave the terminal in mouse-capture mode | must |

---

## 3. Architecture

### 3.1 Reducer boundary

```
                ┌──────────────────────────────────────────────┐
crossterm event │  HANDLER (decode only — keys.rs / mouse.rs)  │
──────────────► │  "Ctrl+L in Main view"  →  Action::ClearTranscript
                └──────────────────────┬───────────────────────┘
                                       │ Action
                                       ▼
                ┌──────────────────────────────────────────────┐
                │  reduce(&mut App, Action) -> Vec<Effect>      │  PURE w.r.t. outside world:
                │  — sole key/mouse state-mutation site         │  no channel sends,
                │  — returns Effects it cannot run itself       │  no clipboard, no spawn
                └──────────────────────┬───────────────────────┘
                                       │ Vec<Effect>
                                       ▼
                ┌──────────────────────────────────────────────┐
                │  run_effects(&mut App, Vec<Effect>)           │  performs side-effects
                │  — user_input_tx.try_send(...)               │
                │  — command_tx.try_send(...)                   │
                │  — clipboard.copy(...)                        │
                │  — parks SetMouseCapture in pending field     │
                └──────────────────────────────────────────────┘
                                       │ pending_mouse_capture
                                       ▼
                ┌──────────────────────────────────────────────┐
                │  tui_loop post-select block                   │
                │  — drains app.take_mouse_capture_request()   │
                │  — runs crossterm::execute! on backend        │
                └──────────────────────────────────────────────┘
```

`reduce` is `pub(crate) fn reduce(app: &mut App, action: Action) -> Vec<Effect>` in a
new module `app/reducer.rs`. It takes `&mut App` (not a narrower `AppState`) because
state is split across `App` globals + `SessionRegistry` + widget sub-states; relocating
~40 fields is out of scope. Purity = "no I/O", enforced by convention + tests + an
optional grep-based CI lint (see §5).

### 3.2 Effect enum

```rust
/// A side-effect the reducer requests but does not perform.
#[derive(Debug)]
pub(crate) enum Effect {
    /// Forward user input to the agent via `user_input_tx`.
    SendUserInput(String),
    /// Dispatch a structured TUI command to the agent via `command_tx`.
    SendCommand(TuiCommand),
    /// Copy text to the OS clipboard.
    CopyToClipboard(String),
    /// Answer a pending confirm dialog (consumes the one-shot sender from modal state).
    ResolveConfirm(bool),
    /// Answer a pending elicitation dialog (consumes the one-shot sender from modal state).
    ResolveElicitation(zeph_core::channel::ElicitationResponse),
    /// Kick off the background file-index build for the `@` file picker.
    StartFileIndex,
    /// Enable or disable crossterm mouse capture at runtime (#5103).
    /// run_effects parks this in `pending_mouse_capture`; tui_loop drains it.
    SetMouseCapture(bool),
    /// Quit the application.
    Quit,
}
```

Effects carrying `oneshot::Sender`s (`ResolveConfirm`, `ResolveElicitation`) take the
sender out of modal state *inside the reducer* (INV-R3). The reducer leaves
`confirm_state = None`; `run_effects` calls `tx.send(answer)`.

### 3.3 Action enum

`Action` lives in `app/action.rs`. Flat and semantic — variants describe intent,
not mechanism. Supporting enums (`CursorMove`, `ScrollDir`, `VertDir`, `PaletteEdit`,
`ElicitationEdit`) live alongside it.

```rust
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    // ---- Input composer ----
    InsertChar(char),
    InsertNewline,
    InsertText(String),          // bracketed paste
    DeleteCharBackward,
    DeleteCharForward,
    DeleteWordBackward,
    MoveCursor(CursorMove),
    ClearInput,
    SubmitInput,
    SetInputMode(InputMode),

    // ---- Input history ----
    HistoryPrev,
    HistoryNext,

    // ---- Transcript scroll ----
    ScrollLines(i32),            // positive = up
    ScrollPage(ScrollDir),
    ScrollToTop,
    ScrollToBottom,
    ClearTranscript,

    // ---- Layout / panel toggles ----
    ToggleSidePanels,
    SetActivePanel(Panel),
    CyclePanelForward,
    ToggleTaskPanel,
    TogglePanelCollapse(usize),
    ToggleHelp,
    SetHelp(bool),
    TogglePlanView,
    ToggleToolExpanded,
    CycleToolDensity,

    // ---- View target ----
    SetViewTarget(AgentViewTarget),
    SubAgentSelectNext,
    SubAgentSelectPrev,
    OpenSelectedSubAgent,

    // ---- Theme / motion / mouse runtime toggles ----
    CycleTheme,
    SetTheme(String),
    SetMotion(zeph_config::Motion),
    SetMouse(bool),

    // ---- Modal lifecycle ----
    OpenCommandPalette,
    CloseCommandPalette,
    PaletteMove(VertDir),
    PaletteInput(PaletteEdit),
    PaletteAccept,

    OpenFilePicker,
    CloseFilePicker,
    FilePickerMove(VertDir),
    FilePickerInput(PaletteEdit),
    FilePickerAccept,

    SlashAutocompleteMove(VertDir),
    SlashAutocompleteInput(PaletteEdit),
    SlashAutocompleteAccept,
    CloseSlashAutocomplete,

    OpenReverseSearch,
    ReverseSearchInput(PaletteEdit),
    ReverseSearchNext,
    ReverseSearchPrev,
    ReverseSearchAccept,
    CloseReverseSearch,

    ConfirmRespond(bool),
    ElicitationField(ElicitationEdit),
    ElicitationSubmit,
    ElicitationCancel,

    // ---- Clipboard ----
    CopyLastAssistant,
    CopyLastCodeBlock(usize),

    // ---- Lifecycle ----
    Quit,

    // ---- Escape hatch for agent pass-through commands ----
    Dispatch(TuiCommand),
}
```

`PaletteEdit` and `ElicitationEdit` must cover all sub-mutations (C4):

```rust
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PaletteEdit {
    PushChar(char),
    PopChar,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ElicitationEdit {
    PushChar(char),
    PopChar,
    NextField,
    PrevField,
    ToggleBool,
    EnumNext,
    EnumPrev,
}
```

> **Rationale for `&mut App`:** state contains non-`Clone` handles (channels,
> `oneshot::Sender`s in modal state). A value-returning reducer would clone the transcript
> per keystroke. In-place mutation is the pragmatic Rust idiom (cf. `iced::update`).
> Purity is scoped to "no I/O", not "no mutation".

> **Minimality:** No `Action` per agent event — agent events stay in `events.rs`. No
> `Action::Tick` or `Action::Resize` — those are animation/cache concerns handled inline.

### 3.4 Handler → Action flow

Handlers shrink to translation tables. Example:

```rust
// BEFORE (keys.rs handle_normal_key)
KeyCode::Char('e') => { self.tool_expanded = !self.tool_expanded; self.render_cache_clear(); }
KeyCode::Up | KeyCode::Char('k') => { self.scroll_up_one(); }

// AFTER
KeyCode::Char('e') => Some(Action::ToggleToolExpanded),
KeyCode::Up | KeyCode::Char('k') => Some(Action::ScrollLines(1)),
```

`handle_key` becomes:

```rust
pub fn handle_key(&mut self, key: KeyEvent) {
    if let Some(action) = self.decode_key(key) {
        let effects = reducer::reduce(self, action);
        self.run_effects(effects);
    }
}
```

### 3.5 Mouse mode (#5103)

#### Native-scroll tension (critical design constraint)

`init_terminal` (`lib.rs:251`) enables **alternate-scroll mode** (`\x1b[?1007h`,
`EnableAlternateScroll`). This converts wheel events to arrow keys and preserves native
text selection (documented `lib.rs:219-223`). `EnableMouseCapture` is mutually exclusive
with this — enabling it steals drag events, breaking native selection.

**Resolution:**
- `mouse = false` (default): alternate-scroll on, no `MouseEvent`s, native selection works.
- `mouse = true`: emit `DisableAlternateScroll` + `EnableMouseCapture` together; wheel
  arrives as `MouseEventKind::ScrollUp/Down`; selection requires Shift+drag.

#### Event plumbing

Add `Mouse(crossterm::event::MouseEvent)` to `AppEvent`:

```rust
// event.rs CrosstermEventSource::next_event
Ok(CrosstermEvent::Mouse(m)) => Some(AppEvent::Mouse(m)),
```

`handle_event` gains:

```rust
AppEvent::Mouse(m) => self.handle_mouse(m),
```

`handle_mouse` lives in new `app/mouse.rs` and is a pure decoder:

```rust
pub fn handle_mouse(&mut self, m: MouseEvent) {
    if !self.mouse_enabled { return; }
    if let Some(action) = self.decode_mouse(m) {
        let effects = reducer::reduce(self, action);
        self.run_effects(effects);
    }
}
```

**C6 — Moved/Drag flood filter:** `decode_mouse` returns `None` for
`MouseEventKind::Moved` and `Drag(_)`. The event source (`event.rs`) should additionally
map these to `Tick` (not `AppEvent::Mouse`) so they never set `dirty = Full` in the main
loop — preventing a full-redraw storm on mouse hover when capture is on.

#### Hit-testing model

`App` gains:

```rust
/// Layout Rects from the last `draw()`, retained for mouse hit-testing.
/// `None` until the first frame is drawn.
pub(crate) last_layout: Option<AppLayout>,
```

`AppLayout` (`layout.rs:70`) must derive `Clone, Copy` (it is ten `Rect`s, ~80 bytes).

`decode_mouse` guards `None` immediately (FR-010) and returns `None` without panicking.

Hit-test table:

| Region | `MouseEventKind` | `Action` |
|--------|-----------------|---------|
| `chat` | `ScrollDown` | `ScrollLines(-3)` |
| `chat` | `ScrollUp` | `ScrollLines(3)` |
| `chat` | `Down(Left)` on hyperlink | hit-test scan → `Dispatch(open-url)` / OSC8 fallback |
| `chat` | `Down(Left)` on tool-group header | `ToggleToolExpanded` |
| `chat` | `Down(Left)` elsewhere | `SetActivePanel(Panel::Chat)` |
| `skills` | `Down(Left)` | `SetActivePanel(Panel::Skills)` |
| `memory` | `Down(Left)` | `SetActivePanel(Panel::Memory)` |
| `resources` | `Down(Left)` | `SetActivePanel(Panel::Resources)` |
| `subagents` | `Down(Left)` | `SetActivePanel(Panel::SubAgents)` + select row |
| `subagents` | `ScrollUp/Down` | `SubAgentSelectPrev/Next` |
| `input` | `Down(Left)` | `SetInputMode(Insert)` |
| `status` | `Down(Left)` | ignored in v1 |
| any panel | `ScrollUp/Down` | scroll that panel's list (fallback to `ScrollLines` on chat) |
| any | `Moved` / `Drag(_)` | `None` — filtered at source (see C6) |

`rect_contains(r: Rect, col: u16, row: u16) -> bool` is a small helper in `app/mouse.rs`.

#### OSC8 hyperlink activation (C1 — corrected semantics)

> [!warning] C1 Correction
> The OSC8 spans are NOT simply "drained every frame." `take_hyperlinks()` is only called
> inside the `if should_draw { … }` block in `tui_loop` (`lib.rs:204-210`), so on idle
> (non-redraw) frames the spans already persist. On redraw frames, `set_hyperlinks()` is
> called from `widgets/chat.rs:97` **during draw**, which replaces the whole vec — so
> spans are always "as of the last drawn frame," exactly what hit-testing wants.

The correct fix: change `take_hyperlinks()` to a borrowing accessor for the OSC8 write.
Keep writing OSC8 from a borrow; let `set_hyperlinks()` at draw time replace the vec.
The developer MUST verify the OSC8 escape is still emitted on every redraw after
switching `take_` → borrow (regression risk: links stop being natively clickable).

#### `SetMouseCapture` effect path (C2 — drain placement)

`run_effects` handles all effects except `SetMouseCapture`, which it parks in
`pending_mouse_capture: Option<bool>` on `App`. `tui_loop`'s post-select block
(the shared block after the `tokio::select!` that already calls `app.poll_*`) drains
it via `app.take_mouse_capture_request() -> Option<bool>`.

This single drain site covers both runtime toggle (slash command on any select arm) and
startup enable — they converge on the same path. Do **not** place the drain inside
the event arm only.

#### Startup capture ordering (C3 — after first draw)

If `config.tui.mouse = true`, enable capture **after the first successful
`terminal.draw`**, not before the loop. This guarantees `last_layout` is populated
before any mouse event can arrive. Document in `decode_mouse` that `None`-guard is
correct behavior (first-scroll-after-boot is silently dropped), and cover it with a
unit test.

#### Panic hook / teardown guard (C7 — INV-M4 precondition)

INV-M4 ("a crash never leaves the terminal in capture mode") is only satisfied if
`restore_terminal` runs on panic. Before shipping: confirm an existing panic hook or
`Drop` guard calls `restore_terminal`, or add one. Check `lib.rs`/bootstrap.
`DisableMouseCapture` when not enabled is idempotent (terminal ignores `?1000l`), so
always emitting it in `restore_terminal` is safe.

### 3.6 Config and command surface

**Config field** (`crates/zeph-config/src/ui.rs`, `TuiConfig` ~line 505):

```rust
/// Enable opt-in mouse mode (wheel scroll, click-to-focus, OSC8 click fallback).
/// Default `false`. When `true`, text selection requires Shift+drag.
#[serde(default)]
pub mouse: bool,
```

**App fields** (`app/mod.rs`):

```rust
pub(crate) mouse_enabled: bool,      // default false
pub(crate) last_layout: Option<AppLayout>,  // default None
pub(crate) pending_mouse_capture: Option<bool>, // default None
```

**Builder** (`app/state.rs`):

```rust
#[must_use]
pub fn with_mouse(mut self, enabled: bool) -> Self {
    self.mouse_enabled = enabled;
    self
}
```

**`/mouse` command** (`command.rs`, `keys.rs`):

```rust
// TuiCommand additions:
SetMouse(bool),    // explicit on/off from slash parse
ToggleMouse,       // palette entry (reads current state at accept time)
```

Slash parse (`parse_session_slash` ~line 606):

```rust
[cmd] if cmd.eq_ignore_ascii_case("/mouse") => Some(TuiCommand::ToggleMouse),
[cmd, s] if cmd.eq_ignore_ascii_case("/mouse")
    && matches!(s.to_ascii_lowercase().as_str(), "on" | "off") =>
    Some(TuiCommand::SetMouse(s.eq_ignore_ascii_case("on"))),
```

**Status hint** (`widgets/status.rs`): when `mouse_enabled`, display
`mouse on — text selection via Shift+drag` (truncate on narrow widths).

**Wizard + migration** (mandatory per CLAUDE.md):
- `--init`: yes/no prompt for `[tui] mouse` in the TUI section (default No).
- `--migrate-config`: insert `mouse = false` under `[tui]` for pre-existing configs
  (step number: next after the `tui.theme` step 66, i.e. step 67).

---

## 4. Implementation Plan

Eight steps, each compiling with tests green before the next begins.

**Step 1 — Action/Effect skeleton.**
Add `app/action.rs` and `app/reducer.rs`. Wire a first slice: `ScrollLines`,
`ScrollPage`, `ScrollToTop/Bottom`, `ToggleToolExpanded`, `CycleToolDensity`,
`ToggleSidePanels`, `ToggleHelp`. Convert matching `handle_normal_key` leaves. Unit-test
each Action in the reducer.

**Step 2 — Normal-mode + command dispatch.**
Convert remaining `handle_normal_key` and `execute_command`. Pass-through agent commands
become `Action::Dispatch(cmd)` → `Effect::SendUserInput/SendCommand`. Clipboard →
`Effect::CopyToClipboard`.

**Step 3 — Insert-mode + modals (largest step, split per modal).**
Convert `handle_insert_*`, palette, file picker, slash autocomplete, reverse search,
confirm, elicitation. C4 compliance: `ElicitationEdit` must enumerate PushChar, PopChar,
NextField, PrevField, ToggleBool, EnumNext, EnumPrev before declaring this step done.
C5 compliance: conversion is all-or-nothing per leaf — a converted sub-handler performs
no mutation by any path (direct or via helper), verified by reading (not by grepping
`self.\w+ =`). One-shot senders move into `Effect::ResolveConfirm/ResolveElicitation`.

**Step 4 — Cache layout + OSC8 accessor fix (C1).**
Derive `Clone, Copy` on `AppLayout`. Store `last_layout` at end of `draw`. Switch
`take_hyperlinks` to a borrowing accessor; verify OSC8 sequences still emit on redraw.

**Step 5 — Mouse plumbing (C6).**
Add `AppEvent::Mouse`, extend `CrosstermEventSource`. Add `app/mouse.rs` with
`decode_mouse` + `rect_contains`. Route wheel/click → Action. Filter `Moved`/`Drag` at
source (return `Tick`). Unit-test `decode_mouse` with synthetic events against a fixed
`AppLayout`.

**Step 6 — Config + runtime toggle (C2, C3, C7).**
Add `TuiConfig.mouse`, `with_mouse`, tui_bridge wiring. Implement
`pending_mouse_capture` + `take_mouse_capture_request()`. Drain in post-select block.
Enable capture only after first draw (C3). Always disable in `restore_terminal`.
Confirm or add panic hook for INV-M4 (C7).

**Step 7 — Command + UX.**
`TuiCommand::SetMouse/ToggleMouse`, palette entry, slash parse, status hint, wizard
prompt, migration step 67.

**Step 8 — Docs/tests/playbook.**
mdBook `[tui] mouse` doc. `.local/testing/playbooks/tui-reducer-mouse.md`.
`coverage-status.md` rows for `TUI Reducer` and `TUI Mouse Mode` → `Untested`.

---

## 5. Key Invariants

### Always

- **INV-R1 (single mutation site):** After migration, no key/mouse handler mutates `App`
  or `SessionSlot` directly. All such mutation flows through `reduce`. Conversion is
  all-or-nothing per leaf: a "converted" handler performs no mutation by any path —
  direct or via helper (C5). Agent-event handlers in `events.rs` are exempt in this PR.
- **INV-R2 (reducer purity):** `reduce` performs no I/O — no channel send, no clipboard,
  no task spawn, no terminal escape. It only mutates `App` state and returns `Effect`s.
- **INV-R3 (effect ownership):** Any `oneshot::Sender` (confirm/elicitation) is taken
  out of modal state by the reducer and carried in the `Effect`. The reducer never
  leaves a dangling sender. `run_effects` never reaches back into modal state.
- **INV-M1 (mouse ⇒ reducer):** Mouse events produce `Action`s via `decode_mouse` and
  apply through `reduce`. Hit-testing lives in `decode_mouse`; no mouse handler mutates
  state directly.
- **INV-M2 (scroll-mode exclusivity):** Alternate-scroll and mouse capture are never
  both enabled. Turning mouse on disables alternate-scroll first; turning it off
  re-enables alternate-scroll.
- **INV-M3 (selection preserved when off):** With `mouse = false` (default), terminal
  behaviour is byte-for-byte unchanged from before this PR.
- **INV-M4 (teardown safety):** `restore_terminal` always disables mouse capture,
  regardless of whether it was enabled. A panic hook or `Drop` guard ensures this runs
  even on unclean exit. Violation: crash mid-session leaves the terminal in capture mode.

### Ask First

- Changing `reduce` to take ownership of `App` (would require cloning channels/senders).
- Adding a second field or channel through which mouse/key handlers communicate
  state changes outside `reduce` + `Effect`.
- Merging agent-event handlers into the same `reduce` function in this PR.
- Adding a browser-spawn or OS `open` effect for OSC8 clicks (security review required).

### Never

- **NEVER** call `user_input_tx`, `command_tx`, `clipboard`, `tokio::spawn`, or
  `crossterm::execute!` from inside `reduce`. Those are `Effect`s.
- **NEVER** enable `EnableMouseCapture` without first emitting `DisableAlternateScroll`
  in the same `crossterm::execute!` call, and vice versa. They conflict over the wheel
  and over selection.
- **NEVER** mutate `scroll_offset`, `input`, `cursor_position`, panel/view/modal state
  from a key or mouse handler after migration — emit an `Action`.
- **NEVER** invent a browser/URL-spawn side-effect for OSC8 mouse clicks in this PR.
  Ship only the hit-test scaffold; rely on terminal-native OSC8 for link opening.
- **NEVER** place the `pending_mouse_capture` drain inside only one `tokio::select!` arm.
  It must be in the shared post-select block so it fires regardless of which event arm
  matched (C2).
- **NEVER** set `dirty = Full` for `Moved`/`Drag` events — filter them at the event
  source to prevent a full-redraw storm on mouse hover (C6).
- **NEVER** enable mouse capture before the first successful `terminal.draw` — `last_layout`
  is `None` until then (C3).

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Mouse event arrives before first draw (`last_layout = None`) | `decode_mouse` returns `None`; event silently dropped. Document as known v1 limitation; covered by unit test. |
| `EnableMouseCapture` issued while alternate-scroll is active | NEVER rule; the toggle path always emits `DisableAlternateScroll` first in the same `execute!` |
| `DisableMouseCapture` when capture was never enabled | Idempotent — terminal ignores `?1000l`; no error |
| `ConfirmRespond` or `ElicitationSubmit` with `modal_state = None` | Reducer returns empty `Vec<Effect>`; no panic (sender already taken or dialog already closed) |
| Rapid `/mouse on` + `/mouse off` before first drain | `pending_mouse_capture` is `Option<bool>` — last write wins; correct because `tui_loop` drains once per post-select cycle |
| Scroll/click while `mouse_enabled = false` | `handle_mouse` returns early; no Action emitted |
| `Moved`/`Drag` floods with mouse=on | Mapped to `Tick` at event source; `dirty` not set to `Full`; no storm |
| Panic mid-session with mouse=on | Panic hook / `Drop` guard calls `restore_terminal` which always emits `DisableMouseCapture` |
| OSC8 borrow after switching `take_` → borrow | OSC8 sequences still emitted on every redraw; regression covered by integration test |
| `ElicitationEdit` sub-mutation not covered by enum | Compile error — enum is exhaustive; no fallback to opaque mutation |
| Partial-migration: converted handler calls a helper that mutates | Caught in code review — INV-R1 definition requires zero-mutation path including via helpers |

---

## 7. Testing & Acceptance Criteria

### Unit tests

- **Reducer tests** (`app/reducer.rs` `#[cfg(test)]`): for each `Action`, build a
  minimal `App` (via `App::new` + dummy channels), apply `reduce`, assert targeted
  field changed + unrelated fields did not, and assert returned `Effect`s.
- **Decode tests:** table-driven `(KeyEvent, mode/modal) → Option<Action>` for
  `decode_key`; synthetic `MouseEvent { column, row, kind }` against fixed `AppLayout`
  for `decode_mouse` (wheel in chat → `ScrollLines`, click in skills →
  `SetActivePanel(Skills)`, drag → `None`).
- **`decode_mouse` None-guard test:** verify no panic when `last_layout = None` (C3).
- **One-shot sender test:** `ConfirmRespond(true)` yields exactly `[ResolveConfirm(true)]`
  and leaves `confirm_state = None`.

### Cross-mode consistency

A test applies `Action::ScrollLines(-3)` reached via (a) Down-arrow key and (b) a chat-
region `ScrollDown` wheel event, and asserts identical resulting `App` state. Proves
keyboard and mouse share the reducer (INV-M1).

### Purity guard

Optional but recommended: a grep-based CI lint that fails if `reduce`'s function body
contains `try_send`, `.send(`, `spawn`, `execute!`, or `clipboard`. Comment-mark
`// PURITY:` at the function signature.

### Snapshot tests

Existing `insta` snapshots in `app/snapshots/` must remain unchanged. Run:
`cargo insta test --workspace --features full --check --lib --bins`.
Any snapshot diff in Steps 1–3 is a regression to investigate, not to accept blindly.

### Acceptance criteria (all must pass before PR merge)

- [ ] All existing TUI tests + snapshots pass unchanged (keyboard behaviour preserved).
- [ ] `reduce` is the sole state-mutation site for key + mouse paths (INV-R1, INV-M1).
- [ ] `mouse = false` (default): terminal behaviour identical to pre-PR (INV-M3).
- [ ] `/mouse on`: wheel scrolls chat, click focuses panels, status hint shown, selection requires Shift.
- [ ] `/mouse off` restores alternate-scroll + native selection.
- [ ] Config `[tui] mouse = true` enables capture after first draw (C3).
- [ ] `restore_terminal` always disables capture (INV-M4).
- [ ] Panic hook confirmed to call `restore_terminal` (C7 / INV-M4).
- [ ] OSC8 sequences still emitted on redraw after `take_` → borrow switch (C1).
- [ ] `ElicitationEdit` covers all 7 sub-mutations (C4).
- [ ] `Moved`/`Drag` events do not trigger `dirty = Full` (C6).
- [ ] `pending_mouse_capture` drains in post-select block, not inside an event arm (C2).
- [ ] New unit tests: reducer per-Action, decode_key table, decode_mouse, None-guard, cross-mode scroll-equivalence.
- [ ] Playbook at `.local/testing/playbooks/tui-reducer-mouse.md` authored.
- [ ] `coverage-status.md` rows added for `TUI Reducer` and `TUI Mouse Mode`.

---

## 8. Migration Risks

| Risk | Mitigation |
|------|-----------|
| Partial-migration double-apply (leaf emits Action AND mutates) | Delete inline mutation in the same edit; INV-R1 says "zero mutation by any path"; review checks helpers too (C5) |
| One-shot sender leak (dialog cleared but agent blocks forever) | INV-R3 + unit test that `ConfirmRespond(true)` → exactly one `ResolveConfirm(true)` + `confirm_state = None` |
| OSC8 regression (links stop firing after `take_` → borrow switch) | Integration test; check `lib.rs:204-210` should_draw path manually (C1) |
| Scroll-mode toggle ordering (wheel double-interpreted) | NEVER rule; `DisableAlternateScroll + EnableMouseCapture` in one `execute!` |
| First-frame mouse event dropped | Documented v1 limitation; unit test guards `None` (C3) |
| Moved/Drag redraw storm | Filter at event source to `Tick`; no `Full` dirty on `None` decode return (C6) |
| Panic leaves terminal in capture mode | Panic hook / Drop guard confirmed before merge (C7) |
| Snapshot churn during Steps 1–3 | Treat any diff as regression; investigate before accepting |

---

## 9. Integration Points Checklist (CLAUDE.md Development Rules)

1. **config.toml** — `[tui] mouse` field (§3.6). Designed.
2. **CLI** — no new subcommand; mouse is TUI-only. N/A.
3. **TUI command** — `/mouse [on|off]` + palette "Toggle mouse mode" entry (§3.6). Designed.
4. **Wizard (`--init`)** — `[tui]` mouse yes/no prompt (§3.6). Designed.
5. **Migration (`--migrate-config`)** — step 67: insert `mouse = false` under `[tui]` (§3.6). Designed.
6. **Playbook** — `.local/testing/playbooks/tui-reducer-mouse.md` (developer to author in Step 8).
7. **coverage-status.md** — rows `TUI Reducer` and `TUI Mouse Mode` → `Untested` (developer, Step 8).

---

## 10. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide non-negotiable rules
- [[001-system-invariants/spec]] — Cross-cutting architectural invariants
- [[011-tui/spec]] — TUI dashboard baseline spec
- [[039-background-task-supervisor/spec]] — TaskSupervisor contract (async/spawn rules)
