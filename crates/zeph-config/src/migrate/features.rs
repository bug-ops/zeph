// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Feature-specific (compaction, autodream, goals, orchestration persistence) config migration steps.
//!
//! Extracted from the former `migrate/mod.rs` monolith (#4874). Shared TOML helpers,
//! the [`Migration`](super::Migration) trait, and the [`MIGRATIONS`](super::MIGRATIONS)
//! registry remain in the parent module.

use regex::Regex;

use super::{MigrateError, MigrationResult, insert_after_section, section_header_present};

/// Regex matching the `[tui]` section header line (used by step 67).
static TUI_HEADER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*\[tui\][ \t]*(?:#[^\r\n]*)?\r?\n").expect("static pattern")
});

/// Inject a commented-out `[tui.delights]` advisory block when absent (#5104).
///
/// No-op when `[tui]` is absent (config doesn't use TUI at all) or when
/// `[tui.delights]` already exists (active or commented) — idempotent.
///
/// # Errors
///
/// Returns `MigrateError::TomlParse` if the input is not valid TOML; infallible otherwise.
pub fn migrate_tui_delights(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // No [tui] section → no-op.
    if !section_header_present(toml_src, "tui") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Idempotency: scan for [tui.delights] already present (active or commented-out).
    let already_present = section_header_present(toml_src, "tui.delights")
        || toml_src.lines().any(|l| l.trim() == "# [tui.delights]");
    if already_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Normalise trailing newline so the regex always matches.
    let owned;
    let src = if toml_src.ends_with('\n') {
        toml_src
    } else {
        owned = format!("{toml_src}\n");
        &owned
    };

    if !TUI_HEADER_RE.is_match(src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let advisory = "\n# [tui.delights] — micro-delight toggles (all default true, #5104).\n\
         # motion = off acts as a master kill-switch regardless of individual settings.\n\
         # [tui.delights]\n\
         # stream_metrics   = true  # tok/s during streaming + TTFT after turn in status bar\n\
         # toasts           = true  # ephemeral overlay notifications (theme switch, copy, etc.)\n\
         # completion_flash = true  # accent tint on a finished tool group for ~400 ms\n\
         # smooth_scroll    = true  # eased multi-frame interpolation on page-up / page-down\n\
         # splash_shimmer   = true  # one-shot gradient sweep across the wordmark at startup\n";

    let output = TUI_HEADER_RE
        .replacen(src, 1, |caps: &regex::Captures| {
            format!("{}{advisory}", &caps[0])
        })
        .into_owned();

    let changed = output != toml_src;
    let changed_count = usize::from(changed);
    Ok(MigrationResult {
        output,
        changed_count,
        sections_changed: if changed {
            vec!["tui.delights".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Inject `mouse = false` under `[tui]` when absent (#5103).
///
/// No-op when `[tui]` is absent, or when `mouse` is already present (active or
/// commented-out) — idempotent.
///
/// # Errors
///
/// Returns `MigrateError::TomlParse` if the input is not valid TOML; infallible otherwise.
pub fn migrate_tui_mouse(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if !section_header_present(toml_src, "tui") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let already_present = toml_src.lines().any(|l| {
        let t = l.trim().trim_start_matches('#').trim();
        t.starts_with("mouse")
    });
    if already_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let owned;
    let src = if toml_src.ends_with('\n') {
        toml_src
    } else {
        owned = format!("{toml_src}\n");
        &owned
    };

    if !TUI_HEADER_RE.is_match(src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let insert =
        "# mouse = false  # opt-in mouse capture: wheel scrolls, clicks focus panels (#5103)\n";
    let output = TUI_HEADER_RE
        .replacen(src, 1, |caps: &regex::Captures| {
            format!("{}{insert}", &caps[0])
        })
        .into_owned();

    let changed = output != toml_src;
    let changed_count = usize::from(changed);
    Ok(MigrationResult {
        output,
        changed_count,
        sections_changed: if changed {
            vec!["tui".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Strip any existing `[memory.compression.predictor]` section from the config (#3251).
///
/// The compression predictor feature was removed. This migration cleans up both active
/// and commented-out sections that previous `--migrate-config` runs may have injected.
/// # Errors
///
/// This function is a pure string operation and always returns `Ok`. The `Result`
/// return type is kept for API consistency with other migration functions.
pub fn migrate_compression_predictor_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    // Strip any [memory.compression.predictor] section (active or commented-out) that
    // prior migrate-config runs may have injected. The feature is removed (#3251).
    let has_active = section_header_present(toml_src, "memory.compression.predictor");
    let has_commented = toml_src.contains("# [memory.compression.predictor]");
    if !has_active && !has_commented {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Remove lines that belong to the section header variants and their key lines.
    // A line belongs to the section when the section header has been seen and the
    // line is not a new `[section]` header (excluding the predictor header itself).
    let mut output_lines: Vec<&str> = Vec::new();
    let mut in_predictor = false;
    for line in toml_src.lines() {
        let trimmed = line.trim();
        // Detect active or commented-out section header.
        if trimmed == "[memory.compression.predictor]"
            || trimmed == "# [memory.compression.predictor]"
        {
            in_predictor = true;
            continue;
        }
        // Any new `[section]` header (not commented-out) ends the predictor block.
        if in_predictor && trimmed.starts_with('[') && !trimmed.starts_with("# [") {
            in_predictor = false;
        }
        if !in_predictor {
            output_lines.push(line);
        }
    }
    // Preserve trailing newline if original had one.
    let mut output = output_lines.join("\n");
    if toml_src.ends_with('\n') {
        output.push('\n');
    }

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.compression.predictor".to_owned()],
    })
}

/// Add a commented-out `[memory.microcompact]` block if absent (#2699).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_microcompact_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if section_header_present(toml_src, "memory.microcompact")
        || toml_src.contains("# [memory.microcompact]")
    {
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

    let comment = "\n# Time-based microcompact (#2699). Strips stale low-value tool outputs after idle.\n\
         # [memory.microcompact]\n\
         # enabled = false\n\
         # gap_threshold_minutes = 60   # idle gap before clearing stale outputs\n\
         # keep_recent = 3              # always keep this many recent outputs intact\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.microcompact".to_owned()],
    })
}

/// Add a commented-out `[memory.autodream]` block if absent (#2697).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_autodream_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if section_header_present(toml_src, "memory.autodream")
        || toml_src.contains("# [memory.autodream]")
    {
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

    let comment = "\n# autoDream background memory consolidation (#2697). Disabled by default.\n\
         # [memory.autodream]\n\
         # enabled = false\n\
         # min_sessions = 5             # sessions since last consolidation\n\
         # min_hours = 8                # hours since last consolidation\n\
         # consolidation_provider = \"\" # provider name from [[llm.providers]]; empty = primary\n\
         # max_iterations = 5\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.autodream".to_owned()],
    })
}

/// Add a commented-out `[magic_docs]` block if absent (#2702).
///
/// Idempotent: skipped when an active `magic_docs` key is present, or when this step's own
/// prior commented output (`# [magic_docs]`) is already present — `doc.contains_key` alone only
/// recognizes an active key, so without the second check this step re-appended the block on
/// every subsequent run (#5945).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_magic_docs_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    use toml_edit::{Item, Table};

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let commented_present = toml_src.lines().any(|l| l.trim() == "# [magic_docs]");
    if doc.contains_key("magic_docs") || commented_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    doc.insert("magic_docs", Item::Table(Table::new()));
    let comment = "# MagicDocs auto-maintained markdown (#2702). Disabled by default.\n\
         # [magic_docs]\n\
         # enabled = false\n\
         # min_turns_between_updates = 10\n\
         # update_provider = \"\"         # provider name from [[llm.providers]]; empty = primary\n\
         # max_iterations = 3\n";
    // Remove the just-inserted empty table and replace with a comment.
    doc.remove("magic_docs");
    // Append as a trailing comment on the document root.
    let raw = doc.to_string();
    let output = format!("{raw}\n{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["magic_docs".to_owned()],
    })
}

/// Add a commented-out `persistence_enabled` key under `[orchestration]` when absent (#3107).
///
/// Existing configs that omit this key pick up `true` via `#[serde(default)]`, so this
/// migration is informational — it surfaces the new option without changing behaviour.
///
/// Uses `insert_after_section` rather than an exact `"[orchestration]\n"` substring match, so
/// insertion still succeeds when the header line carries trailing content (e.g. an inline
/// comment from another step) instead of being followed immediately by a newline. The report
/// only claims success when `output` actually differs from `toml_src` — `String::replacen`
/// silently returns the input unchanged when its pattern isn't found, and blindly trusting it
/// produced a false "added" report on every run without ever writing anything (#5945).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML document cannot be parsed.
pub fn migrate_orchestration_persistence(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Skip if the key is already present — `contains` is a substring search, so this already
    // matches both the active key and this step's own commented output (`# persistence_enabled`).
    if toml_src.contains("persistence_enabled") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Only inject under an existing [orchestration] section.
    if !section_header_present(toml_src, "orchestration") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# persistence_enabled = true  \
        # persist task graphs to SQLite after each tick; enables `/plan resume <id>` (#3107)\n";
    let output = insert_after_section(toml_src, "orchestration", comment);
    // Defensive: the guards above already guarantee `[orchestration]` is present and
    // `insert_after_section` always inserts non-empty content when reached, so this is
    // currently unreachable — kept so a future change to either guard can't silently
    // regress into reporting a change that didn't happen (the original defect, #5945).
    if output == toml_src {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["orchestration.persistence_enabled".to_owned()],
    })
}

/// Add the `[goals]` section as commented-out defaults when it is absent.
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] when `toml_src` is not valid TOML.
pub fn migrate_goals_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "goals") || toml_src.contains("# [goals]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Long-horizon goal lifecycle tracking (#3567).\n\
         # [goals]\n\
         # enabled = false\n\
         # inject_into_system_prompt = true\n\
         # max_text_chars = 2000\n\
         # max_history = 50\n";

    Ok(MigrationResult {
        output: format!("{toml_src}{comment}"),
        changed_count: 1,
        sections_changed: vec!["goals".to_owned()],
    })
}

/// Add a commented-out `[caveman]` block if absent (#4985).
///
/// All `CavemanConfig` fields have `#[serde(default)]` so existing configs parse without changes;
/// this migration only surfaces the section so users can discover and enable it.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_caveman_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "caveman") || toml_src.contains("# [caveman]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [caveman] — ultra-compressed telegraphic output mode (#4985).\n\
         # Toggle at runtime with /caveman [on|off] or via the bundled caveman skill.\n\
         # [caveman]\n\
         # default_on = false\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["caveman".to_owned()],
    })
}

