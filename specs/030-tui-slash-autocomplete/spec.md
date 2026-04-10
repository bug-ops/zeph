---
aliases:
  - TUI Slash Autocomplete
  - Autocomplete
tags:
  - sdd
  - spec
  - tui
  - ui
created: 2026-04-04
status: draft
related:
  - "[[MOC-specs]]"
  - "[[011-tui/spec]]"
---

# Feature: TUI Slash-Command Autocomplete

> **Status**: Draft
> **Author**: Andrei G.
> **Date**: 2026-04-04
> **Branch**: feat/m{N}/{issue}-tui-slash-autocomplete

---

## 1. Overview

### Problem Statement

The TUI input bar accepts `/`-prefixed commands (e.g. `/skill list`, `/graph stats`,
`/plan status`). Users must know the exact command spelling to type it — there is no
discovery aid, no completion, and no validation before submission. This increases
cognitive load and leads to "command not found" errors for new or infrequent users.

### Goal

When the user types `/` in Insert mode, an inline autocomplete dropdown appears
immediately below the input bar. The dropdown filters and ranks suggestions from the
existing `CommandEntry` registry as the user continues typing. Pressing Tab or Enter
accepts the highlighted suggestion; Esc dismisses.

### Out of Scope

- Autocomplete for free-text arguments after a fully-typed command
  (e.g., `/graph facts <entity-name>`)
- Autocomplete in Normal mode (`:` opens the full Command Palette, which is separate)
- Autocomplete in Telegram or CLI channels
- Fuzzy-search engine replacement — the existing `filter_commands` function in
  `crates/zeph-tui/src/command.rs` is reused as-is

---

## 2. User Stories

### US-001: Discover commands by typing `/`

AS A TUI user  
I WANT an autocomplete dropdown to appear the moment I type `/` in the input bar  
SO THAT I can discover available commands without memorising their names

**Acceptance criteria:**

```
GIVEN the input bar is in Insert mode
WHEN the user types the `/` character
THEN a dropdown appears showing all available commands, ranked by the existing
     filter_commands("") result, limited to the first 8 entries visible at a time
```

### US-002: Filter suggestions while typing

AS A TUI user  
I WANT the suggestion list to narrow as I type more characters  
SO THAT I can quickly locate a specific command

```
GIVEN the autocomplete dropdown is visible
WHEN the user types additional characters after `/`
THEN the dropdown re-filters using filter_commands(<text-after-slash>)
AND  the selection resets to index 0 on each keystroke
```

### US-003: Navigate and accept a suggestion

AS A TUI user  
I WANT to navigate the dropdown with Tab / Up / Down and accept with Tab or Enter  
SO THAT I can complete a command without typing its full name

```
GIVEN the autocomplete dropdown is visible with at least one suggestion
WHEN the user presses Tab or Down
THEN the selection advances to the next entry (wrapping at the bottom)

WHEN the user presses Shift-Tab or Up
THEN the selection moves to the previous entry (wrapping at the top)

WHEN the user presses Tab while a suggestion is highlighted
THEN the input is replaced with the command's slash-form derived from its id
     (e.g., "skill:list" → "/skill list")
AND  the cursor is placed at the end of the inserted text
AND  the dropdown is dismissed

WHEN the user presses Enter while a suggestion is highlighted
THEN the same replacement + dismiss happens as for Tab
```

### US-004: Dismiss the dropdown

AS A TUI user  
I WANT to dismiss the dropdown without accepting a suggestion  
SO THAT I can type a free-text message that starts with `/`

```
GIVEN the autocomplete dropdown is visible
WHEN the user presses Esc
THEN the dropdown is dismissed
AND  the input buffer retains its current content (no modification)

GIVEN the autocomplete dropdown is visible
WHEN the user presses Backspace and the input becomes empty (the `/` itself is deleted)
THEN the dropdown is dismissed automatically
```

### US-005: Empty filter shows no extra noise

