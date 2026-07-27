---
aliases:
  - TUI Inline Mention Picker
  - @mention Autocomplete
  - File/Skill/Agent Mention Picker
tags:
  - sdd
  - spec
  - tui
  - ui
  - ux
created: 2026-07-22
status: approved
related:
  - "[[MOC-specs]]"
  - "[[011-tui/spec]]"
  - "[[030-tui-slash-autocomplete/spec]]"
  - "[[tui-reducer/spec]]"
  - "[[UX/mention-routing]]"
  - "[[044-subagent-lifecycle/spec]]"
  - "[[005-skills/spec]]"
---

# Feature: TUI Inline Mention Picker (@files / @skills / @agents)

> **Status**: Approved
> **Author**: Andrei G.
> **Date**: 2026-07-22
> **Branch**: ux-search-status-improvements-bdc0d7
> **GitHub Issues**: #6647 (inline picker), #6648 (skill/agent categories); follow-ups #6650 (fuzzy-engine unification), #6651 (empty-query ordering)

---

## 1. Overview

### Problem Statement

The TUI input accepts mentions using `@` (currently a modal file picker) and free-text
skill/agent names. Users must know exact file paths and skill names, and the file picker
modal interrupts input flow. There is no discovery aid for available skills or agents.

### Goal

Transform `@` from a modal file picker into an inline non-modal popup (matching the
inline slash-autocomplete from spec 030) with three browseable categories:
- **Files** — repo-relative paths with match highlighting
- **Skills** — registered skill names with descriptions
- **Agents** — spawnable/available sub-agent names with descriptions

Support word-start trigger (`@` only opens when at position 0 or after whitespace),
fuzzy matching via `nucleo`, tab cycling, and accept semantics that differ by
category (file paths as plain text, skill names without forced activation,
agent mentions with `@` sigil preserved).

### Out of Scope

- Multi-line skill/agent descriptions in the popup
- Force-opening remote agent registries (A2A peer lookup) — only local definitions
- **Amended**: cursor-movement shortcuts inside the popup beyond Up/Down (selection) and
  Left/Right (tab cycling, FR-004/D2) — Left/Right were originally scoped out here but
  are a `must` requirement per FR-004; both are implemented. Genuinely out of scope:
  any additional in-popup cursor gesture beyond those two pairs
- Changing the existing file index TTL (30s) or MAX_RESULTS (10)

---

## 2. User Stories

### US-001: Discover files with `@` inline picker

AS A TUI user  
I WANT to type `@` at the start of my input and see an inline popup listing repository files  
SO THAT I can quickly reference file paths without typing them fully

**Acceptance criteria:**

```
GIVEN the input bar is in Insert mode with empty or whitespace-only prefix
WHEN the user types `@` as the first character (or after whitespace)
THEN a non-modal popup appears showing filtered repository files (All tab, files category)
AND the popup never steals focus from the input buffer
AND typing continues into the buffer; the popup reflects the typed query in real time
```

### US-002: Filter across files, skills, agents with tabs

AS A TUI user  
I WANT to switch between tabs (All | Files | Skills | Agents) to narrow the result set  
SO THAT I can browse available resources by type

**Acceptance criteria:**

```
GIVEN the mention picker popup is visible
WHEN the user presses Left/Right arrow
THEN the active tab changes
AND the list re-filters to show only entries from that category
AND the All tab shows mixed results — ranked by fuzzy match score when a query is typed,
  or round-robined across non-empty categories when the query is empty (see FR-018 amendment)
```

### US-003: Complete a mention and continue typing

AS A TUI user  
I WANT to select a file/skill/agent from the popup with Tab or Enter  
SO THAT it is inserted into the input and I can continue typing

**Acceptance criteria:**

```
GIVEN the mention picker is visible with a selection
WHEN the user presses Tab or Enter
THEN the whole mention token (`@` through the next whitespace, not just the text up to
  the cursor — see M4 in "Accepting a Selection") is replaced with the chosen entry
AND a trailing space is inserted (unless already present)
AND the popup closes
AND the cursor is positioned after the space, ready to continue typing
AND the selected entry type determines insertion format:
  - File: bare repo-relative path (e.g., "src/main.rs")
  - Skill: skill NAME without activation prefix (e.g., "web_search")
  - Agent: mention sigil retained (e.g., "@my_agent")
```

### US-004: Dismiss the picker cleanly

AS A TUI user  
I WANT to close the mention picker and keep my typed input  
SO THAT I can edit or discard the `@` mention without starting over

**Acceptance criteria:**