/// Add a commented-out `[deep_link]` section if absent (spec-066, #5011).
///
/// All `DeepLinkConfig` fields have `#[serde(default)]` so existing configs parse without
/// changes; this migration only surfaces the section so users can discover and configure it.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the migration
/// function convention.
pub fn migrate_deep_link_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "deep_link") || toml_src.contains("# [deep_link]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [deep_link] — zeph:// URI scheme configuration (spec-066, #5011).\n\
         # Requires the `deep-link` Cargo feature to be active.\n\
         # [deep_link]\n\
         # confirm_before_prompt = true   # require y/N before injecting prompt (secure default)\n\
         # allowed_cwd_roots = []          # restrict cwd to these prefixes; empty = any non-denylisted path\n\
         # prefer_acp = \"never\"           # v1 only: \"never\"; \"auto\"/\"always\" reserved for v2\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["deep_link".to_owned()],
    })
}

/// Add a commented-out `[memory.five_signal]` section if absent (#4374).
///
/// All five-signal fields have `#[serde(default)]` so existing configs parse without changes.
/// This step surfaces the new section for users upgrading from older configs.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_five_signal_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "memory.five_signal")
        || toml_src.contains("# [memory.five_signal]")
    {
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

    let comment = "\n# Five-signal SYNAPSE retrieval (#4374). Disabled by default.\n\
         # [memory.five_signal]\n\
         # enabled = false\n\
         # w_recency = 0.35\n\
         # w_relevance = 0.35\n\
         # w_frequency = 0.15\n\
         # w_causal = 0.10\n\
         # w_novelty = 0.05\n\
         # causal_bfs_max_depth = 10\n\
         # neutral_causal_distance = 5\n\
         # novelty_decay_rate = 0.1\n\
         #\n\
         # [memory.five_signal.consolidation_daemon]\n\
         # enabled = false\n\
         # interval_seconds = 7200\n\
         # batch_size = 500\n\
         # promotion_score_threshold = 0.70\n\
         # demotion_score_threshold = 0.20\n\
         # top_k_per_run = 500\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.five_signal".to_owned()],
    })
}

