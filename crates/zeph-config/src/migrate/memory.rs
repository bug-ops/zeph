// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Memory subsystem (graph, retrieval, reasoning, hebbian, fidelity) config migration steps.
//!
//! Extracted from the former `migrate/mod.rs` monolith (#4874). Shared TOML helpers,
//! the [`Migration`](super::Migration) trait, and the [`MIGRATIONS`](super::MIGRATIONS)
//! registry remain in the parent module.

use super::{MigrateError, MigrationResult, insert_after_section, section_header_present};

/// Add a commented-out `[memory.forgetting]` section if absent (#2397).
///
/// All forgetting fields have `#[serde(default)]` so existing configs parse without changes.
/// This step surfaces the new section for users upgrading from older configs.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_forgetting_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if toml_src.contains("[memory.forgetting]") || toml_src.contains("# [memory.forgetting]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    if !doc.contains_key("memory") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# SleepGate forgetting sweep (#2397). Disabled by default.\n\
         # [memory.forgetting]\n\
         # enabled = false\n\
         # decay_rate = 0.1                   # per-sweep importance decay\n\
         # forgetting_floor = 0.05            # prune below this score\n\
         # sweep_interval_secs = 7200         # run every 2 hours\n\
         # sweep_batch_size = 500\n\
         # protect_recent_hours = 24\n\
         # protect_min_access_count = 3\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.forgetting".to_owned()],
    })
}

/// Add commented-out `[memory.graph]` retrieval strategy options if the config lacks them (#3317).
///
/// Introduced alongside the multi-strategy graph retrieval and experience memory feature (#3311).
/// All `MemoryGraphConfig` fields have `#[serde(default)]` so existing configs parse without
/// changes; this migration surfaces the new options for discoverability.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_memory_graph_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("retrieval_strategy")
        || toml_src.contains("[memory.graph.beam_search]")
        || toml_src.contains("# [memory.graph.beam_search]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [memory.graph] retrieval strategy options (#3311).\n\
         # retrieval_strategy = \"synapse\"    # synapse | bfs | astar | watercircles | beam_search | hybrid\n\
         #\n\
         # [memory.graph.beam_search]        # active when retrieval_strategy = \"beam_search\"\n\
         # beam_width = 10                   # top-K candidates kept per hop\n\
         #\n\
         # [memory.graph.watercircles]       # active when retrieval_strategy = \"watercircles\"\n\
         # ring_limit = 0                    # max facts per ring; 0 = auto\n\
         #\n\
         # [memory.graph.experience]         # experience memory recording\n\
         # enabled = false\n\
         # evolution_sweep_enabled = false\n\
         # confidence_prune_threshold = 0.1  # prune edges below this threshold\n\
         # evolution_sweep_interval = 50     # turns between sweeps\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.graph.retrieval".to_owned()],
    })
}

/// Add a commented-out `[memory.retrieval]` block if the config lacks it (#3340).
///
/// MemMachine-inspired retrieval-stage tuning: ANN candidate depth, search-prompt template,
/// and context snippet format. All fields have defaults so existing configs parse unchanged;
/// this migration surfaces the section for discoverability.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the migration
/// function convention for use in chained pipelines.
pub fn migrate_memory_retrieval_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src
        .lines()
        .any(|l| l.trim() == "[memory.retrieval]" || l.trim() == "# [memory.retrieval]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [memory.retrieval] — MemMachine-inspired retrieval tuning (#3340, #3341).\n\
         # [memory.retrieval]\n\
         # depth = 0                          # ANN candidates fetched from the vector store, directly.\n\
         #                                    # 0 = legacy behavior (recall_limit * 2). Set to an explicit\n\
         #                                    # value >= recall_limit * 2 to enlarge the candidate pool.\n\
         # search_prompt_template = \"\"        # embedding query template; {query} = raw user query; empty = identity\n\
         # context_format = \"structured\"      # structured | plain — memory snippet rendering format\n\
         # query_bias_correction = true        # shift first-person queries towards user profile centroid (MM-F3)\n\
         # query_bias_profile_weight = 0.25    # blend weight [0.0, 1.0]; 0.0 = off, 1.0 = full centroid\n\
         # query_bias_centroid_ttl_secs = 300  # seconds before profile centroid cache is recomputed\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.retrieval".to_owned()],
    })
}

