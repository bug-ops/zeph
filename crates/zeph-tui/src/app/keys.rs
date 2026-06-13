// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(super) const SCROLL_STEP_PAGE: usize = 10;

use crate::app::action::{Action, CursorMove, ElicitationEdit, PaletteEdit, ScrollDir, VertDir};
use crate::app::reducer::{reduce, run_effects};
use crate::command::TuiCommand;
use crate::file_picker::{FileIndex, FilePickerState};
use crate::layout::truncate_to_width;

use super::{
    AgentViewTarget, App, ChatMessage, InputMode, MessageRole, Panel, PasteState,
    format_security_report, oneshot,
};

impl App {
    /// Main keyboard entry point. Decodes `key` into an `Action` and routes it
    /// through `reduce → run_effects` (INV-R1). Modal layers and legacy handlers
    /// that cannot be trivially expressed as a single `Action` are routed through
    /// `Action::*` variants that the reducer already handles.
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        if let Some(action) = self.decode_key(key) {
            let effects = reduce(self, action);
            run_effects(self, effects);
        }
    }

    /// Decode a `KeyEvent` into the corresponding `Action`, or `None` if the event
    /// has no effect (e.g. an unrecognised key in a modal that ignores it).
    #[allow(clippy::too_many_lines)]
    fn decode_key(&self, key: KeyEvent) -> Option<Action> {
        // Global: Ctrl-C always quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Quit);
        }

        // Help overlay: only '?' and Esc close it.
        if self.show_help {
            return match key.code {
                KeyCode::Char('?') | KeyCode::Esc => Some(Action::SetHelp(false)),
                _ => None,
            };
        }

        // Confirm dialog
        if self.confirm_state.is_some() {
            return Self::decode_confirm_key(key);
        }

        // Elicitation dialog
        if self.elicitation_state.is_some() {
            return Self::decode_elicitation_key(key);
        }

        // Command palette
        if self.command_palette.is_some() {
            return Self::decode_palette_key(key);
        }

        // File picker
        if self.file_picker_state.is_some() {
            return Self::decode_file_picker_key(key);
        }

        match self.sessions.current().input_mode {
            InputMode::Normal => self.decode_normal_key(key),
            InputMode::Insert => self.decode_insert_key(key),
        }
    }

    fn decode_confirm_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(Action::ConfirmRespond(true)),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Action::ConfirmRespond(false)),
            _ => None,
        }
    }

    fn decode_elicitation_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => Some(Action::ElicitationCancel),
            KeyCode::Enter => Some(Action::ElicitationSubmit),
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                Some(Action::ElicitationField(ElicitationEdit::PrevField))
            }
            KeyCode::Tab => Some(Action::ElicitationField(ElicitationEdit::NextField)),
            KeyCode::BackTab => Some(Action::ElicitationField(ElicitationEdit::PrevField)),
            KeyCode::Up => Some(Action::ElicitationField(ElicitationEdit::EnumPrev)),
            KeyCode::Down => Some(Action::ElicitationField(ElicitationEdit::EnumNext)),
            KeyCode::Char(' ') => Some(Action::ElicitationField(ElicitationEdit::ToggleBool)),
            KeyCode::Char(c) => Some(Action::ElicitationField(ElicitationEdit::PushChar(c))),
            KeyCode::Backspace => Some(Action::ElicitationField(ElicitationEdit::PopChar)),
            _ => None,
        }
    }

    fn decode_palette_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => Some(Action::CloseCommandPalette),
            KeyCode::Enter => Some(Action::PaletteAccept),
            KeyCode::Up => Some(Action::PaletteMove(VertDir::Up)),
            KeyCode::Down => Some(Action::PaletteMove(VertDir::Down)),
            KeyCode::Backspace => Some(Action::PaletteInput(PaletteEdit::PopChar)),
            KeyCode::Char(c) => Some(Action::PaletteInput(PaletteEdit::PushChar(c))),
            _ => None,
        }
    }

    fn decode_file_picker_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => Some(Action::CloseFilePicker),
            KeyCode::Enter | KeyCode::Tab => Some(Action::FilePickerAccept),
            KeyCode::Up => Some(Action::FilePickerMove(VertDir::Up)),
            KeyCode::Down => Some(Action::FilePickerMove(VertDir::Down)),
            KeyCode::Char(c) => Some(Action::FilePickerInput(PaletteEdit::PushChar(c))),
            KeyCode::Backspace => Some(Action::FilePickerInput(PaletteEdit::PopChar)),
            _ => None,
        }
    }

    #[allow(clippy::too_many_lines)] // large match over all TuiCommand variants
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
                self.cycle_theme();
                self.push_system_message(format!("Theme: {}", self.active_theme_name()));
            }
            TuiCommand::ListThemes => {
                self.push_system_message(
                    "Available themes: zephyr, zephyr-light, high-contrast, classic, \
                     catppuccin-mocha, gruvbox-dark, solarized-dark\n\
                     Usage: /theme <name>"
                        .to_owned(),
                );
            }
            TuiCommand::SetTheme(name) => {
                let name = name.clone();
                match self.apply_theme(&name) {
                    Ok(()) => {
                        self.push_system_message(format!(
                            "Theme switched to: {}",
                            self.active_theme_name()
                        ));
                    }
                    Err(e) => {
                        self.push_system_message(format!("Theme error: {e}"));
                    }
                }
            }
            TuiCommand::SetMotion(m) => {
                self.motion = m;
                let label = match m {
                    zeph_config::Motion::Full => "full (wave animation)",
                    zeph_config::Motion::Minimal => "minimal (breeze spinner)",
                    zeph_config::Motion::Off => "off (static)",
                };
                self.push_system_message(format!("Motion set to: {label}"));
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
            TuiCommand::FleetPanel => {
                self.active_panel = Panel::Fleet;
            }
            TuiCommand::DurablePanel => {
                self.active_panel = Panel::Durable;
            }
            TuiCommand::CocoonStatus => {
                self.push_system_message("Querying Cocoon sidecar...".to_owned());
                let _ = self.user_input_tx.try_send("/cocoon status".to_owned());
            }
            TuiCommand::CocoonModels => {
                self.push_system_message("Querying Cocoon models...".to_owned());
                let _ = self.user_input_tx.try_send("/cocoon models".to_owned());
            }
            TuiCommand::CopyLastAssistant => {
                if let Some(text) = self.last_assistant_content_pub() {
                    match self.clipboard.copy(&text) {
                        Ok(()) => self.push_system_message(
                            "Last assistant message copied to clipboard".to_owned(),
                        ),
                        Err(e) => {
                            self.push_system_message(format!("Copy failed: {e}"));
                        }
                    }
                } else {
                    self.push_system_message("No assistant message to copy.".to_owned());
                }
            }
            TuiCommand::CopyLastCodeBlock(n) => {
                let blocks = self.last_assistant_code_blocks_pub();
                let text = if blocks.is_empty() {
                    None
                } else if n == 0 {
                    blocks.last().cloned()
                } else {
                    blocks.get(n.saturating_sub(1)).cloned()
                };
                if let Some(text) = text {
                    match self.clipboard.copy(&text) {
                        Ok(()) => {
                            self.push_system_message("Code block copied to clipboard".to_owned());
                        }
                        Err(e) => {
                            self.push_system_message(format!("Copy failed: {e}"));
                        }
                    }
                } else {
                    self.push_system_message("No code block found.".to_owned());
                }
            }
            TuiCommand::SendClearQueue => {
                let _ = self.user_input_tx.try_send("/clear-queue".to_owned());
            }
            // SubAgent sidebar navigation (routed from decode_normal_key via Action::Dispatch)
            TuiCommand::SubagentSidebarDown => {
                let count = self.metrics.sub_agents.len();
                self.subagent_sidebar.select_next(count);
            }
            TuiCommand::SubagentSidebarUp => {
                let count = self.metrics.sub_agents.len();
                self.subagent_sidebar.select_prev(count);
            }
            // Mouse toggle: route through reduce() so SetMouseCapture effect is queued.
            TuiCommand::SetMouse(b) => {
                use crate::app::reducer::{reduce, run_effects};
                let effects = reduce(self, crate::app::action::Action::SetMouse(b));
                run_effects(self, effects);
            }
            TuiCommand::ToggleMouse => {
                use crate::app::reducer::{reduce, run_effects};
                let cur = self.mouse_enabled;
                let effects = reduce(self, crate::app::action::Action::SetMouse(!cur));
                run_effects(self, effects);
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
        if self.handle_knowledge_command(&cmd) {
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
                self.push_system_message("Loading plugins...".to_owned());
                let _ = self.user_input_tx.try_send("/plugins list".to_owned());
            }
            TuiCommand::PluginAdd => self.prefill_input("/plugins add "),
            TuiCommand::PluginRemove => self.prefill_input("/plugins remove "),
            TuiCommand::PluginListOverlay => {
                self.push_system_message("Loading plugin overlay...".to_owned());
                let _ = self.user_input_tx.try_send("/plugins overlay".to_owned());
            }
            TuiCommand::SessionSwitchNext
            | TuiCommand::SessionSwitchPrev
            | TuiCommand::SessionClose => self.try_switch(cmd),
            _ => return false,
        }
        true
    }

    fn handle_knowledge_command(&mut self, cmd: &TuiCommand) -> bool {
        match cmd {
            TuiCommand::KnowledgeStatus => {
                self.push_system_message("Loading knowledge ingest status...".to_owned());
                let _ = self.user_input_tx.try_send("/knowledge status".to_owned());
            }
            TuiCommand::KnowledgeRollbackPrompt => {
                self.prefill_input("/knowledge rollback ");
            }
            TuiCommand::KnowledgeIngestPrompt => {
                self.push_system_message(
                    "To ingest project artifacts: run \
                     `zeph knowledge ingest --source <specs|changelog|handoff|coverage|git-log>` \
                     from the CLI."
                        .to_owned(),
                );
            }
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
            TuiCommand::Undo => {
                let _ = self.user_input_tx.try_send("/undo".to_owned());
            }
            TuiCommand::Redo => {
                let _ = self.user_input_tx.try_send("/redo".to_owned());
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
            [cmd] if cmd.eq_ignore_ascii_case("/copy") => Some(TuiCommand::CopyLastAssistant),
            [cmd] if cmd.eq_ignore_ascii_case("/copyblock") => {
                Some(TuiCommand::CopyLastCodeBlock(0))
            }
            [cmd, n] if cmd.eq_ignore_ascii_case("/copyblock") => {
                let idx = n.parse::<usize>().unwrap_or(0);
                Some(TuiCommand::CopyLastCodeBlock(idx))
            }
            // /theme — list presets (bare command, token count == 1)
            [cmd] if cmd.eq_ignore_ascii_case("/theme") => Some(TuiCommand::ListThemes),
            // /theme <name> — switch to named theme (any non-empty name token)
            [cmd, name] if cmd.eq_ignore_ascii_case("/theme") && !name.is_empty() => {
                Some(TuiCommand::SetTheme((*name).to_owned()))
            }
            // /motion <full|minimal|off> — set animation budget at runtime
            [cmd, level]
                if cmd.eq_ignore_ascii_case("/motion")
                    && matches!(
                        level.to_ascii_lowercase().as_str(),
                        "full" | "minimal" | "off"
                    ) =>
            {
                let m = match level.to_ascii_lowercase().as_str() {
                    "minimal" => zeph_config::Motion::Minimal,
                    "off" => zeph_config::Motion::Off,
                    _ => zeph_config::Motion::Full,
                };
                Some(TuiCommand::SetMotion(m))
            }
            // /mouse on|off|toggle — opt-in mouse capture (#5103)
            [cmd] if cmd.eq_ignore_ascii_case("/mouse") => Some(TuiCommand::ToggleMouse),
            [cmd, "on"] if cmd.eq_ignore_ascii_case("/mouse") => Some(TuiCommand::SetMouse(true)),
            [cmd, "off"] if cmd.eq_ignore_ascii_case("/mouse") => Some(TuiCommand::SetMouse(false)),
            _ => None,
        }
    }

    /// Public(crate) wrapper so the reducer can call `parse_session_slash` across modules.
    pub(crate) fn parse_session_slash_pub(text: &str) -> Option<TuiCommand> {
        Self::parse_session_slash(text)
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
                let model_display = truncate_to_width(&usage.model, 26);
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

    /// Decode a key event while the `SubAgents` panel has focus or a subagent
    /// transcript is active. Returns `Some(Action)` when the key is consumed.
    fn decode_subagent_panel_key(&self, key: KeyEvent) -> Option<Action> {
        if self.active_panel == Panel::SubAgents {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    return Some(Action::Dispatch(TuiCommand::SubagentSidebarDown));
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    return Some(Action::Dispatch(TuiCommand::SubagentSidebarUp));
                }
                KeyCode::Enter => {
                    if let Some(idx) = self.subagent_sidebar.selected()
                        && let Some(sa) = self.metrics.sub_agents.get(idx)
                    {
                        let target = AgentViewTarget::SubAgent {
                            id: sa.id.clone(),
                            name: sa.name.clone(),
                        };
                        return Some(Action::SetViewTarget(target));
                    }
                    return None;
                }
                KeyCode::Esc => {
                    return Some(Action::SetActivePanel(Panel::Chat));
                }
                _ => {}
            }
        }
        // Esc while viewing a subagent transcript returns to Main.
        if key.code == KeyCode::Esc && !self.sessions.current().view_target.is_main() {
            return Some(Action::SetViewTarget(AgentViewTarget::Main));
        }
        None
    }

    #[allow(clippy::too_many_lines)]
    fn decode_normal_key(&self, key: KeyEvent) -> Option<Action> {
        if let Some(a) = self.decode_subagent_panel_key(key) {
            return Some(a);
        }
        match key.code {
            KeyCode::Esc if self.is_agent_busy() => Some(Action::CancelAgent),
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Char('H') => Some(Action::Dispatch(TuiCommand::SessionBrowser)),
            KeyCode::Char('i') => Some(Action::EnterInsert),
            KeyCode::Char(':') => Some(Action::OpenCommandPalette),
            KeyCode::Up | KeyCode::Char('k') => Some(Action::ScrollLines(-1)),
            KeyCode::Down | KeyCode::Char('j') => Some(Action::ScrollLines(1)),
            KeyCode::PageUp => Some(Action::ScrollPage(ScrollDir::Up)),
            KeyCode::PageDown => Some(Action::ScrollPage(ScrollDir::Down)),
            KeyCode::Home => Some(Action::ScrollToTop),
            KeyCode::End => Some(Action::ScrollToBottom),
            KeyCode::Char('d') => Some(Action::ToggleSidePanels),
            KeyCode::Char('e') => Some(Action::ToggleToolExpanded),
            KeyCode::Char('c') => Some(Action::CycleToolDensity),
            KeyCode::Tab => Some(Action::CyclePanelFocus),
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::ClearTranscript)
            }
            KeyCode::Char('?') => Some(Action::SetHelp(true)),
            KeyCode::Char('p') => Some(Action::TogglePlanView),
            KeyCode::Char('f') => Some(Action::SetActivePanel(Panel::Fleet)),
            KeyCode::Char('D') => Some(Action::SetActivePanel(Panel::Durable)),
            KeyCode::Char('a') => Some(Action::SetActivePanel(Panel::SubAgents)),
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::CopyLastAssistant)
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::CopyLastCodeBlock(0))
            }
            KeyCode::Char('1') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(0))
            }
            KeyCode::Char('2') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(1))
            }
            KeyCode::Char('3') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(2))
            }
            KeyCode::Char('4') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(3))
            }
            _ => None,
        }
    }

    /// Returns the byte offset of the char at the given char index.
    pub(super) fn byte_offset_of_char(&self, char_idx: usize) -> usize {
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

    fn decode_insert_key(&self, key: KeyEvent) -> Option<Action> {
        // Reverse-search dispatch is checked BEFORE slash-autocomplete so that
        // printable chars (including '/') typed into the search query are not
        // stolen by the autocomplete trigger (C4).
        if self.reverse_search.is_some() {
            return Self::decode_reverse_search_key(key);
        }
        if self.slash_autocomplete.is_some() {
            return Self::decode_slash_autocomplete_key(key);
        }
        if let Some(a) = Self::decode_insert_text_key(key) {
            return Some(a);
        }
        if let Some(a) = Self::decode_insert_delete_key(key) {
            return Some(a);
        }
        if let Some(a) = Self::decode_insert_scroll_key(key) {
            return Some(a);
        }
        if let Some(a) = Self::decode_insert_history_key(key) {
            return Some(a);
        }
        if let Some(a) = Self::decode_insert_cursor_key(key) {
            return Some(a);
        }
        self.decode_insert_control_key(key)
    }

    fn decode_insert_scroll_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::PageUp => Some(Action::ScrollPage(ScrollDir::Up)),
            KeyCode::PageDown => Some(Action::ScrollPage(ScrollDir::Down)),
            _ => None,
        }
    }

    /// Insert a newline character at the current cursor position.
    ///
    /// Shared body for `Shift+Enter` and `Ctrl+J`.
    pub(super) fn insert_newline_at_cursor(&mut self) {
        self.sessions.current_mut().paste_state = None;
        let byte_offset = self.byte_offset_of_char(self.sessions.current().cursor_position);
        self.sessions.current_mut().input.insert(byte_offset, '\n');
        self.sessions.current_mut().cursor_position += 1;
    }

    fn decode_insert_text_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                Some(Action::InsertNewline)
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::InsertNewline)
            }
            KeyCode::Enter => Some(Action::SubmitInput),
            KeyCode::Esc => Some(Action::EnterNormal),
            _ => None,
        }
    }

    fn decode_insert_delete_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::DeleteWordBackward)
            }
            KeyCode::Backspace => Some(Action::DeleteCharBackward),
            KeyCode::Delete => Some(Action::DeleteCharForward),
            _ => None,
        }
    }

    fn decode_insert_history_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Up => Some(Action::HistoryPrev),
            KeyCode::Down => Some(Action::HistoryNext),
            _ => None,
        }
    }

    fn decode_insert_cursor_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::MoveCursor(CursorMove::WordLeft))
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::MoveCursor(CursorMove::WordRight))
            }
            KeyCode::Left => Some(Action::MoveCursor(CursorMove::Left)),
            KeyCode::Right => Some(Action::MoveCursor(CursorMove::Right)),
            KeyCode::Home => Some(Action::MoveCursor(CursorMove::Home)),
            KeyCode::End => Some(Action::MoveCursor(CursorMove::End)),
            _ => None,
        }
    }

    fn decode_insert_control_key(&self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::MoveCursor(CursorMove::Home))
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::MoveCursor(CursorMove::End))
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::ClearInput)
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // /clear-queue is a user-input command, not an Action mutation.
                Some(Action::Dispatch(TuiCommand::SendClearQueue))
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::CopyLastAssistant)
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::CopyLastCodeBlock(0))
            }
            KeyCode::Char('1') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(0))
            }
            KeyCode::Char('2') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(1))
            }
            KeyCode::Char('3') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(2))
            }
            KeyCode::Char('4') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(Action::TogglePanelCollapse(3))
            }
            // Ignore Ctrl+R when slash autocomplete is open — mutual exclusion.
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.slash_autocomplete.is_none() {
                    Some(Action::OpenReverseSearch)
                } else {
                    None
                }
            }
            KeyCode::Char('@') => Some(Action::OpenFilePicker),
            KeyCode::Char(c) => Some(Action::InsertChar(c)),
            _ => None,
        }
    }

    fn decode_slash_autocomplete_key(key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => Some(Action::CloseSlashAutocomplete),
            KeyCode::Tab => Some(Action::SlashAutocompleteAccept),
            KeyCode::Enter => {
                // Accept and immediately submit.
                Some(Action::SlashAutocompleteAcceptAndSubmit)
            }
            KeyCode::Down => Some(Action::SlashAutocompleteMove(VertDir::Down)),
            KeyCode::Up | KeyCode::BackTab => Some(Action::SlashAutocompleteMove(VertDir::Up)),
            KeyCode::Backspace => Some(Action::SlashAutocompletePopChar),
            KeyCode::Char(c) => Some(Action::SlashAutocompletePushChar(c)),
            _ => None,
        }
    }

    fn decode_reverse_search_key(key: KeyEvent) -> Option<Action> {
        let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let is_alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => Some(Action::CloseReverseSearch),
            KeyCode::Enter => Some(Action::ReverseSearchAccept),
            KeyCode::Char('r') if is_ctrl => Some(Action::ReverseSearchNext),
            KeyCode::Char('s') if is_ctrl => Some(Action::ReverseSearchPrev),
            KeyCode::Backspace => Some(Action::ReverseSearchInput(PaletteEdit::PopChar)),
            KeyCode::Char(c) if !is_ctrl && !is_alt => {
                Some(Action::ReverseSearchInput(PaletteEdit::PushChar(c)))
            }
            _ => None,
        }
    }

    pub(super) fn handle_history_up(&mut self) {
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

    pub(super) fn open_file_picker(&mut self) {
        use std::sync::Arc;

        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let needs_rebuild = self.file_index.as_ref().is_none_or(FileIndex::is_stale);
        if needs_rebuild && self.pending_file_index.is_none() {
            self.sessions.current_mut().status_label = Some("indexing files...".to_owned());
            let pending = if let Some(sup) = &self.task_supervisor {
                let handle = sup.spawn_blocking(Arc::from("tui.file_index.build"), move || {
                    FileIndex::build(&root)
                });
                super::PendingFileIndex::Supervised(handle)
            } else {
                // EXEMPT: supervisor not wired (test environments); bare spawn is acceptable here
                // because the oneshot receiver is stored in pending_file_index and polled every tick.
                let (tx, rx) = oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    let _ = tx.send(FileIndex::build(&root));
                });
                super::PendingFileIndex::Bare(rx)
            };
            self.pending_file_index = Some(pending);
            return;
        }
        if let Some(idx) = &self.file_index {
            self.file_picker_state = Some(FilePickerState::new(idx));
        }
    }

    /// Checks if the background file index build has completed and, if so,
    /// installs the result and opens the picker.
    pub fn poll_pending_file_index(&mut self) {
        let Some(pending) = self.pending_file_index.take() else {
            return;
        };
        let poll_result = match pending {
            super::PendingFileIndex::Supervised(handle) => match handle.try_join() {
                Ok(Ok(idx)) => Some(Ok(idx)),
                Ok(Err(_)) => Some(Err(())),
                Err(handle) => {
                    self.pending_file_index = Some(super::PendingFileIndex::Supervised(handle));
                    return;
                }
            },
            super::PendingFileIndex::Bare(mut rx) => match rx.try_recv() {
                Ok(idx) => Some(Ok(idx)),
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.pending_file_index = Some(super::PendingFileIndex::Bare(rx));
                    return;
                }
                Err(oneshot::error::TryRecvError::Closed) => Some(Err(())),
            },
        };
        match poll_result {
            Some(Ok(idx)) => {
                let picker = FilePickerState::new(&idx);
                self.file_index = Some(idx);
                self.file_picker_state = Some(picker);
                self.sessions.current_mut().status_label = None;
            }
            Some(Err(())) | None => {
                self.sessions.current_mut().status_label = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::event::AgentEvent;
    use crate::types::MessageRole;

    fn make_app() -> (App, mpsc::Receiver<String>, mpsc::Sender<AgentEvent>) {
        let (user_tx, user_rx) = mpsc::channel(16);
        let (agent_tx, agent_rx) = mpsc::channel(16);
        let mut app = App::new(user_tx, agent_rx);
        app.sessions.current_mut().messages.clear();
        (app, user_rx, agent_tx)
    }

    #[test]
    fn last_assistant_content_returns_none_when_empty() {
        let (app, _rx, _tx) = make_app();
        assert_eq!(app.last_assistant_content_pub(), None);
    }

    #[test]
    fn last_assistant_content_returns_none_when_only_user_messages() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::User, "hello"));
        assert_eq!(app.last_assistant_content_pub(), None);
    }

    #[test]
    fn last_assistant_content_returns_latest() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::Assistant, "first"));
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::User, "follow-up"));
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::Assistant, "second"));
        assert_eq!(app.last_assistant_content_pub(), Some("second".to_owned()));
    }

    #[test]
    fn slash_copy_parses_to_copy_last_assistant() {
        assert_eq!(
            App::parse_session_slash("/copy"),
            Some(TuiCommand::CopyLastAssistant)
        );
    }

    #[test]
    fn slash_copy_case_insensitive() {
        assert_eq!(
            App::parse_session_slash("/COPY"),
            Some(TuiCommand::CopyLastAssistant)
        );
    }

    #[test]
    fn slash_unknown_returns_none() {
        assert_eq!(App::parse_session_slash("/unknown"), None);
    }

    #[test]
    fn slash_theme_bare_lists_themes() {
        assert_eq!(
            App::parse_session_slash("/theme"),
            Some(TuiCommand::ListThemes)
        );
    }

    #[test]
    fn slash_theme_with_name_sets_theme() {
        assert_eq!(
            App::parse_session_slash("/theme zephyr"),
            Some(TuiCommand::SetTheme("zephyr".to_owned()))
        );
    }

    #[test]
    fn slash_theme_trailing_space_lists_themes() {
        assert_eq!(
            App::parse_session_slash("/theme "),
            Some(TuiCommand::ListThemes)
        );
    }
}
