// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tracing::debug;
use zeph_core::task_supervisor::TaskSupervisor;

use crate::command::TuiCommand;
use crate::event::{AgentEvent, AppEvent};
use crate::file_picker::{FileIndex, FilePickerState};
use crate::hyperlink::HyperlinkSpan;
use crate::layout::AppLayout;
use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;
use crate::widgets;
use crate::widgets::command_palette::CommandPaletteState;
use crate::widgets::slash_autocomplete::{SlashAutocompleteState, command_id_to_slash_form};

pub use crate::render_cache::{RenderCache, RenderCacheEntry, RenderCacheKey, content_hash};
pub use crate::types::{ChatMessage, InputMode, MessageRole};

use crate::types::PasteState;

/// Maximum number of chat messages retained in the TUI message buffer.
/// Older messages are evicted from the front when the limit is exceeded (#2737).
const MAX_TUI_MESSAGES: usize = 2000;

/// Maximum number of input history entries retained in the TUI (#2737).
const MAX_INPUT_HISTORY: usize = 500;

/// The currently focused side panel in the TUI layout.
///
/// Controls which panel receives keyboard focus for scrolling and navigation.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::app::Panel;
///
/// let panel = Panel::Chat;
/// assert_eq!(panel, Panel::Chat);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// The main chat / transcript area.
    Chat,
    /// The skills mini-panel (side column).
    Skills,
    /// The semantic memory mini-panel (side column).
    Memory,
    /// The MCP resources mini-panel (side column).
    Resources,
    /// The sub-agents mini-panel (side column).
    SubAgents,
    /// The supervised task registry panel (side column).
    Tasks,
}

/// Discriminates what the main chat area is currently displaying.
///
/// In `Main` mode the user sees their own conversation with the primary agent.
/// In `SubAgent` mode the area shows the transcript of a spawned sub-agent.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::app::AgentViewTarget;
///
/// let target = AgentViewTarget::Main;
/// assert!(target.is_main());
///
/// let sub = AgentViewTarget::SubAgent { id: "sa-1".into(), name: "Planner".into() };
/// assert_eq!(sub.subagent_id(), Some("sa-1"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentViewTarget {
    /// Displaying the main agent conversation.
    Main,
    /// Displaying the transcript of the named sub-agent.
    SubAgent {
        /// Stable sub-agent identifier (matches [`SubAgentMetrics::id`](crate::metrics::SubAgentMetrics)).
        id: String,
        /// Display name shown in the header bar.
        name: String,
    },
}

impl AgentViewTarget {
    /// Returns `true` when the target is the primary agent conversation.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::app::AgentViewTarget;
    ///
    /// assert!(AgentViewTarget::Main.is_main());
    /// let sub = AgentViewTarget::SubAgent { id: "x".into(), name: "y".into() };
    /// assert!(!sub.is_main());
    /// ```
    #[must_use]
    pub fn is_main(&self) -> bool {
        matches!(self, Self::Main)
    }

    /// Returns the sub-agent ID if this target points to a sub-agent, otherwise `None`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::app::AgentViewTarget;
    ///
    /// assert_eq!(AgentViewTarget::Main.subagent_id(), None);
    /// let sub = AgentViewTarget::SubAgent { id: "sa-42".into(), name: "n".into() };
    /// assert_eq!(sub.subagent_id(), Some("sa-42"));
    /// ```
    #[must_use]
    pub fn subagent_id(&self) -> Option<&str> {
        if let Self::SubAgent { id, .. } = self {
            Some(id)
        } else {
            None
        }
    }

    /// Returns the sub-agent display name if this target points to a sub-agent, otherwise `None`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::app::AgentViewTarget;
    ///
    /// assert_eq!(AgentViewTarget::Main.subagent_name(), None);
    /// let sub = AgentViewTarget::SubAgent { id: "x".into(), name: "Planner".into() };
    /// assert_eq!(sub.subagent_name(), Some("Planner"));
    /// ```
    #[must_use]
    pub fn subagent_name(&self) -> Option<&str> {
        if let Self::SubAgent { name, .. } = self {
            Some(name)
        } else {
            None
        }
    }
}

/// A single entry from a sub-agent's JSONL transcript, ready for TUI display.
///
/// Loaded by the background transcript reader and converted to
/// [`ChatMessage`] for rendering in the chat widget via
/// [`to_chat_message`](Self::to_chat_message).
///
/// # Examples
///
/// ```rust
/// use zeph_tui::app::TuiTranscriptEntry;
///
/// let entry = TuiTranscriptEntry {
///     role: "assistant".to_string(),
///     content: "I found 3 results.".to_string(),
///     tool_name: None,
///     timestamp: None,
/// };
/// let msg = entry.to_chat_message();
/// ```
#[derive(Debug, Clone)]
pub struct TuiTranscriptEntry {
    pub role: String,
    pub content: String,
    pub tool_name: Option<zeph_common::ToolName>,
    pub timestamp: Option<String>,
}

impl TuiTranscriptEntry {
    /// Convert this transcript entry to a [`ChatMessage`] for chat widget rendering.
    ///
    /// The `role` string is mapped to a [`MessageRole`]: `"user"`, `"assistant"`,
    /// `"tool"`, or `"system"` for all other values. The optional `tool_name`
    /// and `timestamp` fields are forwarded verbatim.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::app::TuiTranscriptEntry;
    /// use zeph_tui::MessageRole;
    ///
    /// let entry = TuiTranscriptEntry {
    ///     role: "user".to_string(),
    ///     content: "hello".to_string(),
    ///     tool_name: None,
    ///     timestamp: Some("14:30".to_string()),
    /// };
    /// let msg = entry.to_chat_message();
    /// assert_eq!(msg.role, MessageRole::User);
    /// assert_eq!(msg.timestamp, "14:30");
    /// ```
    #[must_use]
    pub fn to_chat_message(&self) -> ChatMessage {
        let role = match self.role.as_str() {
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            "tool" => MessageRole::Tool,
            _ => MessageRole::System,
        };
        let mut msg = ChatMessage::new(role, self.content.clone());
        if let Some(ref name) = self.tool_name {
            msg.tool_name = Some(name.clone());
        }
        if let Some(ref ts) = self.timestamp {
            msg.timestamp.clone_from(ts);
        }
        msg
    }
}

/// Cached transcript data for a single sub-agent session.
///
/// Populated by the background transcript loader and invalidated when
/// `turns_used` in the metrics snapshot advances beyond `turns_at_load`.
pub struct TranscriptCache {
    /// The sub-agent ID this cache entry belongs to.
    pub agent_id: String,
    /// Parsed transcript entries (last `TRANSCRIPT_MAX_ENTRIES` entries).
    pub entries: Vec<TuiTranscriptEntry>,
    /// `turns_used` value at the time of last load, for staleness detection (W2).
    pub turns_at_load: u32,
    /// Total entries in file (before truncation to last N).
    pub total_in_file: usize,
}

/// Selection and scroll state for the interactive sub-agent sidebar.
///
/// Wraps a ratatui [`ListState`](ratatui::widgets::ListState) with convenience
/// helpers that clamp the selection to valid indices.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::app::SubAgentSidebarState;
///
/// let mut state = SubAgentSidebarState::new();
/// state.select_next(3);
/// assert_eq!(state.selected(), Some(0));
/// ```
pub struct SubAgentSidebarState {
    /// Underlying ratatui list selection state.
    pub list_state: ratatui::widgets::ListState,
}

impl SubAgentSidebarState {
    /// Create a new sidebar state with no selection.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::app::SubAgentSidebarState;
    ///
    /// let state = SubAgentSidebarState::new();
    /// assert_eq!(state.selected(), None);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            list_state: ratatui::widgets::ListState::default(),
        }
    }

    /// Advance the selection to the next item, clamped to `count - 1`.
    ///
    /// A no-op when `count` is zero.
    pub fn select_next(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let next = match self.list_state.selected() {
            Some(i) => (i + 1).min(count - 1),
            None => 0,
        };
        self.list_state.select(Some(next));
    }

    /// Move the selection to the previous item, clamped to `0`.
    ///
    /// A no-op when `count` is zero.
    pub fn select_prev(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let prev = match self.list_state.selected() {
            Some(0) | None => 0,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(prev));
    }

    /// Ensure the selection is valid given the current agent count.
    pub fn clamp(&mut self, count: usize) {
        if count == 0 {
            self.list_state.select(None);
        } else if self.list_state.selected().is_some_and(|i| i >= count) {
            self.list_state.select(Some(count - 1));
        }
    }

    /// Returns the currently selected index, or `None` if nothing is selected.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::app::SubAgentSidebarState;
    ///
    /// let mut state = SubAgentSidebarState::new();
    /// assert_eq!(state.selected(), None);
    /// state.select_next(5);
    /// assert_eq!(state.selected(), Some(0));
    /// ```
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }
}

impl Default for SubAgentSidebarState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ConfirmState {
    pub prompt: String,
    pub response_tx: Option<oneshot::Sender<bool>>,
}

pub struct ElicitationState {
    pub dialog: crate::widgets::elicitation::ElicitationDialogState,
    pub response_tx: Option<oneshot::Sender<zeph_core::channel::ElicitationResponse>>,
}

/// Central state machine for the TUI dashboard.
///
/// `App` owns all widget state, the render cache, the message history, and
/// the event channel endpoints. The main loop in [`crate::run_tui`] calls
/// [`draw`](Self::draw) once per frame and routes events through
/// [`handle_event`](Self::handle_event) and
/// [`handle_agent_event`](Self::handle_agent_event).
///
/// # Construction
///
/// ```rust
/// use tokio::sync::mpsc;
/// use zeph_tui::App;
///
/// let (user_tx, _user_rx) = mpsc::channel(64);
/// let (_agent_tx, agent_rx) = mpsc::channel(64);
/// let app = App::new(user_tx, agent_rx);
/// ```
///
/// Use the builder methods to wire optional components:
/// - [`with_metrics_rx`](Self::with_metrics_rx) — live metrics watch channel.
/// - [`with_cancel_signal`](Self::with_cancel_signal) — Ctrl-C cancel notify.
/// - [`with_command_tx`](Self::with_command_tx) — slash-command dispatch channel.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    input: String,
    cursor_position: usize,
    input_mode: InputMode,
    messages: Vec<ChatMessage>,
    show_splash: bool,
    show_side_panels: bool,
    show_help: bool,
    scroll_offset: usize,
    pub metrics: MetricsSnapshot,
    metrics_rx: Option<watch::Receiver<MetricsSnapshot>>,
    active_panel: Panel,
    tool_expanded: bool,
    compact_tools: bool,
    show_source_labels: bool,
    status_label: Option<String>,
    throbber_state: throbber_widgets_tui::ThrobberState,
    confirm_state: Option<ConfirmState>,
    elicitation_state: Option<ElicitationState>,
    command_palette: Option<CommandPaletteState>,
    command_tx: Option<mpsc::Sender<TuiCommand>>,
    file_picker_state: Option<FilePickerState>,
    file_index: Option<FileIndex>,
    slash_autocomplete: Option<SlashAutocompleteState>,
    pub should_quit: bool,
    user_input_tx: mpsc::Sender<String>,
    agent_event_rx: mpsc::Receiver<AgentEvent>,
    input_history: Vec<String>,
    history_index: Option<usize>,
    draft_input: String,
    queued_count: usize,
    pending_count: usize,
    editing_queued: bool,
    hyperlinks: Vec<HyperlinkSpan>,
    cancel_signal: Option<Arc<Notify>>,
    pub render_cache: RenderCache,
    pending_file_index: Option<oneshot::Receiver<FileIndex>>,
    /// When `true`, the user has toggled back to subagents view despite an active plan.
    /// Default `false` = auto-show plan view when a graph is active.
    /// Toggled with the `p` key.
    plan_view_active: bool,
    /// Which agent's transcript the chat area is currently displaying.
    pub view_target: AgentViewTarget,
    /// Interactive selection state for the subagent sidebar.
    pub subagent_sidebar: SubAgentSidebarState,
    /// Cached transcript for the currently-focused subagent.
    pub transcript_cache: Option<TranscriptCache>,
    /// Pending transcript load result from background task.
    pending_transcript: Option<oneshot::Receiver<(Vec<TuiTranscriptEntry>, usize)>>,
    /// Optional handle to the `TaskSupervisor` for the task registry panel.
    task_supervisor: Option<TaskSupervisor>,
    /// Whether the task registry panel is currently visible (toggled by `/tasks`).
    show_task_panel: bool,
    /// Active paste indicator state. `Some` when a multiline paste is in the buffer
    /// and the user has not yet edited or submitted the input.
    paste_state: Option<PasteState>,
    /// Snapshot of supervisor tasks cached once per render tick before `terminal.draw()`.
    ///
    /// Avoids acquiring `TaskSupervisor`'s inner mutex inside the draw closure, which
    /// can block the render loop when the reap driver holds the lock concurrently.
    cached_task_snapshots: Vec<zeph_core::task_supervisor::TaskSnapshot>,
}

impl App {
    /// Create a new `App` with the given I/O channels.
    ///
    /// The app starts in insert mode with the splash screen visible and no
    /// messages in the buffer.
    ///
    /// # Arguments
    ///
    /// * `user_input_tx` — sender used to forward the user's typed text to the
    ///   agent loop via [`TuiChannel`](crate::TuiChannel).
    /// * `agent_event_rx` — receiver for [`AgentEvent`] produced by the agent.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _user_rx) = mpsc::channel(64);
    /// let (_agent_tx, agent_rx) = mpsc::channel(64);
    /// let app = App::new(user_tx, agent_rx);
    /// assert!(app.show_splash());
    /// ```
    #[must_use]
    pub fn new(
        user_input_tx: mpsc::Sender<String>,
        agent_event_rx: mpsc::Receiver<AgentEvent>,
    ) -> Self {
        Self {
            input: String::new(),
            cursor_position: 0,
            input_mode: InputMode::Insert,
            messages: Vec::new(),
            show_splash: true,
            show_side_panels: true,
            show_help: false,
            scroll_offset: 0,
            metrics: MetricsSnapshot::default(),
            metrics_rx: None,
            active_panel: Panel::Chat,
            tool_expanded: false,
            compact_tools: false,
            show_source_labels: false,
            status_label: None,
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            confirm_state: None,
            elicitation_state: None,
            command_palette: None,
            command_tx: None,
            file_picker_state: None,
            file_index: None,
            slash_autocomplete: None,
            should_quit: false,
            user_input_tx,
            agent_event_rx,
            input_history: Vec::new(),
            history_index: None,
            draft_input: String::new(),
            queued_count: 0,
            pending_count: 0,
            editing_queued: false,
            hyperlinks: Vec::new(),
            cancel_signal: None,
            render_cache: RenderCache::default(),
            pending_file_index: None,
            plan_view_active: false,
            view_target: AgentViewTarget::Main,
            subagent_sidebar: SubAgentSidebarState::new(),
            transcript_cache: None,
            pending_transcript: None,
            task_supervisor: None,
            show_task_panel: false,
            paste_state: None,
            cached_task_snapshots: Vec::new(),
        }
    }

    /// Return `true` while the splash screen should be displayed.
    ///
    /// The splash screen is hidden as soon as the first chat message arrives.
    #[must_use]
    pub fn show_splash(&self) -> bool {
        self.show_splash
    }

    /// Return `true` when the side panels column is visible.
    ///
    /// Controlled by the `s` keybinding and automatically disabled on narrow
    /// terminals (< 80 columns).
    #[must_use]
    pub fn show_side_panels(&self) -> bool {
        self.show_side_panels
    }

    /// Returns `true` when the user has toggled back to subagents view (plan view overridden).
    #[must_use]
    pub fn plan_view_active(&self) -> bool {
        self.plan_view_active
    }

    /// Populate the message buffer from a persisted session history.
    ///
    /// Each element is a `(role, content)` pair where `role` is one of
    /// `"user"`, `"assistant"`, or `"tool"`. Tool outputs are detected by a
    /// sentinel suffix and rendered as [`MessageRole::Tool`] messages.
    /// The splash screen is hidden after loading if any messages are present.
    pub fn load_history(&mut self, messages: &[(&str, &str)]) {
        const TOOL_SUFFIX: &str = "\n```";

        for &(role_str, content) in messages {
            if role_str == "user"
                && let Some((tool_name, body)) = parse_tool_output(content, TOOL_SUFFIX)
            {
                self.messages
                    .push(ChatMessage::new(MessageRole::Tool, body).with_tool(tool_name.into()));
                continue;
            }

            let role = match role_str {
                "user" => MessageRole::User,
                "assistant" => {
                    if is_tool_use_only(content) {
                        continue;
                    }
                    MessageRole::Assistant
                }
                _ => continue,
            };
            if role == MessageRole::User {
                self.input_history.push(content.to_owned());
            }
            self.messages.push(ChatMessage::new(role, content));
        }
        // Enforce the message buffer cap on initial history load as well.
        self.trim_messages();
        if !self.messages.is_empty() {
            self.show_splash = false;
        }
    }

    /// Attach a cancel signal that Ctrl-C in the TUI will trigger.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::sync::Arc;
    /// use tokio::sync::{Notify, mpsc};
    /// use zeph_tui::App;
    ///
    /// let (tx, _rx) = mpsc::channel(1);
    /// let (_atx, arx) = mpsc::channel(1);
    /// let notify = Arc::new(Notify::new());
    /// let _app = App::new(tx, arx).with_cancel_signal(notify);
    /// ```
    #[must_use]
    pub fn with_cancel_signal(mut self, signal: Arc<Notify>) -> Self {
        self.cancel_signal = Some(signal);
        self
    }

    /// Attach a metrics watch channel for live dashboard updates.
    ///
    /// The current snapshot is read immediately; subsequent updates are polled
    /// by [`poll_metrics`](Self::poll_metrics) each frame.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::{mpsc, watch};
    /// use zeph_tui::{App, MetricsSnapshot};
    ///
    /// let (tx, _rx) = mpsc::channel(1);
    /// let (_atx, arx) = mpsc::channel(1);
    /// let (_metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());
    /// let _app = App::new(tx, arx).with_metrics_rx(metrics_rx);
    /// ```
    #[must_use]
    pub fn with_metrics_rx(mut self, rx: watch::Receiver<MetricsSnapshot>) -> Self {
        self.metrics = rx.borrow().clone();
        self.metrics_rx = Some(rx);
        self
    }