/// Add a commented-out `[memory.reasoning]` block if the config lacks it (#3369).
///
/// `ReasoningBank` distilled strategy memory was added in v0.19.3 (commit b99b2d30).
/// All fields have defaults so existing configs parse unchanged; this migration
/// surfaces the section for discoverability.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the migration
/// function convention for use in chained pipelines.
pub fn migrate_memory_reasoning_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src
        .lines()
        .any(|l| l.trim() == "[memory.reasoning]" || l.trim() == "# [memory.reasoning]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [memory.reasoning] — ReasoningBank: distilled strategy memory (#3369).\n\
         # [memory.reasoning]\n\
         # enabled = false\n\
         # extract_provider = \"\"         # SLM: self-judge (JSON response) — leave blank to use primary\n\
         # distill_provider = \"\"         # SLM: strategy distillation — leave blank to use primary\n\
         # top_k = 3                      # strategies injected per turn\n\
         # store_limit = 1000             # max rows in reasoning_strategies table\n\
         # context_budget_tokens = 500\n\
         # extraction_timeout_secs = 30\n\
         # distill_timeout_secs = 30\n\
         # max_messages = 6\n\
         # min_messages = 2\n\
         # max_message_chars = 2000\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.reasoning".to_owned()],
    })
}

/// Insert commented-out `self_judge_window` and `min_assistant_chars` keys under an existing
/// `[memory.reasoning]` block when they are absent (#3383).
///
/// Configs that lack a `[memory.reasoning]` section are returned unchanged (the
/// [`migrate_memory_reasoning_config`] step is responsible for adding the section).
/// Idempotent when either key is already present.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the migration
/// function convention for use in chained pipelines.
pub fn migrate_memory_reasoning_judge_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    let has_section = toml_src.lines().any(|l| l.trim() == "[memory.reasoning]");
    if !has_section {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Check if both keys are already present (active or commented).
    let has_window = toml_src.lines().any(|l| {
        let t = l.trim().trim_start_matches('#').trim();
        t.starts_with("self_judge_window")
    });
    let has_min_chars = toml_src.lines().any(|l| {
        let t = l.trim().trim_start_matches('#').trim();
        t.starts_with("min_assistant_chars")
    });
    if has_window && has_min_chars {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Append the new keys after the last line belonging to [memory.reasoning].
    // Strategy: find the last line of the [memory.reasoning] block (before the next section
    // header) and insert the commented-out keys after it.
    let lines: Vec<&str> = toml_src.lines().collect();
    let mut section_start = None;
    let mut insert_after = None;

    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "[memory.reasoning]" {
            section_start = Some(i);
        }
        if let Some(start) = section_start {
            let trimmed = line.trim();
            // A new top-level section header ends the current section.
            if i > start && trimmed.starts_with('[') && !trimmed.starts_with("[[") {
                break;
            }
            insert_after = Some(i);
        }
    }

    let Some(insert_idx) = insert_after else {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    };

    let mut new_lines: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    let mut additions = Vec::new();
    if !has_window {
        additions.push(
            "# self_judge_window = 2   # max recent messages passed to self-judge (#3383)"
                .to_owned(),
        );
    }
    if !has_min_chars {
        additions.push(
            "# min_assistant_chars = 50  # skip self-judge for short replies (#3383)".to_owned(),
        );
    }
    for (offset, line) in additions.iter().enumerate() {
        new_lines.insert(insert_idx + 1 + offset, line.clone());
    }

    let output = new_lines.join("\n") + if toml_src.ends_with('\n') { "\n" } else { "" };
    Ok(MigrationResult {
        output,
        changed_count: additions.len(),
        sections_changed: vec!["memory.reasoning".to_owned()],
    })
}

/// Append a commented-out `[memory.hebbian]` block to `toml_src` when it is absent (HL-F1/F2, #3344).
///
/// Idempotent: skipped when an active `[memory.hebbian]` header is present — via
/// `section_header_present`, which tolerates a trailing inline comment (the shipped
/// `config/default.toml` header reads `[memory.hebbian]  # HL-F1/F2 (#3344) ...`, which an exact
/// `l.trim() == "[memory.hebbian]"` line match fails to recognize, #5945) — or when this step's
/// own prior commented output (`# [memory.hebbian]`) is already present.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the migration
/// function convention for use in chained pipelines.
pub fn migrate_memory_hebbian_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let commented_present = toml_src.lines().any(|l| l.trim() == "# [memory.hebbian]");
    if section_header_present(toml_src, "memory.hebbian") || commented_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [memory.hebbian]                       # HL-F1/F2 (#3344) Hebbian edge reinforcement\n\
         # [memory.hebbian]\n\
         # enabled = false                        # opt-in master switch; no DB writes when false\n\
         # hebbian_lr = 0.1                       # weight increment per co-activation (0.01–0.5)\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.hebbian".to_owned()],
    })
}

