---
aliases:
  - TUI Reducer Plan
  - Action Decomposition Implementation Plan
tags:
  - sdd
  - plan
  - tui
  - config
created: 2026-06-13
status: approved
related:
  - "[[tui-reducer/spec]]"
  - "[[MOC-specs]]"
---

# Plan: TUI Reducer / Action Decomposition + Opt-in Mouse Mode

> [!info]
> Technical implementation plan for `specs/tui-reducer/spec.md`. Eight ordered steps,
> each compilable and test-green before the next begins. References critic findings C1–C7.

## File Map

### New files

| File | Purpose |
|------|---------|
| `crates/zeph-tui/src/app/action.rs` | `Action`, `CursorMove`, `ScrollDir`, `VertDir`, `PaletteEdit`, `ElicitationEdit` enums |
| `crates/zeph-tui/src/app/reducer.rs` | `reduce()`, `run_effects()`, reducer unit tests |
| `crates/zeph-tui/src/app/mouse.rs` | `decode_mouse()`, `rect_contains()`, mouse unit tests |

### Modified files

| File | Change |
|------|--------|
| `crates/zeph-tui/src/app/mod.rs` | Remove TODO:4-6; add `mouse_enabled`, `last_layout`, `pending_mouse_capture` fields; add `take_mouse_capture_request()`, `with_mouse()` builder |
| `crates/zeph-tui/src/app/keys.rs` | Convert all handler leaves to return `Option<Action>`; `handle_key` calls `decode_key → reduce → run_effects` |
| `crates/zeph-tui/src/event.rs` | Add `AppEvent::Mouse(MouseEvent)` variant; extend `CrosstermEventSource::next_event`; filter `Moved`/`Drag` → `Tick` |
| `crates/zeph-tui/src/app/events.rs` | Add `AppEvent::Mouse` arm calling `handle_mouse` |
| `crates/zeph-tui/src/lib.rs` | Drain `pending_mouse_capture` in post-select block; enable capture after first draw; always `DisableMouseCapture` in `restore_terminal`; confirm/add panic hook |
| `crates/zeph-tui/src/app/draw.rs` | Store `self.last_layout = Some(layout)` at end of `draw()` |
| `crates/zeph-tui/src/layout.rs` | Add `derive(Clone, Copy)` to `AppLayout` |
| `crates/zeph-tui/src/hyperlink.rs` | Add borrowing `fn hyperlinks(&self) -> &[HyperlinkSpan]`; keep `set_hyperlinks` as replace-on-draw |
| `crates/zeph-tui/src/command.rs` | Add `TuiCommand::SetMouse(bool)`, `TuiCommand::ToggleMouse`; palette entry |
| `crates/zeph-tui/src/widgets/status.rs` | Render mouse hint when `mouse_enabled` |
| `crates/zeph-config/src/ui.rs` | Add `pub mouse: bool` to `TuiConfig` with `#[serde(default)]` |
| `src/tui_bridge.rs` | Add `.with_mouse(config.tui.mouse)` to builder chain |
| `src/wizard.rs` | Add mouse yes/no prompt in `[tui]` section |
| `src/migration.rs` | Add step 67: insert `mouse = false` under `[tui]` |
| `.local/testing/playbooks/tui-reducer-mouse.md` | Test scenarios (Step 8) |
| `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` | Add `TUI Reducer` and `TUI Mouse Mode` rows → `Untested` |

## Step-by-step Detail

### Step 1 — Action/Effect Skeleton

**Goal:** reducer compiles and handles a first slice of scroll/toggle actions.

1. Create `app/action.rs`:
   - `Action` enum with: `ScrollLines(i32)`, `ScrollPage(ScrollDir)`, `ScrollToTop`,
     `ScrollToBottom`, `ToggleToolExpanded`, `CycleToolDensity`, `ToggleSidePanels`,
     `ToggleHelp`, `SetHelp(bool)`, and a placeholder `Dispatch(TuiCommand)`.
   - Supporting enums: `ScrollDir { Up, Down }`, `VertDir { Up, Down }`.
2. Create `app/reducer.rs`:
   - `pub(crate) fn reduce(app: &mut App, action: Action) -> Vec<Effect>`.
   - Handle the Step 1 Action slice; all others → `vec![]`.
   - `pub(crate) fn run_effects(app: &mut App, effects: Vec<Effect>)` — stub that handles
     the Step 1 effects (none yet, just scaffolding).
3. Create `app/effect.rs` (or inline in `reducer.rs`): `Effect` enum full definition
   (all variants as per spec §3.2).
