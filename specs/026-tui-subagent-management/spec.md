---
aliases:
  - TUI Subagent Management
  - Subagent Sidebar
tags:
  - sdd
  - spec
  - tui
  - agents
created: 2026-03-29
status: approved
related:
  - "[[MOC-specs]]"
  - "[[011-tui/spec]]"
  - "[[033-subagent-context-propagation/spec]]"
  - "[[027-runtime-layer/spec]]"
---

# Feature: TUI Subagent Management

> **Author**: rust-architect
> **Date**: 2026-03-29 (updated 2026-03-29)
> **Spec**: 026

## 1. Overview

### Problem Statement

Subagents in Zeph run as async Tokio tasks and are orchestrated through the
`SubAgentManager` in `zeph-core`. The TUI currently shows a minimal list of
subagents in the side panel (`widgets/subagents.rs`) as a non-interactive summary
(name, state, turns, elapsed time). There is no way for a user to:

- Select a specific subagent to inspect its chat/tool output.
- Switch between the main agent conversation and a subagent's transcript.
- See detailed subagent progress without leaving the chat view.

When multiple subagents are running in parallel (e.g., during orchestrated DAG
plans), the user has no visibility into individual agent behavior, making
debugging and monitoring difficult.

### Goal

Add interactive subagent management to the TUI: a navigable subagent sidebar
with selection, a per-agent transcript viewer in the main chat area, and keyboard
shortcuts for fast switching between the main agent and any active subagent.

### Out of Scope

- Sending input/messages to subagents from the TUI (subagents are autonomous).
- Canceling or spawning subagents from the sidebar (existing `/agent cancel`
  and `/agent spawn` commands already handle this).
- Multi-pane split view showing multiple subagent outputs simultaneously.
- Streaming real-time subagent output via `watch` channels (MVP uses poll-based
  snapshot reads from transcript files).
- Changes to the `zeph-subagent` crate or `SubAgentManager` API (read-only).
- CLI or Telegram integration (TUI-only feature).

## 2. User Stories

### US-001: View running subagents

AS A user monitoring an orchestrated plan,
I WANT to see all currently active subagents with their name, status, and
progress,
SO THAT I can understand what work is happening in parallel.

**Acceptance criteria:**
```
GIVEN the TUI is running with side panels visible
AND at least one subagent is active
WHEN the subagent sidebar renders
THEN each subagent row shows: name, state (color-coded), turns used/max,
  elapsed seconds, and a spinner for "working" state
AND the currently selected subagent is visually highlighted
```

### US-002: Select and view subagent output

AS A user debugging a subagent failure,
I WANT to select a subagent from the sidebar and see its chat transcript in the
main chat area,
SO THAT I can review what the subagent did, which tools it called, and where it
went wrong.

**Acceptance criteria:**
```
GIVEN the subagent sidebar is visible with at least one subagent
WHEN I press Enter on a highlighted subagent row
THEN the main chat area replaces the main conversation with the selected
  subagent's transcript
AND the header bar shows the subagent name and status instead of the main
  provider/model
AND the status bar shows "Viewing: <agent-name>" indicator
```

### US-003: Switch back to main agent

AS A user who finished inspecting a subagent,
I WANT to quickly return to the main agent conversation,
SO THAT I can continue my interaction without losing context.

**Acceptance criteria:**
```
GIVEN the chat area is showing a subagent's transcript
WHEN I press Escape
THEN the chat area reverts to the main conversation
AND the header bar shows the main provider/model again
AND the sidebar selection is preserved (same agent still highlighted)
```

### US-004: Cycle through subagents with keyboard

AS A power user monitoring several parallel subagents,
I WANT to cycle through agents with keyboard shortcuts without opening the
sidebar,
SO THAT I can quickly scan each agent's output.

**Acceptance criteria:**
```
GIVEN at least two subagents exist in the sidebar
WHEN I press Tab while viewing the main agent
THEN the view switches to the first subagent's transcript
WHEN I press Tab again
THEN the view switches to the next subagent's transcript
WHEN I press Shift+Tab
THEN the view switches to the previous subagent's transcript
WHEN I press Escape from any subagent view
THEN the view returns to the main agent conversation
```