/// Splice missing HL-F3/F4 consolidation fields into an existing `[memory.hebbian]` section
/// (HL-F3/F4, #3345).
///
/// Three branches:
/// - Section absent → no-op (handled by `migrate_memory_hebbian_config`).
/// - Section present but missing consolidation fields → append commented-out defaults.
/// - Section present with all fields → no-op.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_memory_hebbian_consolidation_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    // `section_header_present` tolerates a trailing inline comment on the header line — an
    // exact `l.trim() == "[memory.hebbian]"` match fails against the shipped
    // `config/default.toml` header (`[memory.hebbian]  # HL-F1/F2 (#3344) ...`), which meant
    // this step never engaged against the project's own real config (#5945).
    let has_section = section_header_present(toml_src, "memory.hebbian");

    if !has_section {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Check if all consolidation fields already present (active or commented). The step's
    // own prior output is always commented (`# consolidation_interval_secs = ...`), so the
    // leading `#` must be stripped before the prefix check — otherwise this guard never
    // matches its own output and the block duplicates on every subsequent run (#5945).
    let has_field = |field: &str| {
        toml_src.lines().any(|l| {
            let trimmed = l.trim();
            trimmed
                .strip_prefix('#')
                .map_or(trimmed, str::trim_start)
                .starts_with(field)
        })
    };
    let has_interval = has_field("consolidation_interval_secs");
    let has_threshold = has_field("consolidation_threshold");
    let has_provider = has_field("consolidate_provider");

    if has_interval && has_threshold && has_provider {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let extra = "\n# HL-F3/F4 consolidation fields (#3345) — splice into existing [memory.hebbian] section:\n\
        # consolidation_interval_secs = 3600   # how often the sweep runs (0 = disabled)\n\
        # consolidation_threshold = 5.0        # degree × avg_weight score to qualify\n\
        # consolidate_provider = \"fast\"        # provider name for LLM distillation\n\
        # max_candidates_per_sweep = 10\n\
        # consolidation_cooldown_secs = 86400  # re-consolidation cooldown per entity\n\
        # consolidation_prompt_timeout_secs = 30\n\
        # consolidation_max_neighbors = 20\n";

    let output = format!("{toml_src}{extra}");
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.hebbian".to_owned()],
    })
}

/// Splice missing HL-F5 spreading-activation fields into an existing `[memory.hebbian]` section
/// (HL-F5, #3346).
///
/// Three branches:
/// - Section absent → no-op (handled by `migrate_memory_hebbian_config`).
/// - Section present but missing HL-F5 fields → append commented-out defaults.
/// - Section present with all fields → no-op.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_memory_hebbian_spread_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    // `section_header_present` tolerates a trailing inline comment on the header line — an
    // exact `l.trim() == "[memory.hebbian]"` match fails against the shipped
    // `config/default.toml` header (`[memory.hebbian]  # HL-F1/F2 (#3344) ...`), which meant
    // this step never engaged against the project's own real config (#5945).
    let has_section = section_header_present(toml_src, "memory.hebbian");

    if !has_section {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Check if all HL-F5 fields are already present (active or commented). The step's own
    // prior output is always commented (`# spreading_activation = ...`), so the leading `#`
    // must be stripped before the prefix check — otherwise this guard never matches its own
    // output and the block duplicates on every subsequent run (#5945).
    let has_field = |field: &str| {
        toml_src.lines().any(|l| {
            let trimmed = l.trim();
            trimmed
                .strip_prefix('#')
                .map_or(trimmed, str::trim_start)
                .starts_with(field)
        })
    };
    let has_spreading = has_field("spreading_activation");
    let has_depth = has_field("spread_depth");
    let has_budget = has_field("step_budget_ms");
    let has_embed_timeout = has_field("embed_timeout_secs");

    if has_spreading && has_depth && has_budget && has_embed_timeout {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let extra = "\n# HL-F5 spreading-activation fields (#3346) — splice into existing [memory.hebbian] section:\n\
        # spreading_activation = false   # opt-in BFS from top-1 ANN anchor; requires enabled=true\n\
        # spread_depth = 2               # BFS hops, clamped [1,6]\n\
        # spread_edge_types = []         # MAGMA edge types to traverse; empty = all\n\
        # step_budget_ms = 80            # per-step circuit-breaker timeout (anchor ANN / edges / vectors)\n\
        # embed_timeout_secs = 5         # timeout for the initial query embedding call (0 = disabled)\n";

    let output = format!("{toml_src}{extra}");
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.hebbian.spreading_activation".to_owned()],
    })
}

