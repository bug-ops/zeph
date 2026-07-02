// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `[serve]` config migration step (spec-068 §9, #5343).

use super::{MigrateError, MigrationResult};

/// Append a commented-out `[serve]` block if the config lacks it (spec-068 §9, #5343).
///
/// `ServeConfig` uses `#[serde(default)]` so existing configs without this section parse fine
/// (the `zeph serve` command is opt-in via the CLI subcommand regardless of config presence).
/// This step is discoverability-only, mirroring
/// [`super::session::migrate_acp_subagents_config`].
///
/// Idempotent: skipped when `[serve]` (active or commented) is already present.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_serve_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src
        .lines()
        .any(|l| l.trim() == "[serve]" || l.trim() == "# [serve]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [serve] — `zeph serve` persistent agent service (spec-068 §9, #5343).\n\
         # [serve]\n\
         # http_addr = \"127.0.0.1:8420\"          # HTTP/SSE API bind address\n\
         # require_auth = true                    # require a bearer token on /sessions* endpoints\n\
         # auth_token_vault_key = \"ZEPH_SERVE_AUTH_TOKEN\"  # age-vault key name for the bearer token\n\
         # max_sessions = 50                       # concurrent live sessions LiveSessionRegistry holds\n\
         # session_idle_ttl_secs = 1800            # idle SessionActor eviction threshold\n\
         # max_queued_prompts = 8                  # per-session prompt mailbox capacity\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["serve".to_owned()],
    })
}