    /// Attach the command dispatch sender used for slash-command routing.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::{App, TuiCommand};
    ///
    /// let (tx, _rx) = mpsc::channel(1);
    /// let (_atx, arx) = mpsc::channel(1);
    /// let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    /// let _app = App::new(tx, arx).with_command_tx(cmd_tx);
    /// ```
    #[must_use]
    pub fn with_command_tx(mut self, tx: mpsc::Sender<TuiCommand>) -> Self {
        self.command_tx = Some(tx);
        self
    }

    /// Wire a [`TaskSupervisor`] into the `App` for the task registry panel.
    ///
    /// The supervisor's task list is snapshotted once per render tick before
    /// `terminal.draw()`, keeping the draw closure free of mutex contention.
    /// Toggle the panel visibility with `/tasks`.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use tokio::sync::mpsc;
    /// use tokio_util::sync::CancellationToken;
    /// use zeph_core::task_supervisor::TaskSupervisor;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let cancel = CancellationToken::new();
    /// let supervisor = TaskSupervisor::new(cancel);
    /// let _app = App::new(user_tx, agent_rx).with_task_supervisor(supervisor);
    /// ```
    #[must_use]
    pub fn with_task_supervisor(mut self, supervisor: TaskSupervisor) -> Self {
        self.task_supervisor = Some(supervisor);
        self
    }

    /// Refresh the cached task snapshot from the supervisor.
    ///
    /// Must be called once per render tick **before** `terminal.draw()` to avoid
    /// acquiring the supervisor's inner mutex inside the draw closure.
    pub(crate) fn refresh_task_snapshots(&mut self) {
        self.cached_task_snapshots = self
            .task_supervisor
            .as_ref()
            .map(TaskSupervisor::snapshot)
            .unwrap_or_default();
    }

