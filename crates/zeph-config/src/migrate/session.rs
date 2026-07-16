// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session persistence and lifecycle hooks config migration steps.
//!
//! Extracted from the former `migrate/mod.rs` monolith (#4874). Shared TOML helpers,
//! the [`Migration`](super::Migration) trait, and the [`MIGRATIONS`](super::MIGRATIONS)
//! registry remain in the parent module.

use super::{MigrateError, MigrationResult, section_header_present};

/// Add commented-out `[session.recap]` block if absent (#3064).
///
/// All recap fields have `#[serde(default)]` so existing configs parse without changes.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_session_recap_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: check both active and commented forms.
    if section_header_present(toml_src, "session.recap") || toml_src.contains("# [session.recap]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [session.recap] — show a recap when resuming a conversation (#3064).\n\
         # [session.recap]\n\
         # on_resume = true\n\
         # max_tokens = 200\n\
         # provider = \"\"\n\
         # max_input_messages = 20\n";
    let raw = toml_src.parse::<toml_edit::DocumentMut>()?.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["session.recap".to_owned()],
    })
}

/// Add a commented-out `[acp.subagents]` block if the config lacks it (#3304).
///
/// Introduced alongside the ACP sub-agent delegation feature (#3289). All `AcpSubagentsConfig`
/// fields have `#[serde(default)]` so existing configs parse without changes; this migration
/// only surfaces the section so users can discover and enable it.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_acp_subagents_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "acp.subagents")
        || toml_src.lines().any(|l| l.trim() == "# [acp.subagents]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [acp.subagents] — sub-agent delegation via ACP protocol (#3289).\n\
         # [acp.subagents]\n\
         # enabled = false\n\
         #\n\
         # [[acp.subagents.presets]]\n\
         # name = \"inner\"                         # identifier used in /subagent commands\n\
         # command = \"cargo run --quiet -- --acp\" # shell command to spawn the sub-agent\n\
         # # cwd = \"/path/to/agent\"              # optional working directory\n\
         # # handshake_timeout_secs = 30          # initialize+session/new timeout\n\
         # # prompt_timeout_secs = 600            # single round-trip timeout\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["acp.subagents".to_owned()],
    })
}

/// Add a commented-out `[[acp.auth_clients]]` block if the config lacks it (#5868).
///
/// Introduced alongside named-client bearer-token isolation for ACP HTTP/WS session
/// persistence scoping. `AcpConfig::auth_clients` has `#[serde(default)]` so existing
/// configs (including those with only the legacy scalar `[acp] auth_token`) parse without
/// changes; this migration only surfaces the new array for discoverability.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_acp_auth_clients_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src
        .lines()
        .any(|l| l.trim() == "[[acp.auth_clients]]" || l.trim() == "# [[acp.auth_clients]]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [[acp.auth_clients]] — named bearer-token clients for genuine \
         multi-tenant/multi-window\n\
         # isolation of ACP HTTP/WS persisted session listing (#5868). Coexists with the \
         legacy\n\
         # scalar `auth_token` above (synthesized as the \"default\" client). Reserved ids: \
         \"default\",\n\
         # \"acp-local\". Exactly one of `token` / `token_vault_key` must be set per entry.\n\
         # [[acp.auth_clients]]\n\
         # id = \"zed-alice\"\n\
         # token = \"...\"                # inline, or:\n\
         # token_vault_key = \"ZEPH_ACP_TOKEN_ALICE\"  # resolved from the age vault at startup\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["acp.auth_clients".to_owned()],
    })
}

/// Add a commented-out `[[hooks.permission_denied]]` block if the config lacks it (#3309).
///
/// Introduced alongside the reactive env hooks and MCP tool dispatch feature (#3303).
/// All hook arrays have `#[serde(default)]` so existing configs parse without changes;
/// this migration surfaces the section for discoverability.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_hooks_permission_denied_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    if toml_src.lines().any(|l| {
        l.trim() == "[[hooks.permission_denied]]" || l.trim() == "# [[hooks.permission_denied]]"
    }) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [[hooks.permission_denied]] — hook fired when a tool call is denied (#3303).\n\
         # Available env vars: ZEPH_TOOL, ZEPH_DENY_REASON, ZEPH_TOOL_INPUT_JSON.\n\
         # [[hooks.permission_denied]]\n\
         # [hooks.permission_denied.action]\n\
         # type = \"command\"\n\
         # command = \"echo denied: $ZEPH_TOOL\"\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["hooks.permission_denied".to_owned()],
    })
}

