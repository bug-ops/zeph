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
- Cursor-movement shortcuts inside the popup (use Up/Down only)
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
AND the All tab shows mixed results ranked by fuzzy match score
```

### US-003: Complete a mention and continue typing

AS A TUI user  
I WANT to select a file/skill/agent from the popup with Tab or Enter  
SO THAT it is inserted into the input and I can continue typing

**Acceptance criteria:**

```
GIVEN the mention picker is visible with a selection
WHEN the user presses Tab or Enter
THEN the `@query` text is replaced with the chosen entry
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
| FR-006 | WHEN Tab or Enter is pressed on a selected entry THE SYSTEM SHALL replace the typed query with the selected entry plus one trailing space; the popup closes | must |
| FR-007 | WHEN Space is pressed while the popup is visible THE SYSTEM SHALL close the popup; the Space is inserted at the cursor position as an ordinary character and the `@query` text stays in the buffer as plain prose | must |
| FR-008 | WHEN Esc is pressed THE SYSTEM SHALL close only the popup, retain Insert mode, keep the input buffer intact | must |
| FR-009 | WHEN Backspace is pressed and the `@` is deleted THE SYSTEM SHALL close the popup automatically | must |
| FR-010 | WHEN cursor movement (arrow keys outside Up/Down for selection) leaves the `@query` span THE SYSTEM SHALL close the popup | must |
| FR-011 | WHEN the file index is (re)building (first build or stale-TTL rebuild) THE SYSTEM SHALL show an "indexing files…" placeholder row in the Files tab with no input loss race | must |
| FR-012 | WHEN the mention picker renders THE SYSTEM SHALL use nucleo fuzzy matching for all three categories | must |
| FR-013 | THE SYSTEM SHALL render match-character highlighting (nucleo indices) on all results | must |
| FR-014 | THE SYSTEM SHALL display an `N/M` result counter in the popup border title (e.g., "Files (3/50)") | must |
| FR-015 | WHEN a File entry is accepted THE SYSTEM SHALL insert the bare repo-relative path WITHOUT a file:// prefix or quotes | must |
| FR-016 | WHEN a Skill entry is accepted THE SYSTEM SHALL insert the skill name as plain text (no `/skill` prefix, no forced activation) | must |
| FR-017 | WHEN an Agent entry is accepted THE SYSTEM SHALL insert the agent name with the `@` sigil (e.g., `@my_agent`) | must |
| FR-018 | WHEN the All tab is active THE SYSTEM SHALL rank results by fuzzy match score across all categories, with per-row type indicators | must |
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

Cursor movement that leaves this span (e.g., moving left past the `@`, or moving to another word) closes the popup.

---

## 6. Data Model

### Mention Picker State