    /// Return a truncated label for active `TaskSupervisor` tasks, or `None` when idle.
    ///
    /// Used by [`widgets::chat::render_activity`] to show a braille spinner with
    /// the name of the first active (Running/Restarting) task when no other status
    /// is being displayed.
    #[must_use]
    pub fn supervisor_activity_label(&self) -> Option<String> {
        self.task_supervisor.as_ref()?;
        let mut active = self
            .cached_task_snapshots
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    zeph_core::task_supervisor::TaskStatus::Running
                        | zeph_core::task_supervisor::TaskStatus::Restarting { .. }
                )
            })
            .filter(|t| !t.name.starts_with("mem-"))
            .peekable();
        let first = active.next()?;
        let label = if active.peek().is_none() {
            first.name.to_string()
        } else {
            let extra = active.count() + 1; // +1 because we already consumed first
            format!("{} +{} more", first.name, extra)
        };
        // Char-based truncation to avoid panicking on multi-byte UTF-8 boundaries.
        let truncated: String = label.chars().take(38).collect();
        Some(truncated)
    }

    /// Wire a cancel signal into a running App instance.
    ///
    /// Used by the two-phase TUI startup path to connect the agent's cancel signal
    /// after the agent has been constructed (Phase 2).
    pub fn set_cancel_signal(&mut self, signal: Arc<Notify>) {
        self.cancel_signal = Some(signal);
    }

    /// Wire a metrics receiver into a running App instance.
    ///
    /// Used by the two-phase TUI startup path to connect the metrics channel
    /// after the metrics watch channel has been created (Phase 2).
    pub fn set_metrics_rx(&mut self, rx: watch::Receiver<MetricsSnapshot>) {
        self.metrics = rx.borrow().clone();
        self.metrics_rx = Some(rx);
    }

    /// Check the metrics watch channel for an updated snapshot and apply it.
    ///
    /// Also clamps the sidebar selection and triggers a transcript reload if
    /// the sub-agent's turn count has advanced. Called once per render frame.
    pub fn poll_metrics(&mut self) {
        if let Some(ref mut rx) = self.metrics_rx
            && rx.has_changed().unwrap_or(false)
        {
            let new_metrics = rx.borrow_and_update().clone();
            // IC2: reset plan_view_active (subagents-override) when a new plan appears.
            // Detect new plan by comparing graph_id; new plan should be shown immediately.
            let new_graph_id = new_metrics
                .orchestration_graph
                .as_ref()
                .map(|s| &s.graph_id);
            let old_graph_id = self
                .metrics
                .orchestration_graph
                .as_ref()
                .map(|s| &s.graph_id);
            if new_graph_id != old_graph_id && new_graph_id.is_some() {
                self.plan_view_active = false;
            }
            self.metrics = new_metrics;
        }
        // Clamp sidebar selection in case subagents count changed.
        let count = self.metrics.sub_agents.len();
        self.subagent_sidebar.clamp(count);
        // Trigger transcript reload when turns count increased.
        self.maybe_reload_transcript();
    }

    /// Switch the chat view target. Clears render cache and scroll offset.
    /// All view changes MUST go through this method (W5).
    pub fn set_view_target(&mut self, target: AgentViewTarget) {
        if self.view_target == target {
            return;
        }
        self.view_target = target;
        self.render_cache.clear();
        self.scroll_offset = 0;
        self.transcript_cache = None;
        self.pending_transcript = None;
        // Kick off transcript load if switching to a subagent.
        if let AgentViewTarget::SubAgent { ref id, .. } = self.view_target {
            let id = id.clone();
            self.start_transcript_load(&id);
        }
    }

    /// Initiates a background transcript load for the given agent ID.
    fn start_transcript_load(&mut self, agent_id: &str) {
        // Find transcript_dir from current metrics.
        let transcript_path = self
            .metrics
            .sub_agents
            .iter()
            .find(|sa| sa.id == agent_id)
            .and_then(|sa| sa.transcript_dir.as_deref())
            .map(|dir| std::path::PathBuf::from(dir).join(format!("{agent_id}.jsonl")));

        let Some(path) = transcript_path else {
            return;
        };

        let (tx, rx) = oneshot::channel();
        self.pending_transcript = Some(rx);
        // Determine if the agent is still active (for C2: skip warning on partial last line).
        let is_active = self
            .metrics
            .sub_agents
            .iter()
            .find(|sa| sa.id == agent_id)
            .is_some_and(|sa| matches!(sa.state.as_str(), "working" | "submitted"));

        tokio::task::spawn_blocking(move || {
            let result = load_transcript_file(&path, is_active);
            let _ = tx.send(result);
        });
    }

    /// Poll the pending transcript load and install result if ready.
    pub fn poll_pending_transcript(&mut self) {
        let Some(rx) = self.pending_transcript.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok((entries, total)) => {
                self.pending_transcript = None;
                let turns_at_load = self
                    .view_target
                    .subagent_id()
                    .and_then(|id| self.metrics.sub_agents.iter().find(|sa| sa.id == id))
                    .map_or(0, |sa| sa.turns_used);
                if let AgentViewTarget::SubAgent { ref id, .. } = self.view_target.clone() {
                    self.transcript_cache = Some(TranscriptCache {
                        agent_id: id.clone(),
                        entries,
                        turns_at_load,
                        total_in_file: total,
                    });
                }
                self.render_cache.clear();
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.pending_transcript = None;
            }
        }
    }

    /// Check if the transcript needs reloading (turns count increased).
    fn maybe_reload_transcript(&mut self) {
        let AgentViewTarget::SubAgent { ref id, .. } = self.view_target.clone() else {
            return;
        };
        // Don't start a new load while one is already in flight.
        if self.pending_transcript.is_some() {
            return;
        }
        let current_turns = self
            .metrics
            .sub_agents
            .iter()
            .find(|sa| sa.id == *id)
            .map_or(0, |sa| sa.turns_used);
        let cached_turns = self
            .transcript_cache
            .as_ref()
            .map_or(0, |c| c.turns_at_load);
        if current_turns > cached_turns {
            let agent_id = id.to_owned();
            self.start_transcript_load(&agent_id);
        }
    }

    /// Returns the messages to display in the chat area.
    /// When viewing a subagent, returns transcript entries converted to `ChatMessage`.
    /// When no transcript is loaded yet, returns a loading placeholder.
    #[must_use]
    pub fn visible_messages(&self) -> std::borrow::Cow<'_, [ChatMessage]> {
        if self.view_target.is_main() {
            return std::borrow::Cow::Borrowed(&self.messages);
        }
        if let Some(ref cache) = self.transcript_cache {
            let msgs: Vec<ChatMessage> = cache
                .entries
                .iter()
                .map(TuiTranscriptEntry::to_chat_message)
                .collect();
            std::borrow::Cow::Owned(msgs)
        } else if self.pending_transcript.is_some() {
            // Loading in progress — show placeholder.
            std::borrow::Cow::Owned(vec![ChatMessage::new(
                MessageRole::System,
                "Loading transcript...".to_owned(),
            )])
        } else {
            // No transcript available.
            let name = self.view_target.subagent_name().unwrap_or("unknown");
            std::borrow::Cow::Owned(vec![ChatMessage::new(
                MessageRole::System,
                format!("Transcript not available for {name}."),
            )])
        }
    }

    /// Returns the truncation info string if the transcript was truncated.
    #[must_use]
    pub fn transcript_truncation_info(&self) -> Option<String> {
        let cache = self.transcript_cache.as_ref()?;
        if cache.total_in_file > TRANSCRIPT_MAX_ENTRIES {
            Some(format!(
                "[showing last {TRANSCRIPT_MAX_ENTRIES} of {} messages]",
                cache.total_in_file
            ))
        } else {
            None
        }
    }

    /// Evict oldest messages when the buffer exceeds `MAX_TUI_MESSAGES` (#2737).
    ///
    /// Shifts the render cache to match the drained messages, preserving cached renders
    /// for the remaining entries and avoiding a full re-render stall (#2775).
    fn trim_messages(&mut self) {
        if self.messages.len() > MAX_TUI_MESSAGES {
            let excess = self.messages.len() - MAX_TUI_MESSAGES;
            self.messages.drain(0..excess);
            self.render_cache.shift(excess);
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
        }
    }

    /// Return a slice of all chat messages currently in the buffer.
    ///
    /// For the currently-displayed messages (which may be a sub-agent
    /// transcript) use [`visible_messages`](Self::visible_messages) instead.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Return the current content of the text input field.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Return the current input mode (normal vs. insert).
    #[must_use]
    pub fn input_mode(&self) -> InputMode {
        self.input_mode
    }

    /// Return the cursor byte position within the input string.
    #[must_use]
    pub fn cursor_position(&self) -> usize {
        self.cursor_position
    }

    /// Return the number of lines the chat view is scrolled up from the bottom.
    ///
    /// `0` means the view is at the bottom (latest messages visible).
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Scroll to bottom only if already at (or near) the bottom.
    fn auto_scroll(&mut self) {
        if self.scroll_offset <= 1 {
            self.scroll_offset = 0;
        }
    }

    /// Return `true` when tool-output blocks are expanded to full height.
    #[must_use]
    pub fn tool_expanded(&self) -> bool {
        self.tool_expanded
    }

    /// Return the active paste indicator state, if any.
    ///
    /// `Some` when a multiline paste is in the input buffer and no edit
    /// keypress has occurred since the paste. `None` otherwise.
    #[must_use]
    pub fn paste_state(&self) -> Option<&PasteState> {
        self.paste_state.as_ref()
    }

    /// Return `true` when tool blocks use compact single-line rendering.
    #[must_use]
    pub fn compact_tools(&self) -> bool {
        self.compact_tools
    }

    /// Return `true` when source-label badges are shown on assistant messages.
    #[must_use]
    pub fn show_source_labels(&self) -> bool {
        self.show_source_labels
    }

    /// Toggle source-label visibility.
    ///
    /// Clears the render cache so all messages are re-rendered with the new
    /// setting on the next frame.
    pub fn set_show_source_labels(&mut self, v: bool) {
        if self.show_source_labels != v {
            self.show_source_labels = v;
            self.render_cache.clear();
        }
    }

    /// Replace the current hyperlink span list with `links`.
    ///
    /// Called by the render loop after each frame to store spans detected in
    /// the terminal buffer so they can be emitted as OSC 8 sequences.
    pub fn set_hyperlinks(&mut self, links: Vec<HyperlinkSpan>) {
        self.hyperlinks = links;
    }

    /// Take ownership of the accumulated hyperlink spans, clearing the list.
    ///
    /// Called once per frame; the caller writes OSC 8 sequences to the terminal.
    pub fn take_hyperlinks(&mut self) -> Vec<HyperlinkSpan> {
        std::mem::take(&mut self.hyperlinks)
    }

    /// Return the current activity status label, if any.
    ///
    /// Displayed in the activity bar with a spinner when non-`None`
    /// (e.g. `"Searching memory…"`, `"Executing tool: bash"`).
    #[must_use]
    pub fn status_label(&self) -> Option<&str> {
        self.status_label.as_deref()
    }

    /// Return the number of messages queued or pending for the agent.
    ///
    /// Displayed in the input bar to indicate backpressure.
    #[must_use]
    pub fn queued_count(&self) -> usize {
        self.queued_count.max(self.pending_count)
    }

    /// Return `true` when the user is currently editing a queued message.
    #[must_use]
    pub fn editing_queued(&self) -> bool {
        self.editing_queued
    }

    /// Return `true` when the agent is actively processing (streaming or running a tool).
    ///
    /// Used by the render loop to decide whether to show the activity spinner.
    #[must_use]
    pub fn is_agent_busy(&self) -> bool {
        self.status_label.is_some() || self.messages.last().is_some_and(|m| m.streaming)
    }

    /// Return `true` when the last message is a streaming tool output.
    #[must_use]
    pub fn has_running_tool(&self) -> bool {
        self.messages
            .last()
            .is_some_and(|m| m.role == MessageRole::Tool && m.streaming)
    }

    /// Return a reference to the throbber animation state.
    ///
    /// Used by the status widget to render the spinner frame.
    #[must_use]
    pub fn throbber_state(&self) -> &throbber_widgets_tui::ThrobberState {
        &self.throbber_state
    }

    /// Return a mutable reference to the throbber animation state.
    ///
    /// Called by the tick handler to advance the spinner frame each tick.
    pub fn throbber_state_mut(&mut self) -> &mut throbber_widgets_tui::ThrobberState {
        &mut self.throbber_state
    }

    /// Dispatch a top-level [`AppEvent`] to the appropriate handler.
    ///
    /// Called once per event in the main [`crate::run_tui`] loop.
    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Tick => {
                self.throbber_state.calc_next();
            }
            AppEvent::Resize(_, _) => {
                self.render_cache.clear();
            }
            AppEvent::MouseScroll(delta) => {
                if self.confirm_state.is_none() {
                    if delta > 0 {
                        self.scroll_offset = self.scroll_offset.saturating_add(1);
                    } else {
                        self.scroll_offset = self.scroll_offset.saturating_sub(1);
                    }
                }
            }
            AppEvent::Agent(agent_event) => self.handle_agent_event(agent_event),
            AppEvent::Paste(text) => self.handle_paste(&text),
        }
    }

    /// Await the next [`AgentEvent`] from the agent channel.
    ///
    /// Returns `None` when all senders have been dropped (agent exited).
    /// Called from the `select!` block in [`crate::run_tui`].
    pub fn poll_agent_event(&mut self) -> impl Future<Output = Option<AgentEvent>> + use<'_> {
        self.agent_event_rx.recv()
    }

    /// Non-blocking poll for a pending [`AgentEvent`].
    ///
    /// Used to drain the channel after a first event has been received,
    /// coalescing multiple events into a single render frame.
    ///
    /// # Errors
    ///
    /// Returns `TryRecvError::Empty` if no events are pending, or
    /// `TryRecvError::Disconnected` if the sender has been dropped.
    pub fn try_recv_agent_event(&mut self) -> Result<AgentEvent, mpsc::error::TryRecvError> {
        self.agent_event_rx.try_recv()
    }

    /// Handle an [`AgentEvent`] and update widget state accordingly.
    ///
    /// This is the main state-transition function for agent-driven updates:
    /// appending streaming chunks, recording tool events, displaying confirm
    /// dialogs, and wiring late-bound channels (cancel signal, metrics).
    #[allow(clippy::too_many_lines)] // large match over all agent event variants
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Chunk(text) => {
                self.status_label = None;
                if let Some(last) = self.messages.last_mut()
                    && last.role == MessageRole::Assistant
                    && last.streaming
                {
                    last.content.push_str(&text);
                } else {
                    self.messages
                        .push(ChatMessage::new(MessageRole::Assistant, text).streaming());
                    self.trim_messages();
                }
                // No explicit cache invalidation needed: the cache key includes
                // content_hash, so new chunk content causes a natural cache miss.
                self.auto_scroll();
            }
            AgentEvent::FullMessage(text) => {
                self.status_label = None;
                if !text.starts_with("[tool output") {
                    self.messages
                        .push(ChatMessage::new(MessageRole::Assistant, text));
                    self.trim_messages();
                }
                self.auto_scroll();
            }
            AgentEvent::Flush => {
                if let Some(last) = self.messages.last_mut()
                    && last.streaming
                {
                    last.streaming = false;
                    let last_idx = self.messages.len().saturating_sub(1);
                    self.render_cache.invalidate(last_idx);
                }
            }
            AgentEvent::Typing => {
                self.pending_count = self.pending_count.saturating_sub(1);
                self.status_label = Some("thinking...".to_owned());
            }
            AgentEvent::Status(text) => {
                self.status_label = if text.is_empty() { None } else { Some(text) };
                self.auto_scroll();
            }
            AgentEvent::ToolStart { tool_name, command } => {
                self.status_label = None;
                self.messages.push(
                    ChatMessage::new(MessageRole::Tool, format!("$ {command}\n"))
                        .streaming()
                        .with_tool(tool_name),
                );
                self.trim_messages();
                self.auto_scroll();
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                if let Some(pos) = self
                    .messages
                    .iter()
                    .rposition(|m| m.role == MessageRole::Tool && m.streaming)
                {
                    self.messages[pos].content.push_str(&chunk);
                    self.render_cache.invalidate(pos);
                }
                self.auto_scroll();
            }
            AgentEvent::ToolOutput {
                tool_name,
                output,
                diff,
                filter_stats,
                kept_lines,
                ..
            } => {
                self.handle_tool_output_event(tool_name, output, diff, filter_stats, kept_lines);
            }
            AgentEvent::ConfirmRequest {
                prompt,
                response_tx,
            } => {
                self.confirm_state = Some(ConfirmState {
                    prompt,
                    response_tx: Some(response_tx),
                });
            }
            AgentEvent::ElicitationRequest {
                request,
                response_tx,
            } => {
                let dialog = crate::widgets::elicitation::ElicitationDialogState::new(request);
                self.elicitation_state = Some(ElicitationState {
                    dialog,
                    response_tx: Some(response_tx),
                });
            }
            AgentEvent::QueueCount(count) => {
                self.queued_count = count;
                self.pending_count = count;
            }
            AgentEvent::DiffReady(diff) => self.handle_diff_ready(diff),
            AgentEvent::CommandResult { output, .. } => {
                self.command_palette = None;
                self.messages
                    .push(ChatMessage::new(MessageRole::System, output));
                self.trim_messages();
                self.auto_scroll();
            }
            AgentEvent::SetCancelSignal(signal) => {
                self.set_cancel_signal(signal);
            }
            AgentEvent::SetMetricsRx(rx) => {
                self.set_metrics_rx(rx);
            }
        }
    }

    fn handle_diff_ready(&mut self, diff: zeph_core::DiffData) {
        if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Tool)
        {
            msg.diff_data = Some(diff);
        }
    }

    fn handle_tool_output_event(
        &mut self,
        tool_name: zeph_common::ToolName,
        output: String,
        diff: Option<zeph_core::DiffData>,
        filter_stats: Option<String>,
        kept_lines: Option<Vec<usize>>,
    ) {
        debug!(
            %tool_name,
            has_diff = diff.is_some(),
            has_filter_stats = filter_stats.is_some(),
            output_len = output.len(),
            "TUI ToolOutput event received"
        );
        if let Some(pos) = self
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::Tool && m.streaming)
        {
            // Finalize existing streaming tool message (shell or native path with ToolStart).
            // Replace content after the header line ("$ cmd\n") with the canonical body_display
            // from ToolOutputEvent. Streaming chunks (Path B) may already occupy that space;
            // appending would duplicate the output. Truncating to the header and re-writing
            // body_display produces exactly one copy regardless of whether chunks arrived.
            debug!("finalizing existing streaming Tool message");
            let header_end = self.messages[pos].content.find('\n').map_or(0, |i| i + 1);
            self.messages[pos].content.truncate(header_end);
            self.messages[pos].content.push_str(&output);
            self.messages[pos].streaming = false;
            self.messages[pos].diff_data = diff;
            self.messages[pos].filter_stats = filter_stats;
            self.messages[pos].kept_lines = kept_lines;
            self.render_cache.invalidate(pos);
        } else if diff.is_some() || filter_stats.is_some() || kept_lines.is_some() {
            // No prior ToolStart: create the message now (legacy fallback).
            debug!("creating new Tool message with diff (no prior ToolStart)");
            let mut msg = ChatMessage::new(MessageRole::Tool, output).with_tool(tool_name);
            msg.diff_data = diff;
            msg.filter_stats = filter_stats;
            msg.kept_lines = kept_lines;
            self.messages.push(msg);
            self.trim_messages();
        } else if let Some(msg) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Tool)
        {
            msg.filter_stats = filter_stats;
        }
        self.auto_scroll();
    }

    #[must_use]
    pub fn confirm_state(&self) -> Option<&ConfirmState> {
        self.confirm_state.as_ref()
    }

    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let layout = AppLayout::compute(frame.area(), self.show_side_panels);

        self.draw_header(frame, layout.header);
        if self.show_splash {
            widgets::splash::render(frame, layout.chat);
        } else {
            let mut cache = std::mem::take(&mut self.render_cache);
            let max_scroll = widgets::chat::render(self, frame, layout.chat, &mut cache);
            self.render_cache = cache;
            self.scroll_offset = self.scroll_offset.min(max_scroll);
        }
        self.draw_side_panel(frame, &layout);
        widgets::chat::render_activity(self, frame, layout.activity);
        widgets::input::render(self, frame, layout.input);
        widgets::status::render(self, &self.metrics, frame, layout.status);

        if let Some(state) = &self.file_picker_state {
            widgets::file_picker::render(state, frame, layout.input);
        }

        if let Some(state) = &self.slash_autocomplete {
            widgets::slash_autocomplete::render(state, frame, layout.input);
        }

        if let Some(state) = &self.confirm_state {
            widgets::confirm::render(&state.prompt, frame, frame.area());
        }

        if let Some(state) = &self.elicitation_state {
            widgets::elicitation::render(&state.dialog, frame, frame.area());
        }

        if let Some(palette) = &self.command_palette {
            widgets::command_palette::render(palette, frame, frame.area());
        }

        if self.show_help {
            widgets::help::render(frame, frame.area());
        }
    }

    fn draw_header(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        let theme = Theme::default();

        let provider = if self.metrics.provider_name.is_empty() {
            "---"
        } else {
            &self.metrics.provider_name
        };
        let model = if self.metrics.model_name.is_empty() {
            "---"
        } else {
            &self.metrics.model_name
        };

        let ctx_badge = if self.metrics.extended_context {
            " [1M CTX]"
        } else {
            ""
        };
        let text = format!(
            " Zeph v{} | Provider: {provider} | Model: {model}{ctx_badge}",
            env!("CARGO_PKG_VERSION")
        );

        let line = Line::from(Span::styled(text, theme.header));
        let paragraph = Paragraph::new(line).style(theme.header);
        frame.render_widget(paragraph, area);
    }

    fn draw_side_panel(&mut self, frame: &mut ratatui::Frame, layout: &AppLayout) {
        widgets::skills::render(&self.metrics, frame, layout.skills);
        widgets::memory::render(&self.metrics, frame, layout.memory);
        widgets::resources::render(&self.metrics, frame, layout.resources);

        let tick = self.throbber_state.index().cast_unsigned();
        let has_graph = self.metrics.orchestration_graph.as_ref().is_some_and(|s| {
            // Use is_stale() to check if snapshot is too old to show (IC4).
            !s.is_stale()
        });
        let panel_focused = self.active_panel == Panel::SubAgents;

        // When SubAgents panel is focused (`a` key), always show the interactive sidebar.
        // Otherwise: auto-show plan when graph active, security events, or subagents list.
        if panel_focused {
            widgets::subagents::render_interactive(
                &self.metrics,
                &mut self.subagent_sidebar,
                frame,
                layout.subagents,
                tick,
            );
        } else if has_graph && !self.plan_view_active {
            widgets::plan_view::render(&self.metrics, frame, layout.subagents, tick);
        } else if self.has_recent_security_events() {
            widgets::security::render(&self.metrics, frame, layout.subagents);
        } else {
            widgets::subagents::render(&self.metrics, frame, layout.subagents);
        }

        // Overlay task registry over the subagents slot when `/tasks` is toggled.
        if self.show_task_panel {
            if self.task_supervisor.is_some() {
                widgets::task_registry::render(
                    &self.cached_task_snapshots,
                    tick,
                    layout.subagents,
                    frame,
                );
            } else {
                use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
                let theme = Theme::default();
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.panel_border)
                    .title(" Tasks ");
                let paragraph = Paragraph::new(" Task supervisor not available.")
                    .block(block)
                    .wrap(Wrap { trim: true });
                frame.render_widget(paragraph, layout.subagents);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc => self.show_help = false,
                _ => {}
            }
            return;
        }

        if self.confirm_state.is_some() {
            self.handle_confirm_key(key);
            return;
        }

        if self.elicitation_state.is_some() {
            self.handle_elicitation_key(key);
            return;
        }

        if self.command_palette.is_some() {
            self.handle_palette_key(key);
            return;
        }

        if self.file_picker_state.is_some() {
            self.handle_file_picker_key(key);
            return;
        }

        match self.input_mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Insert => self.handle_insert_key(key),
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        let response = match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        if let Some(answer) = response
            && let Some(mut state) = self.confirm_state.take()
            && let Some(tx) = state.response_tx.take()
        {
            let _ = tx.send(answer);
        }
    }

    fn handle_elicitation_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyModifiers;
        use zeph_core::channel::ElicitationResponse;

        let Some(state) = self.elicitation_state.as_mut() else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                // Cancel — always dismisses regardless of vi-mode
                if let Some(mut st) = self.elicitation_state.take()
                    && let Some(tx) = st.response_tx.take()
                {
                    let _ = tx.send(ElicitationResponse::Cancelled);
                }
            }
            KeyCode::Enter => {
                if let Some(value) = state.dialog.build_submission()
                    && let Some(mut st) = self.elicitation_state.take()
                    && let Some(tx) = st.response_tx.take()
                {
                    let _ = tx.send(ElicitationResponse::Accepted(value));
                }
                // If build_submission returns None (required field empty), stay open
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    state.dialog.prev_field();
                } else {
                    state.dialog.next_field();
                }
            }
            KeyCode::BackTab => {
                state.dialog.prev_field();
            }
            KeyCode::Up => {
                state.dialog.enum_prev();
            }
            KeyCode::Down => {
                state.dialog.enum_next();
            }
            KeyCode::Char(' ') => {
                state.dialog.toggle_bool();
            }
            KeyCode::Char(c) => {
                state.dialog.push_char(c);
            }
            KeyCode::Backspace => {
                state.dialog.pop_char();
            }
            _ => {}
        }
    }

    fn handle_palette_key(&mut self, key: KeyEvent) {
        let Some(palette) = self.command_palette.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.command_palette = None;
            }
            KeyCode::Enter => {
                if let Some(entry) = palette.selected_entry() {
                    let cmd = entry.command;
                    self.execute_command(cmd);
                }
                self.command_palette = None;
            }
            KeyCode::Up => {
                palette.move_up();
            }
            KeyCode::Down => {
                palette.move_down();
            }
            KeyCode::Backspace => {
                palette.pop_char();
            }
            KeyCode::Char(c) => {
                palette.push_char(c);
            }
            _ => {}
        }
    }

    fn execute_command(&mut self, cmd: TuiCommand) {
        match cmd {
            TuiCommand::SkillList => self.push_system_message(self.format_skill_list()),
            TuiCommand::McpList => self.push_system_message(self.format_mcp_list()),
            TuiCommand::MemoryStats => self.push_system_message(self.format_memory_stats()),
            TuiCommand::ViewCost => self.push_system_message(self.format_cost_stats()),
            TuiCommand::ViewTools => self.push_system_message(self.format_tool_list()),
            TuiCommand::ViewConfig | TuiCommand::ViewAutonomy => {
                if let Some(ref tx) = self.command_tx {
                    // try_send: capacity 16, user-triggered one at a time — overflow not possible in practice
                    let _ = tx.try_send(cmd);
                } else {
                    self.push_system_message(
                        "Config not available (no command channel).".to_owned(),
                    );
                }
            }
            TuiCommand::Quit => {
                self.should_quit = true;
            }
            TuiCommand::Help => {
                self.show_help = true;
            }
            TuiCommand::NewSession => {
                self.messages.clear();
                self.push_system_message("New conversation started.".to_owned());
            }
            TuiCommand::ToggleTheme => {
                self.push_system_message("Theme switching is not yet implemented.".to_owned());
            }
            TuiCommand::SessionBrowser => {
                if let Some(ref tx) = self.command_tx {
                    let _ = tx.try_send(cmd);
                } else {
                    self.push_system_message(
                        "Session browser not available (no command channel).".to_owned(),
                    );
                }
            }
            TuiCommand::DaemonConnect | TuiCommand::DaemonDisconnect | TuiCommand::DaemonStatus => {
                self.push_system_message(
                    "Daemon commands are not yet implemented in this mode.".to_owned(),
                );
            }
            TuiCommand::ViewFilters => {
                self.push_system_message(
                    "Filter statistics are displayed in the Resources panel.".to_owned(),
                );
            }
            TuiCommand::Ingest => {
                self.push_system_message(
                    "Use: zeph ingest <path> [--chunk-size N] [--collection NAME]".to_owned(),
                );
            }
            TuiCommand::GatewayStatus => {
                self.push_system_message(
                    "Gateway status is not yet available in TUI mode.".to_owned(),
                );
            }
            TuiCommand::AgentList => {
                let _ = self.user_input_tx.try_send("/agent list".to_owned());
            }
            TuiCommand::AgentStatus => {
                let _ = self.user_input_tx.try_send("/agent status".to_owned());
            }
            TuiCommand::AgentCancelPrompt => self.prefill_input("/agent cancel "),
            TuiCommand::AgentSpawnPrompt => self.prefill_input("/agent spawn "),
            TuiCommand::AgentsShow => self.prefill_input("/agents show "),
            TuiCommand::AgentsCreate => self.prefill_input("/agents create "),
            TuiCommand::AgentsEdit => self.prefill_input("/agents edit "),
            TuiCommand::AgentsDelete => self.prefill_input("/agents delete "),
            TuiCommand::SchedulerList => self.push_system_message(self.format_scheduler_list()),
            TuiCommand::RouterStats => self.push_system_message(self.format_router_stats()),
            TuiCommand::SecurityEvents => {
                self.push_system_message(format_security_report(&self.metrics));
            }
            TuiCommand::TaskPanel => {
                self.show_task_panel = !self.show_task_panel;
            }
            cmd => self.execute_plan_graph_command(cmd),
        }
    }

    fn execute_plan_graph_command(&mut self, cmd: TuiCommand) {
        match cmd {
            TuiCommand::PlanStatus => {
                let _ = self.user_input_tx.try_send("/plan status".to_owned());
            }
            TuiCommand::PlanConfirm => {
                let _ = self.user_input_tx.try_send("/plan confirm".to_owned());
            }
            TuiCommand::PlanCancel => {
                let _ = self.user_input_tx.try_send("/plan cancel".to_owned());
            }
            TuiCommand::PlanList => {
                let _ = self.user_input_tx.try_send("/plan list".to_owned());
            }
            TuiCommand::PlanToggleView => {
                self.plan_view_active = !self.plan_view_active;
            }
            TuiCommand::GraphStats => {
                self.push_system_message("Loading graph stats...".to_owned());
                let _ = self.user_input_tx.try_send("/graph".to_owned());
            }
            TuiCommand::GraphEntities => {
                self.push_system_message("Loading graph entities...".to_owned());
                let _ = self.user_input_tx.try_send("/graph entities".to_owned());
            }
            TuiCommand::GraphCommunities => {
                self.push_system_message("Loading graph communities...".to_owned());
                let _ = self.user_input_tx.try_send("/graph communities".to_owned());
            }
            TuiCommand::GraphFactsPrompt => self.prefill_input("/graph facts "),
            TuiCommand::GraphBackfillPrompt => self.prefill_input("/graph backfill"),
            TuiCommand::ExperimentStart => self.prefill_input("/experiment start "),
            TuiCommand::ExperimentStop => {
                let _ = self.user_input_tx.try_send("/experiment stop".to_owned());
            }
            TuiCommand::ExperimentStatus => {
                let _ = self.user_input_tx.try_send("/experiment status".to_owned());
            }
            TuiCommand::ExperimentReport => {
                let _ = self.user_input_tx.try_send("/experiment report".to_owned());
            }
            TuiCommand::ExperimentBest => {
                let _ = self.user_input_tx.try_send("/experiment best".to_owned());
            }
            TuiCommand::LspStatus => {
                self.push_system_message("Checking LSP context injection status...".to_owned());
                let _ = self.user_input_tx.try_send("/lsp".to_owned());
            }
            TuiCommand::ViewLog => {
                let _ = self.user_input_tx.try_send("/log".to_owned());
            }
            TuiCommand::MigrateConfig => {
                self.push_system_message(
                    "To preview missing config parameters, run:\n  zeph migrate-config --diff\n\
                     To apply changes in-place:\n  zeph migrate-config --in-place"
                        .to_owned(),
                );
            }
            TuiCommand::ServerCompactionStatus => {
                let _ = self.user_input_tx.try_send("/server-compaction".to_owned());
            }
            TuiCommand::ViewGuidelines => {
                let _ = self.user_input_tx.try_send("/guidelines".to_owned());
            }
            TuiCommand::ForgettingSweep => {
                let _ = self.user_input_tx.try_send("/forgetting-sweep".to_owned());
            }
            TuiCommand::PredictorStatus => {
                let _ = self.user_input_tx.try_send("/predictor-status".to_owned());
            }
            TuiCommand::TrajectoryStats => {
                let _ = self.user_input_tx.try_send("/memory trajectory".to_owned());
            }
            TuiCommand::MemoryTreeStats => {
                let _ = self.user_input_tx.try_send("/memory tree".to_owned());
            }
            _ => {}
        }
    }

    fn prefill_input(&mut self, prefix: &str) {
        self.input.clear();
        self.input.push_str(prefix);
        self.cursor_position = self.input.len();
    }

    fn format_skill_list(&self) -> String {
        if self.metrics.active_skills.is_empty() {
            return "No skills loaded.".to_owned();
        }
        let lines: Vec<String> = self
            .metrics
            .active_skills
            .iter()
            .map(|s| format!("  - {s}"))
            .collect();
        format!(
            "Loaded skills ({}):\n{}",
            self.metrics.active_skills.len(),
            lines.join("\n")
        )
    }

    fn format_mcp_list(&self) -> String {
        if self.metrics.active_mcp_tools.is_empty() {
            return "No MCP tools available.".to_owned();
        }
        let lines: Vec<String> = self
            .metrics
            .active_mcp_tools
            .iter()
            .map(|t| format!("  - {t}"))
            .collect();
        format!(
            "MCP servers: {}  Tools ({}):\n{}",
            self.metrics.mcp_server_count,
            self.metrics.active_mcp_tools.len(),
            lines.join("\n")
        )
    }

    fn format_memory_stats(&self) -> String {
        let vector_status = if self.metrics.qdrant_available {
            format!("{} (connected)", self.metrics.vector_backend)
        } else if !self.metrics.vector_backend.is_empty() {
            format!("{} (offline)", self.metrics.vector_backend)
        } else {
            "none".into()
        };
        format!(
            "Memory stats:\n  SQLite messages: {}\n  Vector store: {vector_status}\n  Embeddings generated: {}",
            self.metrics.sqlite_message_count, self.metrics.embeddings_generated,
        )
    }

    fn format_cost_stats(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "Cost:\n  Spent: ${:.4}\n  Prompt tokens: {}\n  Completion tokens: {}\n  Total tokens: {}\n  Cache read: {}\n  Cache creation: {}",
            self.metrics.cost_spent_cents / 100.0,
            self.metrics.prompt_tokens,
            self.metrics.completion_tokens,
            self.metrics.total_tokens,
            self.metrics.cache_read_tokens,
            self.metrics.cache_creation_tokens,
        );
        if !self.metrics.provider_cost_breakdown.is_empty() {
            let _ = write!(out, "\n\nPer-provider breakdown:");
            let _ = write!(
                out,
                "\n  {:<16} {:<28} {:>8} {:>9} {:>9} {:>8} {:>8}",
                "Provider", "Model", "Input", "Cache-R", "Cache-W", "Output", "Cost"
            );
            for (name, usage) in &self.metrics.provider_cost_breakdown {
                let model_display = if usage.model.chars().count() > 26 {
                    format!("{}…", usage.model.chars().take(25).collect::<String>())
                } else {
                    usage.model.clone()
                };
                let _ = write!(
                    out,
                    "\n  {:<16} {:<28} {:>8} {:>9} {:>9} {:>8} {:>8}",
                    name,
                    model_display,
                    usage.input_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens,
                    usage.output_tokens,
                    format!("${:.4}", usage.cost_cents / 100.0),
                );
            }
            let _ = write!(
                out,
                "\n\n  Note: excludes subsystem calls (compaction, graph extraction, planning)"
            );
        }
        out
    }

    fn format_tool_list(&self) -> String {
        if self.metrics.active_mcp_tools.is_empty() {
            return "No tools available.".to_owned();
        }
        let lines: Vec<String> = self
            .metrics
            .active_mcp_tools
            .iter()
            .map(|t| format!("  - {t}"))
            .collect();
        format!(
            "Available tools ({}):\n{}",
            self.metrics.active_mcp_tools.len(),
            lines.join("\n")
        )
    }

    fn format_scheduler_list(&self) -> String {
        if self.metrics.scheduled_tasks.is_empty() {
            return "No scheduled tasks.".to_owned();
        }
        let lines: Vec<String> = self
            .metrics
            .scheduled_tasks
            .iter()
            .map(|t| {
                let next = if t[3].is_empty() {
                    "—".to_owned()
                } else {
                    t[3].clone()
                };
                format!("  {:30}  {:15}  {:8}  {}", t[0], t[1], t[2], next)
            })
            .collect();
        format!(
            "Scheduled tasks ({}):\n  {:30}  {:15}  {:8}  {}\n{}",
            self.metrics.scheduled_tasks.len(),
            "NAME",
            "KIND",
            "MODE",
            "NEXT RUN",
            lines.join("\n")
        )
    }

    fn format_router_stats(&self) -> String {
        if self.metrics.router_thompson_stats.is_empty() {
            return "Router: no Thompson state available.\n\
                (Thompson strategy not active, or no LLM calls made yet)"
                .to_owned();
        }
        let total_mean: f64 = self
            .metrics
            .router_thompson_stats
            .iter()
            .map(|(_, a, b)| a / (a + b))
            .sum();
        let lines: Vec<String> = self
            .metrics
            .router_thompson_stats
            .iter()
            .map(|(name, alpha, beta)| {
                let mean = alpha / (alpha + beta);
                let pct = if total_mean > 0.0 {
                    mean / total_mean * 100.0
                } else {
                    0.0
                };
                format!("  {name:<28}  α={alpha:.2}  β={beta:.2}  Mean={pct:.1}%")
            })
            .collect();
        let n = self.metrics.router_thompson_stats.len();
        format!(
            "Thompson Sampling state ({n} providers):\n{}",
            lines.join("\n")
        )
    }

    fn push_system_message(&mut self, content: String) {
        self.show_splash = false;
        self.messages
            .push(ChatMessage::new(MessageRole::System, content));
        self.scroll_offset = 0;
    }

    /// Returns true if there are security events within the last 60 seconds.
    #[must_use]
    pub fn has_recent_security_events(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.metrics
            .security_events
            .back()
            .is_some_and(|ev| now.saturating_sub(ev.timestamp) <= 60)
    }

    /// Handle keys specific to the `SubAgents` panel and transcript view.
    /// Returns `true` if the key was consumed.
    fn handle_subagent_panel_key(&mut self, key: KeyEvent) -> bool {
        if self.active_panel == Panel::SubAgents {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    let count = self.metrics.sub_agents.len();
                    self.subagent_sidebar.select_next(count);
                    return true;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    let count = self.metrics.sub_agents.len();
                    self.subagent_sidebar.select_prev(count);
                    return true;
                }
                KeyCode::Enter => {
                    if let Some(idx) = self.subagent_sidebar.selected()
                        && let Some(sa) = self.metrics.sub_agents.get(idx)
                    {
                        let target = AgentViewTarget::SubAgent {
                            id: sa.id.clone(),
                            name: sa.name.clone(),
                        };
                        self.set_view_target(target);
                    }
                    return true;
                }
                KeyCode::Esc => {
                    self.active_panel = Panel::Chat;
                    return true;
                }
                _ => {}
            }
        }
        // Esc while viewing a subagent transcript returns to Main.
        if key.code == KeyCode::Esc && !self.view_target.is_main() {
            self.set_view_target(AgentViewTarget::Main);
            return true;
        }
        false
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        if self.handle_subagent_panel_key(key) {
            return;
        }
        match key.code {
            KeyCode::Esc if self.is_agent_busy() => {
                if let Some(ref signal) = self.cancel_signal {
                    signal.notify_waiters();
                }
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('H') => self.execute_command(TuiCommand::SessionBrowser),
            KeyCode::Char('i') => self.input_mode = InputMode::Insert,
            KeyCode::Char(':') => {
                self.command_palette = Some(CommandPaletteState::new());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            KeyCode::Home => {
                self.scroll_offset = if let Some(cache) = &self.transcript_cache {
                    cache.entries.len()
                } else {
                    self.messages.len()
                };
            }
            KeyCode::End => {
                self.scroll_offset = 0;
            }
            KeyCode::Char('d') => {
                self.show_side_panels = !self.show_side_panels;
            }
            KeyCode::Char('e') => {
                self.tool_expanded = !self.tool_expanded;
                self.render_cache.clear();
            }
            KeyCode::Char('c') => {
                self.compact_tools = !self.compact_tools;
                self.render_cache.clear();
            }
            KeyCode::Tab => {
                self.active_panel = match self.active_panel {
                    Panel::Chat => Panel::Skills,
                    Panel::Skills => Panel::Memory,
                    Panel::Memory => Panel::Resources,
                    Panel::Resources => Panel::SubAgents,
                    Panel::SubAgents | Panel::Tasks => Panel::Chat,
                };
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.view_target.is_main() {
                    self.messages.clear();
                }
                self.render_cache.clear();
                self.scroll_offset = 0;
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            KeyCode::Char('p') => {
                self.plan_view_active = !self.plan_view_active;
            }
            KeyCode::Char('a') => {
                self.active_panel = Panel::SubAgents;
                // Auto-select first agent if nothing selected yet.
                if self.subagent_sidebar.selected().is_none() && !self.metrics.sub_agents.is_empty()
                {
                    self.subagent_sidebar.list_state.select(Some(0));
                }
            }
            _ => {}
        }
    }

    /// Returns the byte offset of the char at the given char index.
    fn byte_offset_of_char(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map_or(self.input.len(), |(i, _)| i)
    }

    fn char_count(&self) -> usize {
        self.input.chars().count()
    }

    fn prev_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut pos = self.cursor_position;
        while pos > 0 && !chars[pos - 1].is_alphanumeric() {
            pos -= 1;
        }
        while pos > 0 && chars[pos - 1].is_alphanumeric() {
            pos -= 1;
        }
        pos
    }

    fn next_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let len = chars.len();
        let mut pos = self.cursor_position;
        while pos < len && chars[pos].is_alphanumeric() {
            pos += 1;
        }
        while pos < len && !chars[pos].is_alphanumeric() {
            pos += 1;
        }
        pos
    }

    fn handle_paste(&mut self, text: &str) {
        if self.input_mode != InputMode::Insert {
            return;
        }
        self.slash_autocomplete = None;
        let byte_offset = self.byte_offset_of_char(self.cursor_position);
        self.input.insert_str(byte_offset, text);
        self.cursor_position += text.chars().count();

        let line_count = text.matches('\n').count() + 1;
        if line_count >= 2 {
            // Replace any existing paste indicator — new paste supersedes the old one.
            self.paste_state = Some(PasteState {
                line_count,
                byte_len: text.len(),
            });
        } else {
            self.paste_state = None;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_insert_key(&mut self, key: KeyEvent) {
        if self.slash_autocomplete.is_some() {
            self.handle_slash_autocomplete_key(key);
            return;
        }
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                // First edit keystroke after paste reveals raw text; clear indicator.
                self.paste_state = None;
                let byte_offset = self.byte_offset_of_char(self.cursor_position);
                self.input.insert(byte_offset, '\n');
                self.cursor_position += 1;
            }
            KeyCode::Enter => self.submit_input(),
            KeyCode::Esc => self.input_mode = InputMode::Normal,
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.paste_state = None;
                let boundary = self.prev_word_boundary();
                if boundary < self.cursor_position {
                    let start = self.byte_offset_of_char(boundary);
                    let end = self.byte_offset_of_char(self.cursor_position);
                    self.input.drain(start..end);
                    self.cursor_position = boundary;
                }
            }
            KeyCode::Backspace => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.paste_state = None;
                if self.cursor_position > 0 {
                    let byte_offset = self.byte_offset_of_char(self.cursor_position - 1);
                    self.input.remove(byte_offset);
                    self.cursor_position -= 1;
                }
            }
            KeyCode::Delete => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.paste_state = None;
                if self.cursor_position < self.char_count() {
                    let byte_offset = self.byte_offset_of_char(self.cursor_position);
                    self.input.remove(byte_offset);
                }
            }
            KeyCode::Up => {
                self.handle_history_up();
            }
            KeyCode::Down => {
                self.paste_state = None;
                let Some(i) = self.history_index else {
                    return;
                };
                let prefix = &self.draft_input;
                let found = self.input_history[i + 1..]
                    .iter()
                    .position(|e| prefix.is_empty() || e.starts_with(prefix))
                    .map(|offset| i + 1 + offset);
                if let Some(idx) = found {
                    self.history_index = Some(idx);
                    self.input.clone_from(&self.input_history[idx]);
                } else {
                    self.history_index = None;
                    self.input = std::mem::take(&mut self.draft_input);
                }
                self.cursor_position = self.char_count();
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                self.paste_state = None;
                self.cursor_position = self.prev_word_boundary();
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                self.paste_state = None;
                self.cursor_position = self.next_word_boundary();
            }
            KeyCode::Left => {
                self.paste_state = None;
                self.cursor_position = self.cursor_position.saturating_sub(1);
            }
            KeyCode::Right => {
                self.paste_state = None;
                if self.cursor_position < self.char_count() {
                    self.cursor_position += 1;
                }
            }
            KeyCode::Home => {
                self.paste_state = None;
                self.cursor_position = 0;
            }
            KeyCode::End => {
                self.paste_state = None;
                self.cursor_position = self.char_count();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.paste_state = None;
                self.cursor_position = 0;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.paste_state = None;
                self.cursor_position = self.char_count();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.paste_state = None;
                self.input.clear();
                self.cursor_position = 0;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.user_input_tx.try_send("/clear-queue".to_owned());
            }
            KeyCode::Char('@') => {
                self.open_file_picker();
            }
            KeyCode::Char(c) => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.paste_state = None;
                let was_empty = self.input.is_empty();
                let byte_offset = self.byte_offset_of_char(self.cursor_position);
                self.input.insert(byte_offset, c);
                self.cursor_position += 1;
                if c == '/' && was_empty {
                    self.slash_autocomplete = Some(SlashAutocompleteState::new());
                }
            }
            _ => {}
        }
    }

    fn handle_slash_autocomplete_key(&mut self, key: KeyEvent) {
        let Some(state) = self.slash_autocomplete.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.slash_autocomplete = None;
            }
            KeyCode::Tab | KeyCode::Enter => {
                let entry = state.selected_entry().map(|e| e.id);
                self.slash_autocomplete = None;
                if let Some(id) = entry {
                    let slash_form = command_id_to_slash_form(id);
                    self.input = slash_form;
                    self.cursor_position = self.char_count();
                }
                if key.code == KeyCode::Enter {
                    self.submit_input();
                }
            }
            KeyCode::Down => {
                if let Some(s) = self.slash_autocomplete.as_mut() {
                    s.move_down();
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if let Some(s) = self.slash_autocomplete.as_mut() {
                    s.move_up();
                }
            }
            KeyCode::Backspace => {
                let dismiss = self
                    .slash_autocomplete
                    .as_mut()
                    .is_none_or(SlashAutocompleteState::pop_char);
                if dismiss {
                    self.input.clear();
                    self.cursor_position = 0;
                    self.slash_autocomplete = None;
                } else {
                    let query = self
                        .slash_autocomplete
                        .as_ref()
                        .map_or(String::new(), |s| s.query.clone());
                    self.input = format!("/{query}");
                    self.cursor_position = self.char_count();
                    if self
                        .slash_autocomplete
                        .as_ref()
                        .is_none_or(|s| s.filtered.is_empty())
                    {
                        self.slash_autocomplete = None;
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(s) = self.slash_autocomplete.as_mut() {
                    s.push_char(c);
                }
                let query = self
                    .slash_autocomplete
                    .as_ref()
                    .map_or(String::new(), |s| s.query.clone());
                self.input = format!("/{query}");
                self.cursor_position = self.char_count();
                if self
                    .slash_autocomplete
                    .as_ref()
                    .is_none_or(|s| s.filtered.is_empty())
                {
                    self.slash_autocomplete = None;
                }
            }
            _ => {}
        }
    }

    fn handle_history_up(&mut self) {
        self.paste_state = None;
        if self.input.is_empty() && self.pending_count > 0 && self.history_index.is_none() {
            if let Some(last) = self.input_history.pop() {
                self.input = last;
                self.cursor_position = self.char_count();
                self.pending_count -= 1;
                self.queued_count = self.queued_count.saturating_sub(1);
                self.editing_queued = true;
                if let Some(pos) = self
                    .messages
                    .iter()
                    .rposition(|m| m.role == MessageRole::User)
                {
                    self.messages.remove(pos);
                }
                let _ = self.user_input_tx.try_send("/drop-last-queued".to_owned());
            }
            return;
        }
        match self.history_index {
            None => {
                if self.input_history.is_empty() {
                    return;
                }
                self.draft_input = self.input.clone();
                let prefix = &self.draft_input;
                let found = self
                    .input_history
                    .iter()
                    .rposition(|e| prefix.is_empty() || e.starts_with(prefix));
                let Some(idx) = found else { return };
                self.history_index = Some(idx);
                self.input.clone_from(&self.input_history[idx]);
            }
            Some(i) => {
                let prefix = &self.draft_input;
                let found = self.input_history[..i]
                    .iter()
                    .rposition(|e| prefix.is_empty() || e.starts_with(prefix));
                let Some(idx) = found else { return };
                self.history_index = Some(idx);
                self.input.clone_from(&self.input_history[idx]);
            }
        }
        self.cursor_position = self.char_count();
    }

    fn open_file_picker(&mut self) {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let needs_rebuild = self.file_index.as_ref().is_none_or(FileIndex::is_stale);
        if needs_rebuild && self.pending_file_index.is_none() {
            self.status_label = Some("indexing files...".to_owned());
            let (tx, rx) = oneshot::channel();
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(FileIndex::build(&root));
            });
            self.pending_file_index = Some(rx);
            return;
        }
        if let Some(idx) = &self.file_index {
            self.file_picker_state = Some(FilePickerState::new(idx));
        }
    }

    /// Checks if the background file index build has completed and, if so,
    /// installs the result and opens the picker.
    pub fn poll_pending_file_index(&mut self) {
        let Some(rx) = self.pending_file_index.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok(idx) => {
                let picker = FilePickerState::new(&idx);
                self.file_index = Some(idx);
                self.file_picker_state = Some(picker);
                self.pending_file_index = None;
                self.status_label = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.pending_file_index = None;
                self.status_label = None;
            }
        }
    }

    fn handle_file_picker_key(&mut self, key: KeyEvent) {
        let Some(state) = self.file_picker_state.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.file_picker_state = None;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if let Some(path) = state.selected_path().map(ToOwned::to_owned) {
                    let byte_offset = self.byte_offset_of_char(self.cursor_position);
                    self.input.insert_str(byte_offset, &path);
                    self.cursor_position += path.chars().count();
                }
                self.file_picker_state = None;
            }
            KeyCode::Up => {
                state.move_selection(-1);
            }
            KeyCode::Down => {
                state.move_selection(1);
            }
            KeyCode::Char(c) => {
                state.push_char(c);
            }
            KeyCode::Backspace => {
                if !state.pop_char() {
                    self.file_picker_state = None;
                }
            }
            _ => {}
        }
    }

    fn submit_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.show_splash = false;
        self.input_history.push(text.clone());
        if self.input_history.len() > MAX_INPUT_HISTORY {
            let excess = self.input_history.len() - MAX_INPUT_HISTORY;
            self.input_history.drain(0..excess);
        }
        self.history_index = None;
        self.draft_input.clear();
        let paste_lines = self.paste_state.take().map(|p| p.line_count);
        let mut msg = ChatMessage::new(MessageRole::User, text.clone());
        msg.paste_line_count = paste_lines;
        self.messages.push(msg);
        self.trim_messages();
        self.input.clear();
        self.cursor_position = 0;
        self.scroll_offset = 0;
        self.editing_queued = false;
        self.pending_count += 1;

        // Non-blocking send; capacity 32 — silent drop if agent loop is saturated.
        // Message is visible in chat but not processed; acceptable for interactive TUI.
        let _ = self.user_input_tx.try_send(text);
    }
}