```
GIVEN the mention picker is open
WHEN the user presses Esc
THEN the popup closes WITHOUT modifying the input
AND Insert mode is retained (do NOT switch to Normal)
WHEN the user presses Space
THEN the popup closes and the Space is inserted at the cursor position as ordinary typed text (the `@query` text stays in the buffer as plain prose)
WHEN the user presses Backspace past the `@` character
THEN the popup closes automatically
```

### US-005: Graceful degradation with empty catalogs

AS A TUI user in a sparse environment  
I WANT the mention picker to show helpful messages when a category is empty  
SO THAT I understand why no results appear

**Acceptance criteria:**

```
GIVEN the mention picker is visible on an empty Skills category tab
WHEN the tab shows no registered skills
THEN a dimmed row appears: "no skills loaded"
WHEN the All tab is visible and one category is empty (e.g., no agents defined)
THEN the All tab omits that category entirely (rows are files + skills only)
WHEN all categories are empty
THEN a message "no results" appears and the popup remains open
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN the user types `@` at a word-start position (position 0 or preceded by whitespace) THE SYSTEM SHALL insert the character into the buffer AND open the non-modal mention picker popup | must |
| FR-002 | WHEN `@` is typed mid-word (e.g., `user@example.com`) THE SYSTEM SHALL NOT open the popup; the `@` is inserted as a literal character | must |
| FR-003 | WHEN the popup is visible every keystroke appends/deletes from the input buffer (not captured away); the popup reflects the buffer text after the `@` in real time | must |
| FR-004 | WHEN the user presses Left/Right while the popup is visible THE SYSTEM SHALL cycle through tabs (All → Files → Skills → Agents → All) | must |
| FR-005 | WHEN the user presses Up/Down while the popup is visible THE SYSTEM SHALL move the selection highlight within the current tab, wrapping at boundaries | must |
| FR-006 | **(Amended, M4)** WHEN Tab or Enter is pressed on a selected entry THE SYSTEM SHALL replace the whole mention *token* (not just the typed query up to the cursor — see "Accepting a Selection") with the selected entry, appending one trailing space unless the next character is already whitespace; the popup closes | must |
| FR-007 | WHEN Space is pressed while the popup is visible THE SYSTEM SHALL close the popup; the Space is inserted at the cursor position as an ordinary character and the `@query` text stays in the buffer as plain prose | must |
| FR-008 | WHEN Esc is pressed THE SYSTEM SHALL close only the popup, retain Insert mode, keep the input buffer intact | must |
| FR-009 | WHEN Backspace is pressed and the `@` is deleted THE SYSTEM SHALL close the popup automatically | must |
| FR-010 | **(Amended, D2)** WHEN cursor-mutating input other than Up/Down (selection) or Left/Right (tab cycling, FR-004) leaves the `@query` span — i.e. Home/End, Alt+Left/Alt+Right, Ctrl+A/Ctrl+E, or a mouse click — THE SYSTEM SHALL close the popup. Plain Left/Right never move the cursor while the popup is open, so they cannot trigger this rule. A cursor landing exactly one position after `@` (empty query, not yet past `@`) is still inside the span and does not close the popup by itself (see "Cursor Movement") | must |
| FR-011 | WHEN the file index is (re)building (first build or stale-TTL rebuild) THE SYSTEM SHALL show an "indexing files…" placeholder row in the Files tab with no input loss race | must |
| FR-012 | WHEN the mention picker renders THE SYSTEM SHALL use nucleo fuzzy matching for all three categories | must |
| FR-013 | THE SYSTEM SHALL render match-character highlighting (nucleo indices) on all results | must |
| FR-014 | THE SYSTEM SHALL display an `N/M` result counter in the popup border title (e.g., "Files (3/50)") | must |
| FR-015 | WHEN a File entry is accepted THE SYSTEM SHALL insert the bare repo-relative path WITHOUT a file:// prefix or quotes | must |
| FR-016 | WHEN a Skill entry is accepted THE SYSTEM SHALL insert the skill name as plain text (no `/skill` prefix, no forced activation) | must |
| FR-017 | WHEN an Agent entry is accepted THE SYSTEM SHALL insert the agent name with the `@` sigil (e.g., `@my_agent`) | must |
| FR-018 | **(Amended)** WHEN the All tab is active with a non-empty typed query THE SYSTEM SHALL rank results by fuzzy match score across all categories, with per-row type indicators. WHEN the query is empty, results are instead round-robined across non-empty categories (not score-ranked — an empty query scores every candidate equally, so score-ranking would let files, the largest category, crowd out Skills/Agents entirely). **#6651 (implemented)**: within the Files category specifically, the candidates fed into that round-robin are recency-ordered — uncommitted changes first (most recently modified first), then remaining tracked files by mtime descending, computed by `FileIndex::build` — instead of alphabetical; Skills/Agents keep their catalog (alphabetical) order. Round-robin itself (the All-tab interleaving) is unchanged by #6651 | must |
| FR-019 | WHEN a Skills or Agents category is empty THE SYSTEM SHALL show a dimmed placeholder row; the All tab shall omit that category entirely | must |
| FR-020 | THE SYSTEM SHALL apply the word-start trigger rule consistently: the popup opens ONLY when `@` is at position 0 or preceded by whitespace | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Fuzzy match, filter, and re-render must complete within 16 ms; no blocking I/O on the render thread |
| NFR-002 | Performance | The file index build (FileIndex::build) must not block user input; it runs as a supervised background task with a 30s TTL |
| NFR-003 | Correctness | Typing is never captured away from the input buffer; all keystrokes go into the buffer, the popup merely reflects and filters |
| NFR-004 | Correctness | No input loss during index build; keystrokes do not leak if the popup opens late |
| NFR-005 | Correctness | Esc closes only the popup and never leaves Insert mode; Must not fall through to Insert→Normal transition |
| NFR-006 | Correctness | Unknown agent mentions must not error; they must pass through to the LLM for normal routing (see spec 044 slash_commands dispatch) |
| NFR-007 | Correctness | Popup interaction must obey tui-reducer INV-R1 and INV-R2 (mutation only in reduce, no I/O in reduce) |
| NFR-008 | Correctness | No blocking I/O on the render thread when loading skill/agent catalogs or file index |
| NFR-009 | UX | Match highlighting and type indicators must be visible in both dark and light themes |
| NFR-010 | Accessibility | The popup must be dismissible at any point without accepting a suggestion |

---

## 5. Trigger and Scope Rules

### Word-Start Trigger

The `@` picker opens **only when all** of the following hold:

1. The user is in `InputMode::Insert`
2. The character `@` is typed
3. The character is at **position 0** OR is **preceded by whitespace** (space, newline, tab)

Examples that **open** the popup:
- Empty input: type `@` → popup opens
- After space: `"hello "` then type `@` → popup opens
- After newline: `"line1\n"` then type `@` → popup opens

Examples that **do NOT open** the popup:
- Mid-word: `"user"` then type `@` → inserts literal `@`, no popup
- After colon: `"file:"` then type `@` → inserts literal `@`, no popup (edge case per FR-002, mid-token)
- Inside a mention: `"@hello"` then type anything → no new popup (single active mention at a time)

### Query Span

The "query" is the text between the `@` character and the cursor, not including the `@` itself.

- Input: `"@file"`, cursor at end → query = `"file"`
- Input: `"hello @search"`, cursor at end → query = `"search"`
- Input: `"@foo bar"`, cursor after `@foo` → query = `"foo"` (space ends the span)

Cursor movement that leaves this span (e.g., moving left past the `@`, or moving to another word) closes the popup — via Home/End, Alt+Left/Alt+Right, Ctrl+A/Ctrl+E, or a mouse click; plain Left/Right do not move the cursor at all while the popup is open (they cycle tabs instead, FR-004/D2) and so cannot trigger this rule. See §7 "Cursor Movement" for the full, amended rule including the post-`@` boundary case.

---

## 6. Data Model

### Mention Picker State (as implemented — corrects the original draft below)

> **Amended (post-implementation).** The original draft stored `query: String` alongside
> `all_entries: MentionEntries`. The approved architecture (2026-07-27 R1) deliberately
> does **not** mirror the query into a second field: `at_char_index` (the char index of
> the triggering `@`) is the only position stored, and the query is always the buffer
> slice `input[at_char_index+1..cursor_position]`, re-derived by the reducer
> (`reducer::mention_picker_query`) after every action. This avoids the parallel-string
> bug class visible in `SlashAutocomplete*PushChar/PopChar`. Catalogs are also
> heterogeneous (`MentionCatalog`, not a single `MentionEntries`) since Files/Skills are
> `Option` (loading vs. loaded-empty) while Agents is not (see Data Sources below).

```rust
struct MentionPickerState {
    at_char_index: usize,                      // char index of the triggering `@`; query is derived, never stored
    active_tab: MentionTab,                    // All | Files | Skills | Agents
    selected: usize,                            // current selection index in filtered list
    filtered: Vec<MentionEntry>,               // filtered results for active tab (≤ MAX_RESULTS)
    catalog: MentionCatalog,                    // Files/Skills/Agents sources
    matcher: Matcher,                           // nucleo matcher, reused across refilters
}

