// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use dialoguer::{Confirm, Input, Select};
use zeph_subagent::def::{MemoryScope, PermissionMode};

use super::WizardState;

pub(super) fn step_orchestration(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Orchestration (/plan command) ==\n");

    state.orchestration_enabled = Confirm::new()
        .with_prompt("Enable task orchestration? (enables the /plan command)")
        .default(false)
        .interact()?;

    if state.orchestration_enabled {
        state.orchestration_max_tasks = Input::new()
            .with_prompt("Maximum tasks per plan")
            .default(20u32)
            .interact_text()?;

        state.orchestration_max_parallel = Input::new()
            .with_prompt("Maximum parallel tasks")
            .default(4u32)
            .interact_text()?;

        // MF6: warn if max_parallel > max_tasks.
        if state.orchestration_max_parallel > state.orchestration_max_tasks {
            println!(
                "Warning: max_parallel ({}) is greater than max_tasks ({}). \
                 Setting max_parallel = max_tasks.",
                state.orchestration_max_parallel, state.orchestration_max_tasks
            );
            state.orchestration_max_parallel = state.orchestration_max_tasks;
        }

        state.orchestration_confirm_before_execute = Confirm::new()
            .with_prompt("Require confirmation before executing plans?")
            .default(true)
            .interact()?;

        let strategies = ["abort", "retry", "skip", "ask"];
        let strategy_idx = Select::new()
            .with_prompt("Default failure strategy")
            .items(strategies)
            .default(0)
            .interact()?;
        state.orchestration_failure_strategy = strategies[strategy_idx].into();

        let provider: String = Input::new()
            .with_prompt("Provider name for planning LLM calls (empty = primary provider)")
            .default(String::new())
            .interact_text()?;
        // Validate provider name: alphanumeric + `-_`, max 64 chars.
        state.orchestration_planner_provider = if provider.is_empty() {
            None
        } else if provider.len() > 64
            || !provider
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            println!(
                "Warning: provider name contains invalid characters or exceeds 64 chars. \
                 Ignoring and using the primary provider."
            );
            None
        } else {
            Some(provider)
        };
    }

    println!();
    Ok(())
}

pub(super) fn step_agents(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 9/10: Sub-Agent Defaults ==\n");

    let modes = ["default", "accept_edits", "dont_ask"];
    let sel = Select::new()
        .with_prompt("Default permission mode for sub-agents")
        .items(modes)
        .default(0)
        .interact()?;
    state.agents_default_permission_mode = match sel {
        1 => Some(PermissionMode::AcceptEdits),
        2 => Some(PermissionMode::DontAsk),
        _ => None,
    };

    let tools_raw: String = Input::new()
        .with_prompt("Globally disallowed tools (comma-separated, leave empty for none)")
        .default(String::new())
        .interact_text()?;
    state.agents_default_disallowed_tools = tools_raw
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    state.agents_allow_bypass_permissions = Confirm::new()
        .with_prompt("Allow sub-agents to use bypass_permissions mode?")
        .default(false)
        .interact()?;

    let user_dir_raw: String = Input::new()
        .with_prompt(
            "User-level agents directory (absolute path, leave empty for platform default)",
        )
        .default(String::new())
        .interact_text()?;
    state.agents_user_dir = if user_dir_raw.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(user_dir_raw.trim()))
    };

    let memory_scopes = ["none", "local", "project", "user"];
    let memory_sel = Select::new()
        .with_prompt("Default memory scope for sub-agents (none = no memory by default)")
        .items(memory_scopes)
        .default(0)
        .interact()?;
    state.agents_default_memory_scope = match memory_sel {
        1 => Some(MemoryScope::Local),
        2 => Some(MemoryScope::Project),
        3 => Some(MemoryScope::User),
        _ => None,
    };

    println!();
    Ok(())
}