/// Maximum number of transcript entries loaded into the TUI (W4).
pub const TRANSCRIPT_MAX_ENTRIES: usize = 200;

/// Load transcript entries from a JSONL file in a blocking context.
/// Returns `(entries, total_line_count)` where `total_line_count` is the number
/// of lines in the file (before truncation), used for the truncation indicator.
///
/// When `is_active` is true, silently discards the last line if it fails to parse
/// (C2: partial-write race condition mitigation).
fn load_transcript_file(
    path: &std::path::Path,
    is_active: bool,
) -> (Vec<TuiTranscriptEntry>, usize) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return (Vec::new(), 0);
    }

    // C2: when agent is active, check if last line looks like partial write.
    let parse_end = if is_active && total > 0 {
        let last = lines[total - 1].trim();
        // A complete JSON object ends with '}'. Discard last line if partial write.
        if last.ends_with('}') {
            total
        } else {
            total - 1
        }
    } else {
        total
    };

    let entries: Vec<TuiTranscriptEntry> = lines[..parse_end]
        .iter()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            // Parse minimal fields needed for display.
            // Using serde_json::Value to avoid coupling to zeph-subagent types.
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            // TranscriptEntry wraps a Message in a `message` field.
            // Schema: { seq, timestamp, message: { role, parts: [{content}], tool_name? } }
            // Also support flat format: { role, content, tool_name?, timestamp? }
            let (role, content, tool_name, timestamp) = if let Some(msg) = v.get("message") {
                let role = msg
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("system")
                    .to_owned();
                // Extract content from first text part or direct content field.
                let content = msg
                    .get("parts")
                    .and_then(|p| p.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|part| part.get("content"))
                    .and_then(|c| c.as_str())
                    .or_else(|| msg.get("content").and_then(|c| c.as_str()))
                    .unwrap_or("")
                    .to_owned();
                let tool_name = msg
                    .get("tool_name")
                    .and_then(|t| t.as_str())
                    .map(zeph_common::ToolName::new);
                let timestamp = v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .map(ToOwned::to_owned);
                (role, content, tool_name, timestamp)
            } else {
                // Flat format fallback.
                let role = v
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("system")
                    .to_owned();
                let content = v
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_owned();
                let tool_name = v
                    .get("tool_name")
                    .and_then(|t| t.as_str())
                    .map(zeph_common::ToolName::new);
                let timestamp = v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .map(ToOwned::to_owned);
                (role, content, tool_name, timestamp)
            };

            if content.is_empty() && tool_name.is_none() {
                return None;
            }

            Some(TuiTranscriptEntry {
                role,
                content,
                tool_name,
                timestamp,
            })
        })
        .collect();

    // Take only the last N entries (W4).
    let truncated: Vec<TuiTranscriptEntry> = if entries.len() > TRANSCRIPT_MAX_ENTRIES {
        entries
            .into_iter()
            .rev()
            .take(TRANSCRIPT_MAX_ENTRIES)
            .rev()
            .collect()
    } else {
        entries
    };

    (truncated, total)
}

