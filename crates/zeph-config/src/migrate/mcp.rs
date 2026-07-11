// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Mcp client config migration steps.
//!
//! Extracted from the former `migrate/mod.rs` monolith (#4874). Shared TOML helpers,
//! the [`Migration`](super::Migration) trait, and the [`MIGRATIONS`](super::MIGRATIONS)
//! registry remain in the parent module.

use super::{MigrateError, MigrationResult, section_header_present};

/// Migrate `[[mcp.servers]]` entries to add `trust_level = "trusted"` for any entry
/// that lacks an explicit `trust_level`.
///
/// Before this PR all config-defined servers skipped SSRF validation (equivalent to
/// `trust_level = "trusted"`). Without migration, upgrading to the new default
/// (`Untrusted`) would silently break remote servers on private networks.
///
/// This function adds `trust_level = "trusted"` only to entries that are missing the
/// field, preserving entries that already have it set.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_mcp_trust_levels(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    let mut added = 0usize;

    let Some(mcp) = doc.get_mut("mcp").and_then(toml_edit::Item::as_table_mut) else {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    };

    let Some(servers) = mcp
        .get_mut("servers")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
    else {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    };

    for entry in servers.iter_mut() {
        if !entry.contains_key("trust_level") {
            entry.insert(
                "trust_level",
                toml_edit::value(toml_edit::Value::from("trusted")),
            );
            added += 1;
        }
    }

    if added > 0 {
        eprintln!(
            "Migration: added trust_level = \"trusted\" to {added} [[mcp.servers]] \
             entr{} (preserving previous SSRF-skip behavior). \
             Review and adjust trust levels as needed.",
            if added == 1 { "y" } else { "ies" }
        );
    }

    Ok(MigrationResult {
        output: doc.to_string(),
        changed_count: added,
        sections_changed: if added > 0 {
            vec!["mcp.servers.trust_level".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Add commented-out MCP elicitation keys to `[mcp]` section if absent (#3141).
///
/// All elicitation fields have `#[serde(default)]` so existing configs parse without changes.
///
/// Idempotent: the guard for the anchor section checks for an *active* `[mcp]` header via
/// [`section_header_present`], not a naive substring match on `"[mcp]\n"` — the latter also
/// matches inside a commented `# [mcp]` header (e.g. the stub the `ConfigMigrator` catch-all
/// pass writes when the whole section is absent), which would splice this step's advisory
/// comment into the middle of that commented block on a later run instead of staying a no-op
/// (#6018).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_mcp_elicitation_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: the written form is always commented, so a plain substring check catches
    // both the active and commented form of the key.
    if toml_src.contains("elicitation_enabled") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    // Only inject under an active [mcp] section. Also guards against Windows line endings
    // or `[mcp]` at EOF, where the literal `"[mcp]\n"` anchor used by `replacen` below
    // would not match.
    if !section_header_present(toml_src, "mcp") || !toml_src.contains("[mcp]\n") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# elicitation_enabled = false          \
        # opt-in: servers may request user input mid-task (#3141)\n\
        # elicitation_timeout = 120            # seconds to wait for user response\n\
        # elicitation_queue_capacity = 16      # beyond this limit requests are auto-declined\n\
        # elicitation_warn_sensitive_fields = true  # warn before prompting for password/token/etc.\n";
    let output = toml_src.replacen("[mcp]\n", &format!("[mcp]\n{comment}"), 1);

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["mcp.elicitation".to_owned()],
    })
}

/// Add a commented-out `max_connect_attempts` key under `[mcp]` if absent (#3568).
///
/// This key was introduced alongside the MCP startup auto-retry feature. All prior
/// configs omit it and get the default value of `3`. This migration surfaces the key
/// as a comment so users can discover and tune it.
///
/// Idempotent: the anchor guard requires an *active* `[mcp]` header via
/// [`section_header_present`] rather than a naive substring match on `"[mcp]\n"`, which
/// would also match inside a commented `# [mcp]` header (#6018, same defect shape as
/// [`migrate_mcp_elicitation_config`]).
///
/// # Errors
///
/// Returns `Ok` with unchanged output when the key is already present or `[mcp]` is absent.
pub fn migrate_mcp_max_connect_attempts(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("max_connect_attempts") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    if !section_header_present(toml_src, "mcp") || !toml_src.contains("[mcp]\n") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# max_connect_attempts = 3  \
        # startup retry count per server (1 = no retry, 1..=10, backoff: 500ms/1s/2s/...)\n";
    let output = toml_src.replacen("[mcp]\n", &format!("[mcp]\n{comment}"), 1);

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["mcp".to_owned()],
    })
}

/// Add commented-out `startup_retry_backoff_ms` and `tool_timeout_secs` keys under `[mcp]`
/// if absent (#4004).
///
/// Both keys have `#[serde(default)]` and require no user action; this migration surfaces them
/// so operators can discover and tune the new retry and per-call timeout settings.
///
/// Idempotent: the anchor guard requires an *active* `[mcp]` header via
/// [`section_header_present`] rather than a naive substring match on `"[mcp]\n"`, which
/// would also match inside a commented `# [mcp]` header (#6018, same defect shape as
/// [`migrate_mcp_elicitation_config`]).
///
/// # Errors
///
/// Returns `Ok` with unchanged output when either key is already present or `[mcp]` is absent.
pub fn migrate_mcp_retry_and_tool_timeout(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let has_backoff = toml_src.contains("startup_retry_backoff_ms");
    let has_timeout = toml_src.contains("tool_timeout_secs");

    if (has_backoff && has_timeout)
        || !section_header_present(toml_src, "mcp")
        || !toml_src.contains("[mcp]\n")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut output = toml_src.to_owned();
    let mut changed = false;

    if !has_backoff {
        let comment = "# startup_retry_backoff_ms = 1000  \
            # base backoff ms between startup retries (doubles per attempt, cap 8000 ms)\n";
        output = output.replacen("[mcp]\n", &format!("[mcp]\n{comment}"), 1);
        changed = true;
    }

    if !has_timeout {
        let comment = "# tool_timeout_secs = 60  \
            # per-call timeout for tools/call requests; when absent, per-server timeout is used\n";
        output = output.replacen("[mcp]\n", &format!("[mcp]\n{comment}"), 1);
        changed = true;
    }

    if changed {
        Ok(MigrationResult {
            output,
            changed_count: 1,
            sections_changed: vec!["mcp".to_owned()],
        })
    } else {
        Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        })
    }
}
