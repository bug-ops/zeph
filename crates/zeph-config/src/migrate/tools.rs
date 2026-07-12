// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent retry, budget, and tool execution config migration steps.
//!
//! Extracted from the former `migrate/mod.rs` monolith (#4874). Shared TOML helpers,
//! the [`Migration`](super::Migration) trait, and the [`MIGRATIONS`](super::MIGRATIONS)
//! registry remain in the parent module.

use super::{MigrateError, MigrationResult, section_header_present};

/// Migrate `[agent].max_tool_retries` → `[tools.retry].max_attempts` and
/// `[agent].max_retry_duration_secs` → `[tools.retry].budget_secs`.
///
/// Old fields are preserved (not removed) to avoid breaking configs that rely on them
/// until they are officially deprecated in a future release. The new `[tools.retry]` section
/// is added if missing, populated with the migrated values.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML is invalid.
pub fn migrate_agent_retry_to_tools_retry(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let max_retries = doc
        .get("agent")
        .and_then(toml_edit::Item::as_table)
        .and_then(|t| t.get("max_tool_retries"))
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_integer)
        .map(i64::cast_unsigned);

    let budget_secs = doc
        .get("agent")
        .and_then(toml_edit::Item::as_table)
        .and_then(|t| t.get("max_retry_duration_secs"))
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_integer)
        .map(i64::cast_unsigned);

    if max_retries.is_none() && budget_secs.is_none() {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Ensure [tools.retry] section exists.
    if !doc.contains_key("tools") {
        doc.insert("tools", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let tools_table = doc
        .get_mut("tools")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure("[tools] is not a table"))?;

    if !tools_table.contains_key("retry") {
        tools_table.insert("retry", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let retry_table = tools_table
        .get_mut("retry")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or(MigrateError::InvalidStructure(
            "[tools.retry] is not a table",
        ))?;

    let mut changed_count = 0usize;

    if let Some(retries) = max_retries
        && !retry_table.contains_key("max_attempts")
    {
        retry_table.insert(
            "max_attempts",
            toml_edit::value(i64::try_from(retries).unwrap_or(2)),
        );
        changed_count += 1;
    }

    if let Some(secs) = budget_secs
        && !retry_table.contains_key("budget_secs")
    {
        retry_table.insert(
            "budget_secs",
            toml_edit::value(i64::try_from(secs).unwrap_or(30)),
        );
        changed_count += 1;
    }

    if changed_count > 0 {
        eprintln!(
            "Migration: [agent].max_tool_retries / max_retry_duration_secs migrated to \
             [tools.retry].max_attempts / budget_secs. Old fields preserved for compatibility."
        );
    }

    Ok(MigrationResult {
        output: doc.to_string(),
        changed_count,
        sections_changed: if changed_count > 0 {
            vec!["tools.retry".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Migration step: add `budget_hint_enabled` as a commented-out entry under `[agent]` if absent.
///
/// # Errors
///
/// Returns an error if the config cannot be parsed or the `[agent]` section is malformed.
pub fn migrate_agent_budget_hint(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if toml_src.contains("budget_hint_enabled") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    if !doc.contains_key("agent") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Inject <budget> XML into the system prompt so the LLM can self-regulate (#2267).\n\
         # budget_hint_enabled = true\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["agent.budget_hint_enabled".to_owned()],
    })
}

/// Add a commented-out `[quality]` block if the config lacks it (#3228).
///
/// Introduced alongside the MARCH self-check pipeline (#3226). All `QualityConfig`
/// fields have `#[serde(default)]` so existing configs parse without changes; this
/// migration only surfaces the section so users can discover and enable it.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_quality_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: line-anchored check avoids false-positives on [quality.foo] subtables.
    if section_header_present(toml_src, "quality")
        || toml_src.lines().any(|l| l.trim() == "# [quality]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [quality] — MARCH Proposer+Checker self-check pipeline (#3226, #3228).\n\
         # [quality]\n\
         # self_check = false                    # enable post-response self-check\n\
         # trigger = \"has_retrieval\"             # has_retrieval | always | manual\n\
         # latency_budget_ms = 4000              # hard ceiling for the whole pipeline\n\
         # proposer_provider = \"\"                # optional: provider name from [[llm.providers]]\n\
         # checker_provider = \"\"                 # optional: provider name from [[llm.providers]]\n\
         # min_evidence = 0.6                    # 0.0..1.0; below → flag assertion\n\
         # async_run = false                     # true = fire-and-forget (non-blocking)\n\
         # per_call_timeout_ms = 2000            # per-LLM-call timeout\n\
         # max_assertions = 12                   # maximum assertions extracted from one response\n\
         # max_response_chars = 8000             # skip pipeline when response exceeds this\n\
         # cache_disabled_for_checker = true     # suppress prompt-cache on Checker provider\n\
         # flag_marker = \"[verify]\"              # marker appended when assertions are flagged\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["quality".to_owned()],
    })
}

/// Add the `[tools.compression]` section as commented-out defaults when it is absent.
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] when `toml_src` is not valid TOML.
pub fn migrate_tools_compression_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "tools.compression")
        || toml_src
            .lines()
            .any(|l| l.trim() == "# [tools.compression]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# TACO self-evolving tool output compression (#3306).\n\
         # [tools.compression]\n\
         # enabled = false\n\
         # min_lines_to_compress = 10\n\
         # evolution_provider = \"\"\n\
         # evolution_min_interval_secs = 3600\n\
         # max_rules = 200\n";

    Ok(MigrationResult {
        output: format!("{toml_src}{comment}"),
        changed_count: 1,
        sections_changed: vec!["tools.compression".to_owned()],
    })
}

/// Add `policy_provider` and `utility_window` to `[tools.policy]` / `[tools.utility]`
/// as commented-out defaults when they are absent.
///
/// Introduced by spec-068 (#5067): `policy_provider` selects the LLM for policy checks;
/// `utility_window` is the consecutive low-utility call threshold for hard-stopping.
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] when `toml_src` is not valid TOML.
pub fn migrate_policy_provider_and_utility_window(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    let already_policy = toml_src.contains("policy_provider");
    let already_window = toml_src.contains("utility_window");
    if already_policy && already_window {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut output = toml_src.to_owned();
    let mut changed_count = 0usize;
    let mut sections_changed: Vec<String> = Vec::new();

    if !already_policy {
        output.push_str(
            "\n# LLM provider name for policy checks. Empty = disabled (rules-only). (#5068)\n\
             # [tools.policy]\n\
             # policy_provider = \"\"\n",
        );
        changed_count += 1;
        sections_changed.push("tools.policy".to_owned());
    }

    if !already_window {
        output.push_str(
            "\n# Consecutive low-utility tool calls before the loop hard-stops. 0 = disabled. (#5068)\n\
             # [tools.utility]\n\
             # utility_window = 0\n",
        );
        changed_count += 1;
        sections_changed.push("tools.utility".to_owned());
    }

    Ok(MigrationResult {
        output,
        changed_count,
        sections_changed,
    })
}

/// Add a commented-out `high_gain_tools` hint under `[tools.utility]` when absent.
///
/// Introduced alongside `UtilityScoringConfig::high_gain_tools` (#5659): an opt-in list of
/// tool ids that always receive the same `0.75` "direct action" gain tier as
/// `diagnostics`/`edit`/etc, most useful for MCP-registered tools whose ids the built-in
/// `default_gain` table can never match. The field has `#[serde(default)]` (empty `Vec`), so
/// existing configs parse unchanged; this step only surfaces the option so users can
/// discover it.
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] when `toml_src` is not valid TOML.
pub fn migrate_utility_high_gain_tools(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("high_gain_tools") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Tool ids that always receive the 0.75 \"direct action\" gain tier, bypassing\n\
         # the Retrieve detour on their first call. Useful for MCP-registered tools, whose\n\
         # ids (\"{server_id}_{name}\") never match the built-in default_gain table (#5659).\n\
         # [tools.utility]\n\
         # high_gain_tools = []\n";

    Ok(MigrationResult {
        output: format!("{toml_src}{comment}"),
        changed_count: 1,
        sections_changed: vec!["tools.utility.high_gain_tools".to_owned()],
    })
}

/// Add `checkpoints_enabled` and `max_checkpoints` to `[tools.shell]` as commented-out defaults.
///
/// # Errors
///
/// Returns [`MigrateError::Parse`] when `toml_src` is not valid TOML.
pub fn migrate_shell_checkpoints_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("checkpoints_enabled") || toml_src.contains("max_checkpoints") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Session-scoped /undo and /redo checkpoint history (#4990).\n\
         # Checkpoints are in-memory only and lost on agent restart.\n\
         # [tools.shell]\n\
         # checkpoints_enabled = false\n\
         # max_checkpoints = 20\n";

    Ok(MigrationResult {
        output: format!("{toml_src}{comment}"),
        changed_count: 1,
        sections_changed: vec!["tools.shell".to_owned()],
    })
}