/// Append a commented-out `[[hooks.turn_complete]]` block to `toml_src` when it is absent (#3308).
///
/// Idempotent: if a `[[hooks.turn_complete]]` or `# [[hooks.turn_complete]]` line already exists,
/// the input is returned unchanged with `changed_count = 0`.
///
/// The template uses a single `command` string (not `args`) to match the `HookAction::Command`
/// schema, and avoids embedding `$ZEPH_TURN_PREVIEW` directly in the command string to prevent
/// shell injection.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the migration
/// function convention for use in chained pipelines.
pub fn migrate_hooks_turn_complete_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src
        .lines()
        .any(|l| l.trim() == "[[hooks.turn_complete]]" || l.trim() == "# [[hooks.turn_complete]]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [[hooks.turn_complete]] — hook fired after every agent turn completes (#3308).\n\
         # Available env vars: ZEPH_TURN_DURATION_MS, ZEPH_TURN_STATUS, ZEPH_TURN_PREVIEW,\n\
         # ZEPH_TURN_LLM_REQUESTS, ZEPH_TURN_TOOL_CALLS.\n\
         # Note: ZEPH_TURN_PREVIEW is available as env var but should not be embedded\n\
         # directly in the command string to avoid shell injection. Use a wrapper script instead.\n\
         # [[hooks.turn_complete]]\n\
         # command = \"osascript -e 'display notification \\\"Task complete\\\" with title \\\"Zeph\\\"'\"\n\
         # timeout_secs = 3\n\
         # fail_closed = false\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["hooks.turn_complete".to_owned()],
    })
}

/// Add `[session]` with `provider_persistence = true` to configs that lack the section (#3308).
///
/// Provider persistence was verified stable in CI-608 (restored persisted provider preference
/// from `SQLite`). Configs that already declare `[session]` or the commented `# [session]` are
/// returned unchanged.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_session_provider_persistence(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "session")
        || toml_src.lines().any(|l| l.trim() == "# [session]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [session] — session-scoped user experience settings (#3308).\n\
         [session]\n\
         # Persist the last-used provider per channel across restarts.\n\
         # When true, the agent saves the active provider name to SQLite after each\n\
         # /provider switch and restores it on the next session start for the same channel.\n\
         provider_persistence = true\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["session".to_owned()],
    })
}

/// Splice `persist_provider_overrides = true` into an existing `[session]` block (#4654).
///
/// `SessionConfig` uses `#[serde(default)]` so existing configs without this key parse
/// fine (the field defaults to `true`). This step is discoverability-only: it adds the
/// commented-out key so users can see the option when they open their config file.
///
/// Idempotent: skipped when the key is already present (commented or live) or when there
/// is no `[session]` section (a `migrate_session_provider_persistence` will add the full
/// block on future runs).
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_session_persist_provider_overrides(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("persist_provider_overrides") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }
    // Also require the literal `"[session]\n"` anchor used by `replacen` below — guards
    // against an inline-commented header (`[session]  # note`) passing `section_header_present`
    // but not matching the anchor, which would otherwise report a change that never happened.
    if !section_header_present(toml_src, "session") || !toml_src.contains("[session]\n") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# persist_provider_overrides = true  \
        # persist generation overrides (e.g. reasoning_effort) alongside provider name (#4654)\n";
    let output = toml_src.replacen("[session]\n", &format!("[session]\n{comment}"), 1);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["session".to_owned()],
    })
}

/// Splice the session-persistence keys (`enabled`, `data_dir`, `encrypt`, `max_event_log_mb`)
/// into an existing `[session]` block and append a commented-out `[session.condense]` block
/// (spec-068, #5343).
///
/// `SessionConfig`/`CondenseConfig` use `#[serde(default)]` so existing configs without these
/// keys parse fine (event-log persistence defaults to enabled). This step is
/// discoverability-only, mirroring [`migrate_session_persist_provider_overrides`].
///
/// Idempotent: skipped when `max_event_log_mb` is already present (commented or live), or when
/// there is no `[session]` section yet (a prior run of
/// [`migrate_session_provider_persistence`] creates the block itself).
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_session_persistence_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("max_event_log_mb") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }
    // Also require the literal `"[session]\n"` anchor used by `replacen` below — guards
    // against an inline-commented header (`[session]  # note`) passing `section_header_present`
    // but not matching the anchor, which would otherwise report a change that never happened.
    if !section_header_present(toml_src, "session") || !toml_src.contains("[session]\n") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let session_keys = "# enabled = true  \
        # maintain a durable, replayable JSONL event log per session (spec-068, #5343)\n\
        # data_dir = \".zeph/sessions\"  # directory for per-session event logs\n\
        # encrypt = false  # opt-in AEAD encryption of session event logs (deferred to post-MVP)\n\
        # max_event_log_mb = 256  # event-log size (MB) that guards a rotate/condense trigger\n";
    let output = toml_src.replacen("[session]\n", &format!("[session]\n{session_keys}"), 1);

    let condense_block = "\n# [session.condense] — durable context condensation policy (spec-068 §8, #5343).\n\
         # [session.condense]\n\
         # condense_provider = \"\"  # [[llm.providers]] name; empty falls back to the primary provider\n\
         # threshold = 0.85            # fraction of context budget that triggers condensation\n\
         # keep_recent = 20            # minimum recent events preserved after condensation\n";
    let output = format!("{output}{condense_block}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["session".to_owned(), "session.condense".to_owned()],
    })
}