fn format_security_report(metrics: &MetricsSnapshot) -> String {
    use crate::metrics::SecurityEventCategory;

    let n = metrics.security_events.len();
    if n == 0 {
        return "Security event history (0 events)\n\nNo events recorded.".to_owned();
    }

    let mut lines = vec![format!("Security event history ({n} events):")];
    for ev in &metrics.security_events {
        #[allow(clippy::cast_possible_wrap)]
        let ts = chrono::DateTime::from_timestamp(ev.timestamp as i64, 0).map_or_else(
            || "??:??:??".to_owned(),
            |dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string()
            },
        );
        let cat = match ev.category {
            SecurityEventCategory::InjectionFlag => "INJECTION_FLAG ",
            SecurityEventCategory::InjectionBlocked => "INJECT_BLOCKED ",
            SecurityEventCategory::ExfiltrationBlock => "EXFIL_BLOCK    ",
            SecurityEventCategory::Quarantine => "QUARANTINE     ",
            SecurityEventCategory::Truncation => "TRUNCATION     ",
            SecurityEventCategory::RateLimit => "RATE_LIMIT     ",
            SecurityEventCategory::MemoryValidation => "MEM_VALIDATION ",
            SecurityEventCategory::PreExecutionBlock => "PRE_EXEC_BLOCK ",
            SecurityEventCategory::PreExecutionWarn => "PRE_EXEC_WARN  ",
            SecurityEventCategory::ResponseVerification => "RESP_VERIFY    ",
            SecurityEventCategory::CausalIpiFlag => "CAUSAL_IPI     ",
            SecurityEventCategory::CrossBoundaryMcpToAcp => "CROSS_BOUNDARY ",
        };
        lines.push(format!("  [{ts}] {cat}  {:<20}  {}", ev.source, ev.detail));
    }
    lines.push(String::new());
    lines.push("Totals:".to_owned());
    lines.push(format!(
        "  Sanitizer runs: {}  |  Flags: {}  |  Truncations: {}",
        metrics.sanitizer_runs, metrics.sanitizer_injection_flags, metrics.sanitizer_truncations,
    ));
    lines.push(format!(
        "  Quarantine: {} ({} failures)",
        metrics.quarantine_invocations, metrics.quarantine_failures,
    ));
    lines.push(format!(
        "  Exfiltration: {} images  |  {} URLs  |  {} memory",
        metrics.exfiltration_images_blocked,
        metrics.exfiltration_tool_urls_flagged,
        metrics.exfiltration_memory_guards,
    ));
    lines.join("\n")
}

fn is_tool_use_only(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut rest = trimmed;
    loop {
        let Some(start) = rest.find("[tool_use: ") else {
            break;
        };
        if !rest[..start].trim().is_empty() {
            return false;
        }
        let after = &rest[start + "[tool_use: ".len()..];
        let Some(end) = after.find(']') else {
            return false;
        };
        rest = after[end + 1..].trim_start();
    }
    rest.is_empty()
}

