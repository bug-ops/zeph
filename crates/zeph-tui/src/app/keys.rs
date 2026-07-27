// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(super) const SCROLL_STEP_PAGE: usize = 10;

use crate::app::action::{
    Action, CursorMove, ElicitationEdit, HorizDir, PaletteEdit, ScrollDir, VertDir,
};
use crate::app::reducer::{reduce, run_effects};
use crate::command::TuiCommand;
use crate::file_picker::FileIndex;
use crate::layout::truncate_to_width;

use super::{
    AgentViewTarget, App, ChatMessage, InputMode, MessageRole, Panel, PasteState, oneshot,
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
        // Global: Ctrl-C cancels a busy agent turn immediately; when idle it arms a
        // double-press quit window (see `Action::RequestQuit`, reducer.rs).
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(if self.is_agent_busy() {
                Action::CancelAgent
            } else {
                Action::RequestQuit
            });
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

        // Transcript search (issue #6023): routed mode-agnostically at the top level
        // (unlike reverse-search, which is Insert-only) so Ctrl+F works whether it was
        // opened from Normal or Insert mode, and so the two overlays are mutually
        // exclusive — while this one is open, all keys route here, so Ctrl+R cannot
        // open reverse-search underneath it (the inverse is guarded by the Ctrl+F
        // open-arms' `reverse_search.is_none()` check).
        if self.transcript_search.is_some() {
            return Self::decode_transcript_search_key(key);
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

    #[allow(clippy::too_many_lines)] // large match over all TuiCommand variants
    pub(super) fn execute_command(&mut self, cmd: TuiCommand) {
        match cmd {
            TuiCommand::ViewConfig
            | TuiCommand::ViewAutonomy
            | TuiCommand::SandboxStatus
            | TuiCommand::TafcStatus => {
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
            TuiCommand::ToggleTheme => {
                self.cycle_theme();
                self.push_system_message(format!("Theme: {}", self.active_theme_name()));
            }
            TuiCommand::SetTheme(name) => {
                let name = name.clone();
                match self.apply_theme(&name) {
                    Ok(true) => {
                        // Preset applied immediately.
                        self.push_system_message(format!(
                            "Theme switched to: {}",
                            self.active_theme_name()
                        ));
                    }
                    Ok(false) => {
                        // User file load dispatched; confirmation arrives via poll_pending_theme.
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
                // Dispatched as a normal user-input command (like AgentList/AgentStatus below)
                // rather than routed through `command_tx` — `/history` needs real access to
                // `ctx.messages` (the agent's own message state), which only the session/debug
                // command registry provides; `forward_tui_commands` (`command_tx` path) only
                // handles TUI-local, agent-state-free commands (spec-068 §13.7).
                let _ = self.user_input_tx.try_send("/history".to_owned());
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
        if self.handle_graph_command(&cmd) {
            return;
        }
        if self.handle_experiment_command(&cmd) {
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
        // mention_picker joins this block (not the resync predicate) because
        // `input`/`cursor_position` are session-local while `mention_picker` is a
        // global `App` field — a resync could otherwise re-derive a valid-looking span
        // from the *other* session's buffer after the switch (S2).
        self.command_palette = None;
        self.mention_picker = None;
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

    pub(crate) fn format_skill_list(&self) -> String {
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

    pub(crate) fn format_mcp_list(&self) -> String {
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

    pub(crate) fn format_memory_stats(&self) -> String {
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

    pub(crate) fn format_cost_stats(&self) -> String {
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

    pub(crate) fn format_latency_stats(&self) -> String {
        use std::fmt::Write as _;

        if self.metrics.timing_sample_count == 0 {
            return "No turn-timing samples recorded yet.".to_owned();
        }
        let avg = &self.metrics.avg_turn_timings;
        let max = &self.metrics.max_turn_timings;
        let mut out = format!(
            "Turn latency (rolling avg/max over last {} turn(s)):\n  {:<10} {:>9} {:>9}",
            self.metrics.timing_sample_count, "phase", "avg", "max"
        );
        for (label, avg_ms, max_ms) in [
            ("context", avg.prepare_context_ms, max.prepare_context_ms),
            ("llm", avg.llm_chat_ms, max.llm_chat_ms),
            ("tool", avg.tool_exec_ms, max.tool_exec_ms),
            ("persist", avg.persist_message_ms, max.persist_message_ms),
        ] {
            let _ = write!(out, "\n  {label:<10} {avg_ms:>7}ms {max_ms:>7}ms");
        }

        let c = &self.metrics.classifier;
        let tasks = [
            ("injection", &c.injection),
            ("pii", &c.pii),
            ("feedback", &c.feedback),
        ];
        if tasks.iter().any(|(_, t)| t.call_count > 0) {
            let _ = write!(out, "\n\nClassifier latency (p50/p95):");
            for (label, task) in tasks {
                if task.call_count == 0 {
                    continue;
                }
                let p50 = task
                    .p50_ms
                    .map_or_else(|| "-".to_owned(), |v| format!("{v}ms"));
                let p95 = task
                    .p95_ms
                    .map_or_else(|| "-".to_owned(), |v| format!("{v}ms"));
                let _ = write!(
                    out,
                    "\n  {label:<10} calls:{:<5} p50:{p50:>6} p95:{p95:>6}",
                    task.call_count
                );
            }
        } else {
            out.push_str("\n\nClassifier latency: no samples recorded yet.");
        }
        out
    }

    pub(crate) fn format_tool_list(&self) -> String {
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

    pub(crate) fn format_scheduler_list(&self) -> String {
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

    pub(crate) fn format_router_stats(&self) -> String {
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

    /// Decode a key event while the read-only `Settings` panel has focus (issue #6024).
    /// Mirrors [`decode_subagent_panel_key`]: `Left`/`Right`/`h`/`l` switch tabs,
    /// `j`/`k`/`Down`/`Up` move the row selection, `Esc` returns to `Chat`. No mutation
    /// keys — v1 is read-only.
    fn decode_settings_panel_key(&self, key: KeyEvent) -> Option<Action> {
        if self.active_panel != Panel::Settings {
            return None;
        }
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => Some(Action::SettingsTabPrev),
            KeyCode::Right | KeyCode::Char('l') => Some(Action::SettingsTabNext),
            KeyCode::Down | KeyCode::Char('j') => Some(Action::SettingsSelectMove(VertDir::Down)),
            KeyCode::Up | KeyCode::Char('k') => Some(Action::SettingsSelectMove(VertDir::Up)),
            KeyCode::Esc => Some(Action::SetActivePanel(Panel::Chat)),
            _ => None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn decode_normal_key(&self, key: KeyEvent) -> Option<Action> {
        if let Some(a) = self.decode_subagent_panel_key(key) {
            return Some(a);
        }
        if let Some(a) = self.decode_settings_panel_key(key) {
            return Some(a);
        }
        match key.code {
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
            // Ctrl+F (transcript search, issue #6023) must be checked BEFORE the plain
            // `f`->Fleet arm below, which is itself guarded with `!CONTROL` so it no
            // longer swallows Ctrl+F (mirrors the Ctrl+L precedent above).
            KeyCode::Char('f')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.reverse_search.is_none() =>
            {
                Some(Action::OpenTranscriptSearch)
            }
            KeyCode::Char('?') => Some(Action::SetHelp(true)),
            KeyCode::Char('p') => Some(Action::TogglePlanView),
            KeyCode::Char('f') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::SetActivePanel(Panel::Fleet))
            }
            KeyCode::Char('D') => Some(Action::SetActivePanel(Panel::Durable)),
            KeyCode::Char('S') => Some(Action::SetActivePanel(Panel::Settings)),
            KeyCode::Char('a') => Some(Action::SetActivePanel(Panel::SubAgents)),
            KeyCode::Char('t') => Some(Action::ToggleTaskPanel),
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
        // Mention picker is an Insert-mode overlay, not a modal: only Esc/Tab/Enter/
        // arrows are intercepted here (`None` for anything else), so Space, Backspace,
        // Delete, Home/End, Alt+arrows and Ctrl+* all fall through to normal Insert
        // decoding below and rely on `sync_mention_picker` to close/refilter as needed.
        if self.mention_picker.is_some()
            && let Some(a) = Self::decode_mention_picker_key(key)
        {
            return Some(a);
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
            // Ctrl+F (transcript search, issue #6023): must precede the `Char(c)`
            // catch-all below, which has no modifier guard and would otherwise insert
            // a literal 'f' into the input. Mutual exclusion with Ctrl+R mirrors the
            // arm above.
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.slash_autocomplete.is_none() {
                    Some(Action::OpenTranscriptSearch)
                } else {
                    None
                }
            }
            KeyCode::Char(c) => Some(Action::InsertChar(c)),
            _ => None,
        }
    }

    /// Key routing for the open mention-picker popup (NFR-005/invariant 4): `Esc`
    /// closes without submitting, is checked before `decode_insert_text_key`'s
    /// `Esc → EnterNormal` fallback so Insert mode is retained. `Tab`/`Enter` accept
    /// (unlike slash-autocomplete, accepting a mention never auto-submits). Plain
    /// `Left`/`Right` cycle tabs (FR-004/D2) rather than moving the cursor — but
    /// `Alt+Left`/`Alt+Right` (word-boundary cursor movement) are deliberately
    /// excluded here so they fall through to normal Insert-mode decoding, where
    /// `sync_mention_picker` closes the popup if they exit the `@query` span.
    /// Everything else returns `None` and falls through the same way.
    fn decode_mention_picker_key(key: KeyEvent) -> Option<Action> {
        let is_alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => Some(Action::CloseMentionPicker),
            KeyCode::Tab | KeyCode::Enter => Some(Action::MentionPickerAccept),
            KeyCode::Up => Some(Action::MentionPickerMove(VertDir::Up)),
            KeyCode::Down => Some(Action::MentionPickerMove(VertDir::Down)),
            KeyCode::Left if !is_alt => Some(Action::MentionPickerTabChange(HorizDir::Left)),
            KeyCode::Right if !is_alt => Some(Action::MentionPickerTabChange(HorizDir::Right)),
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

    /// Decode a key event while the transcript-search overlay is open (issue #6023).
    /// Mirrors [`decode_reverse_search_key`]: `Esc` cancels, `Enter` accepts,
    /// `Ctrl+F`/`Down` advance to the next match, `Up` moves to the previous match.
    fn decode_transcript_search_key(key: KeyEvent) -> Option<Action> {
        let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let is_alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => Some(Action::CloseTranscriptSearch),
            KeyCode::Enter => Some(Action::TranscriptSearchAccept),
            KeyCode::Char('f') if is_ctrl => Some(Action::TranscriptSearchNext),
            KeyCode::Down => Some(Action::TranscriptSearchNext),
            KeyCode::Up => Some(Action::TranscriptSearchPrev),
            KeyCode::Backspace => Some(Action::TranscriptSearchInput(PaletteEdit::PopChar)),
            KeyCode::Char(c) if !is_ctrl && !is_alt => {
                Some(Action::TranscriptSearchInput(PaletteEdit::PushChar(c)))
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

    /// Kicks off the background file-index build if needed. Never opens the mention
    /// picker itself (that already happened synchronously in the reducer's `InsertChar`
    /// arm) — this is the race-free fix for FR-011/NFR-004: no keystroke path ever
    /// depends on the index arriving.
    pub(super) fn ensure_file_index(&mut self) {
        use std::sync::Arc;

        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let needs_rebuild = self.file_index.as_ref().is_none_or(FileIndex::is_stale);
        if !needs_rebuild || self.pending_file_index.is_some() {
            return;
        }
        self.sessions.current_mut().status_label = Some("indexing files...".to_owned());
        // Status change counts as progress so the wave animates (never reads Stalled).
        self.last_progress_at = std::time::Instant::now();
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
    }

    /// Checks if the background file index build has completed and, if so, installs
    /// the result and refreshes an open mention picker's Files category (FR-011/
    /// NFR-004) — seamless transition, no input loss even if the popup opened before
    /// the index was ready.
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
                let files_arc = idx.paths_arc();
                self.file_index = Some(idx);
                self.sessions.current_mut().status_label = None;
                if self.mention_picker.is_some() {
                    let query = crate::app::reducer::mention_picker_query(self);
                    if let Some(picker) = self.mention_picker.as_mut() {
                        picker.catalog.files = Some(files_arc);
                        picker.refilter(&query);
                    }
                }
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

    // ── #5983 SandboxStatus/TafcStatus dispatch (was silently dropped) ──────────

    #[test]
    fn execute_command_forwards_sandbox_status_through_command_tx() {
        let (mut app, _user_rx, _agent_tx) = make_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        app.command_tx = Some(cmd_tx);

        app.execute_command(TuiCommand::SandboxStatus);

        let forwarded = cmd_rx.try_recv().expect("command must be forwarded");
        assert_eq!(forwarded, TuiCommand::SandboxStatus);
        assert!(
            app.sessions.current().messages.is_empty(),
            "must not fall back to a stub system message when command_tx is wired"
        );
    }

    #[test]
    fn execute_command_forwards_tafc_status_through_command_tx() {
        let (mut app, _user_rx, _agent_tx) = make_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        app.command_tx = Some(cmd_tx);

        app.execute_command(TuiCommand::TafcStatus);

        let forwarded = cmd_rx.try_recv().expect("command must be forwarded");
        assert_eq!(forwarded, TuiCommand::TafcStatus);
        assert!(app.sessions.current().messages.is_empty());
    }

    #[test]
    fn execute_command_sandbox_status_falls_back_without_command_tx() {
        // No command_tx wired (e.g. constructed without `with_command_tx`) — must report
        // via a system message instead of silently dropping the command.
        let (mut app, _user_rx, _agent_tx) = make_app();
        assert!(app.command_tx.is_none());

        app.execute_command(TuiCommand::SandboxStatus);

        let msg = &app.sessions.current().messages.last().unwrap().content;
        assert!(msg.contains("not available"));
    }

    // ── #6420 SessionBrowser dispatches /history as user input ──────────────────

    #[test]
    fn execute_command_session_browser_dispatches_history_as_user_input() {
        let (mut app, mut user_rx, _agent_tx) = make_app();

        app.execute_command(TuiCommand::SessionBrowser);

        let forwarded = user_rx.try_recv().expect("must forward as user input");
        assert_eq!(forwarded, "/history");
    }

    // ── Ctrl+F / Ctrl+R key-decode routing (issue #6023) ────────────────────────
    //
    // SC-001 of spec 060 explicitly asks for a regression test proving Ctrl+R is
    // unaffected by the new Ctrl+F binding, plus the edge case of the two overlays
    // being mutually exclusive. These decode `KeyEvent`s directly through the private
    // `decode_key` entry point (accessible from this submodule) rather than the full
    // `handle_key` -> `reduce` -> `run_effects` pipeline, isolating the routing logic.

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn plain_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_f_in_normal_mode_opens_transcript_search_not_fleet() {
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;

        let action = app.decode_key(ctrl_key('f'));

        assert_eq!(action, Some(Action::OpenTranscriptSearch));
    }

    #[test]
    fn plain_f_in_normal_mode_still_opens_fleet() {
        // Regression: the `!CONTROL` guard added to the plain-`f` arm must not affect
        // unmodified `f` — it must still open the Fleet panel exactly as before #6023.
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;

        let action = app.decode_key(plain_key('f'));

        assert_eq!(action, Some(Action::SetActivePanel(Panel::Fleet)));
    }

    #[test]
    fn plain_t_in_normal_mode_toggles_task_panel() {
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;

        let action = app.decode_key(plain_key('t'));

        assert_eq!(action, Some(Action::ToggleTaskPanel));
    }

    #[test]
    fn ctrl_f_in_insert_mode_opens_transcript_search_not_literal_char() {
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;

        let action = app.decode_key(ctrl_key('f'));

        assert_eq!(
            action,
            Some(Action::OpenTranscriptSearch),
            "must not fall through to the InsertChar('f') catch-all"
        );
    }

    #[test]
    fn ctrl_r_in_insert_mode_still_opens_reverse_search() {
        // SC-001 regression: Ctrl+R behavior must be completely unaffected by #6023.
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;

        let action = app.decode_key(ctrl_key('r'));

        assert_eq!(action, Some(Action::OpenReverseSearch));
    }

    #[test]
    fn ctrl_f_is_noop_while_reverse_search_is_open() {
        // Mutual exclusion (spec 060 edge-case table): opening transcript search while
        // ReverseSearchState is already open must not succeed.
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.reverse_search = Some(crate::widgets::reverse_search::ReverseSearchState::new(&[]));

        let action = app.decode_key(ctrl_key('f'));

        assert_eq!(
            action, None,
            "Ctrl+F must not open transcript search while reverse-search is active"
        );
    }

    #[test]
    fn ctrl_r_is_noop_while_transcript_search_is_open() {
        // Inverse of the above: once transcript search is open, ALL keys route to its
        // own decoder (top-level `decode_key` short-circuit), so Ctrl+R cannot open
        // reverse-search underneath it.
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.transcript_search =
            Some(crate::widgets::transcript_search::TranscriptSearchState::new(0));

        let action = app.decode_key(ctrl_key('r'));

        assert_eq!(
            action, None,
            "Ctrl+R must not open reverse-search while transcript search is active"
        );
    }

    #[test]
    fn esc_closes_transcript_search_when_open() {
        let (mut app, _user_rx, _agent_tx) = make_app();
        app.transcript_search =
            Some(crate::widgets::transcript_search::TranscriptSearchState::new(0));

        let action = app.decode_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(action, Some(Action::CloseTranscriptSearch));
    }
}
