# Technical Plan: TUI Subagent Management

> **Spec**: `026-tui-subagent-management/spec.md`
> **Date**: 2026-03-29
> **Status**: Draft

## 1. Architecture

### Approach

Add interactive subagent selection to the existing TUI by extending the current
`widgets/subagents.rs` list into a stateful, navigable component. Introduce an
`AgentViewTarget` enum in `App` to track what the main chat area displays (main
conversation vs. a subagent transcript). Transcript data is loaded on-demand from
existing JSONL files via `TranscriptReader` and cached in a per-agent
`VecDeque<ChatMessage>`.

This approach was chosen over alternatives because:
- **No new crates or data channels needed** -- `SubAgentMetrics` and
  `TranscriptReader` already exist.
- **Minimal change surface** -- only `zeph-tui` is modified; `zeph-core` and
  `zeph-subagent` remain untouched.
- **Consistent with existing patterns** -- the plan view toggle (`p` key) already
  demonstrates conditional rendering in the same sidebar slot.

### Component Diagram

```
App
├── messages: Vec<ChatMessage>              -- main agent conversation (unchanged)
├── view_target: AgentViewTarget            -- Main | SubAgent { id, name }
├── subagent_sidebar: SubAgentSidebarState  -- list_state + cached entries
├── subagent_transcripts: HashMap<String, TranscriptCache>
│                                            -- lazy-loaded per agent
│
├── draw()
│   ├── draw_header()                       -- shows agent name when viewing subagent
│   ├── if view_target == Main:
│   │   └── widgets::chat::render(self.messages)
│   ├── else:
│   │   └── widgets::chat::render(transcript_messages)
│   ├── draw_side_panel()
│   │   └── widgets::subagents::render_interactive()  -- NEW
│   └── widgets::status::render()           -- shows "Viewing: <name>" badge
│
├── handle_key()
│   ├── Tab         → cycle_agent_view(+1)
│   ├── Shift+Tab   → cycle_agent_view(-1)
│   ├── Escape (in subagent view) → view_target = Main
│   └── (sidebar nav delegated to subagent_sidebar)
│
└── poll_metrics()
    └── sync subagent_sidebar.entries from metrics.sub_agents
```

### Key Design Decisions

| Decision | Choice | Rationale | Alternatives Considered |
|----------|--------|-----------|------------------------|
| Transcript source | Read JSONL via `TranscriptReader` | Already exists; no new channels needed; decoupled from agent loop | Watch channel from agent loop (too invasive); store in MetricsSnapshot (too large) |
| Transcript caching | `HashMap<String, TranscriptCache>` in App | Avoids re-reading disk on every render; invalidated on agent state change | No cache (too slow for large transcripts); global LRU (over-engineered for MVP) |
| Chat rendering reuse | Convert `TranscriptEntry` to `ChatMessage` and reuse `widgets::chat::render` | Maximum code reuse; consistent rendering; markdown/diff/tool output works | Dedicated transcript renderer (duplicates chat logic) |
| View target tracking | `AgentViewTarget` enum on `App` | Simple discriminant; no state machine needed for two states | Separate boolean + agent_id (less type-safe) |
| Sidebar interactivity | Extend existing `render()` to accept `ListState` | Consistent with `AgentManagerState` pattern already in subagents.rs | New widget struct (unnecessary abstraction for MVP) |
| Tab cycling scope | Main -> agent1 -> agent2 -> ... -> Main (wrap) | Intuitive circular navigation; Escape always returns to Main | Tree-based nav (over-complex); number keys (limited to 9 agents) |
| Transcript entry limit | Load last 200 entries by default | Prevents memory bloat for long-running agents; 200 is plenty for inspection | Load all (risk OOM); paginated loading (complex for MVP) |
| Sidebar focus mode | Sidebar nav (j/k/Enter) only active when `active_panel == Panel::SubAgents` | Prevents key conflicts with chat input; consistent with existing panel focus model | Always-active sidebar nav (conflicts with insert mode) |

## 2. Project Structure

```
crates/zeph-tui/src/
├── app.rs               -- MODIFY: add view_target, subagent_sidebar, transcript cache
├── layout.rs            -- MODIFY: (minor) no structural changes, sidebar slot reused
├── event.rs             -- NO CHANGE
├── command.rs           -- MODIFY: add AgentSelect command variant
├── widgets/
│   ├── subagents.rs     -- MODIFY: add render_interactive(), SubAgentSidebarState
│   ├── chat.rs          -- MODIFY: accept alternate message slice for transcript view
│   ├── status.rs        -- MODIFY: show "Viewing: <name>" badge
│   ├── help.rs          -- MODIFY: add Tab/Shift+Tab/Enter docs
│   └── (others)         -- NO CHANGE
├── lib.rs               -- NO CHANGE
└── metrics.rs           -- NO CHANGE (re-exports from zeph-core)
```