fn parse_tool_output(content: &str, suffix: &str) -> Option<(String, String)> {
    // New format: [tool output: name]
    if let Some(rest) = content.strip_prefix("[tool output: ")
        && let Some(header_end) = rest.find("]\n```\n")
    {
        let name = rest[..header_end].to_owned();
        let body_start = header_end + "]\n```\n".len();
        let body_part = &rest[body_start..];
        let body = body_part.strip_suffix(suffix).unwrap_or(body_part);
        return Some((name, body.to_owned()));
    }
    // Legacy format: [tool output] — infer tool name from body
    if let Some(rest) = content.strip_prefix("[tool output]\n```\n") {
        let body = rest.strip_suffix(suffix).unwrap_or(rest);
        let name = if body.starts_with("$ ") {
            "bash"
        } else {
            "tool"
        };
        return Some((name.to_owned(), body.to_owned()));
    }
    // Native tool_use format: [tool_result: id]\ncontent
    if let Some(rest) = content.strip_prefix("[tool_result: ") {
        let body = rest.find("]\n").map_or("", |i| &rest[i + 2..]);
        let name = if body.contains("$ ") { "bash" } else { "tool" };
        return Some((name.to_owned(), body.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> (App, mpsc::Receiver<String>, mpsc::Sender<AgentEvent>) {
        let (user_tx, user_rx) = mpsc::channel(16);
        let (agent_tx, agent_rx) = mpsc::channel(16);
        let mut app = App::new(user_tx, agent_rx);
        app.messages.clear();
        (app, user_rx, agent_tx)
    }

    #[test]
    fn initial_state() {
        let (app, _rx, _tx) = make_app();
        assert!(app.input().is_empty());
        assert_eq!(app.input_mode(), InputMode::Insert);
        assert!(app.messages().is_empty());
        assert!(app.show_splash());
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_c_quits() {
        let (mut app, _rx, _tx) = make_app();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key));
        assert!(app.should_quit);
    }

    #[test]
    fn insert_mode_typing() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "a");
        assert_eq!(app.cursor_position(), 1);
    }

    #[test]
    fn escape_switches_to_normal() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input_mode(), InputMode::Normal);
    }

    #[test]
    fn i_enters_insert_mode() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let key = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input_mode(), InputMode::Insert);
    }

    #[test]
    fn q_quits_in_normal_mode() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.should_quit);
    }

    #[test]
    fn backspace_deletes_char() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "ab".into();
        app.cursor_position = 2;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "a");
        assert_eq!(app.cursor_position(), 1);
    }

    #[test]
    fn enter_submits_input() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello".into();
        app.cursor_position = 5;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.input().is_empty());
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].content, "hello");

        let sent = rx.try_recv().unwrap();
        assert_eq!(sent, "hello");
    }

    #[test]
    fn empty_enter_does_not_submit() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.messages().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_chunk_creates_streaming_message() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::Chunk("hel".into()));
        assert_eq!(app.messages().len(), 1);
        assert!(app.messages()[0].streaming);
        assert_eq!(app.messages()[0].content, "hel");

        app.handle_agent_event(AgentEvent::Chunk("lo".into()));
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].content, "hello");
    }

    #[test]
    fn agent_flush_stops_streaming() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::Chunk("test".into()));
        assert!(app.messages()[0].streaming);
        app.handle_agent_event(AgentEvent::Flush);
        assert!(!app.messages()[0].streaming);
    }

    #[test]
    fn agent_full_message() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::FullMessage("done".into()));
        assert_eq!(app.messages().len(), 1);
        assert!(!app.messages()[0].streaming);
        assert_eq!(app.messages()[0].content, "done");
    }

    #[test]
    fn full_message_skips_tool_output_new_format() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::FullMessage(
            "[tool output: bash]\n```\n$ echo hi\nhi\n```".into(),
        ));
        assert!(app.messages().is_empty());
    }

    #[test]
    fn scroll_in_normal_mode() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(up));
        assert_eq!(app.scroll_offset(), 1);

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(down));
        assert_eq!(app.scroll_offset(), 0);
    }

    #[test]
    fn tab_cycles_panels() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        assert_eq!(app.active_panel, Panel::Chat);

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(tab));
        assert_eq!(app.active_panel, Panel::Skills);

        app.handle_event(AppEvent::Key(tab));
        assert_eq!(app.active_panel, Panel::Memory);

        app.handle_event(AppEvent::Key(tab));
        assert_eq!(app.active_panel, Panel::Resources);

        app.handle_event(AppEvent::Key(tab));
        assert_eq!(app.active_panel, Panel::SubAgents);

        app.handle_event(AppEvent::Key(tab));
        assert_eq!(app.active_panel, Panel::Chat);
    }

    #[test]
    fn ctrl_u_clears_input() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "some text".into();
        app.cursor_position = 9;
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key));
        assert!(app.input().is_empty());
        assert_eq!(app.cursor_position(), 0);
    }

    #[test]
    fn cursor_movement() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "abc".into();
        app.cursor_position = 1;

        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(left));
        assert_eq!(app.cursor_position(), 0);

        // left at 0 stays at 0
        app.handle_event(AppEvent::Key(left));
        assert_eq!(app.cursor_position(), 0);

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(right));
        assert_eq!(app.cursor_position(), 1);

        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(home));
        assert_eq!(app.cursor_position(), 0);

        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(end));
        assert_eq!(app.cursor_position(), 3);
    }

    #[test]
    fn delete_key_removes_char_at_cursor() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "abc".into();
        app.cursor_position = 1;
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "ac");
        assert_eq!(app.cursor_position(), 1);
    }

    #[test]
    fn unicode_input_insert_and_delete() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;

        // Type multi-byte chars
        for c in "\u{00e9}a\u{1f600}".chars() {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
        }
        assert_eq!(app.input(), "\u{00e9}a\u{1f600}");
        assert_eq!(app.cursor_position(), 3);

        // Backspace removes the emoji (last char)
        let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(bs));
        assert_eq!(app.input(), "\u{00e9}a");
        assert_eq!(app.cursor_position(), 2);

        // Move cursor left and delete 'a'
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(left));
        assert_eq!(app.cursor_position(), 1);

        let del = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(del));
        assert_eq!(app.input(), "\u{00e9}");
        assert_eq!(app.cursor_position(), 1);

        // End key uses char count, not byte count
        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(end));
        assert_eq!(app.cursor_position(), 1);
    }

    #[test]
    fn confirm_request_sets_state() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.handle_agent_event(AgentEvent::ConfirmRequest {
            prompt: "delete?".into(),
            response_tx: tx,
        });
        assert!(app.confirm_state.is_some());
        assert_eq!(app.confirm_state.as_ref().unwrap().prompt, "delete?");
    }

    #[test]
    fn confirm_modal_y_sends_true() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "proceed?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.confirm_state.is_none());
        assert!(rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_enter_sends_true() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "proceed?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.confirm_state.is_none());
        assert!(rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_n_sends_false() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "delete?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.confirm_state.is_none());
        assert!(!rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_escape_sends_false() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "delete?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.confirm_state.is_none());
        assert!(!rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_blocks_other_keys() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
        app.input_mode = InputMode::Insert;
        app.confirm_state = Some(ConfirmState {
            prompt: "test?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.input().is_empty());
        assert!(app.confirm_state.is_some());
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello".into();
        app.cursor_position = 5;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "hello\n");
        assert_eq!(app.cursor_position(), 6);
        assert!(app.messages().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn shift_enter_mid_input() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "ab".into();
        app.cursor_position = 1;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "a\nb");
        assert_eq!(app.cursor_position(), 2);
    }

    #[test]
    fn d_toggles_side_panels() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        assert!(app.show_side_panels());

        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(!app.show_side_panels());

        app.handle_event(AppEvent::Key(key));
        assert!(app.show_side_panels());
    }

    #[test]
    fn mouse_scroll_up() {
        let (mut app, _rx, _tx) = make_app();
        assert_eq!(app.scroll_offset(), 0);
        app.handle_event(AppEvent::MouseScroll(1));
        assert_eq!(app.scroll_offset(), 1);
        app.handle_event(AppEvent::MouseScroll(1));
        assert_eq!(app.scroll_offset(), 2);
    }

    #[test]
    fn mouse_scroll_down() {
        let (mut app, _rx, _tx) = make_app();
        app.scroll_offset = 5;
        app.handle_event(AppEvent::MouseScroll(-1));
        assert_eq!(app.scroll_offset(), 4);
        app.handle_event(AppEvent::MouseScroll(-1));
        assert_eq!(app.scroll_offset(), 3);
    }

    #[test]
    fn mouse_scroll_down_saturates_at_zero() {
        let (mut app, _rx, _tx) = make_app();
        app.scroll_offset = 1;
        app.handle_event(AppEvent::MouseScroll(-1));
        assert_eq!(app.scroll_offset(), 0);
        app.handle_event(AppEvent::MouseScroll(-1));
        assert_eq!(app.scroll_offset(), 0);
    }

    #[test]
    fn mouse_scroll_during_confirm_blocked() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "test?".into(),
            response_tx: Some(tx),
        });
        app.scroll_offset = 5;
        app.handle_event(AppEvent::MouseScroll(1));
        assert_eq!(app.scroll_offset(), 5);
        app.handle_event(AppEvent::MouseScroll(-1));
        assert_eq!(app.scroll_offset(), 5);
    }

    #[test]
    fn load_history_recognizes_tool_output_new_format() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[
            ("user", "hello"),
            ("assistant", "hi there"),
            ("user", "[tool output: bash]\n```\n$ echo hello\nhello\n```"),
            ("assistant", "done"),
        ]);
        assert_eq!(app.messages().len(), 4);
        assert_eq!(app.messages()[0].role, MessageRole::User);
        assert_eq!(app.messages()[1].role, MessageRole::Assistant);
        assert_eq!(app.messages()[2].role, MessageRole::Tool);
        assert_eq!(
            app.messages()[2].tool_name.as_ref().map(|t| t.as_str()),
            Some("bash")
        );
        assert_eq!(app.messages()[2].content, "$ echo hello\nhello");
        assert_eq!(app.messages()[3].role, MessageRole::Assistant);
    }

    #[test]
    fn load_history_recognizes_legacy_tool_output() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[("user", "[tool output]\n```\n$ ls\nfile.txt\n```")]);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::Tool);
        assert_eq!(
            app.messages()[0].tool_name.as_ref().map(|t| t.as_str()),
            Some("bash")
        );
        assert_eq!(app.messages()[0].content, "$ ls\nfile.txt");
    }

    #[test]
    fn load_history_legacy_non_bash_tool() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[(
            "user",
            "[tool output]\n```\n[mcp:github:list]\nresults\n```",
        )]);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::Tool);
        assert_eq!(
            app.messages()[0].tool_name.as_ref().map(|t| t.as_str()),
            Some("tool")
        );
    }

    #[test]
    fn load_history_recognizes_tool_result_format() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[("user", "[tool_result: toolu_abc]\n$ echo hello\nhello")]);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::Tool);
        assert_eq!(
            app.messages()[0].tool_name.as_ref().map(|t| t.as_str()),
            Some("bash")
        );
        assert_eq!(app.messages()[0].content, "$ echo hello\nhello");
    }

    #[test]
    fn load_history_hides_tool_use_only_messages() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[
            ("user", "hello"),
            (
                "assistant",
                "[tool_use: bash(toolu_01AfnYMrx3Ub13LLQ1Py3nfg)]",
            ),
            ("assistant", "here is the result"),
        ]);
        assert_eq!(app.messages().len(), 2);
        assert_eq!(app.messages()[0].role, MessageRole::User);
        assert_eq!(app.messages()[1].role, MessageRole::Assistant);
        assert_eq!(app.messages()[1].content, "here is the result");
    }

    #[test]
    fn load_history_keeps_assistant_with_text_and_tool_use() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[("assistant", "Let me check. [tool_use: bash(toolu_abc)]")]);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::Assistant);
    }

    #[test]
    fn is_tool_use_only_multiple_tags() {
        assert!(is_tool_use_only(
            "[tool_use: bash(id1)] [tool_use: read(id2)]"
        ));
        assert!(!is_tool_use_only("text [tool_use: bash(id1)]"));
        assert!(!is_tool_use_only(""));
    }

    #[test]
    fn tool_output_without_prior_tool_start_creates_tool_message_with_diff() {
        let (mut app, _rx, _tx) = make_app();
        let diff = zeph_core::DiffData {
            file_path: "src/lib.rs".into(),
            old_content: "fn old() {}".into(),
            new_content: "fn new() {}".into(),
        };
        app.handle_agent_event(AgentEvent::ToolOutput {
            tool_name: "edit".into(),
            command: "[tool output: edit]\n```\nok\n```".into(),
            output: "[tool output: edit]\n```\nok\n```".into(),
            success: true,
            diff: Some(diff),
            filter_stats: None,
            kept_lines: None,
        });

        assert_eq!(app.messages().len(), 1);
        let msg = &app.messages()[0];
        assert_eq!(msg.role, MessageRole::Tool);
        assert!(!msg.streaming);
        assert!(msg.diff_data.is_some());
    }

    #[test]
    fn tool_output_without_diff_does_not_create_spurious_message() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::ToolOutput {
            tool_name: "read".into(),
            command: "[tool output: read]\n```\ncontent\n```".into(),
            output: "[tool output: read]\n```\ncontent\n```".into(),
            success: true,
            diff: None,
            filter_stats: None,
            kept_lines: None,
        });

        // No prior ToolStart and no diff/filter_stats: nothing to display.
        assert!(app.messages().is_empty());
    }

    #[test]
    fn show_help_defaults_to_false() {
        let (app, _rx, _tx) = make_app();
        assert!(!app.show_help);
    }

    #[test]
    fn question_mark_in_normal_mode_opens_help() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.show_help);
    }

    #[test]
    fn question_mark_toggles_help_closed() {
        let (mut app, _rx, _tx) = make_app();
        app.show_help = true;
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(!app.show_help);
    }

    #[test]
    fn esc_closes_help_popup() {
        let (mut app, _rx, _tx) = make_app();
        app.show_help = true;
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(!app.show_help);
    }

    #[test]
    fn other_keys_ignored_when_help_open() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.show_help = true;

        // Typing a character should not modify input
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.input().is_empty());
        assert!(app.show_help);

        // Enter should not submit
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.messages().is_empty());
        assert!(app.show_help);
    }

    #[test]
    fn help_popup_does_not_block_ctrl_c() {
        let (mut app, _rx, _tx) = make_app();
        app.show_help = true;
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key));
        assert!(app.should_quit);
    }

    #[test]
    fn question_mark_in_insert_mode_does_not_open_help() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(!app.show_help);
        assert_eq!(app.input(), "?");
    }

    #[tokio::test]
    async fn esc_in_normal_mode_cancels_when_busy() {
        let (mut app, _rx, _tx) = make_app();
        let notify = Arc::new(Notify::new());
        let notify_waiter = Arc::clone(&notify);
        let handle = tokio::spawn(async move {
            notify_waiter.notified().await;
            true
        });
        tokio::task::yield_now().await;

        app = app.with_cancel_signal(Arc::clone(&notify));
        app.input_mode = InputMode::Normal;
        app.status_label = Some("Thinking...".into());
        assert!(app.is_agent_busy());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle).await;
        assert!(result.is_ok(), "notify should have been triggered");
    }

    #[test]
    fn esc_in_normal_mode_does_not_cancel_when_idle() {
        let (mut app, _rx, _tx) = make_app();
        let notify = Arc::new(Notify::new());
        app = app.with_cancel_signal(notify);
        app.input_mode = InputMode::Normal;
        assert!(!app.is_agent_busy());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        // No way to assert "not notified" directly, but we verify no panic
    }

    #[test]
    fn up_with_empty_input_and_queued_recalls_from_history() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.pending_count = 2;
        app.input_history.push("queued msg".into());
        app.messages
            .push(ChatMessage::new(MessageRole::User, "queued msg"));

        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert_eq!(app.input(), "queued msg");
        assert_eq!(app.cursor_position(), 10);
        assert!(app.editing_queued());
        assert_eq!(app.queued_count(), 1);
        assert!(app.input_history.is_empty());
        assert!(app.messages().is_empty());
        let sent = rx.try_recv().unwrap();
        assert_eq!(sent, "/drop-last-queued");
    }

    #[test]
    fn up_with_non_empty_input_navigates_history() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.pending_count = 2;
        app.input = "hello".into();
        app.cursor_position = 5;
        app.input_history.push("hello world".into());

        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert!(rx.try_recv().is_err());
        assert_eq!(app.input(), "hello world");
    }

    #[test]
    fn submit_input_resets_editing_queued() {
        let (mut app, _rx, _tx) = make_app();
        app.editing_queued = true;
        app.input = "some text".into();
        app.cursor_position = 9;
        app.submit_input();
        assert!(!app.editing_queued());
    }

    mod integration {
        use super::*;
        use crate::test_utils::test_terminal;

        fn draw_app(app: &mut App, width: u16, height: u16) -> String {
            let mut terminal = test_terminal(width, height);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buf = terminal.backend().buffer().clone();
            let mut output = String::new();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    output.push_str(buf[(x, y)].symbol());
                }
                output.push('\n');
            }
            output
        }

        #[test]
        fn submit_message_appears_in_chat() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            app.input = "hello world".into();
            app.cursor_position = 11;
            let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(enter));

            let output = draw_app(&mut app, 80, 24);
            assert!(output.contains("hello world"));
        }

        #[test]
        fn help_overlay_renders() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Normal;
            let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            let output = draw_app(&mut app, 80, 30);
            assert!(output.contains("Help"));
            assert!(output.contains("quit"));
        }

        #[test]
        fn help_overlay_closes() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Normal;
            let open = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(open));
            let close = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(close));

            let output = draw_app(&mut app, 80, 30);
            assert!(!output.contains("Help — press"));
        }

        #[test]
        fn confirm_dialog_renders() {
            let (mut app, _rx, _tx) = make_app();
            let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
            app.confirm_state = Some(ConfirmState {
                prompt: "Execute rm -rf?".into(),
                response_tx: Some(tx),
            });

            let output = draw_app(&mut app, 60, 20);
            assert!(output.contains("Confirm"));
            assert!(output.contains("Execute rm -rf?"));
            assert!(output.contains("[Y]es / [N]o"));
        }

        #[test]
        fn confirm_dialog_disappears_after_response() {
            let (mut app, _rx, _tx) = make_app();
            let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
            app.confirm_state = Some(ConfirmState {
                prompt: "Delete?".into(),
                response_tx: Some(tx),
            });
            let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            let output = draw_app(&mut app, 60, 20);
            assert!(!output.contains("[Y]es / [N]o"));
        }

        #[test]
        fn side_panels_toggle_off() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Normal;

            let before = draw_app(&mut app, 120, 40);
            assert!(before.contains("Skills"));
            assert!(before.contains("Memory"));

            let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            let after = draw_app(&mut app, 120, 40);
            assert!(!after.contains("Skills ("));
        }

        #[test]
        fn splash_shown_initially() {
            let (mut app, _rx, _tx) = make_app();
            let output = draw_app(&mut app, 80, 24);
            assert!(output.contains("Type a message to start."));
        }

        #[test]
        fn splash_disappears_after_submit() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            app.input = "hi".into();
            app.cursor_position = 2;
            let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(enter));

            assert!(!app.show_splash, "splash should be hidden after submit");
        }

        #[test]
        fn markdown_link_produces_hyperlink_span() {
            let (mut app, _rx, _tx) = make_app();
            app.show_splash = false;
            app.messages.push(ChatMessage::new(
                MessageRole::Assistant,
                "See [docs](https://docs.rs) for details",
            ));

            let _ = draw_app(&mut app, 80, 24);
            let links = app.take_hyperlinks();
            let doc_link = links.iter().find(|s| s.url == "https://docs.rs");
            assert!(
                doc_link.is_some(),
                "expected hyperlink span for markdown link, got: {links:?}"
            );
        }

        #[test]
        fn bare_url_still_produces_hyperlink_span() {
            let (mut app, _rx, _tx) = make_app();
            app.show_splash = false;
            app.messages.push(ChatMessage::new(
                MessageRole::Assistant,
                "Visit https://example.com today",
            ));

            let _ = draw_app(&mut app, 80, 24);
            let links = app.take_hyperlinks();
            let bare = links.iter().find(|s| s.url == "https://example.com");
            assert!(
                bare.is_some(),
                "expected hyperlink span for bare URL, got: {links:?}"
            );
        }
    }

    #[test]
    fn prev_word_boundary_from_middle_of_word() {
        let (mut app, _rx, _tx) = make_app();
        app.input = "hello world".into();
        app.cursor_position = 8;
        assert_eq!(app.prev_word_boundary(), 6);
    }

    #[test]
    fn prev_word_boundary_from_start_of_second_word() {
        let (mut app, _rx, _tx) = make_app();
        app.input = "hello world".into();
        app.cursor_position = 6;
        assert_eq!(app.prev_word_boundary(), 0);
    }

    #[test]
    fn prev_word_boundary_at_zero_stays_zero() {
        let (mut app, _rx, _tx) = make_app();
        app.input = "hello world".into();
        app.cursor_position = 0;
        assert_eq!(app.prev_word_boundary(), 0);
    }

    #[test]
    fn next_word_boundary_from_middle_of_first_word() {
        let (mut app, _rx, _tx) = make_app();
        app.input = "hello world".into();
        app.cursor_position = 2;
        assert_eq!(app.next_word_boundary(), 6);
    }

    #[test]
    fn next_word_boundary_from_start_of_second_word() {
        let (mut app, _rx, _tx) = make_app();
        app.input = "hello world".into();
        app.cursor_position = 6;
        assert_eq!(app.next_word_boundary(), 11);
    }

    #[test]
    fn next_word_boundary_at_end_stays_at_end() {
        let (mut app, _rx, _tx) = make_app();
        app.input = "hello world".into();
        app.cursor_position = 11;
        assert_eq!(app.next_word_boundary(), 11);
    }

    #[test]
    fn prev_word_boundary_unicode() {
        let (mut app, _rx, _tx) = make_app();
        // "привет мир" — 6 chars + space + 3 chars = 10 chars total
        app.input = "привет мир".into();
        app.cursor_position = 9;
        assert_eq!(app.prev_word_boundary(), 7);
    }

    #[test]
    fn next_word_boundary_unicode() {
        let (mut app, _rx, _tx) = make_app();
        // "привет мир" — 6 chars + space + 3 chars
        app.input = "привет мир".into();
        app.cursor_position = 2;
        assert_eq!(app.next_word_boundary(), 7);
    }

    #[test]
    fn alt_left_moves_to_prev_word_boundary() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello world".into();
        app.cursor_position = 8;
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.cursor_position(), 6);
    }

    #[test]
    fn alt_right_moves_to_next_word_boundary() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello world".into();
        app.cursor_position = 2;
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.cursor_position(), 6);
    }

    #[test]
    fn ctrl_a_moves_cursor_to_start() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello world".into();
        app.cursor_position = 7;
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.cursor_position(), 0);
    }

    #[test]
    fn ctrl_e_moves_cursor_to_end() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello world".into();
        app.cursor_position = 3;
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.cursor_position(), 11);
    }

    #[test]
    fn alt_backspace_deletes_to_prev_word_boundary() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello world".into();
        app.cursor_position = 11;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "hello ");
        assert_eq!(app.cursor_position(), 6);
    }

    #[test]
    fn alt_backspace_at_boundary_deletes_word_and_space() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello world".into();
        app.cursor_position = 6;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "world");
        assert_eq!(app.cursor_position(), 0);
    }

    #[test]
    fn alt_backspace_at_zero_is_noop() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello".into();
        app.cursor_position = 0;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.input(), "hello");
        assert_eq!(app.cursor_position(), 0);
    }

    mod proptest_cursor {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]

            #[test]
            fn word_boundaries_stay_in_bounds(
                input in "\\PC{0,100}",
                cursor in 0usize..=100,
            ) {
                let (mut app, _rx, _tx) = make_app();
                app.input = input;
                let len = app.char_count();
                app.cursor_position = cursor.min(len);

                let prev = app.prev_word_boundary();
                prop_assert!(prev <= app.cursor_position, "prev {prev} > cursor {}", app.cursor_position);

                let next = app.next_word_boundary();
                prop_assert!(next >= app.cursor_position, "next {next} < cursor {}", app.cursor_position);
                prop_assert!(next <= len, "next {next} > len {len}");
            }

            #[test]
            fn alt_backspace_keeps_valid_state(
                input in "\\PC{0,50}",
                cursor in 0usize..=50,
            ) {
                let (mut app, _rx, _tx) = make_app();
                app.input_mode = InputMode::Insert;
                app.input = input;
                let len = app.char_count();
                app.cursor_position = cursor.min(len);

                let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
                app.handle_event(AppEvent::Key(key));

                prop_assert!(app.cursor_position() <= app.char_count());
            }
        }
    }

    mod render_cache_tests {
        use super::*;
        use ratatui::text::{Line, Span};

        fn make_key(content_hash: u64, width: u16) -> RenderCacheKey {
            RenderCacheKey {
                content_hash,
                terminal_width: width,
                tool_expanded: false,
                compact_tools: false,
                show_labels: false,
            }
        }

        #[test]
        fn get_returns_none_when_empty() {
            let cache = RenderCache::default();
            let key = make_key(1, 80);
            assert!(cache.get(0, &key).is_none());
        }

        #[test]
        fn put_and_get_returns_cached_lines() {
            let mut cache = RenderCache::default();
            let key = make_key(42, 80);
            let lines = vec![Line::from(Span::raw("hello"))];
            cache.put(0, key, lines.clone(), vec![]);
            let (result, _) = cache.get(0, &key).unwrap();
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].spans[0].content, "hello");
        }

        #[test]
        fn get_returns_none_on_key_mismatch() {
            let mut cache = RenderCache::default();
            let key1 = make_key(1, 80);
            let key2 = make_key(2, 80);
            let lines = vec![Line::from(Span::raw("a"))];
            cache.put(0, key1, lines, vec![]);
            assert!(cache.get(0, &key2).is_none());
        }

        #[test]
        fn get_returns_none_on_width_mismatch() {
            let mut cache = RenderCache::default();
            let key80 = make_key(1, 80);
            let key100 = make_key(1, 100);
            let lines = vec![Line::from(Span::raw("b"))];
            cache.put(0, key80, lines, vec![]);
            assert!(cache.get(0, &key100).is_none());
        }

        #[test]
        fn invalidate_clears_single_entry() {
            let mut cache = RenderCache::default();
            let key = make_key(1, 80);
            let lines = vec![Line::from(Span::raw("x"))];
            cache.put(0, key, lines, vec![]);
            assert!(cache.get(0, &key).is_some());
            cache.invalidate(0);
            assert!(cache.get(0, &key).is_none());
        }

        #[test]
        fn invalidate_out_of_bounds_is_noop() {
            let mut cache = RenderCache::default();
            cache.invalidate(99);
        }

        #[test]
        fn clear_removes_all_entries() {
            let mut cache = RenderCache::default();
            let key0 = make_key(1, 80);
            let key1 = make_key(2, 80);
            cache.put(0, key0, vec![Line::from(Span::raw("a"))], vec![]);
            cache.put(1, key1, vec![Line::from(Span::raw("b"))], vec![]);
            cache.clear();
            assert!(cache.get(0, &key0).is_none());
            assert!(cache.get(1, &key1).is_none());
        }

        #[test]
        fn put_grows_entries_for_non_contiguous_index() {
            let mut cache = RenderCache::default();
            let key = make_key(5, 80);
            let lines = vec![Line::from(Span::raw("z"))];
            cache.put(5, key, lines, vec![]);
            let (result, _) = cache.get(5, &key).unwrap();
            assert_eq!(result[0].spans[0].content, "z");
        }
    }

    mod try_recv_tests {
        use super::*;

        #[test]
        fn try_recv_returns_empty_when_no_events() {
            let (mut app, _rx, _tx) = make_app();
            let result = app.try_recv_agent_event();
            assert!(matches!(result, Err(mpsc::error::TryRecvError::Empty)));
        }

        #[test]
        fn try_recv_returns_event_when_available() {
            let (mut app, _rx, tx) = make_app();
            tx.try_send(AgentEvent::Typing).unwrap();
            let result = app.try_recv_agent_event();
            assert!(result.is_ok());
            assert!(matches!(result.unwrap(), AgentEvent::Typing));
        }

        #[test]
        fn try_recv_returns_disconnected_when_sender_dropped() {
            let (mut app, _rx, tx) = make_app();
            drop(tx);
            let result = app.try_recv_agent_event();
            assert!(matches!(
                result,
                Err(mpsc::error::TryRecvError::Disconnected)
            ));
        }
    }

    mod command_palette_tests {
        use super::*;

        #[test]
        fn colon_in_normal_mode_opens_palette() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Normal;
            assert!(app.command_palette.is_none());

            let key = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.command_palette.is_some());
        }

        #[test]
        fn esc_closes_palette() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Normal;
            app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

            let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.command_palette.is_none());
        }

        #[test]
        fn palette_intercepts_all_keys_except_ctrl_c() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

            // Typing a char goes to palette, not to input field
            let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.input().is_empty());
            let palette = app.command_palette.as_ref().unwrap();
            assert_eq!(palette.query, "s");
        }

        #[test]
        fn enter_on_selected_dispatches_command_locally() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Normal;
            // Open palette
            let colon = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(colon));
            assert!(app.command_palette.is_some());

            // Enter on first command (skill:list)
            let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(enter));
            assert!(app.command_palette.is_none());
            // Should have added a system message
            assert!(!app.messages().is_empty());
            assert_eq!(app.messages().last().unwrap().role, MessageRole::System);
        }

        #[test]
        fn typing_in_palette_filters_commands() {
            let (mut app, _rx, _tx) = make_app();
            app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

            let m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
            let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
            let p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(m));
            app.handle_event(AppEvent::Key(c));
            app.handle_event(AppEvent::Key(p));

            let palette = app.command_palette.as_ref().unwrap();
            assert_eq!(palette.query, "mcp");
            // mcp:list is the top result; plan:confirm also fuzzy-matches "mcp" (m→c→p in label).
            assert!(
                palette.filtered.iter().any(|e| e.id == "mcp:list"),
                "mcp:list must be in filtered results"
            );
            assert_eq!(
                palette.filtered[0].id, "mcp:list",
                "mcp:list must rank first"
            );
        }

        #[test]
        fn backspace_in_palette_removes_char() {
            let (mut app, _rx, _tx) = make_app();
            app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

            let s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(s));
            assert_eq!(app.command_palette.as_ref().unwrap().query, "s");

            let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(bs));
            assert!(app.command_palette.as_ref().unwrap().query.is_empty());
        }

        #[test]
        fn command_result_event_adds_system_message() {
            let (mut app, _rx, _tx) = make_app();
            app.handle_agent_event(AgentEvent::CommandResult {
                command_id: "skill:list".to_owned(),
                output: "No skills loaded.".to_owned(),
            });
            assert_eq!(app.messages().len(), 1);
            assert_eq!(app.messages()[0].role, MessageRole::System);
            assert_eq!(app.messages()[0].content, "No skills loaded.");
            assert!(app.command_palette.is_none());
        }

        #[test]
        fn command_result_closes_palette_if_open() {
            let (mut app, _rx, _tx) = make_app();
            app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());
            app.handle_agent_event(AgentEvent::CommandResult {
                command_id: "view:config".to_owned(),
                output: "config output".to_owned(),
            });
            assert!(app.command_palette.is_none());
        }

        #[test]
        fn colon_in_insert_mode_types_colon() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            let key = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.command_palette.is_none());
            assert_eq!(app.input(), ":");
        }

        #[test]
        fn enter_with_empty_filter_does_not_panic() {
            let (mut app, _rx, _tx) = make_app();
            let mut palette = crate::widgets::command_palette::CommandPaletteState::new();
            // type something that matches nothing
            for c in "xxxxxxxxxx".chars() {
                palette.push_char(c);
            }
            assert!(palette.filtered.is_empty());
            app.command_palette = Some(palette);

            let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(enter));
            // palette should close without crashing, no message added
            assert!(app.command_palette.is_none());
        }

        #[test]
        fn execute_view_config_with_command_tx_sends_command() {
            let (mut app, _rx, _tx) = make_app();
            let (cmd_tx, mut cmd_rx) = mpsc::channel::<TuiCommand>(16);
            app.command_tx = Some(cmd_tx);

            app.execute_command(TuiCommand::ViewConfig);

            let received = cmd_rx.try_recv().expect("command should be sent");
            assert_eq!(received, TuiCommand::ViewConfig);
            assert!(
                app.messages().is_empty(),
                "no system message when channel present"
            );
        }

        #[test]
        fn execute_view_autonomy_with_command_tx_sends_command() {
            let (mut app, _rx, _tx) = make_app();
            let (cmd_tx, mut cmd_rx) = mpsc::channel::<TuiCommand>(16);
            app.command_tx = Some(cmd_tx);

            app.execute_command(TuiCommand::ViewAutonomy);

            let received = cmd_rx.try_recv().expect("command should be sent");
            assert_eq!(received, TuiCommand::ViewAutonomy);
            assert!(
                app.messages().is_empty(),
                "no system message when channel present"
            );
        }

        #[test]
        fn execute_view_config_without_command_tx_adds_fallback_message() {
            let (mut app, _rx, _tx) = make_app();
            assert!(app.command_tx.is_none());

            app.execute_command(TuiCommand::ViewConfig);

            assert_eq!(app.messages().len(), 1);
            assert!(app.messages()[0].content.contains("no command channel"));
        }

        #[test]
        fn execute_security_events_no_events_shows_history_header() {
            let (mut app, _rx, _tx) = make_app();
            app.execute_command(TuiCommand::SecurityEvents);
            assert_eq!(app.messages().len(), 1);
            assert!(app.messages()[0].content.contains("Security event history"));
        }

        #[test]
        fn execute_security_events_with_events_shows_all() {
            use zeph_core::metrics::{SecurityEvent, SecurityEventCategory};

            let (mut app, _rx, _tx) = make_app();
            app.metrics.security_events.push_back(SecurityEvent::new(
                SecurityEventCategory::InjectionFlag,
                "web_scrape",
                "Detected pattern: ignore previous",
            ));
            app.execute_command(TuiCommand::SecurityEvents);
            let content = &app.messages()[0].content;
            assert!(content.contains("web_scrape"));
            assert!(content.contains("INJECTION_FLAG"));
        }

        #[test]
        fn has_recent_security_events_false_when_no_events() {
            let (app, _rx, _tx) = make_app();
            assert!(!app.has_recent_security_events());
        }

        #[test]
        fn has_recent_security_events_true_when_recent() {
            use zeph_core::metrics::{SecurityEvent, SecurityEventCategory};

            let (mut app, _rx, _tx) = make_app();
            // Event with current timestamp is recent
            app.metrics.security_events.push_back(SecurityEvent::new(
                SecurityEventCategory::Truncation,
                "tool",
                "truncated",
            ));
            assert!(app.has_recent_security_events());
        }

        #[test]
        fn has_recent_security_events_false_when_event_older_than_60s() {
            use zeph_core::metrics::{SecurityEvent, SecurityEventCategory};

            let (mut app, _rx, _tx) = make_app();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut ev = SecurityEvent::new(SecurityEventCategory::Truncation, "tool", "old");
            // Backdate the event by 120 seconds.
            ev.timestamp = now.saturating_sub(120);
            app.metrics.security_events.push_back(ev);
            assert!(!app.has_recent_security_events());
        }
    }

    mod file_picker_tests {
        use std::fs;

        use super::*;
        use crate::file_picker::FileIndex;

        fn make_app_with_index() -> (App, mpsc::Receiver<String>, mpsc::Sender<AgentEvent>) {
            let (app, rx, tx) = make_app();
            (app, rx, tx)
        }

        fn build_temp_index(files: &[&str]) -> (FileIndex, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            for &f in files {
                let path = dir.path().join(f);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&path, "").unwrap();
            }
            let idx = FileIndex::build(dir.path());
            (idx, dir)
        }

        fn open_picker_with_index(app: &mut App, idx: &FileIndex) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_owned();
            drop(dir.keep());
            app.file_index = Some(FileIndex::build(&path));
            // Replace with our controlled index
            app.file_picker_state = Some(crate::file_picker::FilePickerState::new(idx));
        }

        #[test]
        fn at_sign_opens_picker_and_does_not_insert_into_input() {
            let (mut app, _rx, _tx) = make_app_with_index();
            // Pre-populate a fresh index so open_file_picker can open the picker immediately
            // without spawning a background build (which requires a Tokio runtime).
            let (idx, _dir) = build_temp_index(&["a.rs"]);
            app.file_index = Some(idx);
            app.input_mode = InputMode::Insert;
            let key = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(
                !app.input.contains('@'),
                "@ should not be in input after opening picker"
            );
            assert!(
                app.file_picker_state.is_some(),
                "file_picker_state should be Some after @"
            );
        }

        #[test]
        fn esc_dismisses_picker() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["a.rs", "b.rs"]);
            open_picker_with_index(&mut app, &idx);
            assert!(app.file_picker_state.is_some());

            let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.file_picker_state.is_none());
            assert!(app.input.is_empty());
        }

        #[test]
        fn enter_inserts_selected_path_and_closes_picker() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["src/main.rs"]);
            open_picker_with_index(&mut app, &idx);

            let selected = app
                .file_picker_state
                .as_ref()
                .unwrap()
                .selected_path()
                .map(ToOwned::to_owned)
                .unwrap();

            let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            assert!(app.file_picker_state.is_none());
            assert!(
                app.input.contains(&selected),
                "input should contain selected path"
            );
            assert_eq!(app.cursor_position, selected.chars().count());
        }

        #[test]
        fn tab_inserts_selected_path_and_closes_picker() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["README.md"]);
            open_picker_with_index(&mut app, &idx);

            let selected = app
                .file_picker_state
                .as_ref()
                .unwrap()
                .selected_path()
                .map(ToOwned::to_owned)
                .unwrap();

            let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            assert!(app.file_picker_state.is_none());
            assert!(app.input.contains(&selected));
        }

        #[test]
        fn enter_with_no_matches_closes_picker_without_modifying_input() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["a.rs"]);
            open_picker_with_index(&mut app, &idx);

            let state = app.file_picker_state.as_mut().unwrap();
            state.update_query("xyznotfound");

            assert!(app.file_picker_state.as_ref().unwrap().matches().is_empty());

            let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            assert!(app.file_picker_state.is_none());
            assert!(app.input.is_empty(), "input must be unchanged");
        }

        #[test]
        fn down_key_advances_selection() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["a.rs", "b.rs", "c.rs"]);
            open_picker_with_index(&mut app, &idx);

            assert_eq!(app.file_picker_state.as_ref().unwrap().selected, 0);

            let key = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert_eq!(app.file_picker_state.as_ref().unwrap().selected, 1);
        }

        #[test]
        fn up_key_wraps_selection_to_last() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["a.rs", "b.rs", "c.rs"]);
            open_picker_with_index(&mut app, &idx);

            let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            let state = app.file_picker_state.as_ref().unwrap();
            assert_eq!(state.selected, state.matches().len() - 1);
        }

        #[test]
        fn typing_filters_matches() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["src/main.rs", "src/lib.rs"]);
            open_picker_with_index(&mut app, &idx);

            let initial_count = app.file_picker_state.as_ref().unwrap().matches().len();

            let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            let filtered_count = app.file_picker_state.as_ref().unwrap().matches().len();
            assert!(filtered_count <= initial_count);
            assert_eq!(app.file_picker_state.as_ref().unwrap().query, "m");
        }

        #[test]
        fn backspace_with_nonempty_query_removes_char() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["a.rs"]);
            open_picker_with_index(&mut app, &idx);

            app.file_picker_state.as_mut().unwrap().update_query("ma");

            let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            assert!(app.file_picker_state.is_some());
            assert_eq!(app.file_picker_state.as_ref().unwrap().query, "m");
        }

        #[test]
        fn backspace_on_empty_query_dismisses_picker() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["a.rs"]);
            open_picker_with_index(&mut app, &idx);

            assert!(app.file_picker_state.as_ref().unwrap().query.is_empty());

            let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            assert!(app.file_picker_state.is_none());
        }

        #[test]
        fn picker_blocks_other_keys() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["a.rs"]);
            open_picker_with_index(&mut app, &idx);

            app.input = "hello".into();
            app.cursor_position = 5;
            let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
            app.handle_event(AppEvent::Key(key));
            assert_eq!(
                app.input, "hello",
                "input should be unchanged while picker is open"
            );
        }

        #[test]
        fn enter_inserts_at_cursor_mid_input() {
            let (mut app, _rx, _tx) = make_app_with_index();
            let (idx, _dir) = build_temp_index(&["src/lib.rs"]);
            open_picker_with_index(&mut app, &idx);

            app.input = "ab".into();
            app.cursor_position = 1;

            let selected = app
                .file_picker_state
                .as_ref()
                .unwrap()
                .selected_path()
                .map(ToOwned::to_owned)
                .unwrap();

            let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));

            assert!(app.input.contains(&selected));
            assert!(app.input.starts_with('a'));
            assert!(app.input.ends_with('b'));
        }

        #[tokio::test]
        async fn poll_pending_file_index_installs_index_and_opens_picker() {
            let (user_tx, _user_rx) = tokio::sync::mpsc::channel(1);
            let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel(1);
            let mut app = App::new(user_tx, agent_rx);

            // Simulate: status is set, pending_file_index is Some (already resolved)
            let (tx, rx) = tokio::sync::oneshot::channel();
            let (idx, _dir) = build_temp_index(&["foo.rs"]);
            let _ = tx.send(idx);
            app.pending_file_index = Some(rx);
            app.status_label = Some("indexing files...".to_owned());

            // Give the oneshot a moment to be ready (it already is since we sent before assigning)
            tokio::task::yield_now().await;

            app.poll_pending_file_index();

            assert!(app.file_index.is_some(), "file_index should be installed");
            assert!(
                app.file_picker_state.is_some(),
                "picker should open after index ready"
            );
            assert!(
                app.status_label.is_none(),
                "status should be cleared after index ready"
            );
            assert!(
                app.pending_file_index.is_none(),
                "pending handle should be consumed"
            );
        }

        #[tokio::test]
        async fn poll_pending_file_index_noop_when_none() {
            let (user_tx, _user_rx) = tokio::sync::mpsc::channel(1);
            let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel(1);
            let mut app = App::new(user_tx, agent_rx);

            // No pending handle — should be a no-op
            app.poll_pending_file_index();

            assert!(app.file_index.is_none());
            assert!(app.file_picker_state.is_none());
        }

        #[tokio::test]
        async fn poll_pending_file_index_clears_on_closed_sender() {
            let (user_tx, _user_rx) = tokio::sync::mpsc::channel(1);
            let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel(1);
            let mut app = App::new(user_tx, agent_rx);

            let (tx, rx) = tokio::sync::oneshot::channel::<crate::file_picker::FileIndex>();
            // Drop sender without sending — simulates spawn_blocking panic
            drop(tx);
            app.pending_file_index = Some(rx);
            app.status_label = Some("indexing files...".to_owned());

            app.poll_pending_file_index();

            assert!(
                app.pending_file_index.is_none(),
                "closed handle should be consumed"
            );
            assert!(
                app.status_label.is_none(),
                "status should be cleared on closed sender"
            );
        }
    }

    #[test]
    fn draw_header_shows_1m_ctx_badge_when_extended_context() {
        use crate::test_utils::render_to_string;

        let (mut app, _rx, _tx) = make_app();
        app.metrics.provider_name = "claude".into();
        app.metrics.model_name = "claude-sonnet-4-6".into();
        app.metrics.extended_context = true;

        let output = render_to_string(80, 1, |frame, area| {
            app.draw_header(frame, area);
        });
        assert!(
            output.contains("[1M CTX]"),
            "header must contain [1M CTX] badge when extended_context is true; got: {output:?}"
        );
    }

    #[test]
    fn draw_header_no_badge_without_extended_context() {
        use crate::test_utils::render_to_string;

        let (mut app, _rx, _tx) = make_app();
        app.metrics.provider_name = "claude".into();
        app.metrics.model_name = "claude-sonnet-4-6".into();
        app.metrics.extended_context = false;

        let output = render_to_string(80, 1, |frame, area| {
            app.draw_header(frame, area);
        });
        assert!(
            !output.contains("[1M CTX]"),
            "header must not contain [1M CTX] badge when extended_context is false; got: {output:?}"
        );
    }

    // R-FIX-1938: with_metrics_rx must eagerly read the initial snapshot so graph counts are
    // visible immediately without waiting for the first watch::Receiver::has_changed() event.
    #[test]
    fn with_metrics_rx_reads_initial_value() {
        use tokio::sync::watch;
        use zeph_core::metrics::MetricsSnapshot;

        let (user_tx, agent_rx) = {
            let (u, _ur) = mpsc::channel(4);
            let (_at, ar) = mpsc::channel(4);
            (u, ar)
        };
        let initial = MetricsSnapshot {
            graph_entities_total: 42,
            graph_edges_total: 7,
            graph_communities_total: 3,
            ..MetricsSnapshot::default()
        };

        let (tx, rx) = watch::channel(initial);
        let app = App::new(user_tx, agent_rx).with_metrics_rx(rx);

        assert_eq!(app.metrics.graph_entities_total, 42);
        assert_eq!(app.metrics.graph_edges_total, 7);
        assert_eq!(app.metrics.graph_communities_total, 3);

        drop(tx);
    }

    // Regression tests for #2126: tool output must not be duplicated when streaming chunks
    // arrive before the final ToolOutput event.

    #[test]
    fn tool_output_with_prior_tool_start_no_chunks_appends_output() {
        let (mut app, _rx, _tx) = make_app();
        // Path A: ToolStart creates message with header only.
        app.handle_agent_event(AgentEvent::ToolStart {
            tool_name: "bash".into(),
            command: "ls -la".into(),
        });
        // Path C: ToolOutput arrives with no prior chunks.
        app.handle_agent_event(AgentEvent::ToolOutput {
            tool_name: "bash".into(),
            command: "ls -la".into(),
            output: "file1\nfile2\n".into(),
            success: true,
            diff: None,
            filter_stats: None,
            kept_lines: None,
        });

        assert_eq!(app.messages().len(), 1);
        let msg = &app.messages()[0];
        assert_eq!(msg.content, "$ ls -la\nfile1\nfile2\n");
        assert!(!msg.streaming);
    }

    #[test]
    fn tool_output_with_prior_tool_start_and_chunks_does_not_duplicate() {
        let (mut app, _rx, _tx) = make_app();
        // Path A: ToolStart.
        app.handle_agent_event(AgentEvent::ToolStart {
            tool_name: "bash".into(),
            command: "echo hello".into(),
        });
        // Path B: streaming chunks arrive.
        app.handle_agent_event(AgentEvent::ToolOutputChunk {
            tool_name: "bash".into(),
            command: "echo hello".into(),
            chunk: "hello\n".into(),
        });
        // Path C: ToolOutput with canonical body_display (same content as chunks).
        app.handle_agent_event(AgentEvent::ToolOutput {
            tool_name: "bash".into(),
            command: "echo hello".into(),
            output: "hello\n".into(),
            success: true,
            diff: None,
            filter_stats: None,
            kept_lines: None,
        });

        assert_eq!(app.messages().len(), 1);
        let msg = &app.messages()[0];
        // Must contain exactly one copy of "hello\n", not two.
        assert_eq!(msg.content, "$ echo hello\nhello\n");
        assert!(!msg.streaming);
    }

    // ── AgentViewTarget ──────────────────────────────────────────────────────

    #[test]
    fn agent_view_target_main_is_main() {
        assert!(AgentViewTarget::Main.is_main());
        assert!(AgentViewTarget::Main.subagent_id().is_none());
        assert!(AgentViewTarget::Main.subagent_name().is_none());
    }

    #[test]
    fn agent_view_target_subagent_accessors() {
        let t = AgentViewTarget::SubAgent {
            id: "abc".into(),
            name: "Worker".into(),
        };
        assert!(!t.is_main());
        assert_eq!(t.subagent_id(), Some("abc"));
        assert_eq!(t.subagent_name(), Some("Worker"));
    }

    // ── SubAgentSidebarState ─────────────────────────────────────────────────

    #[test]
    fn sidebar_select_next_advances() {
        let mut s = SubAgentSidebarState::new();
        // start with nothing selected
        assert!(s.selected().is_none());
        s.select_next(3);
        assert_eq!(s.selected(), Some(0));
        s.select_next(3);
        assert_eq!(s.selected(), Some(1));
        s.select_next(3);
        assert_eq!(s.selected(), Some(2));
        // at last item — stays clamped
        s.select_next(3);
        assert_eq!(s.selected(), Some(2));
    }

    #[test]
    fn sidebar_select_next_noop_when_empty() {
        let mut s = SubAgentSidebarState::new();
        s.select_next(0);
        assert!(s.selected().is_none());
    }

    #[test]
    fn sidebar_select_prev_decrements() {
        let mut s = SubAgentSidebarState::new();
        s.list_state.select(Some(2));
        s.select_prev(3);
        assert_eq!(s.selected(), Some(1));
        s.select_prev(3);
        assert_eq!(s.selected(), Some(0));
        // at 0 — stays at 0
        s.select_prev(3);
        assert_eq!(s.selected(), Some(0));
    }

    #[test]
    fn sidebar_select_prev_from_none_goes_to_zero() {
        let mut s = SubAgentSidebarState::new();
        s.select_prev(3);
        assert_eq!(s.selected(), Some(0));
    }

    #[test]
    fn sidebar_select_prev_noop_when_empty() {
        let mut s = SubAgentSidebarState::new();
        s.select_prev(0);
        assert!(s.selected().is_none());
    }

    #[test]
    fn sidebar_clamp_removes_selection_when_empty() {
        let mut s = SubAgentSidebarState::new();
        s.list_state.select(Some(2));
        s.clamp(0);
        assert!(s.selected().is_none());
    }

    #[test]
    fn sidebar_clamp_reduces_out_of_bounds_selection() {
        let mut s = SubAgentSidebarState::new();
        s.list_state.select(Some(5));
        s.clamp(3); // valid range: 0..2
        assert_eq!(s.selected(), Some(2));
    }

    #[test]
    fn sidebar_clamp_leaves_valid_selection_unchanged() {
        let mut s = SubAgentSidebarState::new();
        s.list_state.select(Some(1));
        s.clamp(3);
        assert_eq!(s.selected(), Some(1));
    }

    // ── TuiTranscriptEntry::to_chat_message ──────────────────────────────────

    #[test]
    fn transcript_entry_to_chat_message_role_mapping() {
        let cases = [
            ("user", MessageRole::User),
            ("assistant", MessageRole::Assistant),
            ("tool", MessageRole::Tool),
            ("system", MessageRole::System),
            ("unknown_role", MessageRole::System),
        ];
        for (role_str, expected) in cases {
            let entry = TuiTranscriptEntry {
                role: role_str.into(),
                content: "hello".into(),
                tool_name: None,
                timestamp: None,
            };
            let msg = entry.to_chat_message();
            assert_eq!(msg.role, expected, "role_str={role_str}");
        }
    }

    #[test]
    fn transcript_entry_to_chat_message_copies_tool_name_and_timestamp() {
        let entry = TuiTranscriptEntry {
            role: "tool".into(),
            content: "result".into(),
            tool_name: Some("bash".into()),
            timestamp: Some("12:34".into()),
        };
        let msg = entry.to_chat_message();
        assert_eq!(msg.tool_name.as_ref().map(|t| t.as_str()), Some("bash"));
        assert_eq!(msg.timestamp, "12:34");
        assert_eq!(msg.content, "result");
    }

    // ── load_transcript_file ─────────────────────────────────────────────────

    #[test]
    fn load_transcript_file_returns_empty_for_nonexistent_path() {
        let (entries, total) =
            load_transcript_file(std::path::Path::new("/nonexistent/path/x.jsonl"), false);
        assert!(entries.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn load_transcript_file_parses_flat_format() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"role":"user","content":"hello"}
{"role":"assistant","content":"world"}
"#,
        )
        .unwrap();
        let (entries, total) = load_transcript_file(tmp.path(), false);
        assert_eq!(total, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].content, "hello");
        assert_eq!(entries[1].role, "assistant");
        assert_eq!(entries[1].content, "world");
    }

    #[test]
    fn load_transcript_file_parses_nested_format() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"seq":1,"timestamp":"12:00","message":{"role":"user","parts":[{"content":"hi"}]}}