/// Add a commented-out `[knowledge]` block if absent (spec-067, #5017).
///
/// Idempotent: no-ops when `[knowledge]` is already present in any form.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_knowledge_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "knowledge") || toml_src.contains("# [knowledge]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Knowledge-ingest subsystem (spec-067, #5017). All defaults shown.\n\
         # [knowledge]\n\
         # ingest_provider = \"\"          # provider from [[llm.providers]]; empty = primary (Phase 2 graph)\n\
         # concurrency = 3              # max parallel extract tasks (Phase 2)\n\
         # max_documents = 0            # 0 = unlimited; CLI --max-documents overrides\n\
         # recall_include_imported = true  # include imported rows in semantic recall\n\
         # transcript_scope = \"current-project\"  # INV-6: only current-project supported in Phase 1\n";
    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["knowledge".to_owned()],
    })
}

// `migrate_tui_theme_defaults` below relies on this exact-header regex (not
// `section_header_present`) to place active `name`/`color_mode` keys correctly on
// subtable-only configs (e.g. `[tui.theme.colors]` without a bare `[tui.theme]`) — do not
// simplify that guard to `section_header_present` alone without preserving this regex check.
static TUI_THEME_HEADER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*\[tui\.theme\][ \t]*(?:#[^\r\n]*)?\r?\n").expect("static pattern")
});