```
GIVEN the autocomplete dropdown is visible
WHEN filter_commands returns zero matches for the typed prefix
THEN the dropdown is dismissed automatically
     (rather than showing an empty popup)
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN the user types `/` as the first character of an empty input in Insert mode THE SYSTEM SHALL open the slash autocomplete dropdown | must |
| FR-002 | WHEN the user types additional characters after `/` THE SYSTEM SHALL re-filter and re-render the dropdown on every keystroke | must |
| FR-003 | WHEN the dropdown is visible THE SYSTEM SHALL display at most 8 entries at a time with scroll when more exist | must |
| FR-004 | WHEN the dropdown is visible THE SYSTEM SHALL highlight the currently selected entry using the theme's highlight style | must |
| FR-005 | WHEN Tab or Down is pressed while the dropdown is visible THE SYSTEM SHALL advance the selection, wrapping from the last to the first entry | must |
| FR-006 | WHEN Shift-Tab or Up is pressed while the dropdown is visible THE SYSTEM SHALL move the selection backwards, wrapping from the first to the last | must |
| FR-007 | WHEN Tab or Enter is pressed on a highlighted entry THE SYSTEM SHALL replace the input text with the command's canonical slash-form and dismiss the dropdown | must |
| FR-008 | WHEN Esc is pressed while the dropdown is visible THE SYSTEM SHALL dismiss the dropdown without modifying the input | must |
| FR-009 | WHEN Backspace is pressed and the resulting input no longer starts with `/` THE SYSTEM SHALL dismiss the dropdown automatically | must |
| FR-010 | WHEN filter_commands returns an empty result THE SYSTEM SHALL dismiss the dropdown automatically | must |
| FR-011 | WHEN the dropdown is visible THE SYSTEM SHALL render it in a popup anchored just above the input bar (or below if input is at the bottom row) | must |
| FR-012 | THE SYSTEM SHALL derive the canonical slash-form of a CommandEntry from its `id` field by replacing `:` with ` ` and prepending `/` (e.g. `skill:list` → `/skill list`) | must |
| FR-013 | WHEN the user types `/` in the middle of existing input (cursor not at position 0) THE SYSTEM SHALL NOT trigger the autocomplete dropdown | should |
| FR-014 | WHEN the dropdown is visible THE SYSTEM SHALL show each entry as: `  <id padded>  <label>  [shortcut]` matching the existing CommandPaletteState row format | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Filter and re-render must complete within a single 16 ms frame; no async work is performed — all filtering is synchronous over the static registries |
| NFR-002 | Correctness | Must not interfere with existing key bindings: Esc (→ Normal mode), Enter (→ submit), Up/Down (→ history navigation) are re-routed only while the dropdown is active |
| NFR-003 | Correctness | Must not conflict with the existing Command Palette (`:` in Normal mode) — the two flows are mutually exclusive by construction (`command_palette` vs `slash_autocomplete` state fields) |
| NFR-004 | Correctness | Must not conflict with the `@` file picker (`KeyCode::Char('@')` path in `handle_insert_key`) |
| NFR-005 | Accessibility | The popup must be dismissible without accepting any suggestion at any point |
| NFR-006 | Architecture | The autocomplete state must live in `App` as a dedicated field, not inside `CommandPaletteState` |
| NFR-007 | Architecture | No new crate dependencies are permitted; ratatui `List` + `Clear` widgets (already used) are sufficient |

---

## 5. Data Model

### `SlashAutocompleteState` (new struct in `crates/zeph-tui/src/widgets/slash_autocomplete.rs`)

| Field | Type | Description |
|-------|------|-------------|
| `query` | `String` | The text after the leading `/` in the input bar |
| `selected` | `usize` | Index of the currently highlighted entry |
| `filtered` | `Vec<&'static CommandEntry>` | Filtered + ranked entries from `filter_commands` |
| `scroll_offset` | `usize` | First visible row index (for scroll when `filtered.len() > MAX_VISIBLE`) |

Constant: `MAX_VISIBLE: usize = 8`

### `App` field addition

```rust
slash_autocomplete: Option<SlashAutocompleteState>,
```

`None` = dropdown hidden. `Some(_)` = dropdown active.

### Canonical slash-form derivation (pure function, no allocation beyond one `String`)

```
fn command_id_to_slash_form(id: &str) -> String {
    format!("/{}", id.replace(':', " "))
}
```

Examples:
- `skill:list` → `/skill list`
- `graph:stats` → `/graph stats`
- `app:quit` → `/app quit`
- `ingest` (no colon) → `/ingest`

---

## 6. UX Behaviour Specification

### Trigger condition

The dropdown opens **only** when:
1. The user is in `InputMode::Insert`, and
2. The character `/` is typed, and
3. The current input buffer was empty before that keystroke (i.e., the input becomes
   exactly `"/"` after the event)

Condition 3 prevents the dropdown from hijacking mid-sentence input.

### Rendering position

The dropdown is rendered as a floating popup anchored immediately **above** the input
bar widget area. Height = `min(filtered.len(), MAX_VISIBLE) + 2` rows (border padding).
Width = 60 columns centred in the terminal, same sizing policy as the existing
`CommandPaletteState` rendering in `crates/zeph-tui/src/widgets/command_palette.rs`.

The `frame.render_widget(Clear, popup_rect)` call must precede the popup content
render, following the existing pattern.

### Key routing priority

While `slash_autocomplete.is_some()`, `handle_insert_key` must route to
`handle_slash_autocomplete_key` first, before processing any other Insert-mode keys.
This mirrors the existing pattern for `command_palette`, `file_picker_state`, etc.:

