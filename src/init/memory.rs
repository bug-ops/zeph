// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Input, Select};

use super::WizardState;

#[allow(clippy::too_many_lines)]
pub(super) fn step_memory(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 3/10: Memory ==\n");

    let db_backend = Select::new()
        .with_prompt("Database backend")
        .items(["SQLite (local, zero-config)", "PostgreSQL (server, shared)"])
        .default(0)
        .interact()?;

    if db_backend == 1 {
        // PostgreSQL selected: do not prompt for sqlite_path.
        // Instruct the user to store the URL in the vault instead of writing it to plaintext config.
        println!(
            "\nStore the PostgreSQL URL in the vault after init:\n  \
             zeph vault set ZEPH_DATABASE_URL \"postgres://user:pass@localhost:5432/zeph\"\n"
        );
        println!(
            "Note: binary must be compiled with --features postgres for PostgreSQL support.\n"
        );
        // Write empty placeholder; the vault key ZEPH_DATABASE_URL overrides it at runtime.
        state.database_url = Some(String::new());
    } else {
        state.sqlite_path = Some(
            Input::new()
                .with_prompt("SQLite database path")
                .default(zeph_core::config::default_sqlite_path())
                .interact_text()?,
        );
    }

    state.sessions_max_history = Input::new()
        .with_prompt("Maximum number of sessions to list (0 = unlimited)")
        .default(100usize)
        .interact_text()?;

    state.sessions_title_max_chars = Input::new()
        .with_prompt("Maximum characters for auto-generated session titles")
        .default(60usize)
        .interact_text()?;

    state.semantic_enabled = Confirm::new()
        .with_prompt("Enable semantic memory (requires Qdrant)?")
        .default(true)
        .interact()?;

    if state.semantic_enabled {
        state.qdrant_url = Some(
            Input::new()
                .with_prompt("Qdrant URL")
                .default("http://localhost:6334".into())
                .interact_text()?,
        );
    }

    state.soft_compaction_threshold = Input::new()
        .with_prompt(
            "Soft compaction threshold: prune tool outputs + apply deferred summaries \
             when context usage exceeds this fraction \
             (0.0-1.0, recommended: below 0.90 — the default hard threshold)",
        )
        .default(state.soft_compaction_threshold)
        .validate_with(|v: &f32| {
            if v.is_finite() && *v > 0.0 && *v < 1.0 {
                Ok(())
            } else {
                Err("must be between 0.0 and 1.0 exclusive")
            }
        })
        .interact_text()?;
    // Loop required for cross-field validation (hard > soft): dialoguer's validate_with
    // closure only sees the parsed value, not external state, so we handle the constraint here.
    loop {
        let soft = state.soft_compaction_threshold;
        let val: f32 = Input::new()
            .with_prompt(format!(
                "Hard compaction threshold: full LLM summarization when context usage exceeds \
                 this fraction (0.0-1.0, must be above soft threshold {soft})"
            ))
            .default(state.hard_compaction_threshold)
            .validate_with(|v: &f32| {
                if v.is_finite() && *v > 0.0 && *v < 1.0 {
                    Ok(())
                } else {
                    Err("must be between 0.0 and 1.0 exclusive")
                }
            })
            .interact_text()?;
        if val > soft {
            state.hard_compaction_threshold = val;
            break;
        }
        eprintln!("error: hard threshold must be greater than soft threshold ({soft}), got {val}",);
    }

    state.graph_memory_enabled = Confirm::new()
        .with_prompt("Enable knowledge graph memory? (experimental)")
        .default(false)
        .interact()?;

    if state.graph_memory_enabled {
        let model: String = Input::new()
            .with_prompt("LLM model for entity extraction (empty = same as agent)")
            .default(String::new())
            .interact_text()?;
        if !model.is_empty() {
            state.graph_extract_model = Some(model);
        }

        state.graph_spreading_activation_enabled = Confirm::new()
            .with_prompt(
                "Enable SYNAPSE spreading activation for graph recall? \
                 (replaces BFS; uses temporal decay + lateral inhibition; recommended defaults: \
                 decay_lambda=0.85, max_hops=3)",
            )
            .default(false)
            .interact()?;
    }

    state.compression_guidelines_enabled = Confirm::new()
        .with_prompt(
            "Enable ACON failure-driven compression guidelines? \
             (learns compression rules from detected context-loss events, \
             requires compression-guidelines feature)",
        )
        .default(false)
        .interact()?;

    state.server_compaction_enabled = Confirm::new()
        .with_prompt(
            "Enable Claude server-side context compaction? (compact-2026-01-12 beta, Claude only)",
        )
        .default(false)
        .interact()?;

    state.shutdown_summary = Confirm::new()
        .with_prompt(
            "Store a session summary on shutdown? (enables cross-session recall for short sessions, \
             advanced params shutdown_summary_min_messages and shutdown_summary_max_messages \
             are config-file-only)",
        )
        .default(true)
        .interact()?;

    state.digest_enabled = Confirm::new()
        .with_prompt(
            "Enable session digest generation? (generates a compact summary of key facts and \
             decisions at session end and injects it at the start of the next session)",
        )
        .default(false)
        .interact()?;

    let strategy_options = ["full_history", "adaptive", "memory_first"];
    let strategy_idx = Select::new()
        .with_prompt(
            "Context assembly strategy (full_history: current behavior; adaptive: switches to \
             memory-first after crossover_turn_threshold turns; memory_first: always use memory \
             instead of full history)",
        )
        .items(strategy_options)
        .default(0)
        .interact()?;
    strategy_options[strategy_idx].clone_into(&mut state.context_strategy);

    println!();
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn step_context_compression(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Context Compression ==\n");
    println!(
        "Active context compression reduces token usage by pruning stale tool outputs \
         and compressing exploration phases.\n"
    );

    state.focus_enabled = Confirm::new()
        .with_prompt("Enable Focus Agent? (LLM-driven exploration bracketing)")
        .default(false)
        .interact()?;

    if state.focus_enabled {
        state.focus_compression_interval = Input::new()
            .with_prompt("Focus compression interval (turns between suggestions)")
            .default(state.focus_compression_interval)
            .validate_with(
                |v: &usize| {
                    if *v >= 1 { Ok(()) } else { Err("must be >= 1") }
                },
            )
            .interact_text()?;
    }

    state.memory_tiers_enabled = Confirm::new()
        .with_prompt(
            "Enable AOI three-layer memory tiers? (episodic -> semantic promotion via LLM)",
        )
        .default(false)
        .interact()?;

    if state.memory_tiers_enabled {
        state.memory_tiers_promotion_min_sessions = Input::new()
            .with_prompt("Minimum sessions before episodic fact is promoted to semantic")
            .default(state.memory_tiers_promotion_min_sessions)
            .validate_with(|v: &u32| if *v >= 2 { Ok(()) } else { Err("must be >= 2") })
            .interact_text()?;
    }

    state.sidequest_enabled = Confirm::new()
        .with_prompt("Enable SideQuest eviction? (LLM-driven tool output eviction)")
        .default(false)
        .interact()?;

    if state.sidequest_enabled {
        state.sidequest_interval_turns = Input::new()
            .with_prompt("SideQuest eviction interval (user turns)")
            .default(state.sidequest_interval_turns)
            .validate_with(|v: &u32| if *v >= 1 { Ok(()) } else { Err("must be >= 1") })
            .interact_text()?;
    }

    state.forgetting_enabled = Confirm::new()
        .with_prompt(
            "Enable SleepGate forgetting sweep? \
             (background decay + pruning of low-importance memories)",
        )
        .default(false)
        .interact()?;

    state.compression_predictor_enabled = Confirm::new()
        .with_prompt(
            "Enable compression ratio predictor? \
             (adaptive compaction quality, requires enough probe data to activate)",
        )
        .default(false)
        .interact()?;

    let strategy_options = &[
        "reactive (oldest-first, default)",
        "task_aware (keyword relevance scoring)",
        "mig (relevance minus redundancy)",
        "task_aware_mig (combined goal + MIG)",
        "subgoal (HiAgent subgoal-aware, LLM extraction per turn)",
        "subgoal_mig (subgoal + MIG redundancy scoring)",
    ];
    let default_idx = match state.pruning_strategy.as_str() {
        "task_aware" => 1,
        "mig" => 2,
        "task_aware_mig" => 3,
        "subgoal" => 4,
        "subgoal_mig" => 5,
        _ => 0,
    };
    let idx = Select::new()
        .with_prompt("Pruning strategy")
        .items(strategy_options)
        .default(default_idx)
        .interact()?;
    state.pruning_strategy = match idx {
        1 => "task_aware".into(),
        2 => "mig".into(),
        3 => "task_aware_mig".into(),
        4 => "subgoal".into(),
        5 => "subgoal_mig".into(),
        _ => "reactive".into(),
    };

    state.probe_enabled = Confirm::new()
        .with_prompt(
            "Enable compaction probe? (validates summary quality before committing, \
             adds 2 LLM calls per compaction)",
        )
        .default(false)
        .interact()?;

    if state.probe_enabled {
        let provider: String = Input::new()
            .with_prompt(
                "Provider name for probe LLM calls from [[llm.providers]] \
                 (empty = same as summary provider)",
            )
            .default(String::new())
            .interact_text()?;
        if !provider.is_empty() {
            state.probe_provider = Some(provider);
        }

        state.probe_threshold = Input::new()
            .with_prompt("Probe pass threshold (0.0-1.0, scores below this trigger warnings)")
            .default(state.probe_threshold)
            .validate_with(|v: &f32| {
                if v.is_finite() && *v > 0.0 && *v <= 1.0 {
                    Ok(())
                } else {
                    Err("must be in (0.0, 1.0]")
                }
            })
            .interact_text()?;

        loop {
            let threshold = state.probe_threshold;
            let val: f32 = Input::new()
                .with_prompt(format!(
                    "Probe hard-fail threshold (0.0-1.0, scores below this block compaction, \
                     must be below {threshold})"
                ))
                .default(state.probe_hard_fail_threshold)
                .validate_with(|v: &f32| {
                    if v.is_finite() && *v >= 0.0 && *v < 1.0 {
                        Ok(())
                    } else {
                        Err("must be in [0.0, 1.0)")
                    }
                })
                .interact_text()?;
            if val < threshold {
                state.probe_hard_fail_threshold = val;
                break;
            }
            eprintln!(
                "error: hard-fail threshold must be less than pass threshold ({threshold}), got {val}",
            );
        }
    }

    println!();
    Ok(())
}