/// Insert active `name` and `color_mode` defaults into `[tui.theme]` when the section
/// exists but those keys are absent (#5091).
///
/// Step 65 added a commented-out advisory block so users could discover the new section.
/// This step upgrades configs that already have an active `[tui.theme]` section (either
/// hand-edited or promoted from the advisory block) by injecting the two mandatory keys
/// with their safe defaults so that the runtime never falls back to compiled-in values
/// silently.
///
/// The step is idempotent: if either key is already present the function is a no-op.
/// If the `[tui.theme]` section is absent entirely the step is also a no-op (step 65
/// handles that case).
///
/// # Errors
///
/// Returns `MigrateError::TomlParse` if the input is not valid TOML.
pub fn migrate_tui_theme_defaults(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Check whether a key with the exact name exists inside [tui.theme].
    // Uses exact-key matching: trim the line, strip a leading `#` for commented keys,
    // split on `=`, and compare the key token — prevents prefix false-positives
    // (e.g. `name_hint` must NOT satisfy `has_name`).
    let key_in_tui_theme = |key: &str| {
        let mut in_tui_theme = false;
        toml_src.lines().any(|l| {
            let t = l.trim();
            // Section header line — update scope flag and keep scanning.
            if !t.starts_with('#') && t.starts_with('[') {
                in_tui_theme = t == "[tui.theme]";
                return false;
            }
            if !in_tui_theme {
                return false;
            }
            // Strip optional leading `#` for commented-out keys.
            let body = t.trim_start_matches('#').trim();
            // Extract the key token (everything before `=`), trim whitespace.
            let lhs = body.split('=').next().unwrap_or("").trim();
            lhs == key
        })
    };

    let has_name = key_in_tui_theme("name");
    let has_color_mode = key_in_tui_theme("color_mode");

    // If [tui.theme] is absent, step 65 handles it — this step is a no-op.
    let has_section = section_header_present(toml_src, "tui.theme");
    if !has_section || (has_name && has_color_mode) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Normalise: ensure the source ends with a newline so the regex can always
    // match the header line (hand-edited files may omit the trailing newline).
    let owned;
    let src = if toml_src.ends_with('\n') {
        toml_src
    } else {
        owned = format!("{toml_src}\n");
        &owned
    };

    if !TUI_THEME_HEADER_RE.is_match(src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut insert = String::new();
    if !has_name {
        insert
            .push_str("name       = \"zephyr\"  # built-in preset; see /theme for alternatives\n");
    }
    if !has_color_mode {
        insert.push_str("color_mode = \"auto\"    # auto | truecolor | ansi256 | ansi16 | never\n");
    }

    let output = TUI_THEME_HEADER_RE
        .replacen(src, 1, |caps: &regex::Captures| {
            format!("{}{insert}", &caps[0])
        })
        .into_owned();

    let changed = output != toml_src;
    let changed_count = usize::from(changed);
    Ok(MigrationResult {
        output,
        changed_count,
        sections_changed: if changed {
            vec!["tui.theme".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Inject a commented-out `[tui.theme]` advisory block when absent (#5087).
///
/// The `[tui.theme]` section was added in the TUI Theme System 2.0. Existing configs parse
/// fine without it (all fields have defaults), but surfacing the new keys lets users discover
/// and customise them.
///
/// No-op when `[tui.theme]` is already present (active or commented-out), determined by a
/// section-scoped scan that only looks inside the `[tui]` body — identical to the idempotency
/// strategy used in step 63.
///
/// # Errors
///
/// Returns `MigrateError::TomlParse` if the input is not valid TOML; infallible otherwise.
pub fn migrate_tui_theme_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Section-scoped idempotency: check only inside [tui] for [tui.theme].
    // A raw `toml_src.contains("[tui.theme]")` would also match `# [tui.theme]` advisory
    // blocks appended by earlier runs, but we restrict to the live [tui] body.
    let in_tui_section = {
        let mut in_section = false;
        toml_src.lines().any(|l| {
            let t = l.trim();
            if !t.starts_with('#') && t.starts_with('[') && !t.starts_with("[[") {
                in_section = t == "[tui]";
                return false;
            }
            if t.starts_with('#') {
                let inner = t.trim_start_matches('#').trim();
                if inner.starts_with('[') {
                    in_section = false;
                    return false;
                }
                return in_section && (inner == "[tui.theme]" || inner.starts_with("tui.theme"));
            }
            in_section && (t == "[tui.theme]" || t.starts_with("tui.theme"))
        })
    };

    if in_tui_section
        || section_header_present(toml_src, "tui.theme")
        || toml_src.lines().any(|l| l.trim() == "# [tui.theme]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    let raw = doc.to_string();
    let comment = "\n# [tui.theme] — TUI visual theme (Theme System 2.0, #5087).\n\
         # name sets the colour palette. Built-in presets: classic, zephyr, zephyr-light,\n\
         # high-contrast, catppuccin-mocha, gruvbox-dark, solarized-dark.\n\
         # Custom palettes: drop a TOML file in ~/.config/zeph/themes/<name>.toml.\n\
         # [tui.theme]\n\
         # name         = \"zephyr\"    # default: zephyr (new default since 2.0; use \"classic\" for legacy look)\n\
         # color_mode   = \"auto\"      # auto | truecolor | ansi256 | ansi16 | never\n";
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["tui.theme".to_owned()],
    })
}

/// Step 69 — add `default_asset_sensitivity` advisory comment to `[orchestration]` (spec-068, #3934).
///
/// Advisory only: the migration is informational — it surfaces the new option without
/// changing behaviour. Skipped when the key is already present or `[orchestration]` is absent.
///
/// Uses `insert_after_section` rather than an exact `"[orchestration]\n"` substring match, so
/// insertion still succeeds when the header line carries trailing content (e.g. an inline
/// comment from another step) instead of being followed immediately by a newline. The report
/// only claims success when `output` actually differs from `toml_src` — `String::replacen`
/// silently returns the input unchanged when its pattern isn't found, and blindly trusting it
/// produced a false "added" report on every run without ever writing anything (#5945).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML document cannot be parsed.
pub fn migrate_orchestration_asset_sensitivity(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    // `contains` is a substring search, so this already matches both the active key and this
    // step's own commented output (`# default_asset_sensitivity`).
    if toml_src.contains("default_asset_sensitivity") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    if !section_header_present(toml_src, "orchestration") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# default_asset_sensitivity = \"public\"  \
        # advisory asset sensitivity: public | internal | confidential (spec-068, #3934)\n";
    let output = insert_after_section(toml_src, "orchestration", comment);
    // Defensive: the guards above already guarantee `[orchestration]` is present and
    // `insert_after_section` always inserts non-empty content when reached, so this is
    // currently unreachable — kept so a future change to either guard can't silently
    // regress into reporting a change that didn't happen (the original defect, #5945).
    if output == toml_src {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["orchestration.default_asset_sensitivity".to_owned()],
    })
}

/// Step 88 — add `default_idle_timeout_secs` advisory comment to `[orchestration]`
/// (spec-075-orchestration-node-control-parity, #6021; enforcement activated by #6245).
///
/// Advisory only: `#[serde(default)]` already makes existing configs load with the field
/// unset (`None`), so this migration is purely informational — it surfaces the option
/// without changing behaviour. Skipped when the key is already present or
/// `[orchestration]` is absent. Mirrors [`migrate_orchestration_asset_sensitivity`]'s shape
/// exactly (same section, same advisory-comment idiom).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML document cannot be parsed.
pub fn migrate_orchestration_idle_timeout(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("default_idle_timeout_secs") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    if !section_header_present(toml_src, "orchestration") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# default_idle_timeout_secs = 60  \
        # kill a task if it emits no progress for this many seconds; must be set above the \
        # longest expected single-turn duration (spec-075-orchestration-node-control-parity, #6021)\n";
    let output = insert_after_section(toml_src, "orchestration", comment);
    // Defensive: the guards above already guarantee `[orchestration]` is present and
    // `insert_after_section` always inserts non-empty content when reached, so this is
    // currently unreachable — kept for the same reason as `migrate_orchestration_asset_sensitivity`
    // (#5945: a future change to either guard must not silently regress into reporting a
    // change that didn't happen).
    if output == toml_src {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["orchestration.default_idle_timeout_secs".to_owned()],
    })
}

/// Step 86 — add a commented-out `[orchestration.ensemble]` advisory block for ORCH-style
/// deterministic verifier ensemble-merge, if absent (spec 073, #6232).
///
/// All `EnsembleConfig` fields have `#[serde(default)]` so existing configs parse without
/// changes even without this step — this exists purely for discoverability on config upgrade,
/// mirroring [`migrate_orchestration_persistence`] and [`migrate_memory_type_aware_compose_config`](super::migrate_memory_type_aware_compose_config).
///
/// # Errors
///
/// Returns [`MigrateError`] if `toml_src` fails to parse as TOML.
pub fn migrate_orchestration_ensemble(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if section_header_present(toml_src, "orchestration.ensemble")
        || toml_src.contains("[orchestration.ensemble]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    if !doc.contains_key("orchestration") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# ORCH-style deterministic verifier ensemble-merge — off by default (spec 073, #6232)\n\
         # [orchestration.ensemble]\n\
         # enabled = false\n\
         # verify = false\n\
         # members = []                 # odd length, >= 3, no duplicates, from [[llm.providers]]\n\
         # ema_alpha = 0.3\n\
         # ema_decay = 0.95\n\
         # min_observations = 5\n\
         # member_timeout_secs = 0      # 0 = fall back to verifier_timeout_secs\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["orchestration.ensemble".to_owned()],
    })
}

/// Step 91 — add a commented-out `[orchestration.command]` advisory block for Command-style
/// dynamic task handoff, if absent (spec-080, GitHub #6363).
///
/// All `CommandConfig` fields have `#[serde(default)]` so existing configs parse without
/// changes even without this step — this exists purely for discoverability on config
/// upgrade, mirroring [`migrate_orchestration_ensemble`].
///
/// # Errors
///
/// Returns [`MigrateError`] if `toml_src` fails to parse as TOML.
pub fn migrate_orchestration_command_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if section_header_present(toml_src, "orchestration.command")
        || toml_src.contains("[orchestration.command]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    if !doc.contains_key("orchestration") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Command-style dynamic task handoff — lets a node's agent route \
         execution to a\n\
         # named already-planned node at runtime and write into the cross-thread store's \
         shared\n\
         # state channel. Off by default (spec-080, #6363).\n\
         # [orchestration.command]\n\
         # enabled = false\n\
         # max_handoffs = 16           # per-graph livelock budget, must be > 0\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["orchestration.command".to_owned()],
    })
}

