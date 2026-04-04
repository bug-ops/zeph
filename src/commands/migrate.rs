// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use similar::{ChangeTag, TextDiff};
use zeph_core::config::migrate::{
    ConfigMigrator, migrate_agent_budget_hint, migrate_agent_retry_to_tools_retry,
    migrate_compression_predictor_config, migrate_database_url, migrate_forgetting_config,
    migrate_mcp_trust_levels, migrate_planner_model_to_provider, migrate_shell_transactional,
    migrate_stt_to_provider,
};

/// Handle the `zeph migrate-config` command.
///
/// # Errors
///
/// Returns an error if the config file cannot be read, the migration fails, or the
/// in-place write fails.
pub(crate) fn handle_migrate_config(
    config_path: &Path,
    in_place: bool,
    diff: bool,
) -> anyhow::Result<()> {
    let input = if config_path.exists() {
        std::fs::read_to_string(config_path)?
    } else {
        String::new()
    };

    // Step 1: migrate [llm.stt] model/base_url fields to [[llm.providers]] stt_model.
    let stt_result = migrate_stt_to_provider(&input)?;
    let after_stt = stt_result.output;

    // Step 2: migrate [orchestration] planner_model → planner_provider (rename + semantic change).
    let planner_result = migrate_planner_model_to_provider(&after_stt)?;
    let after_planner = planner_result.output;

    // Step 3: add trust_level = "trusted" to existing [[mcp.servers]] entries that lack it,
    // preserving the previous behavior where all config-defined servers skipped SSRF validation.
    let trust_result = migrate_mcp_trust_levels(&after_planner)?;
    let after_trust = trust_result.output;

    // Step 4: migrate [agent].max_tool_retries / max_retry_duration_secs → [tools.retry].
    let retry_result = migrate_agent_retry_to_tools_retry(&after_trust)?;
    let after_retry = retry_result.output;

    // Step 5: add commented-out database_url under [memory] if absent.
    let db_url_result = migrate_database_url(&after_retry)?;
    let after_db_url = db_url_result.output;

    // Step 6: add commented-out [tools.shell] transactional fields if absent (#2414).
    let shell_txn_result = migrate_shell_transactional(&after_db_url)?;
    let after_shell_txn = shell_txn_result.output;

    // Step 7: add commented-out budget_hint_enabled to [agent] if absent (#2267).
    let budget_hint_result = migrate_agent_budget_hint(&after_shell_txn)?;
    let after_budget_hint = budget_hint_result.output;

    // Step 8: add commented-out [memory.forgetting] section if absent (#2397).
    let forgetting_result = migrate_forgetting_config(&after_budget_hint)?;
    let after_forgetting = forgetting_result.output;

    // Step 9: add commented-out [memory.compression.predictor] block if absent (#2460).
    let predictor_result = migrate_compression_predictor_config(&after_forgetting)?;
    let after_predictor = predictor_result.output;

    // Step 10: add missing default keys as commented-out entries.
    let migrator = ConfigMigrator::new();
    let result = migrator.migrate(&after_predictor)?;

    if diff {
        print_diff(&input, &result.output);
        if stt_result.added_count > 0 {
            eprintln!("STT migration: moved model/base_url to [[llm.providers]] entry.");
        }
        if planner_result.added_count > 0 {
            eprintln!(
                "Planner migration: planner_model renamed to planner_provider (value commented out)."
            );
        }
        if trust_result.added_count > 0 {
            eprintln!(
                "MCP trust migration: added trust_level = \"trusted\" to {} [[mcp.servers]] entries.",
                trust_result.added_count
            );
        }
        if retry_result.added_count > 0 {
            eprintln!("Retry migration: [agent] retry fields migrated to [tools.retry].");
        }
        if db_url_result.added_count > 0 {
            eprintln!("Database URL migration: added database_url placeholder under [memory].");
        }
        if shell_txn_result.added_count > 0 {
            eprintln!(
                "Shell transactional migration: added commented-out transactional fields to [tools.shell]."
            );
        }
        if budget_hint_result.added_count > 0 {
            eprintln!("Budget hint migration: added commented-out budget_hint_enabled to [agent].");
        }
        if forgetting_result.added_count > 0 {
            eprintln!("Forgetting migration: added commented-out [memory.forgetting] section.");
        }
        if predictor_result.added_count > 0 {
            eprintln!(
                "Predictor migration: added commented-out [memory.compression.predictor] block."
            );
        }
        eprintln!(
            "Migration would add {} entries ({} sections).",
            result.added_count,
            result.sections_added.len()
        );
    } else if in_place {
        atomic_write(config_path, &result.output)?;
        eprintln!(
            "Config migrated in-place: {} ({} entries added, sections: {})",
            config_path.display(),
            result.added_count,
            if result.sections_added.is_empty() {
                "none".to_owned()
            } else {
                result.sections_added.join(", ")
            }
        );
    } else {
        print!("{}", result.output);
    }

    Ok(())
}

/// Print a unified-style diff between `old` and `new`.
fn print_diff(old: &str, new: &str) {
    let diff = TextDiff::from_lines(old, new);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => print!(" {change}"),
            ChangeTag::Insert => print!("+{change}"),
            ChangeTag::Delete => print!("-{change}"),
        }
    }
}

/// Write `content` to `path` atomically using a temporary file in the same directory,
/// preserving the original file's permissions before renaming into place.
fn atomic_write(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::io::Write;

    let original_perms = if path.exists() {
        Some(std::fs::metadata(path)?.permissions())
    } else {
        None
    };

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;

    if let Some(perms) = original_perms {
        std::fs::set_permissions(tmp.path(), perms)?;
    }

    tmp.persist(path)?;

    Ok(())
}
