// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/trajectory` command handler, plus the [`zeph_commands::TrackingAccess`] implementation
//! for [`Agent<C>`] (`/trajectory`, `/scope`, `/goal`).
//!
//! Operator-only: score, level, and alert data MUST NOT appear in LLM context.
//!
//! [`Agent<C>`]: super::Agent

use std::future::Future;
use std::pin::Pin;

use zeph_commands::{CommandError, TrackingAccess};

use super::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Handle `/trajectory [status|reset]` and return a user-visible result.
    pub(super) fn handle_trajectory_command_as_string(&mut self, args: &str) -> String {
        let subcmd = args.split_whitespace().next().unwrap_or("status");
        if subcmd == "reset" {
            // S-HIGH-02: reset is operator-only; refuse from ACP/LLM-callable sessions.
            if self.services.security.is_acp_session {
                return "Permission denied: /trajectory reset is operator-only.".to_owned();
            }
            self.services.security.trajectory.reset();
            *self.services.security.trajectory_risk_slot.write() = 0;
            "Trajectory sentinel reset.".to_owned()
        } else {
            let level = self.services.security.trajectory.current_risk();
            let score = self.services.security.trajectory.score_now();
            let turn = self.services.security.trajectory.current_turn();
            let signals = self.services.security.trajectory.signal_count();
            format!(
                "Trajectory: level={level:?}, score={score:.2}, turn={turn}, signals_in_window={signals}"
            )
        }
    }
}

type GoalStore = crate::goal::GoalStore;
type GoalAccounting = crate::goal::GoalAccounting;

/// Hard cap on `--turns` to prevent runaway autonomous loops (Security Low).
const AUTONOMOUS_MAX_TURNS_CAP: u32 = 1000;

async fn goal_status(accounting: &GoalAccounting) -> Result<String, CommandError> {
    match accounting.get_active().await {
        Ok(Some(g)) => {
            let budget_line = g.token_budget.map_or_else(
                || format!("  tokens used: {}", g.tokens_used),
                |b| format!("  budget: {}/{b}", g.tokens_used),
            );
            Ok(format!(
                "Active goal [{}]: {}\n  status: {}\n  turns: {}\n{}",
                &g.id[..8],
                g.text,
                g.status,
                g.turns_used,
                budget_line
            ))
        }
        Ok(None) => Ok("No active goal. Use `/goal create <text>` to set one.".to_owned()),
        Err(e) => Ok(format!("Goal lookup failed: {e}")),
    }
}

/// Returns `(display_message, auto_start_request)`.
///
/// `auto_start_request` is `Some((goal_id, goal_text, max_turns))` when `--auto` was passed and
/// the goal was successfully created. The caller must relay this to `AutonomousDriver` via the
/// `pending_start_arc` side-channel before the future resolves.
async fn goal_create(
    args: &str,
    accounting: &GoalAccounting,
    store: &GoalStore,
    max_chars: usize,
    default_budget: Option<u64>,
    autonomous_enabled: bool,
    autonomous_max_turns: u32,
) -> Result<(String, Option<(String, String, u32)>), CommandError> {
    let rest = args.strip_prefix("create").unwrap_or("").trim();

    // Strip --auto / --turns before passing text to the budget parser.
    let (stripped, is_auto, explicit_turns) = parse_auto_flags(rest);
    let (text, explicit_budget) = parse_goal_create_args(&stripped);

    if text.is_empty() {
        return Ok((
            "Usage: /goal create <text> [--budget N] [--auto [--turns N]]".to_owned(),
            None,
        ));
    }
    if is_auto && !autonomous_enabled {
        return Ok((
            "Autonomous mode is disabled. Set `[goals] autonomous_enabled = true` in config."
                .to_owned(),
            None,
        ));
    }
    let budget = explicit_budget.or(default_budget.filter(|&b| b > 0));

    let max_turns = explicit_turns
        .unwrap_or(autonomous_max_turns)
        .min(AUTONOMOUS_MAX_TURNS_CAP);
    if explicit_turns.is_some_and(|t| t > AUTONOMOUS_MAX_TURNS_CAP) {
        tracing::warn!(
            requested = explicit_turns,
            capped = AUTONOMOUS_MAX_TURNS_CAP,
            "autonomous max_turns capped to {AUTONOMOUS_MAX_TURNS_CAP}"
        );
    }

    match store.create(text, budget, max_chars).await {
        Ok(g) => {
            let _ = accounting.refresh().await;
            let auto_start = if is_auto {
                Some((g.id.clone(), g.text.clone(), max_turns))
            } else {
                None
            };
            let auto_note = if is_auto {
                " Autonomous mode enabled — use `/goal clear` to stop."
            } else {
                ""
            };
            Ok((
                format!("Goal created [{}]: {}{auto_note}", &g.id[..8], g.text),
                auto_start,
            ))
        }
        Err(crate::goal::store::GoalError::TextTooLong { max }) => Ok((
            format!("Goal text exceeds {max} characters. Please shorten it."),
            None,
        )),
        Err(e) => Ok((format!("Failed to create goal: {e}"), None)),
    }
}