enum MentionTab {
    All,
    Files,
    Skills,
    Agents,
}

struct MentionEntry {
    kind: MentionKind,
    display: String,                           // e.g., "src/main.rs", "web_search", "my_agent"
    description: Option<String>,               // e.g., skill description, agent description (dimmed in popup)
    indices: Vec<u32>,                         // nucleo match char indices, sorted+deduped, for highlighting
}

enum MentionKind {
    File,
    Skill,
    Agent,
}

struct MentionCatalog {
    files: Option<Arc<Vec<String>>>,           // None = index still building; Some(empty) = loaded, no files
    skills: Option<Arc<[SkillCatalogItem]>>,   // None = catalog not yet delivered; Some(empty) = loaded, no skills
    agents: Arc<[AgentDefSummary]>,            // always populated from MetricsSnapshot (D1) — never "loading"
}
```

### Integration with `App`

```rust
// Add to App struct
mention_picker: Option<MentionPickerState>,
```

Initialization: `None` (popup is closed). When `@` opens it at word-start, a new `MentionPickerState` is created with empty query and all entries from the three catalogs.

### Data Sources

| Category | Source | API / Method |
|----------|--------|------|
| **Files** | File index (existing `FileIndex` in `zeph-tui`) | `FileIndex::build()` (TTL 30s, supervised task), `FileIndex::paths_arc()` |
| **Skills** | Skill registry, via a new event | `Channel::send_skill_catalog(&[SkillCatalogItem])` → `AgentEvent::SkillCatalog`, built from `SkillRegistry::all_meta()` (`crates/zeph-skills/src/registry.rs:330`), filtered to exclude `SkillTrustLevel::Blocked` |
| **Agents** | `MetricsSnapshot::agent_definitions` (D1 — no new plumbing) | `App.metrics.agent_definitions: Arc<[AgentDefSummary]>`, already populated and refreshed every render frame |

> **Data plumbing decision (amended, D1 — 2026-07-27 architecture review): Agents need no new plumbing.** The original text below asserted "the TUI currently receives only runtime-active names via `MetricsSnapshot`" — false for agents: `MetricsSnapshot::agent_definitions: Arc<[AgentDefSummary]>` already carries name + description for every `.zeph/agents/*.md` definition, is already in `App.metrics`, and is already consumed by the Settings view's Agents tab. It is an `Arc<[…]>` refreshed once per render frame (`poll_metrics`), so cloning it per picker-open is a refcount bump, not a reallocation — the "avoids bloating every metrics frame" rationale below does not apply to it. It is also re-derived on config reload, whereas a startup-only catalog event would go stale there. **Skills genuinely have no equivalent path** (`MetricsSnapshot` carries only `active_skills: Vec<String>` names, no descriptions, and hot-reload never refreshes even that) — the dedicated-event decision below stands for Skills only.
>
> **Skills catalog delivery via a dedicated event.** Full skill catalogs (name + description) are delivered over the existing agent-event channel as a dedicated catalog event (`Channel::send_skill_catalog` / `AgentEvent::SkillCatalog`) emitted once at agent startup and re-emitted on skill hot-reload — NOT embedded into the per-tick `MetricsSnapshot` (avoids bloating every metrics frame with static catalog data). `Channel::send_skill_catalog` must be explicitly forwarded by every `impl Channel` wrapper (`AnyChannel`, `GatewayChannel`, and — the easiest one to miss — `AppChannel`, the binary's actual TUI-mode dispatcher) or the trait's no-op default silently wins and the Skills tab stays empty in the real binary while `TuiChannel`-level unit tests still pass.
>
> **Rationale**: `zeph-tui` has no dependency on `zeph-skills` (verified in `crates/zeph-tui/Cargo.toml`); event-based delivery keeps it that way and reuses the channel the TUI already consumes. Rejected alternative: direct supervised load in `zeph-tui` mirroring `FileIndex::build()` — would require adding a `zeph-skills` dependency and duplicate catalog-loading logic that `zeph-core` already performs at bootstrap.

---

## 7. UX Behavior

### Opening the Popup

**Amended (matches the approved R1 naming deviation, §6/§9 above — no `Action::OpenMentionPicker` exists).**
When the word-start trigger is satisfied, inside the existing `Action::InsertChar('@')` reducer arm:

1. The `@` is inserted into the input buffer (as any other character would be)
2. `MentionPickerState::new(at_char_index, app.mention_catalog())` is created, where
   `at_char_index` is the char index of the just-inserted `@`; no `query` field is
   stored (see §6) — the query starts implicitly empty since `cursor_position ==
   at_char_index + 1`
3. `refilter("")` runs immediately, populating `filtered` via round-robin (§3 FR-018 amendment)
4. Further keystrokes flow through the ordinary `InsertChar`/`Delete*` reducer arms;
   each is followed by `sync_mention_picker`, which re-derives the query from the
   buffer and re-filters (see §6) — there is no separate "append to query" step

### Typing & Filtering

**Amended** — no `scroll_offset` field exists; `MAX_RESULTS = 10` caps `filtered` at a
size that always fits the popup's fixed height, so ratatui's `ListState` selection alone
keeps `selected` visible with no separate scroll bookkeeping.

On every character typed (while the popup is open):

1. The character is inserted into the input buffer via the ordinary `InsertChar`
   reducer arm (never a separate `query` field — see §6)
2. `sync_mention_picker` re-derives the query from the buffer and calls
   `refilter(&query)`, which re-computes `filtered` for the active tab using
   `nucleo_matcher::Matcher`
3. `selected` resets to 0 inside `refilter`
4. The popup re-renders with highlighted match indices and result count

### Tab Cycling (Left/Right)

- Left/Right arrows (while popup is open) cycle: `All → Files → Skills → Agents → All`
- On tab change, re-filter and reset `selected = 0`
- If a category is empty and tab changes to it, show the dimmed placeholder

### Selection Movement (Up/Down)

- Up/Down arrows move `selected` within the current `filtered` list
- Wraps at boundaries (down on last → wraps to 0; up on 0 → wraps to last)
- **Amended**: no separate `scroll_offset` bookkeeping — `filtered` is always ≤
  `MAX_RESULTS` (10), so every row fits the popup and `ListState::select` alone
  drives ratatui's highlight

### Accepting a Selection (Tab or Enter)

1. **(Amended, M4 — 2026-07-27 architecture review)** The replacement range is the whole
   **mention token** — `[at_char_index .. token_end]`, where `token_end` is the first
   whitespace char at or after the cursor (or end of buffer) — not just `[at_char_index ..
   cursor_position]`. The token is replaced with the selected entry **including the `@`
   sigil** (for agents) or as plain text (for files/skills). This matters when the cursor
   sits *inside* the mention word (reachable via Alt+Left, Ctrl+A, or a mouse click) —
   e.g. `"@foo"` with the cursor after `@f` still replaces the whole `"@foo"`, not just
   `"@f"`, which would otherwise mangle the buffer to `"src/main.rs oo"`. The *query* used
   for filtering is unaffected by this and remains `[at_char_index+1 .. cursor_position]`
   exactly as defined above — only the accept-time replacement range is token-bounded.
2. A trailing space is inserted (for chaining) unless the character immediately after
   `token_end` is already whitespace (avoids a double space, e.g. `"@foo bar"` accepting
   to `"src/main.rs bar"`, not `"src/main.rs  bar"`)
3. The popup closes: `mention_picker = None`
4. For Tab: the cursor is positioned after the space; Insert mode continues
5. For Enter: same insertion + behavior as Tab (unlike slash-autocomplete, Enter does NOT auto-submit when accepting a mention)

#### Accept Format by Entry Type

| Type | Format | Example |
|------|--------|---------|
| **File** | Bare repo-relative path, no quotes | `src/main.rs` |
| **Skill** | Skill name only; semantic matcher picks it up in normal processing | `web_search` (user can then type arguments naturally) |
| **Agent** | Name with `@` sigil preserved | `@my_coder` |

### Dismissal (Esc)

- Popup closes: `mention_picker = None`
- Input buffer is **unmodified** (the `@` and any typed query remain)
- **Critical**: Insert mode is NOT changed (do NOT fall through to Normal)

### Space Behavior

Pressing Space while the popup is open:

1. Popup closes
2. The Space is inserted **at the cursor position** as an ordinary typed character — exactly what would happen with the popup closed
3. The `@query` text remains in the input, unmodified, as plain prose

Example:
- Input before: `"word "`
- User types `@file`, then Space
- Input after: `"word @file "` (cursor after the trailing space)
- Popup closed; further typing continues as plain prose

### Backspace

- If Backspace deletes the `@` character itself, the popup closes immediately
- If Backspace deletes characters from `query`, the popup remains open and re-filters

### Cursor Movement

**Amended (D2 — 2026-07-27 architecture review): supersedes the original text below.**
Plain `Left`/`Right` are claimed by Tab Cycling (FR-004) while the popup is open — they
never move the cursor and never close the popup by themselves. Span-exit closure instead
applies to **`Home`/`End`, `Alt+Left`/`Alt+Right` (word-boundary movement), `Ctrl+A`/
`Ctrl+E`, and mouse clicks** — any cursor mutation that lands outside `[at_char_index+1 ..
cursor_position]`'s valid span (i.e. at or before `at_char_index`, or past a whitespace
character) closes the popup. Note the boundary case: a cursor landing exactly one position
after `@` (e.g. after a single `Alt+Left` from the end of `"@foo"`) is *still inside* the
span (empty query, not yet past `@`) and does **not** close the popup by itself — a second
`Alt+Left` (or equivalent) that moves further left, past the `@`, does close it. This is
the same state Accepting a Selection's M4 amendment discusses for the *accept* semantics
at that same boundary — see above.

- Superseded example (was: "`Left` twice closes the popup") — no longer applicable, since
  plain `Left`/`Right` cycle tabs instead. Use `Alt+Left` for the word-boundary-exit case:
  `"@foo"`, cursor at end, `Alt+Left` once → cursor after `@` (still inside span, popup
  stays open); `Alt+Left` again → cursor moves into whatever precedes `@`, popup closes.

### File Index Building (Race-Free)

The file index build runs as a supervised task (see spec-039 / `TaskSupervisor`). 

- **Before index is ready**: The popup opens immediately with an "indexing files…" placeholder row in the Files tab
- **No input loss**: Keystrokes are never lost; they append to `query` and continue filtering (even if only one placeholder row is shown until the index arrives)
- **Seamless transition (amended)**: `FileIndex::search` does not exist in the shipped code — `PickerMatch`/`FilePickerState` (and their `search`/`update_query` methods) were deleted as dead code, superseded by `MentionPickerState::refilter`. Once the background build resolves, `App::poll_pending_file_index` installs `FileIndex::paths_arc()` into `MentionCatalog.files` and calls `refilter` immediately — no need to wait for the next keystroke

---

## 8. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| User types `@@` (two `@` symbols) | First `@` opens picker; second `@` is appended to `query` and searched (query = "@") |
| Cursor in middle of `@query` (e.g., `@file`, cursor after `@fi`) | **Amended (M4/D2)**: Up/Down move selection (do not affect the cursor); Left/Right cycle tabs (do not affect the cursor); the popup stays **open** in this position — it does not close merely because the cursor is inside the token. Accepting here (Tab/Enter) replaces the *whole* mention token (`@file`, not just `@fi`), per the M4 amendment to "Accepting a Selection" above. Typed chars are inserted mid-query and the popup re-filters normally |
| Terminal is very narrow (<30 cols) | Popup clips gracefully (ratatui `Rect` clamping); no panic |
| File index build takes >30s (timeout) | **Amended**: there is no explicit build timeout and no "index unavailable" placeholder in the shipped code. While `MentionCatalog.files` is `None` (build not yet complete) the Files tab shows "indexing files…"; once loaded, an empty result set shows "no files found" |
| Skills category empty, All tab open | All tab shows only files + agents; Skills section is omitted |
| User accepts an unknown agent name (e.g., `@nonexistent`) | Mention is inserted as `@nonexistent`; it flows to the LLM or slash-command dispatch (see spec-044 dispatch behavior) — never an error in the picker |
| Match highlighting on multi-byte UTF-8 | Use nucleo indices directly; no byte-boundary truncation issues |

---

## 9. Architecture and Integration

### New Files

```
crates/zeph-tui/src/widgets/mention_picker.rs
  — MentionPickerState, MentionCatalog, MentionEntry structs; MentionTab, MentionKind enums
  — **Amended**: `MentionPickerState::new()` / `refilter()` / `move_selection(delta: i32)`
    methods; accept and close are reducer-side (`Action::MentionPickerAccept`/
    `CloseMentionPicker` in `app/reducer.rs`), not widget methods — matches the
    reducer-purity invariant (§11 inv. 8)
  — render(app, state, frame, input_area, theme) function with tabs, highlight, result counter
  — nucleo integration for fuzzy matching (two-phase: score-only, then materialize top MAX_RESULTS)
```

### Modified Files

```
crates/zeph-tui/src/app/
  — add field: mention_picker: Option<MentionPickerState>
  — **Amended (approved naming deviation, R1)**: no `OpenMentionPicker`/`MentionPickerInput`
    variants — opening is a side effect of the existing `Action::InsertChar('@')` arm
    (mirroring how `/` opens slash-autocomplete), and all text edits continue to flow
    through the existing `InsertChar`/`Delete*`/`MoveCursor` arms rather than a
    parallel input action. Adding those two variants would require duplicating buffer
    mutation and reintroduce the parallel-string bug class this design deliberately
    avoids (see the amended §6 Mention Picker State note). Action variants actually
    added: `CloseMentionPicker`, `MentionPickerMove(VertDir)`,
    `MentionPickerTabChange(HorizDir)`, `MentionPickerAccept`
  — modify the Insert-mode Char('@') branch: insert the char, check word-start, open picker
  — the modal file picker path is REPLACED entirely: `file_picker_state`, `decode_file_picker_key`,
    and the FilePicker* Action variants are removed together with their modal key takeover

crates/zeph-tui/src/app/reducer.rs (or action dispatcher)
  — all mention_picker actions routed through reduce(), no direct state mutation

crates/zeph-tui/src/app/draw.rs (render section)
  — render mention_picker popup when active, positioned above/below input

crates/zeph-tui/src/widgets/mod.rs
  — pub mod mention_picker;
```

### Reused Without Modification

- `crates/zeph-tui/src/file_picker.rs` — FileIndex infrastructure (`build()`/`paths_arc()`,
  TTL); `FileIndex::search`/`PickerMatch` did not survive — see the amended note in §7
  "File Index Building"
- `crates/zeph-tui/src/command.rs` — styling, layout utilities
- `zeph-skills` SkillRegistry and `zeph-subagent` SubAgentDef (read-only)
- ratatui List, Clear, Paragraph widgets

### No Changes to

- `zeph-core` agent loop — accepted mentions flow through as plain text
- `zeph-channels` — TUI-internal feature
- Slash-command dispatch (spec-044) — agent mentions pass through and are routed by existing logic

---

## 10. Acceptance Criteria

| ID | Criterion | Verification |
|----|-----------|--------------|
| AC-001 | Typing `@` at start of empty input opens the picker | Unit test: verify `mention_picker.is_some()` after Char('@') on empty input |
| AC-002 | Typing `@` mid-word does NOT open picker | Unit test: input = "user", type '@', assert `mention_picker.is_none()` and input = "user@" |
| AC-003 | **Amended (M7)**: `Left`/`Right` cycle through All → Files → Skills → Agents → All (not `Tab`, which is bound to Accept per FR-006/US-003) | Unit test: verify active_tab sequence after `Left`/`Right` key events |
| AC-004 | Filtering works with nucleo matching | Unit test: query "fil" matches "src/file.rs", "*.filters", etc. |
| AC-005 | Up/Down wraps at boundaries | Unit test |
| AC-006 | Tab on a file entry inserts path + space | Unit test: entry = File("src/main.rs"), accept → input has "src/main.rs " |
| AC-007 | Tab on a skill entry inserts name without prefix | Unit test: entry = Skill("web_search"), accept → input has "web_search " |
| AC-008 | Tab on an agent entry inserts @name | Unit test: entry = Agent("my_agent"), accept → input has "@my_agent " |
| AC-009 | Esc closes picker, retains input | Unit test: picker active, Esc, assert picker.is_none() and input unchanged |
| AC-010 | Space closes picker, mention text stays as plain prose | Unit test: input "@file" (cursor at end), Space, assert input = "@file " and picker.is_none() |
| AC-011 | Backspace past `@` closes picker | Unit test |
| AC-012 | File index building does not cause input loss | Live test: start typing "@fi", before index is ready, no race condition |
| AC-013 | Unknown agent names do not error in picker (pass to LLM) | Live test: accept `@unknown_agent`, verify it flows to LLM or dispatch (not a picker error) |
| AC-014 | Empty Agents/Skills categories show dimmed placeholder in respective tabs | Live test: verify "no agents loaded" / "no skills loaded" rows appear |
| AC-015 | All tab omits empty categories | Live test: All tab shows only files (if skills/agents empty) |
| AC-016 | Slash-autocomplete (spec-030) unaffected by this change | Existing spec-030 tests continue to pass |
| AC-017 | Popup interaction obeys tui-reducer patterns | Code review: all state changes via `reduce()` and `Action` enums |
| AC-018 | Match highlighting visible in both themes | Visual test: dark + light theme rendering |
| AC-019 | Result counter displayed in popup title | Visual test: verify "N/M" count updates as query changes |
| AC-020 | `cargo nextest run -p zeph-tui` passes | CI gate |

---

## 11. Key Invariants

1. **Literal `@` always typeable** — The mid-word `@` never opens a popup; mid-word `@` always inserts as a literal character. This guarantees email addresses, URLs, and other `@`-bearing prose are never hijacked.

2. **Typing never captured** — Every keystroke goes into the input buffer. The popup merely reflects and filters the buffer content after the `@` marker. No keystroke ever disappears.

3. **No input loss during index build** — When the file index is building at startup, the popup opens immediately with a placeholder "indexing…" row. No keystrokes are lost if the popup opens late; they have already been typed into the buffer.

4. **Esc closes only the popup, Insert mode retained** — Pressing Esc dismisses the popup but does NOT trigger a mode transition (Insert → Normal). The user remains in Insert mode and can continue editing the input. This is orthogonal to issue #6646 (Esc-for-agent-cancel), which is handled separately.

5. **Agent mentions preserve the sigil** — When an agent mention is accepted, the `@` sigil is preserved in the inserted text (e.g., `@my_agent`). This allows downstream slash-command dispatch (spec-044) to correctly identify and route agent mentions. Unknown agent names MUST NOT error in the picker; they pass through to the LLM or slash dispatcher for normal routing.

6. **File/skill accepts are plain text** — File paths are inserted without quotes or `file://` prefix. Skill names are inserted as plain identifiers; semantic matching picks them up in normal context analysis. Neither is "forced activated" or prefixed.

7. **Popup never blocks the TUI render loop** — All file index, skill registry, and agent catalog operations that could block are run as supervised background tasks (spec-039, Spinner Rule). The render thread never waits for index builds, skill scans, or catalog loads.

8. **Reducer purity (INV-R1 + INV-R2 from tui-reducer/spec)** — All state mutations happen inside `reduce()`. No I/O (channel sends, file I/O, spawn) occurs in the reducer. Effects that require side-effects are returned and executed post-reduce.

9. **All categories degrade gracefully** — Empty skill or agent registries show a dimmed "no X loaded" placeholder. The All tab automatically omits categories with zero entries. The popup remains open (not auto-dismissed) even if all categories are empty; the user can still type or press Esc.

10. **Unknown agent mentions do not error** — If a user accepts a mention like `@unknown_agent`, the picker does not reject it. The mention is inserted as literal text and flows downstream to the agent loop / slash-command dispatcher (spec-044), which applies the existing unknown-agent routing logic (fallthrough to LLM, not an error).

---

## 12. Cross-Cutting Requirements (from related specs)

### From Spec 030 (Slash-Autocomplete)

The mention picker mirrors the interaction grammar of slash-autocomplete but adds:
- Word-start trigger rule (instead of "empty input only")
- Multiple categories and tab cycling
- Different accept semantics per entry type
- Two different closing behaviors (Esc closes without touching the buffer; Space closes and the mention text stays as plain prose)

### From Spec 011 (TUI Dashboard) — Spinner Rule

File index building, skill registry loading, and agent definition loading must all show a visible spinner (per the Spinner Rule). These are handled by spec-039 (BackgroundSupervisor / TaskSupervisor).

### From Spec 044 (Subagent Lifecycle)

Agent mentions accepted from the picker flow downstream as plain text through the slash-command dispatcher (`dispatch_agent_command` in `zeph-core/src/agent/slash_commands.rs:88→162`). Unknown agent names fall through to the LLM (existing behavior, not changed by this spec).

### From Spec 005 (Skills)

Skill names accepted from the picker are sent as plain text (no `/skill activate` prefix). The semantic matcher (`zeph-skills` matching engine) picks them up in normal context analysis (existing flow).

---

## 13. Open Questions

None — the data plumbing question is resolved in §6 (dedicated catalog event at startup + hot-reload).

---

## 14. Related Specs and References

- `[[030-tui-slash-autocomplete/spec]]` — inline autocomplete pattern to follow
- `[[011-tui/spec]]` — TUI architecture, Spinner Rule
- `[[tui-reducer/spec]]` — Action/Effect/reduce architecture
- `[[044-subagent-lifecycle/spec]]` — slash-command dispatch routing for agent mentions
- `[[005-skills/spec]]` — skill registry and matching
- `[[UX/mention-routing]]` — broader @mention routing research (deferred)
- `[[039-background-task-supervisor/spec]]` — supervised task management (file index, skill loading)
