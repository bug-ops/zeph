// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::TuiCommand;
use crate::file_picker::{FileIndex, FilePickerState};
use crate::widgets::command_palette::CommandPaletteState;
use crate::widgets::slash_autocomplete::{SlashAutocompleteState, command_id_to_slash_form};

use super::{
    AgentViewTarget, App, ChatMessage, InputMode, MAX_INPUT_HISTORY, MessageRole, Panel,
    PasteState, format_security_report, oneshot,
};

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
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

        match self.sessions.current().input_mode {
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
                    let cmd = entry.command.clone();
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

    pub(super) fn execute_command(&mut self, cmd: TuiCommand) {
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
                self.sessions.current_mut().messages.clear();
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
        if self.handle_plan_command(&cmd) {
            return;
        }
        if self.handle_graph_command(&cmd) {
            return;
        }
        if self.handle_experiment_command(&cmd) {
            return;
        }
        if self.handle_memory_command(&cmd) {
            return;
        }
        if self.handle_plugin_command(&cmd) {
            return;
        }
        self.handle_acp_command(cmd);
    }

    fn handle_plan_command(&mut self, cmd: &TuiCommand) -> bool {
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
                self.sessions.current_mut().plan_view_active =
                    !self.sessions.current().plan_view_active;
            }
            _ => return false,
        }
        true
    }

    fn handle_graph_command(&mut self, cmd: &TuiCommand) -> bool {
        match cmd {
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
            _ => return false,
        }
        true
    }

    fn handle_experiment_command(&mut self, cmd: &TuiCommand) -> bool {
        match cmd {
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
            _ => return false,
        }
        true
    }

    fn handle_memory_command(&mut self, cmd: &TuiCommand) -> bool {
        match cmd {
            TuiCommand::ServerCompactionStatus => {
                let _ = self.user_input_tx.try_send("/server-compaction".to_owned());
            }
            TuiCommand::ViewGuidelines => {
                let _ = self.user_input_tx.try_send("/guidelines".to_owned());
            }
            TuiCommand::ForgettingSweep => {
                let _ = self.user_input_tx.try_send("/forgetting-sweep".to_owned());
            }
            TuiCommand::TrajectoryStats => {
                let _ = self.user_input_tx.try_send("/memory trajectory".to_owned());
            }
            TuiCommand::MemoryTreeStats => {
                let _ = self.user_input_tx.try_send("/memory tree".to_owned());
            }
            _ => return false,
        }
        true
    }

    fn handle_plugin_command(&mut self, cmd: &TuiCommand) -> bool {
        match cmd {
            TuiCommand::PluginList => {
                let _ = self.user_input_tx.try_send("/plugins list".to_owned());
            }
            TuiCommand::PluginAdd => self.prefill_input("/plugins add "),
            TuiCommand::PluginRemove => self.prefill_input("/plugins remove "),
            TuiCommand::PluginListOverlay => {
                let _ = self.user_input_tx.try_send("/plugins overlay".to_owned());
            }
            TuiCommand::SessionSwitchNext
            | TuiCommand::SessionSwitchPrev
            | TuiCommand::SessionClose => self.try_switch(cmd),
            _ => return false,
        }
        true
    }

    fn handle_acp_command(&mut self, cmd: TuiCommand) -> bool {
        match cmd {
            TuiCommand::AcpDirsList => {
                self.push_system_message("Querying ACP runtime...".to_owned());
                let _ = self.user_input_tx.try_send("/acp dirs".to_owned());
            }
            TuiCommand::AcpAuthMethodsView => {
                self.push_system_message("Querying ACP runtime...".to_owned());
                let _ = self.user_input_tx.try_send("/acp auth-methods".to_owned());
            }
            TuiCommand::AcpStatus => {
                self.push_system_message("Querying ACP runtime...".to_owned());
                let _ = self.user_input_tx.try_send("/acp status".to_owned());
            }
            TuiCommand::SubagentSpawn { command } => {
                if command.is_empty() {
                    self.prefill_input("/subagent spawn ");
                } else {
                    let _ = self
                        .user_input_tx
                        .try_send(format!("/subagent spawn {command}"));
                }
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
            _ => return false,
        }
        true
    }

    /// Handle a session switch or close command, blocking when a modal with a response channel
    /// is open (would deadlock the agent's `confirm()`/`elicit()` call if dismissed silently).
    fn try_switch(&mut self, cmd: &TuiCommand) {
        if self.confirm_state.is_some() || self.elicitation_state.is_some() {
            self.push_system_message(
                "Resolve the current confirmation dialog before switching sessions.".to_owned(),
            );
            return;
        }
        // Pure-UI overlays carry no response channel — safe to dismiss silently.
        self.command_palette = None;
        self.file_picker_state = None;
        self.slash_autocomplete = None;
        let prev = self.sessions.active();
        match cmd {
            TuiCommand::SessionSwitchNext => self.sessions.switch_next(),
            TuiCommand::SessionSwitchPrev => self.sessions.switch_prev(),
            TuiCommand::SessionClose => {
                let active = self.sessions.active();
                if !self.sessions.close(active) {
                    self.push_system_message("Cannot close the last remaining session.".to_owned());
                }
            }
            _ => {}
        }
        // Only invalidate render cache when the active slot actually changed.
        if self.sessions.active() != prev {
            self.sessions.current_mut().render_cache.clear();
        }
    }

    fn parse_session_slash(text: &str) -> Option<TuiCommand> {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        match tokens.as_slice() {
            [cmd, "next"] if cmd.eq_ignore_ascii_case("/session") => {
                Some(TuiCommand::SessionSwitchNext)
            }
            [cmd, "prev"] if cmd.eq_ignore_ascii_case("/session") => {
                Some(TuiCommand::SessionSwitchPrev)
            }
            [cmd, "close"] if cmd.eq_ignore_ascii_case("/session") => {
                Some(TuiCommand::SessionClose)
            }
            [cmd, "dirs"] if cmd.eq_ignore_ascii_case("/acp") => Some(TuiCommand::AcpDirsList),
            [cmd, "auth-methods"] if cmd.eq_ignore_ascii_case("/acp") => {
                Some(TuiCommand::AcpAuthMethodsView)
            }
            [cmd, "status"] if cmd.eq_ignore_ascii_case("/acp") => Some(TuiCommand::AcpStatus),
            [cmd, "spawn", rest @ ..] if cmd.eq_ignore_ascii_case("/subagent") => {
                Some(TuiCommand::SubagentSpawn {
                    command: rest.join(" "),
                })
            }
            _ => None,
        }
    }

    fn prefill_input(&mut self, prefix: &str) {
        self.sessions.current_mut().input.clear();
        self.sessions.current_mut().input.push_str(prefix);
        self.sessions.current_mut().cursor_position = self.sessions.current().input.len();
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
        let cps_line = match self.metrics.cost_cps_cents {
            Some(cps) => format!("\n  CPS: ${:.4}", cps / 100.0),
            None => String::new(),
        };
        let mut out = format!(
            "Cost:\n  Spent: ${:.4}{}\n  Successful tasks today: {}\n  Prompt tokens: {}\n  Completion tokens: {}\n  Total tokens: {}\n  Cache read: {}\n  Cache creation: {}",
            self.metrics.cost_spent_cents / 100.0,
            cps_line,
            self.metrics.cost_successful_tasks,
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
        self.sessions.current_mut().show_splash = false;
        self.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::System, content));
        self.sessions.current_mut().scroll_offset = 0;
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
        if key.code == KeyCode::Esc && !self.sessions.current().view_target.is_main() {
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
            KeyCode::Char('i') => self.sessions.current_mut().input_mode = InputMode::Insert,
            KeyCode::Char(':') => {
                self.command_palette = Some(CommandPaletteState::new());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sessions.current_mut().scroll_offset =
                    self.sessions.current().scroll_offset.saturating_add(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.sessions.current_mut().scroll_offset =
                    self.sessions.current().scroll_offset.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.sessions.current_mut().scroll_offset =
                    self.sessions.current().scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.sessions.current_mut().scroll_offset =
                    self.sessions.current().scroll_offset.saturating_sub(10);
            }
            KeyCode::Home => {
                self.sessions.current_mut().scroll_offset =
                    if let Some(cache) = &self.sessions.current().transcript_cache {
                        cache.entries.len()
                    } else {
                        self.sessions.current().messages.len()
                    };
            }
            KeyCode::End => {
                self.sessions.current_mut().scroll_offset = 0;
            }
            KeyCode::Char('d') => {
                self.show_side_panels = !self.show_side_panels;
            }
            KeyCode::Char('e') => {
                self.tool_expanded = !self.tool_expanded;
                self.sessions.current_mut().render_cache.clear();
            }
            KeyCode::Char('c') => {
                self.compact_tools = !self.compact_tools;
                self.sessions.current_mut().render_cache.clear();
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
                if self.sessions.current().view_target.is_main() {
                    self.sessions.current_mut().messages.clear();
                }
                self.sessions.current_mut().render_cache.clear();
                self.sessions.current_mut().scroll_offset = 0;
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            KeyCode::Char('p') => {
                self.sessions.current_mut().plan_view_active =
                    !self.sessions.current().plan_view_active;
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
        self.sessions
            .current()
            .input
            .char_indices()
            .nth(char_idx)
            .map_or(self.sessions.current().input.len(), |(i, _)| i)
    }

    pub(super) fn char_count(&self) -> usize {
        self.sessions.current().input.chars().count()
    }

    pub(super) fn prev_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.sessions.current().input.chars().collect();
        let mut pos = self.sessions.current().cursor_position;
        while pos > 0 && !chars[pos - 1].is_alphanumeric() {
            pos -= 1;
        }
        while pos > 0 && chars[pos - 1].is_alphanumeric() {
            pos -= 1;
        }
        pos
    }

    pub(super) fn next_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.sessions.current().input.chars().collect();
        let len = chars.len();
        let mut pos = self.sessions.current().cursor_position;
        while pos < len && chars[pos].is_alphanumeric() {
            pos += 1;
        }
        while pos < len && !chars[pos].is_alphanumeric() {
            pos += 1;
        }
        pos
    }

    pub(super) fn handle_paste(&mut self, text: &str) {
        if self.sessions.current().input_mode != InputMode::Insert {
            return;
        }
        self.slash_autocomplete = None;
        let byte_offset = self.byte_offset_of_char(self.sessions.current().cursor_position);
        self.sessions
            .current_mut()
            .input
            .insert_str(byte_offset, text);
        self.sessions.current_mut().cursor_position += text.chars().count();

        let line_count = text.matches('\n').count() + 1;
        if line_count >= 2 {
            // Replace any existing paste indicator — new paste supersedes the old one.
            self.sessions.current_mut().paste_state = Some(PasteState {
                line_count,
                byte_len: text.len(),
            });
        } else {
            self.sessions.current_mut().paste_state = None;
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent) {
        if self.slash_autocomplete.is_some() {
            self.handle_slash_autocomplete_key(key);
            return;
        }
        if self.handle_insert_text_keys(key) {
            return;
        }
        if self.handle_insert_delete_keys(key) {
            return;
        }
        if self.handle_insert_history_keys(key) {
            return;
        }
        if self.handle_insert_cursor_keys(key) {
            return;
        }
        self.handle_insert_control_keys(key);
    }

    /// Insert a newline character at the current cursor position.
    ///
    /// Shared body for `Shift+Enter` and `Ctrl+J`.
    fn insert_newline_at_cursor(&mut self) {
        self.sessions.current_mut().paste_state = None;
        let byte_offset = self.byte_offset_of_char(self.sessions.current().cursor_position);
        self.sessions.current_mut().input.insert(byte_offset, '\n');
        self.sessions.current_mut().cursor_position += 1;
    }

    /// Handle text insertion keys: Enter (submit), Shift+Enter / Ctrl+J (newline), Esc.
    ///
    /// Returns `true` when the key was handled.
    fn handle_insert_text_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_newline_at_cursor();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_newline_at_cursor();
            }
            KeyCode::Enter => self.submit_input(),
            KeyCode::Esc => self.sessions.current_mut().input_mode = InputMode::Normal,
            _ => return false,
        }
        true
    }

    /// Handle delete keys: Backspace (with or without Alt), Delete.
    ///
    /// Returns `true` when the key was handled.
    fn handle_insert_delete_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.sessions.current_mut().paste_state = None;
                let boundary = self.prev_word_boundary();
                if boundary < self.sessions.current().cursor_position {
                    let start = self.byte_offset_of_char(boundary);
                    let end = self.byte_offset_of_char(self.sessions.current().cursor_position);
                    self.sessions.current_mut().input.drain(start..end);
                    self.sessions.current_mut().cursor_position = boundary;
                }
            }
            KeyCode::Backspace => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.sessions.current_mut().paste_state = None;
                if self.sessions.current().cursor_position > 0 {
                    let byte_offset =
                        self.byte_offset_of_char(self.sessions.current().cursor_position - 1);
                    self.sessions.current_mut().input.remove(byte_offset);
                    self.sessions.current_mut().cursor_position -= 1;
                }
            }
            KeyCode::Delete => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.sessions.current_mut().paste_state = None;
                if self.sessions.current().cursor_position < self.char_count() {
                    let byte_offset =
                        self.byte_offset_of_char(self.sessions.current().cursor_position);
                    self.sessions.current_mut().input.remove(byte_offset);
                }
            }
            _ => return false,
        }
        true
    }

    /// Handle history navigation keys: Up, Down.
    ///
    /// Returns `true` when the key was handled.
    fn handle_insert_history_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => {
                self.handle_history_up();
            }
            KeyCode::Down => {
                self.sessions.current_mut().paste_state = None;
                let Some(i) = self.sessions.current().history_index else {
                    return true;
                };
                let prefix = &self.sessions.current().draft_input;
                let found = self.sessions.current().input_history[i + 1..]
                    .iter()
                    .position(|e| prefix.is_empty() || e.starts_with(prefix))
                    .map(|offset| i + 1 + offset);
                if let Some(idx) = found {
                    self.sessions.current_mut().history_index = Some(idx);
                    let text = self.sessions.current().input_history[idx].clone();
                    self.sessions.current_mut().input = text;
                } else {
                    self.sessions.current_mut().history_index = None;
                    self.sessions.current_mut().input =
                        std::mem::take(&mut self.sessions.current_mut().draft_input);
                }
                self.sessions.current_mut().cursor_position = self.char_count();
            }
            _ => return false,
        }
        true
    }

    /// Handle cursor movement keys: Left, Right (with optional Alt), Home, End.
    ///
    /// Returns `true` when the key was handled.
    fn handle_insert_cursor_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().cursor_position = self.prev_word_boundary();
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().cursor_position = self.next_word_boundary();
            }
            KeyCode::Left => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().cursor_position =
                    self.sessions.current().cursor_position.saturating_sub(1);
            }
            KeyCode::Right => {
                self.sessions.current_mut().paste_state = None;
                if self.sessions.current().cursor_position < self.char_count() {
                    self.sessions.current_mut().cursor_position += 1;
                }
            }
            KeyCode::Home => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().cursor_position = 0;
            }
            KeyCode::End => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().cursor_position = self.char_count();
            }
            _ => return false,
        }
        true
    }

    /// Handle Ctrl-key shortcuts and character insertion (including slash autocomplete trigger).
    ///
    /// Returns `true` when the key was handled; `false` for unrecognised keys.
    fn handle_insert_control_keys(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().cursor_position = 0;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().cursor_position = self.char_count();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sessions.current_mut().paste_state = None;
                self.sessions.current_mut().input.clear();
                self.sessions.current_mut().cursor_position = 0;
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.user_input_tx.try_send("/clear-queue".to_owned());
            }
            KeyCode::Char('@') => {
                self.open_file_picker();
            }
            KeyCode::Char(c) => {
                // First edit keystroke after paste reveals raw text; subsequent keystrokes edit normally.
                self.sessions.current_mut().paste_state = None;
                let was_empty = self.sessions.current().input.is_empty();
                let byte_offset = self.byte_offset_of_char(self.sessions.current().cursor_position);
                self.sessions.current_mut().input.insert(byte_offset, c);
                self.sessions.current_mut().cursor_position += 1;
                if c == '/' && was_empty {
                    self.slash_autocomplete = Some(SlashAutocompleteState::new());
                }
            }
            _ => return false,
        }
        true
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
                    self.sessions.current_mut().input = slash_form;
                    self.sessions.current_mut().cursor_position = self.char_count();
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
                    self.sessions.current_mut().input.clear();
                    self.sessions.current_mut().cursor_position = 0;
                    self.slash_autocomplete = None;
                } else {
                    let query = self
                        .slash_autocomplete
                        .as_ref()
                        .map_or(String::new(), |s| s.query.clone());
                    self.sessions.current_mut().input = format!("/{query}");
                    self.sessions.current_mut().cursor_position = self.char_count();
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
                self.sessions.current_mut().input = format!("/{query}");
                self.sessions.current_mut().cursor_position = self.char_count();
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
        self.sessions.current_mut().paste_state = None;
        if self.sessions.current().input.is_empty()
            && self.pending_count > 0
            && self.sessions.current().history_index.is_none()
        {
            if let Some(last) = self.sessions.current_mut().input_history.pop() {
                self.sessions.current_mut().input = last;
                self.sessions.current_mut().cursor_position = self.char_count();
                self.pending_count -= 1;
                self.queued_count = self.queued_count.saturating_sub(1);
                self.editing_queued = true;
                if let Some(pos) = self
                    .sessions
                    .current_mut()
                    .messages
                    .iter()
                    .rposition(|m| m.role == MessageRole::User)
                {
                    self.sessions.current_mut().messages.remove(pos);
                }
                let _ = self.user_input_tx.try_send("/drop-last-queued".to_owned());
            }
            return;
        }
        match self.sessions.current().history_index {
            None => {
                if self.sessions.current().input_history.is_empty() {
                    return;
                }
                self.sessions.current_mut().draft_input = self.sessions.current().input.clone();
                let prefix = &self.sessions.current().draft_input;
                let found = self
                    .sessions
                    .current()
                    .input_history
                    .iter()
                    .rposition(|e| prefix.is_empty() || e.starts_with(prefix));
                let Some(idx) = found else { return };
                self.sessions.current_mut().history_index = Some(idx);
                let text = self.sessions.current().input_history[idx].clone();
                self.sessions.current_mut().input = text;
            }
            Some(i) => {
                let prefix = &self.sessions.current().draft_input;
                let found = self.sessions.current().input_history[..i]
                    .iter()
                    .rposition(|e| prefix.is_empty() || e.starts_with(prefix));
                let Some(idx) = found else { return };
                self.sessions.current_mut().history_index = Some(idx);
                let text = self.sessions.current().input_history[idx].clone();
                self.sessions.current_mut().input = text;
            }
        }
        self.sessions.current_mut().cursor_position = self.char_count();
    }

    fn open_file_picker(&mut self) {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let needs_rebuild = self.file_index.as_ref().is_none_or(FileIndex::is_stale);
        if needs_rebuild && self.pending_file_index.is_none() {
            self.sessions.current_mut().status_label = Some("indexing files...".to_owned());
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
                self.sessions.current_mut().status_label = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.pending_file_index = None;
                self.sessions.current_mut().status_label = None;
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
                    let byte_offset =
                        self.byte_offset_of_char(self.sessions.current().cursor_position);
                    self.sessions
                        .current_mut()
                        .input
                        .insert_str(byte_offset, &path);
                    self.sessions.current_mut().cursor_position += path.chars().count();
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
            KeyCode::Backspace if !state.pop_char() => {
                self.file_picker_state = None;
            }
            _ => {}
        }
    }

    pub(super) fn submit_input(&mut self) {
        let text = self.sessions.current().input.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Intercept /session slash commands before forwarding to the agent.
        if let Some(cmd) = Self::parse_session_slash(&text) {
            self.sessions.current_mut().input.clear();
            self.sessions.current_mut().cursor_position = 0;
            self.execute_command(cmd);
            return;
        }
        self.sessions.current_mut().show_splash = false;
        self.sessions.current_mut().input_history.push(text.clone());
        if self.sessions.current().input_history.len() > MAX_INPUT_HISTORY {
            let excess = self.sessions.current().input_history.len() - MAX_INPUT_HISTORY;
            self.sessions.current_mut().input_history.drain(0..excess);
        }
        self.sessions.current_mut().history_index = None;
        self.sessions.current_mut().draft_input.clear();
        let paste_lines = self
            .sessions
            .current_mut()
            .paste_state
            .take()
            .map(|p| p.line_count);
        let mut msg = ChatMessage::new(MessageRole::User, text.clone());
        msg.paste_line_count = paste_lines;
        self.sessions.current_mut().messages.push(msg);
        self.trim_messages();
        self.sessions.current_mut().input.clear();
        self.sessions.current_mut().cursor_position = 0;
        self.sessions.current_mut().scroll_offset = 0;
        self.editing_queued = false;
        self.pending_count += 1;

        // Non-blocking send; capacity 32 — silent drop if agent loop is saturated.
        // Message is visible in chat but not processed; acceptable for interactive TUI.
        let _ = self.user_input_tx.try_send(text);
    }
}