async fn goal_pause(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    match accounting.get_active().await {
        Ok(Some(g)) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Paused, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] paused.", &g.id[..8]))
                }
                Err(crate::goal::store::GoalError::StaleUpdate(_)) => {
                    let current = accounting.get_active().await.ok().flatten();
                    Ok(format!(
                        "Goal state changed concurrently. Current: {}",
                        current.map_or_else(|| "none".into(), |g| g.status.to_string())
                    ))
                }
                Err(e) => Ok(format!("Pause failed: {e}")),
            }
        }
        Ok(None) => Ok("No active goal to pause.".to_owned()),
        Err(e) => Ok(format!("Failed: {e}")),
    }
}

async fn goal_resume(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    let goals = store.list(10).await.unwrap_or_default();
    let paused = goals
        .into_iter()
        .find(|g| g.status == crate::goal::GoalStatus::Paused);
    match paused {
        Some(g) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Active, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] resumed: {}", &g.id[..8], g.text))
                }
                Err(crate::goal::store::GoalError::StaleUpdate(_)) => {
                    Ok("Goal state changed concurrently — please retry.".to_owned())
                }
                Err(e) => Ok(format!("Resume failed: {e}")),
            }
        }
        None => Ok("No paused goal to resume.".to_owned()),
    }
}

async fn goal_complete(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    match accounting.get_active().await {
        Ok(Some(g)) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Completed, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] marked complete.", &g.id[..8]))
                }
                Err(e) => Ok(format!("Complete failed: {e}")),
            }
        }
        Ok(None) => Ok("No active goal.".to_owned()),
        Err(e) => Ok(format!("Failed: {e}")),
    }
}

async fn goal_clear(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    let goals = store.list(10).await.unwrap_or_default();
    let target = goals.into_iter().find(|g| {
        g.status == crate::goal::GoalStatus::Active || g.status == crate::goal::GoalStatus::Paused
    });
    match target {
        Some(g) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Cleared, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] cleared.", &g.id[..8]))
                }
                Err(e) => Ok(format!("Clear failed: {e}")),
            }
        }
        None => Ok("No active or paused goal to clear.".to_owned()),
    }
}

async fn goal_list(store: &GoalStore) -> Result<String, CommandError> {
    let goals = store.list(20).await.unwrap_or_default();
    if goals.is_empty() {
        return Ok("No goals recorded.".to_owned());
    }
    let mut out = String::from("Goals:\n");
    for g in goals {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "  {} [{}] {} — {} turns\n",
                g.status.badge_symbol(),
                &g.id[..8],
                g.text,
                g.turns_used
            ),
        );
    }
    Ok(out.trim_end().to_owned())
}

fn parse_goal_create_args(args: &str) -> (&str, Option<u64>) {
    if let Some(pos) = args.find("--budget") {
        let text = args[..pos].trim();
        let rest = args[pos + "--budget".len()..].trim();
        let budget = rest
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok());
        (text, budget)
    } else {
        (args, None)
    }
}

/// Parse `--auto` and `--turns N` flags from the remainder of a `/goal create` argument string.
///
/// Returns `(text_without_auto_flags, is_auto, explicit_turns)`.
fn parse_auto_flags(args: &str) -> (String, bool, Option<u32>) {
    let mut is_auto = false;
    let mut turns: Option<u32> = None;
    let mut text_words: Vec<&str> = Vec::new();
    let mut words = args.split_whitespace();

    while let Some(w) = words.next() {
        if w == "--auto" {
            is_auto = true;
        } else if w == "--turns" {
            turns = words.next().and_then(|n| n.parse::<u32>().ok());
        } else {
            text_words.push(w);
        }
    }

    (text_words.join(" "), is_auto, turns)
}