/// Inject a commented-out `auto_consolidate_min_window` key into `[agent.focus]` if absent (#3313).
///
/// All `FocusConfig` fields have `#[serde(default)]`, so existing configs deserialize without
/// changes. This step surfaces the new field for users upgrading from older configs.
///
/// The comment is inserted *inside* the `[agent.focus]` section using `insert_after_section`,
/// so it ends up in the correct table regardless of where that section appears in the file.
///
/// Idempotent: if `auto_consolidate_min_window` already appears anywhere in the source,
/// the input is returned unchanged with `changed_count = 0`.
/// No-op when `[agent.focus]` is absent or only exists as a comment line.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the migration
/// function convention for use in chained pipelines.
pub fn migrate_focus_auto_consolidate_min_window(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("auto_consolidate_min_window") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Only inject when [agent.focus] exists as a live section (not a comment).
    if !toml_src.lines().any(|l| l.trim() == "[agent.focus]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Minimum messages in a low-relevance window before Focus auto-consolidation \
         runs (#3313).\n\
         # auto_consolidate_min_window = 6\n";
    let output = insert_after_section(toml_src, "agent.focus", comment);

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["agent.focus.auto_consolidate_min_window".to_owned()],
    })
}

/// Add `[memory.retrieval]` with `query_bias_correction = true` if the section is absent.
///
/// `query_bias_correction` shifts first-person queries toward the user profile centroid
/// (MM-F3, #3341) and is verified working in CI-604/CI-605. It is a no-op when the persona
/// table is empty, so enabling it by default is safe.
///
/// Idempotent: the section header (live or commented) suppresses re-injection.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_memory_retrieval_query_bias(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    // Already handled by migrate_memory_retrieval_config if the whole section is absent.
    // This step only splices the key into an existing [memory.retrieval] section.
    if !toml_src.lines().any(|l| l.trim() == "[memory.retrieval]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Idempotent: key already present (active or as comment).
    if toml_src
        .lines()
        .any(|l| l.trim().starts_with("query_bias_correction"))
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# MM-F3 (#3341): shift first-person queries toward the user profile centroid.\n\
         # No-op when the persona table is empty.\n\
         # query_bias_correction = true\n";
    let output = insert_after_section(toml_src, "memory.retrieval", comment);

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.retrieval.query_bias_correction".to_owned()],
    })
}

/// Add a commented-out `[memory.persona]` stub to configs that lack the section.
///
/// The persona profile drives query-bias correction (MM-F3, #3341) and is verified working
/// in CI-604/CI-605. Adding the stub makes the section discoverable via `migrate-config`.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_memory_persona_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src
        .lines()
        .any(|l| l.trim() == "[memory.persona]" || l.trim() == "# [memory.persona]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [memory.persona] — user persona profile for query-bias correction (#3341).\n\
         # Verified working in CI-604/CI-605. No-op when disabled.\n\
         # [memory.persona]\n\
         # enabled = true\n\
         # min_messages = 2       # minimum user messages before persona extraction fires\n\
         # min_confidence = 0.5   # minimum extraction confidence threshold (0.0–1.0)\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.persona".to_owned()],
    })
}