4. In `app/mod.rs`: add `pub(crate) mod action`, `pub(crate) mod reducer`.
5. In `app/keys.rs`: convert `handle_normal_key` scroll/toggle leaves to return
   `Option<Action>`; add `handle_key` wrapper calling `decode_key → reduce → run_effects`.
6. Unit tests in `reducer.rs`: for each Step 1 Action, assert state change + no Effect.

**Acceptance:** `cargo nextest run -p zeph-tui` green; existing snapshots unchanged.

---

### Step 2 — Normal-mode + Command Dispatch

**Goal:** all of `handle_normal_key` and `execute_command` emit Actions.

1. Complete remaining `handle_normal_key` leaves: clipboard (`CopyLastAssistant`,
   `CopyLastCodeBlock`), view toggles (`TogglePlanView`, `SetViewTarget`, etc.), `Quit`.
2. Convert `execute_command`: pass-through agent commands → `Action::Dispatch(cmd)`;
   `Effect::SendUserInput/SendCommand` handles them in `run_effects`.
3. Wire `Effect::CopyToClipboard` in `run_effects`.
4. Unit tests: `Dispatch(cmd)` → `[Effect::SendCommand(cmd)]`; `CopyLastAssistant` →
   `[Effect::CopyToClipboard(text)]` (requires a session with messages).

**Acceptance:** green tests; no `self.<field> =` remaining in `handle_normal_key` or
`execute_command` (verified by review, not grep for helpers).

---

### Step 3 — Insert-mode + Modals

**Goal:** all modal and insert-mode paths emit Actions.

Sub-commits recommended per modal:
- 3a: `PaletteEdit` + `ElicitationEdit` enums (C4 — all 7 variants of `ElicitationEdit`).
- 3b: `handle_insert_key` and input composer leaves.
- 3c: Command palette.
- 3d: File picker.
- 3e: Slash autocomplete.
- 3f: Reverse search.
- 3g: Confirm dialog (one-shot sender → `Effect::ResolveConfirm`; INV-R3).
- 3h: Elicitation dialog (one-shot sender → `Effect::ResolveElicitation`; INV-R3).

**C4 gate:** `ElicitationEdit` must have PushChar, PopChar, NextField, PrevField,
ToggleBool, EnumNext, EnumPrev before 3h lands.

**C5 gate:** each sub-handler is conversion-complete before the sub-commit merges —
zero mutation by any path (direct field write or via helpers like `dialog.push_char()`).

**Tests:** one-shot sender tests (see §7 of spec). Snapshot tests must remain unchanged.

---

### Step 4 — Cache Layout + OSC8 Accessor (C1)

**Goal:** `last_layout` persists between frames; OSC8 uses a borrowing accessor.

1. `layout.rs`: add `#[derive(Clone, Copy)]` to `AppLayout`.
2. `app/mod.rs`: add `pub(crate) last_layout: Option<AppLayout>`.
3. `draw.rs` end of `draw()`: `self.last_layout = Some(layout);`.
4. `hyperlink.rs`: add `pub(crate) fn hyperlinks(&self) -> &[HyperlinkSpan]` (borrow).
   Keep `set_hyperlinks` (replaces vec at draw time). Keep existing `take_hyperlinks`
   caller in `tui_loop` → switch it to the borrowing accessor.
5. Verify OSC8 sequences still emitted on redraw after switch (regression test or manual
   check; document result in PR description).

---

### Step 5 — Mouse Plumbing (C6)

**Goal:** mouse events flow from crossterm → `decode_mouse` → reducer.

1. `event.rs`: add `Mouse(crossterm::event::MouseEvent)` to `AppEvent`.
   In `CrosstermEventSource::next_event`: map `CrosstermEvent::Mouse(m)` →
   `Some(AppEvent::Mouse(m))` ONLY for `ScrollUp`, `ScrollDown`, `Down(_)`, `Up(_)` kinds.
   Map `Moved` and `Drag(_)` → `Tick` (C6: no Full dirty, no flood).
2. `app/events.rs`: add `AppEvent::Mouse(m) => self.handle_mouse(m)` arm.
3. Create `app/mouse.rs`:
   - `pub(crate) fn handle_mouse(&mut self, m: MouseEvent)` (guard `mouse_enabled`).
   - `pub(crate) fn decode_mouse(&self, m: MouseEvent) -> Option<Action>` — guards
     `last_layout.is_none()` first (C3 / FR-010).
   - Hit-test table from spec §3.5.
   - `fn rect_contains(r: Rect, col: u16, row: u16) -> bool`.
4. Unit tests: synthetic `MouseEvent` against fixed `AppLayout` covering all hit-test rows.
   Include `None` for `Moved`/`Drag`. Include `None` when `last_layout = None`.

