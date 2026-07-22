// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// TUI Reducer/Action decomposition implemented in this PR (#5076/#5103).
// See specs/tui-reducer/spec.md for the full design.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Notify, mpsc, oneshot, watch};
use tracing::debug;
use zeph_common::task_supervisor::{BlockingHandle, TaskSupervisor};

use crate::command::TuiCommand;
use crate::event::AgentEvent;
use crate::file_picker::{FileIndex, FilePickerState};
use crate::hyperlink::HyperlinkSpan;
use crate::metrics::MetricsSnapshot;
use crate::session::SessionRegistry;
use crate::widgets::command_palette::CommandPaletteState;
use crate::widgets::slash_autocomplete::SlashAutocompleteState;
use crate::widgets::tool_view::ToolDensity;

pub use crate::render_cache::{RenderCache, RenderCacheEntry, RenderCacheKey, content_hash};
pub use crate::types::{ChatMessage, InputMode, MessageRole};

use crate::types::PasteState;

const MAX_VISIBLE_INPUT_LINES: u16 = 3;

/// Tracks an in-flight background file-index build.
///
/// When a [`TaskSupervisor`] is wired into the `App`, the build is routed through it
/// so it appears in the task registry panel and is bounded by the blocking semaphore.
/// In environments without a supervisor (e.g., tests) the bare oneshot receiver is used.
enum PendingFileIndex {
    /// Supervised via [`TaskSupervisor::spawn_blocking`].
    Supervised(BlockingHandle<crate::file_picker::FileIndex>),
    /// Bare `tokio::task::spawn_blocking` — supervisor not available.
    Bare(oneshot::Receiver<crate::file_picker::FileIndex>),
}

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
#[non_exhaustive]
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
    /// The fleet session overview panel (side column).
    Fleet,
    /// The durable execution journal panel (side column).
    Durable,
    /// The read-only settings view: LLM providers, MCP servers, and agent definitions.
    Settings,
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
#[non_exhaustive]
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
#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
pub struct App {
    // SESSION-LOCAL state (10 fields relocated into SessionSlot)
    pub(crate) sessions: SessionRegistry,

    // GLOBAL state — unchanged from before relocation
    show_side_panels: bool,
    show_help: bool,
    pub metrics: MetricsSnapshot,
    metrics_rx: Option<watch::Receiver<MetricsSnapshot>>,
    active_panel: Panel,
    tool_expanded: bool,
    tool_density: ToolDensity,
    show_source_labels: bool,
    show_balance: bool,
    throbber_state: throbber_widgets_tui::ThrobberState,
    confirm_state: Option<ConfirmState>,
    elicitation_state: Option<ElicitationState>,
    command_palette: Option<CommandPaletteState>,
    command_tx: Option<mpsc::Sender<TuiCommand>>,
    file_picker_state: Option<FilePickerState>,
    file_index: Option<FileIndex>,
    slash_autocomplete: Option<SlashAutocompleteState>,
    reverse_search: Option<crate::widgets::reverse_search::ReverseSearchState>,
    /// `Ctrl+F` transcript-search overlay state (issue #6023). `None` when closed.
    ///
    /// Fully independent of `reverse_search` — no shared mutable state — but the two
    /// overlays are mutually exclusive at the key-routing level (`decode_key`).
    pub(crate) transcript_search: Option<crate::widgets::transcript_search::TranscriptSearchState>,
    /// Read-only settings view state: active tab and per-tab selection (issue #6024).
    pub(crate) settings: crate::widgets::settings::SettingsViewState,
    pub should_quit: bool,
    user_input_tx: mpsc::Sender<String>,
    agent_event_rx: mpsc::Receiver<AgentEvent>,
    // GLOBAL — single shared agent queue counters (stays global per arch v2 §7)
    queued_count: usize,
    pending_count: usize,
    /// Projected context token count from the last context assembly, or 0 if not yet known.
    context_token_estimate: usize,
    editing_queued: bool,
    hyperlinks: Vec<HyperlinkSpan>,
    cancel_signal: Option<Arc<Notify>>,
    pending_file_index: Option<PendingFileIndex>,
    /// Pending user-theme load: fired by `apply_theme` when the name resolves to a
    /// user file on disk rather than a built-in preset.  The background thread reads
    /// and parses `~/.config/zeph/themes/<name>.toml`; the result is installed by
    /// `poll_pending_theme` on the next tick.
    pending_theme: Option<
        oneshot::Receiver<Result<super::theme::SemanticPalette, super::theme::ThemeLoadError>>,
    >,
    /// Theme name paired with `pending_theme` so the poll handler can update `theme_name`.
    pending_theme_name: Option<String>,
    /// Interactive selection state for the subagent sidebar (stays global per arch v2 E5).
    pub subagent_sidebar: SubAgentSidebarState,
    /// Persistent "Resuming session" banner text, set once at startup by
    /// `AgentEvent::ResumeBanner` (spec-068 §13.5). `None` for a fresh conversation — never
    /// rendered in that case (AC-16). Unlike a transient status line, this stays visible
    /// after the first prompt.
    pub(crate) resume_banner: Option<String>,
    /// Optional handle to the `TaskSupervisor` for the task registry panel.
    task_supervisor: Option<TaskSupervisor>,
    /// Whether the task registry panel is currently visible (toggled by `/tasks`).
    show_task_panel: bool,
    /// Snapshot of supervisor tasks cached once per render tick before `terminal.draw()`.
    ///
    /// Avoids acquiring `TaskSupervisor`'s inner mutex inside the draw closure, which
    /// can block the render loop when the reap driver holds the lock concurrently.
    cached_task_snapshots: Vec<zeph_common::task_supervisor::TaskSnapshot>,
    /// Clipboard handle for `/copy` and `Ctrl+O` (#3685).
    pub(crate) clipboard: crate::clipboard::ClipboardHandle,
    /// Cached fleet session data for the fleet panel (#3884).
    pub(crate) fleet_snapshot: crate::widgets::fleet::FleetSnapshot,
    /// List scroll state for the fleet panel.
    pub(crate) fleet_list_state: ratatui::widgets::ListState,
    /// Cached durable execution data for the durable panel (spec-064, #4949).
    pub(crate) durable_snapshot: crate::widgets::durable::DurableSnapshot,
    /// List scroll state for the durable panel.
    pub(crate) durable_list_state: ratatui::widgets::ListState,
    /// Active visual theme. Derived from config at startup via [`crate::theme::Theme::from_palette_with_mode`].
    pub(crate) theme: crate::theme::Theme,
    /// Monotonic counter bumped on every theme swap; threads into [`RenderCacheKey`] to
    /// force cache misses when the user switches themes mid-session.
    pub(crate) theme_generation: u64,
    /// Name of the currently-active theme preset or user file.
    pub(crate) theme_name: String,
    /// Resolved terminal colour capability, stored once at startup for consistent re-derivation.
    pub(crate) effective_color_mode: crate::theme::EffectiveColorMode,
    /// Whether the terminal can render Unicode glyphs. Independent of colour support.
    ///
    /// `false` when `TERM=dumb`; `true` otherwise (default). Used by [`App::is_ascii_only`].
    pub(crate) unicode_capable: bool,
    /// Per-section collapse mask: `[skills, memory, resources, subagents]`.
    ///
    /// Use [`toggle_panel_collapse`](crate::App::toggle_panel_collapse) to toggle and
    /// [`effective_collapsed`](crate::App::effective_collapsed) for the layout-safe mask.
    pub(crate) collapsed_panels: [bool; 4],