/// Step 78 — add a commented-out `[skills.registry]` section with defaults if absent
/// (spec-045, #5869).
///
/// All `RegistryConfig` fields have `#[serde(default)]`, so existing configs parse without
/// changes; this step only surfaces the new opt-in section for users upgrading from older
/// configs. Always writes `enabled = false` in the commented template — a migration must never
/// silently opt a config into a network-calling feature.
///
/// Idempotent: checks both an active and a previously-injected commented header before doing
/// anything, mirroring `migrate_worktree_config` (`infra.rs`).
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] if the TOML cannot be parsed.
pub fn migrate_skills_registry(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let commented_present = toml_src.lines().any(|l| l.trim() == "# [skills.registry]");
    if section_header_present(toml_src, "skills.registry") || commented_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    let raw = doc.to_string();
    let comment = "\n# External skill/plugin registry discovery (spec-045, #5869). Off by\n\
         # default — no network call is made to any registry unless explicitly opted in. See\n\
         # `zeph skill search --help` / `zeph plugin search --help`.\n\
         # [skills.registry]\n\
         # enabled = false\n\
         # backend_kind = \"skills-sh\"\n\
         # backend_url = \"https://www.skills.sh\"\n\
         # auth_vault_key = \"ZEPH_SKILL_REGISTRY_TOKEN\"  # set via `zeph vault set <key> <token>`\n\
         # registry_timeout_secs = 30\n";
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["skills.registry".to_owned()],
    })
}