```
handle_key_event
├── confirm_state.is_some()     → handle_confirm_key
├── elicitation_state.is_some() → handle_elicitation_key
├── command_palette.is_some()   → handle_palette_key
├── file_picker_state.is_some() → handle_file_picker_key
└── match input_mode
    ├── Normal → handle_normal_key
    └── Insert → handle_insert_key (which delegates to handle_slash_autocomplete_key
                                     when slash_autocomplete.is_some())
```

Note: the slash autocomplete is an Insert-mode overlay, not a modal. The routing check
belongs inside `handle_insert_key`, not at the top-level `handle_key_event` dispatch.

### Key behaviour table

| Key | Dropdown visible | Action |
|-----|-----------------|--------|
| Any printable char | yes | Append to query after `/`, re-filter, reset `scroll_offset` to show selection |
| Backspace | yes | Remove last char from query; if input becomes `""` (no `/` left), dismiss |
| Tab | yes | Accept selection (replace input, dismiss) |
| Enter | yes | Accept selection (replace input, dismiss) then submit input immediately |
| Down | yes | Advance selection (wrap), update scroll if needed |
| Up | yes | Retreat selection (wrap), update scroll if needed |
| Shift-Tab | yes | Same as Up |
| Esc | yes | Dismiss, retain input content, return to normal Insert-mode handling |
| Tab | no (Insert mode) | No change (Tab is not currently bound in Insert mode — no conflict) |

Note on Enter: when Enter accepts a suggestion, the input is replaced with the
slash-form and then `submit_input()` is called immediately. If Enter is pressed but
`selected_entry()` returns `None` (empty filtered list), the dropdown has already been
auto-dismissed per FR-010, so Enter falls through to normal submit. This is the
intuitive UX: Enter always submits.

### Scroll behaviour

`scroll_offset` tracks the index of the first rendered row.

- When `selected` moves below `scroll_offset + MAX_VISIBLE - 1`, increment
  `scroll_offset`.
- When `selected` moves above `scroll_offset`, decrement `scroll_offset`.
- `scroll_offset` must never exceed `filtered.len().saturating_sub(MAX_VISIBLE)`.

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behaviour |
|----------|--------------------|
| User types `/x` where `x` matches nothing | `filter_commands("x")` returns empty; dropdown dismissed automatically (FR-010) |
| User moves selection past the last item | Selection wraps to index 0 |
| User moves selection before index 0 | Selection wraps to `filtered.len() - 1` |
| Terminal is very narrow (< 30 cols) | Popup clips gracefully — ratatui Rect clamping handles this; no panic |
| User types `/` with non-empty input before the cursor | Dropdown does not open (FR-013); the `/` is inserted as a literal character |
| filter_commands returns >100 entries (empty query) | Only first `MAX_VISIBLE` shown; scroll navigation reveals the rest |
| User accepts a suggestion whose slash-form contains a trailing space for arguments | Accepted as-is; cursor lands at end of inserted text |
| Command Palette is open when user presses Esc then types `/` | Command Palette closes first (handled by existing Esc routing); then `/` opens autocomplete on next keypress |

---

## 8. Architecture Sketch

### New files

```
crates/zeph-tui/src/widgets/slash_autocomplete.rs   — SlashAutocompleteState struct + render fn
```

### Modified files

```
crates/zeph-tui/src/app.rs
    — add field: slash_autocomplete: Option<SlashAutocompleteState>
    — add method: handle_slash_autocomplete_key(&mut self, key: KeyEvent)
    — modify handle_insert_key: delegate to handle_slash_autocomplete_key when active
    — modify handle_insert_key: Char('/') branch opens autocomplete when input was empty

crates/zeph-tui/src/widgets/mod.rs
    — pub mod slash_autocomplete;

crates/zeph-tui/src/app.rs (render section)
    — render slash_autocomplete popup after input bar, before command_palette
```

### Reused without modification

- `crates/zeph-tui/src/command.rs` — `filter_commands`, `CommandEntry`, registries
- `crates/zeph-tui/src/layout.rs` — `centered_rect` for popup positioning
- `crates/zeph-tui/src/theme.rs` — `Theme` for consistent styling

### No changes to

- `zeph-core` agent loop — slash commands entered via autocomplete are plain text sent
  through `user_input_tx` exactly as if the user had typed them manually
- `zeph-channels` — TUI-internal only
- Any other crate

---

## 9. Acceptance Criteria