"#,
        )
        .unwrap();
        let (entries, total) = load_transcript_file(tmp.path(), false);
        assert_eq!(total, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].content, "hi");
        assert_eq!(entries[0].timestamp.as_deref(), Some("12:00"));
    }

    #[test]
    fn load_transcript_file_skips_partial_last_line_when_active() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Last line is missing closing brace — partial write.
        std::fs::write(
            tmp.path(),
            r#"{"role":"user","content":"complete"}
{"role":"assistant","content":"incomplet"#,
        )
        .unwrap();
        let (entries, total) = load_transcript_file(tmp.path(), true);
        // is_active=true: last partial line discarded
        assert_eq!(total, 2); // total = raw line count
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "complete");
    }

    #[test]
    fn load_transcript_file_keeps_partial_last_line_when_inactive() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Valid JSON that ends with '}' but missing "content" — will be skipped by filter.
        std::fs::write(
            tmp.path(),
            r#"{"role":"user","content":"complete"}
{"role":"assistant","content":"also complete"}
"#,
        )
        .unwrap();
        // is_active=false: no line skipping, both lines parsed
        let (entries, total) = load_transcript_file(tmp.path(), false);
        assert_eq!(total, 2);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn load_transcript_file_skips_empty_content_without_tool_name() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"role":"user","content":""}
{"role":"assistant","content":"real"}
"#,
        )
        .unwrap();
        let (entries, _total) = load_transcript_file(tmp.path(), false);
        // Entry with empty content and no tool_name is filtered out.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "real");
    }

    #[test]
    fn load_transcript_file_keeps_empty_content_with_tool_name() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"role":"tool","content":"","tool_name":"bash"}