## 3. Data Model

### AgentViewTarget

```rust
/// Tracks what the main chat area is currently displaying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentViewTarget {
    /// Main agent conversation (default).
    Main,
    /// Viewing a specific subagent's transcript.
    SubAgent {
        id: String,
        name: String,
    },
}

impl Default for AgentViewTarget {
    fn default() -> Self {
        Self::Main
    }
}
```

### SubAgentSidebarState

```rust
/// Interactive sidebar state for subagent list navigation.
pub struct SubAgentSidebarState {
    /// Cached entries from last metrics poll.
    pub entries: Vec<SubAgentMetrics>,
    /// ratatui ListState for selection tracking.
    pub list_state: ListState,
}

impl SubAgentSidebarState {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            list_state: ListState::default(),
        }
    }

    /// Sync entries from a metrics snapshot. Preserves selection index if valid.
    pub fn sync_from_metrics(&mut self, sub_agents: &[SubAgentMetrics]) {
        let prev_selected_id = self.selected_id();
        self.entries = sub_agents.to_vec();

        if self.entries.is_empty() {
            self.list_state.select(None);
            return;
        }

        // Try to preserve selection by ID.
        if let Some(prev_id) = prev_selected_id {
            if let Some(pos) = self.entries.iter().position(|e| e.id == prev_id) {
                self.list_state.select(Some(pos));
                return;
            }
        }
        // Fallback: clamp to valid range.
        let idx = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some(idx.min(self.entries.len() - 1)));
    }

    pub fn selected_id(&self) -> Option<String> {
        self.list_state
            .selected()
            .and_then(|i| self.entries.get(i))
            .map(|e| e.id.clone())
    }

    pub fn move_down(&mut self) { /* clamp to len-1 */ }
    pub fn move_up(&mut self) { /* saturating_sub(1) */ }
}
```

### TranscriptCache

```rust
/// Lazy-loaded, bounded transcript cache for a single subagent.
pub struct TranscriptCache {
    /// Converted messages ready for chat rendering.
    pub messages: Vec<ChatMessage>,
    /// SubAgentState at the time of last load.
    pub state_at_load: String,
    /// Whether this cache needs refresh (agent state changed since load).
    pub stale: bool,
}
```

No database migrations. No new config fields. Transcript files already exist at
the path stored in `SubAgentHandle.transcript_dir`.

## 4. API Design

No external API changes. All additions are internal to `zeph-tui`.

### New `/agent` Command Extensions

| Command | Action | Implementation |
|---------|--------|----------------|
| `/agent select <id>` | Switch chat view to subagent transcript | Parse in `handle_insert_key`; set `view_target` |
| `/agent output <id>` | Dump latest subagent output as system message in main chat | Parse in `handle_insert_key`; read transcript; push system message |

These extend the existing `/agent list`, `/agent status`, `/agent cancel`,
`/agent spawn` commands already parsed in the TUI input handler.

### New Keyboard Bindings

| Key | Context | Action |
|-----|---------|--------|
| `Tab` | Normal mode, side panels visible | Cycle to next agent view (Main -> SA1 -> SA2 -> ... -> Main) |
| `Shift+Tab` | Normal mode, side panels visible | Cycle to previous agent view |
| `Escape` | Viewing a subagent transcript | Return to main conversation |
| `j` / `Down` | Panel::SubAgents focused | Move sidebar selection down |
| `k` / `Up` | Panel::SubAgents focused | Move sidebar selection up |
| `Enter` | Panel::SubAgents focused, agent selected | Switch chat to selected agent transcript |
| `a` | Normal mode | Toggle active panel to Panel::SubAgents (new panel variant) |

## 5. Integration Points

| System | Direction | Protocol | Notes |
|--------|-----------|----------|-------|
| `MetricsSnapshot.sub_agents` | inbound (read) | `watch::Receiver` | Already polled every tick via `poll_metrics()` |
| `TranscriptReader` | inbound (read) | Filesystem (JSONL) | On-demand when user selects a subagent |
| `SubAgentHandle.transcript_dir` | reference | Derived from `SubAgentMetrics.id` + config | Need transcript dir path from config or convention |
| Status bar (`widgets::status`) | outbound (render) | Direct function call | Pass `view_target` to status renderer |
| Help overlay (`widgets::help`) | outbound (render) | Static text | Add Tab/Shift+Tab/a/Enter docs |
| Existing plan view toggle (`p` key) | coordination | `plan_view_active` flag | Plan view and subagent sidebar share the same layout slot; `p` toggles between them (existing behavior preserved) |

### Transcript Directory Resolution

