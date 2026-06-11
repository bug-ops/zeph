// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! App construction, builder configuration, and accessors over the active session
//! state (input, messages, scroll, panels, metrics, and display toggles).

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Notify, mpsc, watch};
use zeph_common::task_supervisor::TaskSupervisor;

use crate::command::TuiCommand;
use crate::event::AgentEvent;
use crate::hyperlink::HyperlinkSpan;
use crate::metrics::MetricsSnapshot;
use crate::session::SessionRegistry;
use crate::types::PasteState;
use crate::widgets::tool_view::ToolDensity;

use super::{
    AgentViewTarget, App, ChatMessage, InputMode, MAX_VISIBLE_INPUT_LINES, MessageRole, Panel,
    RenderCache, SubAgentSidebarState, TranscriptCache, is_tool_use_only, parse_tool_output,
};

/// No-progress duration after which the wave transitions to `Stalled`.
/// TODO: wire to `config.tui.stall_threshold_secs` (deferred per #5096 v1 scope)
const STALL_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(10);

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
            sessions: SessionRegistry::bootstrap(),
            show_side_panels: true,
            show_help: false,
            metrics: MetricsSnapshot::default(),
            metrics_rx: None,
            active_panel: Panel::Chat,
            tool_expanded: false,
            tool_density: ToolDensity::default(),
            show_source_labels: false,
            show_balance: true,
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            confirm_state: None,
            elicitation_state: None,
            command_palette: None,
            command_tx: None,
            file_picker_state: None,
            file_index: None,
            slash_autocomplete: None,
            reverse_search: None,
            should_quit: false,
            user_input_tx,
            agent_event_rx,
            queued_count: 0,
            pending_count: 0,
            context_token_estimate: 0,
            editing_queued: false,
            hyperlinks: Vec::new(),
            cancel_signal: None,
            pending_file_index: None,
            subagent_sidebar: SubAgentSidebarState::new(),
            task_supervisor: None,
            show_task_panel: false,
            cached_task_snapshots: Vec::new(),
            clipboard: crate::clipboard::ClipboardHandle::new(),
            fleet_snapshot: crate::widgets::fleet::FleetSnapshot::default(),
            fleet_list_state: ratatui::widgets::ListState::default(),
            durable_snapshot: crate::widgets::durable::DurableSnapshot::default(),
            durable_list_state: ratatui::widgets::ListState::default(),
            theme: crate::theme::Theme::default(),
            theme_generation: 0,
            theme_name: "zephyr".to_owned(),
            effective_color_mode: crate::theme::EffectiveColorMode::Truecolor,
            unicode_capable: crate::theme::detect_unicode_capable(),
            collapsed_panels: [false; 4],
            motion: zeph_config::Motion::Full,
            wave_tick: 0,
            last_progress_at: Instant::now(),
            wave_buf: Vec::new(),
            delights: zeph_config::DelightsConfig::default(),
            stream_rate: crate::delights::StreamRate::new(),
            toasts: crate::delights::ToastQueue::new(),
            splash_shimmer: crate::delights::SplashShimmer::new(),
        }
    }

    /// Override the visual theme with a palette-derived [`crate::theme::Theme`].
    ///
    /// Called once at startup after [`crate::theme::Theme::from_palette_with_mode`] has been
    /// built from the user's config and detected terminal colour capability.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::{App, theme::{Theme, SemanticPalette}};
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let app = App::new(user_tx, agent_rx)
    ///     .with_theme(Theme::from_palette(&SemanticPalette::zephyr()));
    /// ```
    #[must_use]
    pub fn with_theme(mut self, theme: crate::theme::Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Set the active theme name for cycle tracking and status echoes.
    ///
    /// Must be called at every construction site that supplies a non-default theme so that
    /// `cycle_theme` starts cycling from the correct position.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let app = App::new(user_tx, agent_rx).with_theme_name("gruvbox-dark");
    /// ```
    #[must_use]
    pub fn with_theme_name(mut self, name: impl Into<String>) -> Self {
        self.theme_name = name.into();
        self
    }

    /// Set the resolved colour mode used to re-derive themes on runtime swap.
    ///
    /// Store the `EffectiveColorMode` resolved once at startup so that `apply_theme`
    /// produces consistent downgrade behaviour without re-running OS detection per swap.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::{App, theme::EffectiveColorMode};
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let app = App::new(user_tx, agent_rx)
    ///     .with_effective_color_mode(EffectiveColorMode::Truecolor);
    /// ```
    #[must_use]
    pub fn with_effective_color_mode(mut self, mode: crate::theme::EffectiveColorMode) -> Self {
        self.effective_color_mode = mode;
        self
    }

    /// Return the current theme generation counter.
    ///
    /// Passed into `RenderCacheKey::theme_generation` so the render cache is
    /// invalidated after every theme swap.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let app = App::new(user_tx, agent_rx);
    /// assert_eq!(app.theme_generation(), 0);
    /// ```
    #[must_use]
    pub fn theme_generation(&self) -> u64 {
        self.theme_generation
    }

    /// Apply a named theme preset or user file, updating the active theme and bumping
    /// the generation counter so that all session render caches are invalidated.
    ///
    /// On success the new theme name is stored for cycle tracking.
    /// On error the existing theme is left unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`crate::theme::ThemeLoadError`] if the name fails validation or the
    /// palette file cannot be parsed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let mut app = App::new(user_tx, agent_rx);
    /// let gen_before = app.theme_generation();
    /// let _ = app.apply_theme("zephyr-light");
    /// assert!(app.theme_generation() > gen_before);
    /// ```
    pub fn apply_theme(&mut self, name: &str) -> Result<(), crate::theme::ThemeLoadError> {
        use crate::theme::{Theme, ThemeLoadError, resolve_palette};
        // Reject empty names — always routes to listing, never implicit preset resolution.
        if name.is_empty() {
            return Err(ThemeLoadError::UnsafeName(String::new()));
        }
        let palette = resolve_palette(name)?;
        let new_theme = Theme::from_palette_with_mode(&palette, self.effective_color_mode);
        self.theme = new_theme;
        name.clone_into(&mut self.theme_name);
        self.theme_generation += 1;
        // Invalidate render caches for ALL sessions (theme is global, not per-session).
        self.clear_all_render_caches();
        Ok(())
    }

    /// Cycle to the next preset in the fixed cycle list `["zephyr", "zephyr-light", "high-contrast"]`.
    ///
    /// Finds the current theme name in the cycle list and advances to the next entry,
    /// wrapping around. If the current name is not in the list, starts from `"zephyr"`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let mut app = App::new(user_tx, agent_rx).with_theme_name("zephyr");
    /// app.cycle_theme();
    /// assert_eq!(app.active_theme_name(), "zephyr-light");
    /// ```
    pub fn cycle_theme(&mut self) {
        const CYCLE: &[&str] = &["zephyr", "zephyr-light", "high-contrast"];
        let pos = CYCLE
            .iter()
            .position(|&n| n == self.theme_name.as_str())
            .unwrap_or(0);
        let next = CYCLE[(pos + 1) % CYCLE.len()];
        if let Err(e) = self.apply_theme(next) {
            tracing::warn!("cycle_theme: failed to load '{}': {e}", next);
        }
    }

    /// Return the name of the currently-active theme.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let app = App::new(user_tx, agent_rx).with_theme_name("gruvbox-dark");
    /// assert_eq!(app.active_theme_name(), "gruvbox-dark");
    /// ```
    #[must_use]
    pub fn active_theme_name(&self) -> &str {
        &self.theme_name
    }

    /// Return the resolved terminal colour mode stored at startup.
    ///
    /// Used by widgets to choose between Unicode and ASCII fallback rendering.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::{App, theme::EffectiveColorMode};
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// let app = App::new(user_tx, agent_rx);
    /// assert_eq!(app.effective_color_mode(), EffectiveColorMode::Truecolor);
    /// ```
    #[must_use]
    pub fn effective_color_mode(&self) -> crate::theme::EffectiveColorMode {
        self.effective_color_mode
    }

    /// Return `true` when the terminal cannot render Unicode glyphs and ASCII-only output
    /// should be used in place of box-drawing characters and spinners.
    ///
    /// Unicode capability is detected independently from colour support. A terminal with
    /// `NO_COLOR` set (which produces `EffectiveColorMode::Never`) may still render `▹▸`
    /// perfectly. Only `TERM=dumb` or a non-UTF-8 locale forces ASCII mode.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(64);
    /// let (_, agent_rx) = mpsc::channel(64);
    /// // Default app created in a normal environment reports Unicode capable.
    /// let app = App::new(user_tx, agent_rx);
    /// // is_ascii_only() depends on TERM/LANG env vars, not color mode.
    /// let _ = app.is_ascii_only();
    /// ```
    #[must_use]
    pub fn is_ascii_only(&self) -> bool {
        !self.unicode_capable
    }

    /// Invalidate render caches in every session slot.
    ///
    /// Called on theme swap because cached `Line`s bake in theme `Style` values — stale
    /// styles from the old theme would otherwise persist until a content change triggers a
    /// miss. Must clear ALL sessions, not only the currently-active one.
    fn clear_all_render_caches(&mut self) {
        for slot in self.sessions.iter_mut() {
            slot.render_cache.clear();
        }
    }

    /// Return `true` while the splash screen should be displayed.
    ///
    /// The splash screen is hidden as soon as the first chat message arrives.
    #[must_use]
    pub fn show_splash(&self) -> bool {
        self.sessions.current().show_splash
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
        self.sessions.current().plan_view_active
    }

    // ---- Accessors for fields relocated into SessionSlot (preserves pub API surface) ----

    /// Returns the active session's render cache.
    #[must_use]
    pub fn render_cache(&self) -> &RenderCache {
        &self.sessions.current().render_cache
    }

    /// Returns a mutable reference to the active session's render cache.
    pub fn render_cache_mut(&mut self) -> &mut RenderCache {
        &mut self.sessions.current_mut().render_cache
    }

    /// Returns the current chat area view target (main conversation or sub-agent transcript).
    #[must_use]
    pub fn view_target(&self) -> &AgentViewTarget {
        &self.sessions.current().view_target
    }

    /// Returns the cached transcript for the currently-focused sub-agent, if any.
    #[must_use]
    pub fn transcript_cache(&self) -> Option<&TranscriptCache> {
        self.sessions.current().transcript_cache.as_ref()
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
                self.sessions
                    .current_mut()
                    .messages
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
                self.sessions
                    .current_mut()
                    .input_history
                    .push(content.to_owned());
            }
            self.sessions
                .current_mut()
                .messages
                .push(ChatMessage::new(role, content));
        }
        // Enforce the message buffer cap on initial history load as well.
        self.trim_messages();
        if !self.sessions.current().messages.is_empty() {
            self.sessions.current_mut().show_splash = false;
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

    /// Set the initial tool-output density from a loaded `TuiConfig`.
    ///
    /// Applied once at startup; runtime changes via the `c` key override this
    /// but are not persisted back to config.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    /// use zeph_config::ToolDensity;
    ///
    /// let (tx, _rx) = mpsc::channel(1);
    /// let (_atx, arx) = mpsc::channel(1);
    /// let _app = App::new(tx, arx).with_tool_density(ToolDensity::Compact);
    /// ```
    #[must_use]
    pub fn with_tool_density(mut self, density: ToolDensity) -> Self {
        self.tool_density = density;
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
    /// use zeph_common::task_supervisor::TaskSupervisor;
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
    /// Used by the input widget to show a braille spinner with the name of the first
    /// active (Running/Restarting) task when no other status is being displayed.
    #[must_use]
    pub fn supervisor_activity_label(&self) -> Option<String> {
        self.task_supervisor.as_ref()?;
        let mut active = self
            .cached_task_snapshots
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    zeph_common::task_supervisor::TaskStatus::Running
                        | zeph_common::task_supervisor::TaskStatus::Restarting { .. }
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
                self.sessions.current_mut().plan_view_active = false;
            }
            self.metrics = new_metrics;
        }
        // Clamp sidebar selection in case subagents count changed.
        let count = self.metrics.sub_agents.len();
        self.subagent_sidebar.clamp(count);
        // Trigger transcript reload when turns count increased.
        self.maybe_reload_transcript();
    }

    /// Evict oldest messages when the buffer exceeds `MAX_TUI_MESSAGES` (#2737).
    ///
    /// Shifts the render cache to match the drained messages, preserving cached renders
    /// for the remaining entries and avoiding a full re-render stall (#2775).
    pub(super) fn trim_messages(&mut self) {
        self.sessions.current_mut().trim_messages();
    }

    /// Return a slice of all chat messages currently in the buffer.
    ///
    /// For the currently-displayed messages (which may be a sub-agent
    /// transcript) use [`visible_messages`](Self::visible_messages) instead.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.sessions.current().messages
    }

    /// Return the current content of the text input field.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.sessions.current().input
    }

    /// Return the current input mode (normal vs. insert).
    #[must_use]
    pub fn input_mode(&self) -> InputMode {
        self.sessions.current().input_mode
    }

    /// Return the cursor byte position within the input string.
    #[must_use]
    pub fn cursor_position(&self) -> usize {
        self.sessions.current().cursor_position
    }

    /// Returns the composer height requested by the current draft, capped at three visible rows.
    #[must_use]
    pub(crate) fn desired_input_height(&self) -> u16 {
        let content_lines = self.input_line_count().min(MAX_VISIBLE_INPUT_LINES);
        content_lines.saturating_add(2)
    }

    /// Returns the number of logical lines in the current draft or indicator.
    #[must_use]
    pub(crate) fn input_line_count(&self) -> u16 {
        if self.sessions.current().paste_state.is_some()
            || (self.sessions.current().input.is_empty()
                && matches!(self.sessions.current().input_mode, InputMode::Insert))
        {
            1
        } else {
            u16::try_from(self.sessions.current().input.matches('\n').count() + 1)
                .unwrap_or(u16::MAX)
        }
    }

    /// Return the number of lines the chat view is scrolled up from the bottom.
    ///
    /// `0` means the view is at the bottom (latest messages visible).
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.sessions.current().scroll_offset
    }

    /// Scroll to bottom only if already at (or near) the bottom.
    pub(super) fn auto_scroll(&mut self) {
        if self.sessions.current().scroll_offset <= 1 {
            self.sessions.current_mut().scroll_offset = 0;
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
        self.sessions.current().paste_state.as_ref()
    }

    /// Return the current tool-output density level.
    #[must_use]
    pub fn tool_density(&self) -> ToolDensity {
        self.tool_density
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
            self.sessions.current_mut().render_cache.clear();
        }
    }

    /// Return `true` when the Cocoon TON balance should be shown in the status bar.
    ///
    /// Controlled by `[cocoon] show_balance` in config (default `true`). When `false`,
    /// the balance is redacted to `*** TON` per spec §15.2.
    #[must_use]
    pub fn show_balance(&self) -> bool {
        self.show_balance
    }

    /// Set whether the Cocoon TON balance is shown in the status bar.
    pub fn set_show_balance(&mut self, v: bool) {
        self.show_balance = v;
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
        self.sessions.current().status_label.as_deref()
    }

    /// Return the number of messages queued or pending for the agent.
    ///
    /// Displayed in the input bar to indicate backpressure.
    #[must_use]
    pub fn queued_count(&self) -> usize {
        self.queued_count.max(self.pending_count)
    }

    /// Return the projected context token count from the last assembly, or 0 if not yet known.
    ///
    /// The value is approximate (character-level heuristic) and is updated once per agent turn.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (tx, _) = mpsc::channel(1);
    /// let (_, rx) = mpsc::channel(1);
    /// let app = App::new(tx, rx);
    /// assert_eq!(app.context_token_estimate(), 0);
    /// ```
    #[must_use]
    pub fn context_token_estimate(&self) -> usize {
        self.context_token_estimate
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
        self.sessions.current().status_label.is_some()
            || self
                .sessions
                .current()
                .messages
                .last()
                .is_some_and(|m| m.streaming)
    }

    /// Return `true` when the last message is a streaming tool output.
    #[must_use]
    pub fn has_running_tool(&self) -> bool {
        self.sessions
            .current()
            .messages
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

    /// Toggle the collapsed state of a side-panel section by index.
    ///
    /// Index mapping: `0` = Skills, `1` = Memory, `2` = Resources, `3` = `SubAgents`.
    /// Out-of-range indices are silently ignored.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (tx, _) = mpsc::channel(1);
    /// let (_, rx) = mpsc::channel(1);
    /// let mut app = App::new(tx, rx);
    /// app.toggle_panel_collapse(0);
    /// assert!(app.collapsed_panels()[0]);
    /// app.toggle_panel_collapse(0);
    /// assert!(!app.collapsed_panels()[0]);
    /// ```
    pub fn toggle_panel_collapse(&mut self, idx: usize) {
        if let Some(slot) = self.collapsed_panels.get_mut(idx) {
            *slot = !*slot;
        }
    }

    /// Return the current per-section collapse mask.
    ///
    /// Index mapping: `0` = Skills, `1` = Memory, `2` = Resources, `3` = `SubAgents`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (tx, _) = mpsc::channel(1);
    /// let (_, rx) = mpsc::channel(1);
    /// let app = App::new(tx, rx);
    /// assert_eq!(app.collapsed_panels(), [false; 4]);
    /// ```
    #[must_use]
    pub fn collapsed_panels(&self) -> [bool; 4] {
        self.collapsed_panels
    }

    /// Compute the effective collapse mask used for layout and rendering.
    ///
    /// Index 3 (`SubAgents` slot) is forced expanded when any overlay currently
    /// owns that slot — Fleet, Durable, Tasks, plan view, or security events.
    /// Indices 0–2 pass through the raw `collapsed_panels` value unchanged.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (tx, _) = mpsc::channel(1);
    /// let (_, rx) = mpsc::channel(1);
    /// let mut app = App::new(tx, rx);
    /// // Collapsing slot 3 is honoured when no overlay is active.
    /// app.toggle_panel_collapse(3);
    /// assert!(app.effective_collapsed()[3]);
    /// ```
    #[must_use]
    pub fn effective_collapsed(&self) -> [bool; 4] {
        let mut eff = self.collapsed_panels;
        // Force-expand slot 3 whenever an overlay is rendering into the subagents rect.
        let slot3_has_overlay = matches!(
            self.active_panel,
            Panel::SubAgents | Panel::Fleet | Panel::Durable
        ) || self.show_task_panel
            || self
                .metrics
                .orchestration_graph
                .as_ref()
                .is_some_and(|s| !s.is_stale() && !self.sessions.current().plan_view_active)
            || self.has_recent_security_events();
        if slot3_has_overlay {
            eff[3] = false;
        }
        eff
    }

    /// Configure the animation budget from config.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_config::Motion;
    /// use zeph_tui::App;
    ///
    /// let (user_tx, _) = mpsc::channel(1);
    /// let (_, agent_rx) = mpsc::channel(1);
    /// let app = App::new(user_tx, agent_rx).with_motion(Motion::Minimal);
    /// assert_eq!(app.motion(), Motion::Minimal);
    /// ```
    #[must_use]
    pub fn with_motion(mut self, motion: zeph_config::Motion) -> Self {
        self.motion = motion;
        self
    }

    /// Return the current animation budget.
    #[must_use]
    pub fn motion(&self) -> zeph_config::Motion {
        self.motion
    }

    /// Return the monotonic wave-tick counter.
    ///
    /// Passed as `t` into [`crate::widgets::wave::sample`] / [`crate::widgets::wave::glyphs`].
    #[must_use]
    pub fn wave_tick(&self) -> u64 {
        self.wave_tick
    }

    /// Apply micro-delight configuration (#5104).
    ///
    /// Called at construction time from `tui_bridge` to propagate `[tui.delights]` config.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    /// use zeph_config::DelightsConfig;
    ///
    /// let (tx, _) = mpsc::channel(1);
    /// let (_, rx) = mpsc::channel(1);
    /// let app = App::new(tx, rx).with_delights(DelightsConfig::default());
    /// ```
    #[must_use]
    pub fn with_delights(mut self, delights: zeph_config::DelightsConfig) -> Self {
        self.delights = delights;
        self
    }

    /// Return the current animation tick counter.
    ///
    /// Aliased from `wave_tick` so animation code can read it by an intent-revealing name.
    /// Free-running at ~10fps (100ms/tick via `EventReader`). Never pauses.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use zeph_tui::App;
    ///
    /// let (tx, _) = mpsc::channel(1);
    /// let (_, rx) = mpsc::channel(1);
    /// let app = App::new(tx, rx);
    /// assert_eq!(app.anim_tick(), 0);
    /// ```
    #[must_use]
    pub fn anim_tick(&self) -> u64 {
        self.wave_tick
    }

    /// Begin an animated scroll to `target_offset` for the current session.
    ///
    /// When smooth-scroll is disabled (`motion = Off` or `delights.smooth_scroll = false`),
    /// the offset is set directly. Single-line scrolls (j/k) bypass this and write
    /// `scroll_offset` directly — animation is reserved for page-sized jumps.
    pub(crate) fn begin_scroll(&mut self, target_offset: usize) {
        let smooth = self.motion != zeph_config::Motion::Off && self.delights.smooth_scroll;
        if smooth {
            // Use the in-flight animation's destination as the starting point so that
            // two rapid PageDown presses chain correctly instead of producing identical
            // animations from the same stale scroll_offset.
            let cur = self.sessions.current();
            let from = cur.scroll_anim.as_ref().map_or(cur.scroll_offset, |a| a.to);
            let now = self.anim_tick();
            self.sessions.current_mut().scroll_anim = Some(crate::session::ScrollAnim {
                from,
                to: target_offset,
                start_tick: now,
            });
        } else {
            self.sessions.current_mut().scroll_offset = target_offset;
        }
    }

    /// Enqueue an ephemeral toast notification.
    ///
    /// **MUST** be called only from the render thread (inside `handle_event` /
    /// `handle_agent_event`). Off-thread origins must be routed as `AgentEvent` or
    /// `AppEvent` variants — never mutate the queue cross-thread.
    pub(crate) fn push_toast(&mut self, text: impl Into<String>, kind: crate::delights::ToastKind) {
        let tick = self.anim_tick();
        self.toasts.push(text, kind, tick);
    }

    /// Whether any animation-driven feature is currently active.
    ///
    /// Provided as an optional future hook for a deferred CPU-optimization issue
    /// (suppress idle redraws when nothing animates). NOT wired to the redraw gate
    /// in this PR — the `EventReader` already drives 10fps unconditionally.
    #[must_use]
    pub fn wants_animation_frame(&self) -> bool {
        if self.motion == zeph_config::Motion::Off {
            return false;
        }
        let t = self.anim_tick();
        let flash_active = self
            .sessions
            .current()
            .flash
            .pending
            .values()
            .any(|&born| t.saturating_sub(born) < crate::session::FLASH_TICKS);
        let scroll_active = self.sessions.current().scroll_anim.is_some();
        self.toasts.has_active(t)
            || flash_active
            || scroll_active
            || self.splash_shimmer.is_active(t)
    }

    /// Derive the current wave animation state from live agent state.
    ///
    /// Stalled is checked first so a hung turn never reads as Streaming or Swell.
    ///
    /// # Stall behaviour
    ///
    /// A slow time-to-first-token > `stall_threshold` shows `Stalled` before any token
    /// arrives, because `last_progress_at` is set when the turn goes busy (Typing/Status)
    /// and the threshold starts counting from that moment. Accepted for v1 simplicity.
    #[must_use]
    pub fn wave_state(&self) -> crate::widgets::wave::WaveState {
        use crate::widgets::wave::WaveState;

        if !self.is_agent_busy() {
            return WaveState::Idle;
        }

        // Stalled: no progress for longer than the threshold.
        if self.last_progress_at.elapsed() > STALL_THRESHOLD {
            return WaveState::Stalled;
        }

        // Tool execution.
        if self.has_running_tool() {
            return WaveState::Tool;
        }

        // Parallel background tasks.
        // Use bg_inflight (all-classes total) — avoids double-counting since
        // bg_enrichment_inflight and bg_telemetry_inflight are already included in bg_inflight.
        let bg = self.metrics.bg_inflight;
        if bg >= 2 {
            #[allow(clippy::cast_possible_truncation)]
            return WaveState::Parallel {
                sines: (bg as u8).clamp(2, 3),
            };
        }

        // Streaming: last message is a streaming assistant message.
        if self
            .sessions
            .current()
            .messages
            .last()
            .is_some_and(|m| m.streaming && m.role == crate::types::MessageRole::Assistant)
        {
            return WaveState::Streaming;
        }

        // Swell: busy but awaiting first token.
        WaveState::Swell
    }

    /// Advance all micro-delight animations by one tick.
    ///
    /// Called from [`crate::app::events`] on every `AppEvent::Tick` so that
    /// animation state advances unconditionally, regardless of whether a draw
    /// frame is suppressed by `DirtyState::AnimationOnly`.
    pub(crate) fn tick_delights(&mut self) {
        let now = self.anim_tick();

        // Prune expired toasts.
        self.toasts.prune(now);

        // Advance current session's scroll animation.
        if let Some(ref anim) = self.sessions.current().scroll_anim {
            let (offset, done) = anim.current_offset(now);
            self.sessions.current_mut().scroll_offset = offset;
            if done {
                self.sessions.current_mut().scroll_anim = None;
            }
        }

        // Prune expired flash entries for the current session.
        self.sessions.current_mut().flash.prune(now);

        // Detect show_splash rising edge (false → true) → reset shimmer for fresh sweep.
        let cur_show_splash = self.sessions.current().show_splash;
        if cur_show_splash && !self.sessions.current().prev_show_splash {
            self.splash_shimmer.reset();
        }
        self.sessions.current_mut().prev_show_splash = cur_show_splash;

        // Activate shimmer on first splash frame.
        let shimmer_enabled =
            self.motion != zeph_config::Motion::Off && self.delights.splash_shimmer;
        if shimmer_enabled && cur_show_splash {
            self.splash_shimmer.activate(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::App;

    fn make_app() -> App {
        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        App::new(user_tx, agent_rx)
    }

    #[test]
    fn apply_theme_path_traversal_rejected() {
        let mut app = make_app();
        assert!(
            app.apply_theme("../../etc/passwd").is_err(),
            "path traversal must be rejected"
        );
        assert!(
            app.apply_theme("bad..name").is_err(),
            "dotdot in name must be rejected"
        );
        assert!(app.apply_theme("").is_err(), "empty name must be rejected");
        // Theme must remain unchanged after all failed attempts.
        assert_eq!(app.active_theme_name(), "zephyr");
    }

    #[test]
    fn apply_theme_valid_bumps_generation() {
        let mut app = make_app();
        let gen_before = app.theme_generation();
        app.apply_theme("zephyr-light").expect("valid theme");
        assert!(
            app.theme_generation() > gen_before,
            "generation must increment"
        );
        assert_eq!(app.active_theme_name(), "zephyr-light");
    }

    #[test]
    fn apply_theme_invalidates_all_session_caches() {
        use crate::app::RenderCacheKey;
        use crate::widgets::tool_view::ToolDensity;

        let mut app = make_app();

        // Add a second session (pub(crate) — accessible within the same crate).
        let _slot2_key = app.sessions.create("session 2");

        // Populate the render cache of the current (first) session.
        let dummy_key = RenderCacheKey {
            content_hash: 1,
            terminal_width: 80,
            tool_expanded: false,
            tool_density: ToolDensity::Inline,
            show_labels: false,
            theme_generation: 0,
        };
        app.sessions
            .current_mut()
            .render_cache
            .put(0, dummy_key, vec![], vec![]);

        // Verify the entry is present before the theme swap.
        let hit_before = app.sessions.current().render_cache.get(0, &dummy_key);
        assert!(hit_before.is_some(), "cache must contain the seeded entry");

        // Swap theme → must clear caches in ALL sessions.
        app.apply_theme("zephyr-light").expect("valid theme");

        // After the swap the key has a stale theme_generation, so get() returns None.
        let hit_after = app.sessions.current().render_cache.get(0, &dummy_key);
        assert!(
            hit_after.is_none(),
            "cache must be cleared (or invalidated) on theme swap"
        );
    }

    #[test]
    fn with_theme_name_builder_sets_name() {
        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx).with_theme_name("gruvbox-dark");
        assert_eq!(app.active_theme_name(), "gruvbox-dark");
    }
}
