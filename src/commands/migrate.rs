// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use similar::{ChangeTag, TextDiff};
use zeph_core::config::migrate::{
    ConfigMigrator, migrate_acp_subagents_config, migrate_agent_budget_hint,
    migrate_agent_retry_to_tools_retry, migrate_autodream_config,
    migrate_compression_predictor_config, migrate_database_url, migrate_egress_config,
    migrate_forgetting_config, migrate_hooks_permission_denied_config, migrate_magic_docs_config,
    migrate_mcp_elicitation_config, migrate_mcp_trust_levels, migrate_memory_graph_config,
    migrate_microcompact_config, migrate_orchestration_persistence, migrate_otel_filter,
    migrate_planner_model_to_provider, migrate_quality_config, migrate_sandbox_config,
    migrate_session_recap_config, migrate_shell_transactional, migrate_stt_to_provider,
    migrate_supervisor_config, migrate_telemetry_config, migrate_vigil_config,
};

/// Handle the `zeph migrate-config` command.
///
/// # Errors
///
/// Returns an error if the config file cannot be read, the migration fails, or the
/// in-place write fails.
#[allow(clippy::too_many_lines)]
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

    // Step 9: strip obsolete [memory.compression.predictor] section (#3251).
    let predictor_result = migrate_compression_predictor_config(&after_forgetting)?;
    let after_predictor = predictor_result.output;

    // Step 10: add commented-out [memory.microcompact] block if absent (#2699).
    let microcompact_result = migrate_microcompact_config(&after_predictor)?;
    let after_microcompact = microcompact_result.output;

    // Step 11: add commented-out [memory.autodream] block if absent (#2697).
    let autodream_result = migrate_autodream_config(&after_microcompact)?;
    let after_autodream = autodream_result.output;

    // Step 12: add commented-out [magic_docs] block if absent (#2702).
    let magic_docs_result = migrate_magic_docs_config(&after_autodream)?;
    let after_magic_docs = magic_docs_result.output;

    // Step 13: add commented-out [telemetry] block if absent (#2846).
    let telemetry_result = migrate_telemetry_config(&after_magic_docs)?;
    let after_telemetry = telemetry_result.output;

    // Step 14: add commented-out [agent.supervisor] block if absent (#2883).
    let supervisor_result = migrate_supervisor_config(&after_telemetry)?;
    let after_supervisor = supervisor_result.output;

    // Step 15: add commented-out otel_filter key under [telemetry] if absent (#2997).
    let otel_filter_result = migrate_otel_filter(&after_supervisor)?;
    let after_otel_filter = otel_filter_result.output;

    // Step 16: add commented-out [tools.egress] block if absent (#3058).
    let egress_result = migrate_egress_config(&after_otel_filter)?;
    let after_egress = egress_result.output;

    // Step 17: add commented-out [security.vigil] block if absent (#3058).
    let vigil_result = migrate_vigil_config(&after_egress)?;
    let after_vigil = vigil_result.output;

    // Step 18: add commented-out [tools.sandbox] block if absent (#3070).
    let sandbox_result = migrate_sandbox_config(&after_vigil)?;
    let after_sandbox = sandbox_result.output;

    // Step 19: add commented-out persistence_enabled under [orchestration] if absent (#3107).
    let orch_persistence_result = migrate_orchestration_persistence(&after_sandbox)?;
    let after_orch_persistence = orch_persistence_result.output;

    // Step 20: add commented-out [session.recap] block if absent (#3064).
    let session_recap_result = migrate_session_recap_config(&after_orch_persistence)?;
    let after_session_recap = session_recap_result.output;

    // Step 21: add commented-out MCP elicitation keys under [mcp] if absent (#3141).
    let mcp_elicitation_result = migrate_mcp_elicitation_config(&after_session_recap)?;
    let after_mcp_elicitation = mcp_elicitation_result.output;

    // Step 22: add commented-out [quality] block if absent (#3228).
    let quality_result = migrate_quality_config(&after_mcp_elicitation)?;
    let after_quality = quality_result.output;

    // Step 23: add commented-out [acp.subagents] block if absent (#3304).
    let acp_subagents_result = migrate_acp_subagents_config(&after_quality)?;
    let after_acp_subagents = acp_subagents_result.output;

    // Step 24: add commented-out [[hooks.permission_denied]] block if absent (#3309).
    let hooks_perm_denied_result = migrate_hooks_permission_denied_config(&after_acp_subagents)?;
    let after_hooks_perm_denied = hooks_perm_denied_result.output;

    // Step 25: add commented-out [memory.graph] retrieval strategy options if absent (#3317).
    let memory_graph_result = migrate_memory_graph_config(&after_hooks_perm_denied)?;
    let after_memory_graph = memory_graph_result.output;

    // Step 26: add missing default keys as commented-out entries.
    let migrator = ConfigMigrator::new();
    let result = migrator.migrate(&after_memory_graph)?;

    if diff {
        print_diff(&input, &result.output);
        if stt_result.changed_count > 0 {
            eprintln!("STT migration: moved model/base_url to [[llm.providers]] entry.");
        }
        if planner_result.changed_count > 0 {
            eprintln!(
                "Planner migration: planner_model renamed to planner_provider (value commented out)."
            );
        }
        if trust_result.changed_count > 0 {
            eprintln!(
                "MCP trust migration: added trust_level = \"trusted\" to {} [[mcp.servers]] entries.",
                trust_result.changed_count
            );
        }
        if retry_result.changed_count > 0 {
            eprintln!("Retry migration: [agent] retry fields migrated to [tools.retry].");
        }
        if db_url_result.changed_count > 0 {
            eprintln!("Database URL migration: added database_url placeholder under [memory].");
        }
        if shell_txn_result.changed_count > 0 {
            eprintln!(
                "Shell transactional migration: added commented-out transactional fields to [tools.shell]."
            );
        }
        if budget_hint_result.changed_count > 0 {
            eprintln!("Budget hint migration: added commented-out budget_hint_enabled to [agent].");
        }
        if forgetting_result.changed_count > 0 {
            eprintln!("Forgetting migration: added commented-out [memory.forgetting] section.");
        }
        if predictor_result.changed_count > 0 {
            eprintln!(
                "Predictor migration: removed obsolete [memory.compression.predictor] section."
            );
        }
        if microcompact_result.changed_count > 0 {
            eprintln!("Microcompact migration: added commented-out [memory.microcompact] block.");
        }
        if autodream_result.changed_count > 0 {
            eprintln!("autoDream migration: added commented-out [memory.autodream] block.");
        }
        if magic_docs_result.changed_count > 0 {
            eprintln!("MagicDocs migration: added commented-out [magic_docs] block.");
        }
        if telemetry_result.changed_count > 0 {
            eprintln!("Telemetry migration: added commented-out [telemetry] block.");
        }
        if supervisor_result.changed_count > 0 {
            eprintln!("Supervisor migration: added commented-out [agent.supervisor] block.");
        }
        if otel_filter_result.changed_count > 0 {
            eprintln!(
                "OTLP filter migration: added commented-out otel_filter key under [telemetry]."
            );
        }
        if egress_result.changed_count > 0 {
            eprintln!("Egress migration: added commented-out [tools.egress] block.");
        }
        if vigil_result.changed_count > 0 {
            eprintln!("VIGIL migration: added commented-out [security.vigil] block.");
        }
        if sandbox_result.changed_count > 0 {
            eprintln!("Sandbox migration: added commented-out [tools.sandbox] block.");
        }
        if orch_persistence_result.changed_count > 0 {
            eprintln!(
                "Orchestration persistence migration: \
                 added commented-out persistence_enabled under [orchestration]."
            );
        }
        if session_recap_result.changed_count > 0 {
            eprintln!("Session recap migration: added commented-out [session.recap] block.");
        }
        if mcp_elicitation_result.changed_count > 0 {
            eprintln!(
                "MCP elicitation migration: added commented-out elicitation keys under [mcp]."
            );
        }
        if quality_result.changed_count > 0 {
            eprintln!("Quality migration: added commented-out [quality] block.");
        }
        if acp_subagents_result.changed_count > 0 {
            eprintln!("ACP subagents migration: added commented-out [acp.subagents] block.");
        }
        if hooks_perm_denied_result.changed_count > 0 {
            eprintln!(
                "Hooks permission_denied migration: \
                 added commented-out [[hooks.permission_denied]] block."
            );
        }
        if memory_graph_result.changed_count > 0 {
            eprintln!(
                "Memory graph migration: added commented-out [memory.graph] retrieval options."
            );
        }
        eprintln!(
            "Migration would add {} entries ({} sections).",
            result.changed_count,
            result.sections_changed.len()
        );
    } else if in_place {
        atomic_write(config_path, &result.output)?;
        eprintln!(
            "Config migrated in-place: {} ({} entries added, sections: {})",
            config_path.display(),
            result.changed_count,
            if result.sections_changed.is_empty() {
                "none".to_owned()
            } else {
                result.sections_changed.join(", ")
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