pub(super) fn step_router(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 10/12: Provider Router ==\n");
    println!("Configure adaptive routing when using multiple LLM providers.");
    println!("Note: routing only takes effect when [llm.router].chain has 2+ providers.");
    println!("Skip this step if you use a single provider.\n");

    let strategy_items = &[
        "None (single provider, no routing)",
        "EMA (latency-aware exponential moving average)",
        "Thompson (probabilistic exploration/exploitation)",
        "Cascade (try cheapest provider first, escalate on degenerate output)",
    ];
    let sel = Select::new()
        .with_prompt("Router strategy")
        .items(strategy_items)
        .default(0)
        .interact()?;

    match sel {
        0 => {
            state.router_strategy = None;
        }
        1 => {
            state.router_strategy = Some("ema".into());
        }
        2 => {
            state.router_strategy = Some("thompson".into());
            let custom_path: String = Input::new()
                .with_prompt(
                    "Thompson state file path (leave empty for default ~/.zeph/router_thompson_state.json)",
                )
                .default(String::new())
                .interact_text()?;
            if !custom_path.is_empty() {
                state.router_thompson_state_path = Some(custom_path);
            }
        }
        3 => {
            state.router_strategy = Some("cascade".into());
            let threshold: f64 = Input::new()
                .with_prompt(
                    "Quality threshold [0.0–1.0] — responses below this score trigger escalation",
                )
                .default(0.5_f64)
                .interact_text()?;
            state.router_cascade_quality_threshold = Some(threshold.clamp(0.0, 1.0));
            let max_esc: u8 = Input::new()
                .with_prompt("Max escalations per request (0 = no escalation)")
                .default(2_u8)
                .interact_text()?;
            state.router_cascade_max_escalations = Some(max_esc);
            let cost_tiers_input: String = Input::new()
                .with_prompt(
                    "Cost tiers: comma-separated provider names cheapest first \
                     (empty = use chain order)",
                )
                .default(String::new())
                .interact_text()?;
            let tiers: Vec<String> = cost_tiers_input
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if !tiers.is_empty() {
                state.router_cascade_cost_tiers = Some(tiers);
            }
        }
        _ => unreachable!(),
    }

    println!();
    Ok(())
}

pub(super) fn step_learning(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 11/12: Feedback Detector ==\n");

    let detector_items = &[
        "regex (default — pattern matching, no LLM)",
        "judge (LLM-based verification)",
        "model (ML classifier via classifiers feature)",
    ];
    let sel = Select::new()
        .with_prompt("Feedback detector mode")
        .items(detector_items)
        .default(0)
        .interact()?;

    match sel {
        1 => {
            state.detector_mode = Some("judge".into());
            let judge_model: String = Input::new()
                .with_prompt(
                    "Judge model name (e.g. claude-sonnet-4-6; leave empty to use primary provider)",
                )
                .default(String::new())
                .interact_text()?;
            if !judge_model.is_empty() {
                state.judge_model = Some(judge_model);
            }
        }
        2 => {
            state.detector_mode = Some("model".into());
            let feedback_provider: String = Input::new()
                .with_prompt(
                    "Provider name from [[llm.providers]] for feedback detection (leave empty to use primary provider)",
                )
                .default(String::new())
                .interact_text()?;
            if !feedback_provider.is_empty() {
                state.feedback_provider = Some(feedback_provider);
            }
        }
        _ => {
            state.detector_mode = Some("regex".into());
        }
    }

    state.skill_cross_session_rollout = Confirm::new()
        .with_prompt(
            "Require cross-session validation before skill promotion? (prevents promotion from a single long session)",
        )
        .default(false)
        .interact()?;
    if state.skill_cross_session_rollout {
        state.skill_min_sessions_before_promote = Input::new()
            .with_prompt("Minimum distinct sessions required for promotion")
            .default(2u32)
            .interact_text()?;
    }

    println!("\n-- Skill Evolution (ARISE / STEM / ERL) --\n");
    state.arise_enabled = Confirm::new()
        .with_prompt(
            "Enable ARISE? (trace-based skill improvement — refines skill bodies from successful tool sequences)",
        )
        .default(false)
        .interact()?;
    state.stem_enabled = Confirm::new()
        .with_prompt(
            "Enable STEM? (pattern-to-skill conversion — detects recurring tool sequences and generates skill candidates)",
        )
        .default(false)
        .interact()?;
    state.erl_enabled = Confirm::new()
        .with_prompt(
            "Enable ERL? (experiential reflective learning — extracts and injects heuristics from successful tasks)",
        )
        .default(false)
        .interact()?;

    state.d2skill_enabled = Confirm::new()
        .with_prompt(
            "Enable D2Skill? (step-level error correction hints injected into reflection prompts from past ARISE traces)",
        )
        .default(false)
        .interact()?;

    println!("\n-- SkillOrchestra: RL Routing Head --\n");
    state.rl_routing_enabled = Confirm::new()
        .with_prompt(
            "Enable RL routing head? (REINFORCE-trained MLP re-ranks skill candidates; starts blending after 50 updates)",
        )
        .default(false)
        .interact()?;

    println!();
    Ok(())
}