/// Inject a commented-out `recall_include_imported` key into `[memory.graph]` when absent (#5015).
///
/// The field defaults to `true` via `#[serde(default = "default_true")]`, so existing configs
/// parse unchanged. This step surfaces the new key for users who want to isolate conversation
/// knowledge from ingest-origin edges.
///
/// No-op when `[memory.graph]` is absent (graph not configured) or when the key is already
/// present (active or commented).
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_memory_graph_recall_include_imported(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    // Only inject when [memory.graph] exists as a live section.
    if !toml_src.lines().any(|l| l.trim() == "[memory.graph]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Idempotent: key already present (active or as comment) within [memory.graph].
    // We check only inside the [memory.graph] section to avoid a false positive from
    // step 61's [knowledge] advisory block, which also contains the string
    // "recall_include_imported" as a comment.
    //
    // A commented-out section header (e.g. `# [knowledge]`) is treated as ending the
    // current section, because advisory comment blocks from other migration steps are
    // appended at the end of the file and their keys must not be attributed to
    // [memory.graph].
    let in_graph_section = {
        let mut in_section = false;
        toml_src.lines().any(|l| {
            let t = l.trim();
            // Active section header: transitions in/out of [memory.graph].
            if !t.starts_with('#') && t.starts_with('[') && !t.starts_with("[[") {
                in_section = t == "[memory.graph]";
                return false;
            }
            if t.starts_with('#') {
                let inner = t.trim_start_matches('#').trim();
                // Commented-out section header (e.g. `# [knowledge]`): end current section.
                // Advisory blocks from other steps appear as `# [section]` and their keys
                // must not be misattributed to [memory.graph].
                if inner.starts_with('[') {
                    in_section = false;
                    return false;
                }
                // Commented-out key — counts as present only if still inside the section.
                return in_section && inner.starts_with("recall_include_imported");
            }
            // Active key inside the section.
            in_section && t.starts_with("recall_include_imported")
        })
    };
    if in_graph_section {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Whether to include ingest-origin edges/entities in recall (#5015).\n\
         # Set false to isolate conversation knowledge from imported knowledge.\n\
         # recall_include_imported = true\n";
    let output = insert_after_section(toml_src, "memory.graph", comment);

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.graph.recall_include_imported".to_owned()],
    })
}

/// No-op migration for the optional `qdrant_api_key` field added in #3543.
///
/// The field has `#[serde(default)]` so existing configs parse as `None` without changes.
/// This step adds a commented-out hint under `[memory]` if not already present.
///
/// # Errors
///
/// Returns `MigrateError` if the TOML cannot be parsed or `[memory]` is malformed.
pub fn migrate_qdrant_api_key(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("qdrant_api_key") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if !doc.contains_key("memory") {
        doc.insert("memory", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let comment = "\n# Qdrant API key (optional; required when connecting to remote/managed Qdrant clusters).\n\
         # Leave empty for local Qdrant instances. Store the actual key in the vault:\n\
         #   zeph vault set ZEPH_QDRANT_API_KEY \"<key>\"\n\
         # qdrant_api_key = \"\"\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.qdrant_api_key".to_owned()],
    })
}

/// No-op migration for the optional `qdrant_timeout_secs` field.
///
/// The field has `#[serde(default = "default_qdrant_timeout_secs")]` (10 seconds, matching the
/// previously hardcoded `QdrantOps` default) so existing configs parse unchanged. This step adds
/// a commented-out hint under `[memory]` if not already present.
///
/// # Errors
///
/// Returns `MigrateError` if the TOML cannot be parsed or `[memory]` is malformed.
pub fn migrate_qdrant_timeout_secs(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("qdrant_timeout_secs") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if !doc.contains_key("memory") {
        doc.insert("memory", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let comment = "\n# Per-call timeout applied to every Qdrant gRPC operation, in seconds.\n\
         # Bounds each call against a hung server or a stalled network path.\n\
         # qdrant_timeout_secs = 10\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.qdrant_timeout_secs".to_owned()],
    })
}

/// Add commented-out `embed_timeout_secs` and `compress_timeout_secs` to `[memory.fidelity]`
/// when it is present in the config but does not yet have these keys (#4645, #4651).
///
/// Both keys default to 30 seconds when absent; this step surfaces them for discovery.
/// Only runs when `[memory.fidelity]` is present — configs without fidelity are unchanged.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_fidelity_timeout_defaults(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let has_embed = toml_src.contains("embed_timeout_secs");
    let has_compress = toml_src.contains("compress_timeout_secs");

    if (has_embed && has_compress) || !toml_src.contains("[memory.fidelity]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut output = toml_src.to_owned();
    let mut changed = false;

    if !has_embed {
        let comment = "# embed_timeout_secs = 30  \
            # timeout in seconds for embed calls in fidelity scoring\n";
        output = output.replacen(
            "[memory.fidelity]\n",
            &format!("[memory.fidelity]\n{comment}"),
            1,
        );
        changed = true;
    }

    if !has_compress {
        let comment = "# compress_timeout_secs = 30  \
            # timeout in seconds for the LLM compress call in fidelity scoring\n";
        output = output.replacen(
            "[memory.fidelity]\n",
            &format!("[memory.fidelity]\n{comment}"),
            1,
        );
        changed = true;
    }

    if changed {
        Ok(MigrationResult {
            output,
            changed_count: 1,
            sections_changed: vec!["memory.fidelity".to_owned()],
        })
    } else {
        Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        })
    }
}