    // --- Wave animation (#5096) ---
    /// Animation budget for the input separator row.
    ///
    /// Sourced from `[tui] motion` in config; runtime-switchable via `/motion`.
    pub(crate) motion: zeph_config::Motion,

    /// Monotonic tick counter for the wave animation phase.
    ///
    /// Incremented once per `AppEvent::Tick` (100 ms). `u64` never wraps within
    /// a session lifetime. Used as the explicit `t` argument to [`crate::widgets::wave::sample`]
    /// so that the wave renderer stays purely deterministic.
    pub(crate) wave_tick: u64,

    /// `anim_tick` captured on the first idle `Ctrl+C` press, arming the double-press
    /// quit window (see [`crate::App::quit_hint_active`]). `None` when no window is armed.
    pub(crate) pending_quit_tick: Option<u64>,

    /// Timestamp of the last observed progress event (token chunk or status change).
    ///
    /// Initialized at the moment the agent transitions to busy, NOT at `App` construction
    /// — otherwise the first frame after a long idle gap would falsely read as `Stalled`.
    pub(crate) last_progress_at: Instant,

    /// Whether the compact equalizer widget is visible in the busy separator row.
    ///
    /// Toggled via [`crate::command::TuiCommand::ToggleEqualizer`].
    /// Defaults to `true`. Ignored when `Motion` is not `Full`.
    pub(crate) show_equalizer: bool,

    // --- Micro-delights (#5104) ---
    /// Individual feature toggles sourced from `[tui.delights]` in config.
    pub(crate) delights: zeph_config::DelightsConfig,
    /// Approximate streaming rate and TTFT for the status bar.
    pub(crate) stream_rate: crate::delights::StreamRate,
    /// Ephemeral toast queue rendered as an overlay above the chat area.
    pub(crate) toasts: crate::delights::ToastQueue,
    /// One-shot shimmer state for the splash wordmark.
    pub(crate) splash_shimmer: crate::delights::SplashShimmer,

    // --- TUI Reducer / Mouse Mode (#5076, #5103) ---
    /// Whether opt-in mouse capture is currently enabled.
    ///
    /// When `true`, the terminal emits `MouseEvent`s instead of converting
    /// wheel events to arrow keys. Toggled by `/mouse on|off` or the palette.
    pub(crate) mouse_enabled: bool,

    /// Last computed layout rects, stored at the end of each `draw()` frame.
    ///
    /// Used by `decode_mouse` for hit-testing. `None` until the first frame
    /// is rendered — `decode_mouse` must guard against this (INV-M1, C3).
    pub(crate) last_layout: Option<crate::layout::AppLayout>,

    /// Pending mouse capture state change requested by `Effect::SetMouseCapture`.
    ///
    /// Drained by `tui_loop` in the shared post-select block (C2 — never
    /// inside an event arm to avoid ordering hazards).
    pub(crate) pending_mouse_capture: Option<bool>,

    /// URL of the remote daemon this session was attached to via `--connect <URL>`, if any.
    ///
    /// Set once at startup by [`with_remote_daemon_url`](Self::with_remote_daemon_url) —
    /// there is no runtime mechanism to attach/detach mid-session (#5509).
    remote_daemon_url: Option<String>,
}

pub(crate) mod action;
mod draw;
mod events;
mod keys;
pub(crate) mod mouse;
pub(crate) mod reducer;
mod state;
mod transcript;

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

pub(crate) fn format_security_report(metrics: &MetricsSnapshot) -> String {
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
            SecurityEventCategory::VigilFlag => "VIGIL_FLAG     ",
            SecurityEventCategory::GoalDrift => "GOAL_DRIFT     ",
            _ => "UNKNOWN        ",
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
    while let Some(start) = rest.find("[tool_use: ") {
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
mod tests;