```rust
struct MentionPickerState {
    query: String,                              // text after `@` and before cursor
    selected: usize,                            // current selection index in filtered list
    filtered: Vec<MentionEntry>,               // filtered results for active tab
    active_tab: MentionTab,                    // All | Files | Skills | Agents
    scroll_offset: usize,                      // for scrolling when >MAX_VISIBLE results
    all_entries: MentionEntries,               // cached: Files + Skills + Agents
}

enum MentionTab {
    All,
    Files,
    Skills,
    Agents,
}

struct MentionEntry {
    entry_type: MentionEntryType,
    display: String,                           // e.g., "src/main.rs", "web_search", "my_agent"
    description: Option<String>,               // e.g., skill description, agent description (dimmed in popup)
    match_indices: Vec<usize>,                // nucleo match positions for highlighting
}

enum MentionEntryType {
    File,
    Skill,
    Agent,
}

struct MentionEntries {
    files: Vec<MentionEntry>,
    skills: Vec<MentionEntry>,
    agents: Vec<MentionEntry>,
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
| **Files** | File index (existing `FileIndex` in `zeph-tui`) | `FileIndex::build()` (TTL 30s, supervised task), `FileIndex::search(query)` |
| **Skills** | Skill registry | `SkillRegistry::all_meta()` from `crates/zeph-skills/src/registry.rs:330` → yields `SkillMeta { name, description, … }` |
| **Agents** | Sub-agent definitions | `SubAgentManager::definitions()` or `SubAgentDef::load_all()` from `crates/zeph-subagent/src/def.rs:635` → yields `SubAgentDef { name, description, … }` |

> **Data plumbing decision (fixed): catalog delivery via a dedicated event.** The TUI currently receives only runtime-active names via `MetricsSnapshot`. Full skill/agent catalogs (name + description) are delivered over the existing agent-event channel as a dedicated catalog event emitted once at startup and re-emitted on registry hot-reload — NOT embedded into the per-tick `MetricsSnapshot` (avoids bloating every metrics frame with static catalog data).
>
> **Rationale**: `zeph-tui` has no dependency on `zeph-skills` (verified in `crates/zeph-tui/Cargo.toml`); event-based delivery keeps it that way and reuses the channel the TUI already consumes. Rejected alternative: direct supervised load in `zeph-tui` mirroring `FileIndex::build()` — would require adding a `zeph-skills` dependency and duplicate catalog-loading logic that `zeph-core` already performs at bootstrap.

---

## 7. UX Behavior

### Opening the Popup

When the word-start trigger is satisfied:

1. An `Action::OpenMentionPicker` is routed through the reducer
2. `MentionPickerState::new()` is created with:
   - `query = ""`
   - `filtered = all_entries` (All tab, no filter)
   - `selected = 0`
   - `active_tab = MentionTab::All`
3. The first keystroke after `@` appends to `query` and re-filters

### Typing & Filtering

On every character typed (while the popup is open):

1. The character is appended to `query` and to the input buffer simultaneously
2. The `filtered` list is re-computed using `nucleo_matcher::Matcher` across the active tab
3. `selected` resets to 0 (or stays at 0 if already there)
4. `scroll_offset` is adjusted if needed to keep `selected` in view
5. The popup re-renders with highlighted match indices and result count

### Tab Cycling (Left/Right)

- Left/Right arrows (while popup is open) cycle: `All → Files → Skills → Agents → All`
- On tab change, re-filter and reset `selected = 0`
- If a category is empty and tab changes to it, show the dimmed placeholder

### Selection Movement (Up/Down)

- Up/Down arrows move `selected` within the current `filtered` list
- Wraps at boundaries (down on last → wraps to 0; up on 0 → wraps to last)
- `scroll_offset` adjusts to keep selection visible

### Accepting a Selection (Tab or Enter)

1. The `@query` text is replaced with the selected entry **including the `@` sigil** (for agents) or as plain text (for files/skills)
2. A trailing space is inserted (for chaining)
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

If the user presses arrow keys (Left/Right/Home/End) and the cursor moves **outside the `@query` span**, the popup closes.

- Example: `"@file"`, cursor at end, user presses Left twice → cursor now before `fil`, popup closes

### File Index Building (Race-Free)

The file index build runs as a supervised task (see spec-039 / `TaskSupervisor`). 

- **Before index is ready**: The popup opens immediately with an "indexing files…" placeholder row in the Files tab
- **No input loss**: Keystrokes are never lost; they append to `query` and continue filtering (even if only one placeholder row is shown until the index arrives)
- **Seamless transition**: Once `FileIndex::search()` returns real results, the filtered list updates on the next keystroke

---

## 8. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| User types `@@` (two `@` symbols) | First `@` opens picker; second `@` is appended to `query` and searched (query = "@") |
| Cursor in middle of `@query` (e.g., `@file`, cursor after `@fi`) | Up/Down/Left/Right at this position closes popup (cursor leaving span); typed chars are inserted mid-query |
| Terminal is very narrow (<30 cols) | Popup clips gracefully (ratatui `Rect` clamping); no panic |
| File index build takes >30s (timeout) | FileIndex::search fails; Files tab shows placeholder "index unavailable" or empty |
| Skills category empty, All tab open | All tab shows only files + agents; Skills section is omitted |
| User accepts an unknown agent name (e.g., `@nonexistent`) | Mention is inserted as `@nonexistent`; it flows to the LLM or slash-command dispatch (see spec-044 dispatch behavior) — never an error in the picker |
| Match highlighting on multi-byte UTF-8 | Use nucleo indices directly; no byte-boundary truncation issues |

---

## 9. Architecture and Integration

### New Files

```
crates/zeph-tui/src/widgets/mention_picker.rs
  — MentionPickerState struct
  — MentionEntry, MentionTab enums
  — open() / refilter() / move_up() / move_down() / accept() methods
  — render(state, frame, area) function with tabs, highlight, result counter
  — nucleo integration for fuzzy matching
```

### Modified Files

```
crates/zeph-tui/src/app/
  — add field: mention_picker: Option<MentionPickerState>
  — add Action variants: OpenMentionPicker, MentionPickerMove(VertDir),
    MentionPickerInput(PaletteEdit), MentionPickerTabChange(Direction),
    MentionPickerAccept, CloseMentionPicker
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

- `crates/zeph-tui/src/file_picker.rs` — FileIndex infrastructure (search, TTL)
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
| AC-003 | Tab cycles through All → Files → Skills → Agents → All | Unit test: verify active_tab sequence |
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