| ID | Criterion | Verification |
|----|-----------|--------------|
| AC-001 | Typing `/` in an empty Insert-mode input opens the dropdown | Unit test: inject `KeyCode::Char('/')` on empty input, assert `slash_autocomplete.is_some()` |
| AC-002 | Typing `/sk` narrows the list to entries matching "sk" via `filter_commands` | Unit test: assert `filtered` equals `filter_commands("sk")` after two keypresses |
| AC-003 | Tab on the first entry replaces input with the correct slash-form | Unit test: `command_id_to_slash_form("skill:list")` == `"/skill list"` |
| AC-004 | Esc dismisses the dropdown without modifying the input | Unit test: assert `slash_autocomplete.is_none()` and `input == "/sk"` |
| AC-005 | Backspace that removes the `/` dismisses the dropdown | Unit test |
| AC-006 | Down wraps from last to first entry | Unit test |
| AC-007 | Up wraps from first to last entry | Unit test |
| AC-008 | Dropdown does not open when `/` is typed mid-input | Unit test: input = `"hello "`, type `/`, assert `slash_autocomplete.is_none()` |
| AC-009 | Empty filter result auto-dismisses the dropdown | Unit test: filter result empty → `slash_autocomplete.is_none()` |
| AC-010 | Command Palette (`:` in Normal mode) is unaffected by this change | Existing command_palette tests continue to pass |
| AC-011 | Existing snapshot tests pass or are updated to reflect the new widget | `cargo insta test` passes |
| AC-012 | `cargo nextest run -p zeph-tui` passes with zero warnings | CI gate |

---

## 10. Implementation Tasks

### T001: Add `SlashAutocompleteState` and render function

- Create `crates/zeph-tui/src/widgets/slash_autocomplete.rs`
- Implement `SlashAutocompleteState` with fields described in Section 5
- Implement `push_char`, `pop_char`, `move_up`, `move_down`, `selected_entry`,
  `refilter` methods following the pattern of `CommandPaletteState`
- Implement `command_id_to_slash_form` pure function
- Implement `render(state, frame, area)` anchored above input bar using `Clear` +
  `List` widgets, capped at `MAX_VISIBLE` rows with scroll
- Register module in `crates/zeph-tui/src/widgets/mod.rs`
- Unit tests for all `SlashAutocompleteState` methods + `command_id_to_slash_form`
- Dependencies: none

### T002: Wire autocomplete into `App`

- Add `slash_autocomplete: Option<SlashAutocompleteState>` field to `App`, initialised
  to `None`
- In `handle_insert_key`, add opening logic in the `KeyCode::Char('/')` branch:
  if input was empty before the keystroke, open `SlashAutocompleteState::new()`
- Add `handle_slash_autocomplete_key(&mut self, key: KeyEvent)` method implementing
  the key behaviour table from Section 6
- Delegate to `handle_slash_autocomplete_key` at the top of `handle_insert_key` when
  `slash_autocomplete.is_some()`
- Dependencies: T001

### T003: Render integration

- In the `App::render` method, call `slash_autocomplete::render` when
  `self.slash_autocomplete.is_some()`, rendering the popup over the input bar area
- Ensure render order: input bar first, then autocomplete popup on top (so popup
  visually overlays), before `command_palette` overlay
- Update or add insta snapshot tests for the new popup
- Dependencies: T002

### T004: Integration tests and coverage check

- Verify `cargo nextest run -p zeph-tui` passes with `--features full`
- Run `cargo insta test --workspace --features full --check --lib --bins`; accept any
  new snapshots with `cargo insta accept`
- Add a row to `.local/testing/coverage-status.md` with status `Untested`
- Add playbook to `.local/testing/playbooks/tui-slash-autocomplete.md`
- Dependencies: T003

---

## 11. Agent Boundaries

### Always (without asking)

- Run `cargo nextest run -p zeph-tui` after every task
- Follow existing `CommandPaletteState` patterns for state management
- Add SPDX headers to new `.rs` files (`.github/scripts/add-spdx-headers.sh`)
- Update insta snapshots when rendering changes (`cargo insta accept`)

### Ask First

- Changing the trigger condition (FR-013 — currently: only when input was empty)
- Adding new key bindings that could conflict with Insert-mode bindings
- Changing `filter_commands` scoring or registry layout

### Never

- Introduce new crate-level dependencies
- Modify `crates/zeph-tui/src/command.rs` registries (command data is source of truth)
- Block or yield inside the key handler
- Open autocomplete in Normal mode (that is the Command Palette's domain)

---

## 12. Open Questions

None. All design decisions are resolved in this spec.

---

## 13. References

- `crates/zeph-tui/src/command.rs` — `CommandEntry`, `filter_commands`, `fuzzy_score`
- `crates/zeph-tui/src/widgets/command_palette.rs` — `CommandPaletteState` pattern to follow
- `crates/zeph-tui/src/app.rs` — `handle_insert_key`, `handle_palette_key`, modal routing
- `.local/specs/011-tui/spec.md` — TUI architecture, Spinner Rule
- `.local/specs/constitution.md` — project-wide constraints