impl<C: Channel + Send + 'static> TrackingAccess for Agent<C> {
    // ----- /trajectory -----

    fn handle_trajectory(&mut self, args: &str) -> String {
        self.handle_trajectory_command_as_string(args)
    }

    // ----- /scope -----

    fn handle_scope(&self, args: &str) -> String {
        self.handle_scope_command_as_string(args)
    }

    // ----- /goal -----

    fn handle_goal<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        // Extract all non-Send data synchronously before entering the async block.
        if self.services.goal_accounting.is_none() {
            if !self.runtime.config.goals.enabled {
                return Box::pin(async {
                    Ok("Goals are disabled. Set `[goals] enabled = true` in config.".to_owned())
                });
            }
            let pool = match self.services.memory.persistence.memory.as_ref() {
                Some(m) => std::sync::Arc::new(m.sqlite().pool().clone()),
                None => {
                    return Box::pin(async {
                        Ok("Goals require a database backend (memory not configured).".to_owned())
                    });
                }
            };
            let store = std::sync::Arc::new(crate::goal::GoalStore::new(pool));
            let accounting = std::sync::Arc::new(crate::goal::GoalAccounting::new(store));
            self.services.goal_accounting = Some(accounting);
        }

        let accounting =
            self.services.goal_accounting.clone().expect(
                "invariant: goal_accounting is always Some at this point (initialized above)",
            );
        let max_chars = self.runtime.config.goals.max_text_chars;
        let default_budget = self.runtime.config.goals.default_token_budget;
        let autonomous_enabled = self.runtime.config.goals.autonomous_enabled;
        let autonomous_max_turns = self.runtime.config.goals.autonomous_max_turns;
        let args_owned = args.to_owned();

        // S1: `goal_create` may need to arm `AutonomousDriver` with a new session.
        // We capture a clone of the pending_start Arc that lives on the driver.
        // The async block fills it; the main agent loop (which has `&mut self`) drains it
        // via `AutonomousDriver::flush_pending_start()` after each command handler returns.
        let pending_start_arc = std::sync::Arc::clone(&self.services.autonomous.pending_start_arc);

        Box::pin(async move {
            let _ = accounting.refresh().await;
            let store = accounting.get_store();
            let args = args_owned.as_str();

            match args {
                "" | "status" => goal_status(&accounting).await,
                "pause" => goal_pause(&accounting, &store).await,
                "resume" => goal_resume(&accounting, &store).await,
                "complete" => goal_complete(&accounting, &store).await,
                "clear" => goal_clear(&accounting, &store).await,
                "list" => goal_list(&store).await,
                _ if args.starts_with("create") => {
                    let (msg, auto_req) = goal_create(
                        args,
                        &accounting,
                        &store,
                        max_chars,
                        default_budget,
                        autonomous_enabled,
                        autonomous_max_turns,
                    )
                    .await?;
                    if let Some(req) = auto_req {
                        *pending_start_arc.lock() = Some(req);
                    }
                    Ok(msg)
                }
                _ => Ok(
                    "Unknown /goal subcommand. Try: create, pause, resume, complete, clear, status, list."
                        .to_owned(),
                ),
            }
        })
    }

    fn active_goal_snapshot(&self) -> Option<zeph_commands::GoalSnapshot> {
        let accounting = self.services.goal_accounting.as_ref()?;
        let snap = accounting.snapshot()?;
        Some(zeph_commands::GoalSnapshot {
            id: snap.id,
            text: snap.text,
            status: match snap.status {
                crate::goal::GoalStatus::Active => zeph_commands::GoalStatusView::Active,
                crate::goal::GoalStatus::Paused => zeph_commands::GoalStatusView::Paused,
                crate::goal::GoalStatus::Completed => zeph_commands::GoalStatusView::Completed,
                crate::goal::GoalStatus::Cleared => zeph_commands::GoalStatusView::Cleared,
            },
            turns_used: snap.turns_used,
            tokens_used: snap.tokens_used,
            token_budget: snap.token_budget,
        })
    }
}