The transcript directory path is derived from the subagent config:
`config.subagent.transcript_dir / <agent_id>.jsonl`. The `MetricsSnapshot` does
not currently include transcript paths, so the TUI needs either:

1. **Option A (preferred)**: Add `transcript_dir: Option<String>` to
   `SubAgentMetrics`. This is a single-field addition to an existing struct in
   `zeph-core/src/metrics.rs`.
2. **Option B**: Derive the path from a known convention
   (`data_dir/transcripts/<id>.jsonl`) and pass the data dir via `App` config.

Option A is cleaner and avoids path assumptions. It requires a minor "Ask First"
change to `SubAgentMetrics`.

## 6. Security

No security implications. All data is read-only from local transcript files.
No new network access. No new user input paths beyond existing `/agent` commands
(which are already validated).

## 7. Testing Strategy

| Level | Framework | What to Test | Coverage Target |
|-------|-----------|-------------|-----------------|
| Unit | `#[cfg(test)]` + insta snapshots | `SubAgentSidebarState` sync, selection navigation, `AgentViewTarget` transitions | All state transitions |
| Unit | `#[cfg(test)]` + `TestBackend` | `render_interactive()` output with 0, 1, 5 entries; selected highlight | Visual regression via snapshots |
| Unit | `#[cfg(test)]` | `TranscriptCache` loading, staleness detection, entry limit (200) | Boundary conditions |
| Unit | `#[cfg(test)]` | Tab cycling logic: wrap-around, empty list, single agent | All edge cases |
| Unit | `#[cfg(test)]` | `/agent select` and `/agent output` command parsing | Valid and invalid inputs |
| Integration | Manual TUI session | Full flow: spawn agents, Tab through, Enter to view, Escape back | US-001 through US-005 |
| Property | `proptest` | Sidebar `sync_from_metrics` with arbitrary `SubAgentMetrics` vectors preserves valid selection | No panic, selection in bounds |

### Test Approach for Chat Reuse

The `widgets::chat::render` function currently takes `&App`. To render transcript
messages, the plan is to have `App` expose a `display_messages()` method that
returns either `self.messages` or the transcript cache based on `view_target`.
This keeps the chat widget unchanged.

## 8. Performance Considerations

- **Expected load**: 1-20 subagents typical; sidebar rendering is O(n) which is
  negligible.
- **Transcript loading**: JSONL parse is I/O-bound. For files > 100 entries,
  spawn a `tokio::task::spawn_blocking` to read, then send results via a
  `oneshot` channel (same pattern as `pending_file_index` in `App`).
- **Render cache**: The existing `RenderCache` in `App` must be cleared when
  switching between main and subagent views (different message sets produce
  different hashes).
- **Memory**: Each cached transcript holds up to 200 `ChatMessage` entries.
  With 20 agents, that is ~4000 messages in memory -- acceptable.

## 9. Rollout Plan

Single PR. Feature is TUI-only and non-breaking. No feature flag needed (the TUI
itself is already feature-gated behind `tui`).

## 10. Constitution Compliance

| Principle | Status | Notes |
|-----------|--------|-------|
| Architecture: crate layer DAG | Compliant | Only `zeph-tui` (Layer 4) modified; reads from `zeph-core` (Layer 3) types |
| Architecture: TUI spinner rule | Compliant | Subagent "working" state shows spinner in sidebar (already exists); state change notifications in status bar |
| Architecture: no blocking I/O | Compliant | Transcript loading via `spawn_blocking` + oneshot for large files |
| Technology: unsafe_code deny | Compliant | No unsafe code |
| Testing: pre-merge checks | Compliant | Unit tests with TestBackend + insta snapshots; manual TUI testing |
| Code style: no emoji | Compliant | N/A |
| Code style: MVP minimal | Compliant | No new abstractions; reuses existing chat renderer; no streaming infrastructure |
| Git: CHANGELOG update | Compliant | Will update `[Unreleased]` section |
| Integration points | Compliant | No config changes (unless Option A for transcript_dir is chosen); no CLI changes; help overlay updated |

## 11. Risks and Mitigations

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| Transcript file not found (agent spawned without transcripts enabled) | low | medium | Show "Transcript not available" message; do not panic |
| Large transcript JSONL causes UI freeze | medium | low | Limit to last 200 entries; use spawn_blocking for I/O |
| Tab key conflicts with insert mode tab character | low | low | Tab cycling only active in Normal mode or when panel focus is not Chat |
| Render cache invalidation missed on view switch | low | medium | Clear cache unconditionally in `set_view_target()` method |
| `SubAgentMetrics` does not include transcript_dir | medium | high | Requires one-field addition to `SubAgentMetrics` in zeph-core (Ask First) OR use convention-based path resolution |
| Subagent list order changes between metrics polls, breaking selection | low | medium | `sync_from_metrics` preserves selection by ID, not index |