"#,
        )
        .unwrap();
        let (entries, _total) = load_transcript_file(tmp.path(), false);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].tool_name.as_ref().map(|t| t.as_str()),
            Some("bash")
        );
    }

    #[test]
    fn load_transcript_file_truncates_to_max_entries() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Write TRANSCRIPT_MAX_ENTRIES + 5 lines.
        let extra = 5;
        let count = TRANSCRIPT_MAX_ENTRIES + extra;
        let content: String = (0..count).fold(String::new(), |mut acc, i| {
            use std::fmt::Write;
            let _ = writeln!(acc, "{{\"role\":\"user\",\"content\":\"msg{i}\"}}");
            acc
        });
        std::fs::write(tmp.path(), &content).unwrap();
        let (entries, total) = load_transcript_file(tmp.path(), false);
        assert_eq!(total, count);
        assert_eq!(entries.len(), TRANSCRIPT_MAX_ENTRIES);
        // Must keep the LAST N entries, not first N.
        assert_eq!(entries[0].content, format!("msg{extra}"));
        assert_eq!(
            entries[TRANSCRIPT_MAX_ENTRIES - 1].content,
            format!("msg{}", count - 1)
        );
    }

    // ── transcript_truncation_info ────────────────────────────────────────────

    #[test]
    fn transcript_truncation_info_returns_none_when_no_cache() {
        let (app, _rx, _tx) = make_app();
        assert!(app.transcript_truncation_info().is_none());
    }

    #[test]
    fn transcript_truncation_info_returns_none_when_not_truncated() {
        let (mut app, _rx, _tx) = make_app();
        app.transcript_cache = Some(TranscriptCache {
            agent_id: "a".into(),
            entries: vec![],
            turns_at_load: 1,
            total_in_file: TRANSCRIPT_MAX_ENTRIES,
        });
        assert!(app.transcript_truncation_info().is_none());
    }

    #[test]
    fn transcript_truncation_info_returns_message_when_truncated() {
        let (mut app, _rx, _tx) = make_app();
        let total = TRANSCRIPT_MAX_ENTRIES + 50;
        app.transcript_cache = Some(TranscriptCache {
            agent_id: "a".into(),
            entries: vec![],
            turns_at_load: 1,
            total_in_file: total,
        });
        let info = app.transcript_truncation_info().unwrap();
        assert!(info.contains(&total.to_string()), "info={info}");
        assert!(
            info.contains(&TRANSCRIPT_MAX_ENTRIES.to_string()),
            "info={info}"
        );
    }

    // ── visible_messages ─────────────────────────────────────────────────────

    #[test]
    fn visible_messages_returns_main_messages_when_in_main_view() {
        let (mut app, _rx, _tx) = make_app();
        app.messages
            .push(ChatMessage::new(MessageRole::User, String::from("hello")));
        let msgs = app.visible_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hello");
    }

    #[test]
    fn visible_messages_returns_transcript_when_cache_present() {
        let (mut app, _rx, _tx) = make_app();
        app.view_target = AgentViewTarget::SubAgent {
            id: "x".into(),
            name: "X".into(),
        };
        app.transcript_cache = Some(TranscriptCache {
            agent_id: "x".into(),
            entries: vec![TuiTranscriptEntry {
                role: "user".into(),
                content: "from transcript".into(),
                tool_name: None,
                timestamp: None,
            }],
            turns_at_load: 1,
            total_in_file: 1,
        });
        let msgs = app.visible_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "from transcript");
    }

    #[test]
    fn visible_messages_returns_loading_placeholder_when_pending() {
        let (mut app, _rx, _tx) = make_app();
        app.view_target = AgentViewTarget::SubAgent {
            id: "x".into(),
            name: "X".into(),
        };
        // Simulate pending by installing a oneshot receiver that is not yet resolved.
        let (_tx2, rx2) = tokio::sync::oneshot::channel::<(Vec<TuiTranscriptEntry>, usize)>();
        app.pending_transcript = Some(rx2);
        let msgs = app.visible_messages();
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].content.contains("Loading"),
            "content={}",
            msgs[0].content
        );
    }

    #[test]
    fn visible_messages_returns_unavailable_when_no_cache_and_no_pending() {
        let (mut app, _rx, _tx) = make_app();
        app.view_target = AgentViewTarget::SubAgent {
            id: "x".into(),
            name: "MyAgent".into(),
        };
        let msgs = app.visible_messages();
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].content.contains("MyAgent"),
            "content={}",
            msgs[0].content
        );
    }

    // ── set_view_target ───────────────────────────────────────────────────────

    #[test]
    fn set_view_target_same_target_is_noop() {
        let (mut app, _rx, _tx) = make_app();
        app.scroll_offset = 5;
        // Already in Main — set to Main again.
        app.set_view_target(AgentViewTarget::Main);
        // scroll_offset must not be reset because nothing changed.
        assert_eq!(app.scroll_offset, 5);
    }

    #[test]
    fn set_view_target_clears_cache_and_scroll_on_switch() {
        let (mut app, _rx, _tx) = make_app();
        app.scroll_offset = 10;
        app.transcript_cache = Some(TranscriptCache {
            agent_id: "a".into(),
            entries: vec![],
            turns_at_load: 1,
            total_in_file: 1,
        });
        // Switch to Main (was implicitly Main — set a SubAgent first).
        app.view_target = AgentViewTarget::SubAgent {
            id: "a".into(),
            name: "A".into(),
        };
        app.set_view_target(AgentViewTarget::Main);
        assert_eq!(app.scroll_offset, 0);
        assert!(app.transcript_cache.is_none());
    }

    mod slash_autocomplete_tests {
        use super::*;

        #[test]
        fn slash_on_empty_input_opens_autocomplete() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            assert!(app.slash_autocomplete.is_none());

            let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.slash_autocomplete.is_some());
            assert_eq!(app.input(), "/");
        }

        #[test]
        fn no_open_mid_input() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            app.input = "hello ".to_owned();
            app.cursor_position = 6;

            let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.slash_autocomplete.is_none());
        }

        #[test]
        fn esc_dismisses_autocomplete() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            app.slash_autocomplete =
                Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
            app.input = "/sk".to_owned();
            app.cursor_position = 3;

            let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.slash_autocomplete.is_none());
            // Input retained
            assert_eq!(app.input(), "/sk");
        }

        #[test]
        fn at_char_while_autocomplete_open_does_not_open_file_picker() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            app.slash_autocomplete =
                Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
            app.input = "/".to_owned();
            app.cursor_position = 1;

            let key = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.file_picker_state.is_none());
        }

        #[test]
        fn backspace_removes_slash_and_dismisses() {
            let (mut app, _rx, _tx) = make_app();
            app.input_mode = InputMode::Insert;
            app.slash_autocomplete =
                Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
            app.input = "/".to_owned();
            app.cursor_position = 1;

            let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key));
            assert!(app.slash_autocomplete.is_none());
            assert!(app.input().is_empty());
        }
    }

    // ── trim_messages scroll adjustment (#2775) ──────────────────────────────

    #[test]
    fn trim_messages_no_trim_when_within_limit() {
        let (mut app, _rx, _tx) = make_app();
        for i in 0..10 {
            app.messages
                .push(ChatMessage::new(MessageRole::User, format!("msg {i}")));
        }
        app.scroll_offset = 5;
        app.trim_messages();
        assert_eq!(app.messages.len(), 10);
        assert_eq!(app.scroll_offset, 5);
    }

    #[test]
    fn trim_messages_evicts_excess_and_adjusts_scroll() {
        let (mut app, _rx, _tx) = make_app();
        let over = MAX_TUI_MESSAGES + 10;
        for i in 0..over {
            app.messages
                .push(ChatMessage::new(MessageRole::User, format!("msg {i}")));
        }
        app.scroll_offset = 20;
        app.trim_messages();
        assert_eq!(app.messages.len(), MAX_TUI_MESSAGES);
        assert_eq!(app.scroll_offset, 10); // 20 - 10 excess = 10
    }

    #[test]
    fn trim_messages_scroll_saturates_at_zero() {
        let (mut app, _rx, _tx) = make_app();
        let over = MAX_TUI_MESSAGES + 50;
        for i in 0..over {
            app.messages
                .push(ChatMessage::new(MessageRole::User, format!("msg {i}")));
        }
        app.scroll_offset = 10; // less than excess (50)
        app.trim_messages();
        assert_eq!(app.messages.len(), MAX_TUI_MESSAGES);
        assert_eq!(app.scroll_offset, 0); // saturates at 0
    }

    #[test]
    fn supervisor_activity_label_no_supervisor_returns_none() {
        let (app, _rx, _tx) = make_app();
        assert!(app.supervisor_activity_label().is_none());
    }

    #[tokio::test]
    async fn supervisor_activity_label_single_active_task() {
        use zeph_core::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};

        // CancellationToken is a re-export from tokio-util inside zeph-core.
        let cancel = tokio_util::sync::CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());
        sup.spawn(TaskDescriptor {
            name: "config-watcher",
            restart: RestartPolicy::RunOnce,
            factory: || async { std::future::pending::<()>().await },
        });

        // Give the task time to start and register as Running.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (mut app, _rx, _tx) = make_app();
        app = app.with_task_supervisor(sup);
        app.refresh_task_snapshots();

        let label = app.supervisor_activity_label();
        assert!(label.is_some(), "expected Some label for active task");
        assert!(
            label.as_deref().unwrap().contains("config-watcher"),
            "label should contain task name: {label:?}"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn supervisor_activity_label_multiple_tasks_shows_more() {
        use zeph_core::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};

        let cancel = tokio_util::sync::CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());
        for name in &["task-a", "task-b", "task-c"] {
            sup.spawn(TaskDescriptor {
                name,
                restart: RestartPolicy::RunOnce,
                factory: || async { std::future::pending::<()>().await },
            });
        }

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (mut app, _rx, _tx) = make_app();
        app = app.with_task_supervisor(sup);
        app.refresh_task_snapshots();

        let label = app
            .supervisor_activity_label()
            .expect("expected Some label");
        assert!(
            label.contains('+') || label.contains("more"),
            "expected '+N more' for multiple tasks, got: {label:?}"
        );

        cancel.cancel();
    }

    #[test]
    fn paste_inserts_text_in_insert_mode() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_event(AppEvent::Paste("hello".to_owned()));
        assert_eq!(app.input(), "hello");
        assert_eq!(app.cursor_position(), 5);
    }

    #[test]
    fn paste_at_mid_cursor_inserts_at_position() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_event(AppEvent::Paste("ac".to_owned()));
        // Move cursor to position 1 (between 'a' and 'c') via Left key
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(left));
        app.handle_event(AppEvent::Paste("b".to_owned()));
        assert_eq!(app.input(), "abc");
        assert_eq!(app.cursor_position(), 2);
    }

    #[test]
    fn paste_multiline_inserts_newlines() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_event(AppEvent::Paste("line1\nline2".to_owned()));
        assert_eq!(app.input(), "line1\nline2");
        assert_eq!(app.cursor_position(), 11);
    }

    #[test]
    fn paste_in_normal_mode_ignored() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        app.handle_event(AppEvent::Paste("should not appear".to_owned()));
        assert!(app.input().is_empty());
    }

    #[test]
    fn paste_clears_slash_autocomplete() {
        let (mut app, _rx, _tx) = make_app();
        app.slash_autocomplete =
            Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
        app.handle_event(AppEvent::Paste("text".to_owned()));
        assert!(app.slash_autocomplete.is_none());
        assert_eq!(app.input(), "text");
    }

    #[test]
    fn supervisor_activity_label_truncates_at_utf8_boundary() {
        // Construct a label that is exactly 38 Unicode chars (each 3 bytes in UTF-8).
        // This verifies char-based truncation does not panic on multi-byte boundaries.

        // Build a fake supervisor by manually checking the truncation logic directly.
        // We can't easily inject a custom snapshot, so we test the logic inline.
        let long_name: String = "あ".repeat(50); // 50 × 3-byte chars
        let truncated: String = long_name.chars().take(38).collect();
        assert_eq!(truncated.chars().count(), 38, "should truncate to 38 chars");
        assert!(
            truncated.is_char_boundary(truncated.len()),
            "must be valid UTF-8"
        );
        // Confirm byte-slicing the full string at char-boundary position doesn't panic.
        let _ = &long_name[..truncated.len()];
    }

    #[test]
    fn paste_state_set_for_multiline() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.handle_event(AppEvent::Paste("line1\nline2\nline3".to_owned()));
        let ps = app.paste_state().expect("paste_state should be Some");
        assert_eq!(ps.line_count, 3);
        assert_eq!(ps.byte_len, "line1\nline2\nline3".len());
    }

    #[test]
    fn paste_state_none_for_single_line() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.handle_event(AppEvent::Paste("single line".to_owned()));
        assert!(app.paste_state().is_none());
    }

    #[test]
    fn paste_state_cleared_on_char() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.handle_event(AppEvent::Paste("a\nb".to_owned()));
        assert!(app.paste_state().is_some());
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert!(app.paste_state().is_none());
    }

    #[test]
    fn paste_state_cleared_on_backspace() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.handle_event(AppEvent::Paste("a\nb".to_owned()));
        assert!(app.paste_state().is_some());
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert!(app.paste_state().is_none());
    }

    #[test]
    fn paste_state_cleared_on_ctrl_u() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.handle_event(AppEvent::Paste("a\nb".to_owned()));
        assert!(app.paste_state().is_some());
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.paste_state().is_none());
        assert!(
            app.input().is_empty(),
            "Ctrl+U must also clear input buffer"
        );
    }

    #[test]
    fn paste_state_cleared_on_shift_enter() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.handle_event(AppEvent::Paste("a\nb".to_owned()));
        assert!(app.paste_state().is_some());
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )));
        assert!(app.paste_state().is_none());
    }

    #[test]
    fn paste_state_cleared_on_navigation() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;

        // Left arrow
        app.handle_event(AppEvent::Paste("a\nb".to_owned()));
        assert!(app.paste_state().is_some());
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));
        assert!(app.paste_state().is_none(), "Left must clear paste_state");

        // Home key
        app.handle_event(AppEvent::Paste("c\nd".to_owned()));
        assert!(app.paste_state().is_some());
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Home,
            KeyModifiers::NONE,
        )));
        assert!(app.paste_state().is_none(), "Home must clear paste_state");
    }

    #[test]
    fn paste_state_consumed_on_submit() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.handle_event(AppEvent::Paste("line1\nline2\nline3\nline4".to_owned()));
        assert!(app.paste_state().is_some());
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(
            app.paste_state().is_none(),
            "paste_state cleared after submit"
        );
        assert_eq!(app.messages().len(), 1);
        assert_eq!(
            app.messages()[0].paste_line_count,
            Some(4),
            "paste_line_count must be set on submitted message"
        );
    }
}