### US-005: Empty state handling

AS A user with no active subagents,
I WANT the sidebar to show a helpful placeholder,
SO THAT I know where subagent information will appear.

**Acceptance criteria:**
```
GIVEN no subagents have been spawned
WHEN the sidebar renders the subagent panel
THEN it shows "No sub-agents. Use /agent spawn <name> to create one."
AND no selection highlight is visible
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a subagent is spawned THE SYSTEM SHALL add an entry to the subagent sidebar with name, state, turns, and elapsed time | must |
| FR-002 | WHEN a subagent state changes THE SYSTEM SHALL update the sidebar row with the new state and color | must |
| FR-003 | WHEN the user presses Up/Down (or j/k) in the subagent sidebar THE SYSTEM SHALL move the selection highlight | must |
| FR-004 | WHEN the user presses Enter on a selected subagent THE SYSTEM SHALL display that subagent's transcript in the main chat area | must |
| FR-005 | WHEN the user presses Escape while viewing a subagent transcript THE SYSTEM SHALL return to the main agent conversation | must |
| FR-006 | WHEN the user presses Tab THE SYSTEM SHALL cycle to the next agent view (main -> agent1 -> agent2 -> ... -> main) | must |
| FR-007 | WHEN the user presses Shift+Tab THE SYSTEM SHALL cycle to the previous agent view | must |
| FR-008 | WHEN viewing a subagent's transcript THE SYSTEM SHALL show a "Viewing: <name>" indicator in the status bar | must |
| FR-009 | WHEN a subagent completes or fails THE SYSTEM SHALL show a status bar notification with the agent name and final state | should |
| FR-010 | WHEN a "working" subagent is displayed in the sidebar THE SYSTEM SHALL show a spinner character that animates with the throbber tick | must |
| FR-011 | WHEN the terminal is resized while viewing a subagent transcript THE SYSTEM SHALL re-render without panic or corruption | must |
| FR-012 | WHEN the subagent list is empty THE SYSTEM SHALL show a placeholder message in the sidebar panel | must |
| FR-013 | WHEN side panels are hidden (narrow terminal or user toggle) THE SYSTEM SHALL disable Tab/Shift+Tab cycling but allow `/agent select <id>` command | should |
| FR-014 | WHEN the user types `/agent select <id>` THE SYSTEM SHALL switch the chat view to the specified subagent's transcript | should |
| FR-015 | WHEN the user types `/agent output <id>` THE SYSTEM SHALL dump the subagent's latest output as a system message in the main chat | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Keyboard input latency for Tab/Enter/Escape MUST be < 50ms (no blocking I/O on render thread) |
| NFR-002 | Performance | Transcript loading from JSONL file MUST NOT block the render thread; use background tokio task if file > 100 entries |
| NFR-003 | Performance | Sidebar re-render on metrics poll MUST NOT cause visible flicker (reuse existing throbber tick cycle) |
| NFR-004 | Resilience | If a transcript file is missing or corrupted, the chat area MUST show an error message instead of panicking |
| NFR-005 | Resilience | If metrics report > 50 subagents, the sidebar MUST remain scrollable and responsive (no O(n^2) rendering) |
| NFR-006 | UX | Selected subagent highlight MUST be visible in both dark and light themes |
| NFR-007 | UX | The header bar MUST clearly distinguish between main agent view and subagent view (different color or prefix) |
| NFR-008 | Accessibility | All keyboard shortcuts MUST be documented in the help overlay (`?` key) |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| SubAgentEntry | TUI-local representation of a subagent for sidebar display | `id: String`, `name: String`, `state: SubAgentState`, `turns_used: u32`, `max_turns: u32`, `elapsed_secs: u64`, `background: bool`, `permission_mode: String` |
| TranscriptLine | A single message in a subagent's chat history | `role: MessageRole`, `content: String`, `tool_name: Option<String>`, `timestamp: String` |
| AgentViewTarget | Discriminates what the main chat area is displaying | Enum: `Main` or `SubAgent { id: String, name: String }` |
| SubAgentSidebar | Selection and scroll state for the sidebar list | `list_state: ListState`, `entries: Vec<SubAgentEntry>` |

The `SubAgentEntry` is derived from `SubAgentMetrics` (already published via
`MetricsSnapshot.sub_agents`). No new data sources are required.

Transcript data is read from the existing JSONL transcript files written by
`TranscriptWriter` in `zeph-subagent`. The `TranscriptReader` already provides
`read_all()` to load all entries from a file.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Subagent crashes mid-transcript | Sidebar shows "failed" state; transcript viewer shows partial output with an error footer |
| Rapid subagent spawning (>10 in <1s) | Sidebar scrolls; selection stays on current item; new entries appear at bottom |
| Terminal resize while viewing subagent | Re-layout via `AppLayout::compute`; transcript re-wraps; no panic (per 011-tui invariant) |
| Transcript file missing on disk | Chat area shows "Transcript not available for <name>" system message |
| Transcript file is very large (>1000 entries) | Load only the last N entries (configurable, default 200); show "[truncated]" marker at top |
| User selects a subagent that completes while viewing | View stays on the completed transcript; state updates in sidebar |
| All subagents complete while one is selected | View stays on transcript; sidebar shows all as completed; Escape returns to main |
| `/agent select` with invalid ID | System message: "No sub-agent with id '<id>' found" |
| Side panels hidden + Tab pressed | Tab is ignored (or inserts tab character in insert mode) |
| Subagent has same name as another (different IDs) | Sidebar shows both; disambiguation via unique ID when needed |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Keyboard switch latency (Tab, Enter, Escape) | < 50ms measured on 120-col terminal |
| SC-002 | Sidebar renders correctly with 0, 1, 5, 20 subagents | All cases render without overflow or panic |
| SC-003 | Transcript loads for completed subagent | Displays within 200ms for transcript < 500 entries |
| SC-004 | Help overlay lists new keybindings | Tab, Shift+Tab, Enter (sidebar) documented |
| SC-005 | All acceptance criteria from US-001 through US-005 pass | Manual TUI testing with live subagents |

## 8. Agent Boundaries

### Always (without asking)
- Run tests after changes
- Follow existing TUI widget patterns (`render()` functions, `Theme`, `MetricsSnapshot`)
- Add new keybindings to the help overlay
- Update `CHANGELOG.md`

### Ask First
- Adding new fields to `MetricsSnapshot` or `SubAgentMetrics`
- Adding new variants to `AgentEvent` or `AppEvent`
- Changing the `AppLayout` side panel split ratios
- Adding new dependencies to `zeph-tui`

### Never
- Modify `zeph-subagent` crate internals
- Add blocking I/O to the render thread
- Remove or change existing keybindings
- Break the `TuiChannel` stdin/stdout ownership invariant
- Skip the spinner rule for subagent state changes

## 9. Implemented Details

The following details reflect the shipped implementation and may differ from the spec in minor ways:

### SubAgents Panel

- Activated with `a` key (not a separate panel key — accessed via panel cycling)
- Navigation: `j` / `k` (vim-style up/down) within the SubAgents panel
- `Enter` loads the selected subagent's JSONL transcript into the main chat area
- `Esc` returns to the main conversation
- Transcript is truncated to the **last 200 entries** (not configurable at runtime)
- `[truncated]` marker shown at top when truncation occurs

### Tab Cycling

Tab cycling now includes `SubAgents` as a panel in the cycle order. `Shift+Tab` is not implemented; use `Esc` to return to main view.

### Transcript Format

Transcripts are JSONL files written by `TranscriptWriter`. Each line is a
`TranscriptEntry` with `role`, `content`, `tool_name`, and `timestamp`.

## 10. References

- `011-tui/spec.md` — TUI invariants (spinner rule, no blocking I/O, resize handling)
- `009-orchestration/spec.md` — DAG scheduler, task states, plan view
- `crates/zeph-tui/src/widgets/subagents.rs` — existing sidebar widget (render-only)
- `crates/zeph-subagent/src/manager.rs` — `SubAgentManager`, `SubAgentStatus`, `SubAgentHandle`
- `crates/zeph-subagent/src/transcript.rs` — `TranscriptReader`, `TranscriptEntry`
- `crates/zeph-core/src/metrics.rs` — `SubAgentMetrics`, `MetricsSnapshot`