---

### Step 6 — Config + Runtime Toggle (C2, C3, C7)

**Goal:** `[tui] mouse` drives capture; toggle works at runtime; teardown is safe.

1. `crates/zeph-config/src/ui.rs`: add `#[serde(default)] pub mouse: bool` to `TuiConfig`.
2. `app/mod.rs`: add `pending_mouse_capture: Option<bool>`.
   Add `pub(crate) fn take_mouse_capture_request(&mut self) -> Option<bool>`.
3. `reducer.rs`: `Action::SetMouse(b)` → sets `app.mouse_enabled = b`, pushes system
   message `"Mouse mode: on/off — text selection via Shift+drag"`, returns
   `[Effect::SetMouseCapture(b)]`.
4. `run_effects`: `Effect::SetMouseCapture(b)` → `app.pending_mouse_capture = Some(b)`.
5. `lib.rs` post-select block: drain via `app.take_mouse_capture_request()` and run
   `crossterm::execute!(terminal.backend_mut(), DisableAlternateScroll, EnableMouseCapture)`
   or the inverse. (C2: drain in shared post-select, not inside an event arm.)
6. `lib.rs`: startup — if `app.mouse_enabled` after first draw, call `take_mouse_capture_request`
   path (or directly run the execute!) after the first `terminal.draw` returns. (C3)
7. `lib.rs` `restore_terminal`: always emit `DisableMouseCapture` in the teardown sequence.
8. `lib.rs` or bootstrap: confirm or add a panic hook that calls `restore_terminal`. (C7)
   If hook already exists, document it in PR description. If not, add:
   ```rust
   std::panic::set_hook(Box::new(|info| {
       let _ = restore_terminal();
       eprintln!("{info}");
   }));
   ```
9. `src/tui_bridge.rs`: add `.with_mouse(config.tui.mouse)` to builder chain.

---

### Step 7 — Command + UX

**Goal:** `/mouse [on|off]` works; status hint renders; wizard and migration in place.

1. `command.rs`: add `TuiCommand::SetMouse(bool)` and `TuiCommand::ToggleMouse`.
   Add palette entry: `{ id: "app:mouse", label: "Toggle mouse mode (wheel scroll, click focus)", category: "app" }`.
   `PaletteAccept` arm for `app:mouse` reads `mouse_enabled` and dispatches
   `Action::SetMouse(!current)`.
2. `keys.rs` `parse_session_slash`: add `/mouse` arm (bare → `ToggleMouse`; with on/off →
   `SetMouse`).
3. `keys.rs` `execute_command`: `SetMouse(b)` → `Action::SetMouse(b)`; `ToggleMouse` →
   `Action::SetMouse(!app.mouse_enabled)`.
4. `widgets/status.rs`: render `mouse on — text selection via Shift+drag` when `mouse_enabled`
   (truncate gracefully on narrow widths).
5. `src/wizard.rs`: add mouse yes/no prompt in `[tui]` section after motion prompt.
6. `src/migration.rs`: add step 67 inserting `mouse = false` under `[tui]` if absent.

---

### Step 8 — Docs / Tests / Playbook

**Goal:** documentation, playbook, and coverage rows complete.

1. Author `.local/testing/playbooks/tui-reducer-mouse.md` with scenarios:
   - `mouse=false` default: wheel works, selection works, no capture.
   - `/mouse on`: wheel in chat scrolls, click focuses skills/memory panels, status hint visible.
   - `/mouse off`: alternate-scroll restored, selection restored.
   - Panic recovery: kill process with mouse=on, re-open terminal, verify no capture residue.
   - OSC8 links: click a link with mouse=on; verify terminal handling (modern) or no crash (basic).
   - Config `mouse=true`: startup enables capture after first frame; status hint immediate.
2. Add rows to `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md`:
   - `TUI Reducer` | `Untested` | `[today's date]` | `playbooks/tui-reducer-mouse.md`
   - `TUI Mouse Mode` | `Untested` | `[today's date]` | `playbooks/tui-reducer-mouse.md`
3. Update `docs/src/` if user-facing TUI mouse mode is documented.

## Dependencies between steps

```
Step 1 (skeleton)
  └── Step 2 (normal-mode)
        └── Step 3 (modals, split 3a–3h)
              └── Step 4 (layout cache)
                    └── Step 5 (mouse plumbing)
                          └── Step 6 (config + toggle)
                                └── Step 7 (command UX)
                                      └── Step 8 (docs)
```

Each step is a PR-ready unit; the developer MAY split into multiple commits.