/// Regex matching the `[skills.trust]` section header line (used by step 80).
static SKILLS_TRUST_HEADER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*\[skills\.trust\][ \t]*(?:#[^\r\n]*)?\r?\n").expect("static pattern")
});

/// Step 80 — add a commented `require_integrity_check_on_promote = true` advisory to an
/// existing active `[skills.trust]` table (#6087).
///
/// The field has `#[serde(default = "default_true")]`, so existing configs already behave as
/// if it were `true` without this migration — this step only surfaces the new key for
/// discoverability on configs that already declare `[skills.trust]` explicitly. No-op when
/// `[skills.trust]` is absent (nothing to annotate) or the key (active or commented) is
/// already present.
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_skill_trust_require_check(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if !section_header_present(toml_src, "skills.trust") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let already_present = toml_src.lines().any(|l| {
        l.trim()
            .trim_start_matches('#')
            .trim()
            .starts_with("require_integrity_check_on_promote")
    });
    if already_present || !SKILLS_TRUST_HEADER_RE.is_match(toml_src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# require_integrity_check_on_promote = true  # arm per-invocation blake3 \
        re-check on promotion to trusted/verified; override with --no-require-check (#6087)\n";
    let output = SKILLS_TRUST_HEADER_RE
        .replacen(toml_src, 1, |caps: &regex::Captures| {
            format!("{}{comment}", &caps[0])
        })
        .into_owned();

    let changed = output != toml_src;
    let changed_count = usize::from(changed);
    Ok(MigrationResult {
        output,
        changed_count,
        sections_changed: if changed {
            vec!["skills.trust.require_integrity_check_on_promote".to_owned()]
        } else {
            Vec::new()
        },
    })
}
