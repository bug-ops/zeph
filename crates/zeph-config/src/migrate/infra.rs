// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Infrastructure (database, shell, telemetry, sandbox, worktree, scheduler) config migration steps.
//!
//! Extracted from the former `migrate/mod.rs` monolith (#4874). Shared TOML helpers,
//! the [`Migration`](super::Migration) trait, and the [`MIGRATIONS`](super::MIGRATIONS)
//! registry remain in the parent module.

use super::{MigrateError, MigrationResult, insert_after_section, section_header_present};
use regex::Regex;
use toml_edit::DocumentMut;

/// Add a commented-out `database_url = ""` entry under `[memory]` if absent.
///
/// If the `[memory]` section does not exist it is created. This migration surfaces the
/// `PostgreSQL` URL option for users upgrading from a pre-postgres config file.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_database_url(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if toml_src.contains("database_url") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Ensure [memory] section exists (created if absent so the comment has context).
    if !doc.contains_key("memory") {
        doc.insert("memory", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let comment = "\n# PostgreSQL connection URL (used when binary is compiled with --features postgres).\n\
         # Leave empty and store the actual URL in the vault:\n\
         #   zeph vault set ZEPH_DATABASE_URL \"postgres://user:pass@localhost:5432/zeph\"\n\
         # database_url = \"\"\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["memory.database_url".to_owned()],
    })
}

/// No-op migration for `[tools.shell]` transactional fields added in #2414.
///
/// All 5 new fields have `#[serde(default)]` so existing configs parse without changes.
/// This step adds them as commented-out hints in `[tools.shell]` if not already present.
///
/// # Errors
///
/// Returns `MigrateError` if the TOML cannot be parsed or `[tools.shell]` is malformed.
pub fn migrate_shell_transactional(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: comments are invisible to toml_edit, so check the raw source.
    if toml_src.contains("transactional") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let tools_shell_exists = doc
        .get("tools")
        .and_then(toml_edit::Item::as_table)
        .is_some_and(|t| t.contains_key("shell"));
    if !tools_shell_exists {
        // No [tools.shell] section — nothing to annotate; new configs will get defaults.
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Transactional shell: snapshot files before write commands, rollback on failure.\n\
         # transactional = false\n\
         # transaction_scope = []          # glob patterns; empty = all extracted paths\n\
         # auto_rollback = false           # rollback when exit code >= 2\n\
         # auto_rollback_exit_codes = []   # explicit exit codes; overrides >= 2 heuristic\n\
         # snapshot_required = false       # abort if snapshot fails (default: warn and proceed)\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["tools.shell.transactional".to_owned()],
    })
}

/// Add a commented-out `[telemetry]` block if the section is absent (#2846).
///
/// Existing configs that were written before the `telemetry` section was introduced will have
/// the block appended as comments so users can discover and enable it without manual hunting.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if `toml_src` is not valid TOML.
pub fn migrate_telemetry_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if doc.contains_key("telemetry") || toml_src.contains("# [telemetry]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n\
         # Profiling and distributed tracing (requires --features profiling). All\n\
         # instrumentation points are zero-overhead when the feature is absent.\n\
         # [telemetry]\n\
         # enabled = false\n\
         # backend = \"local\"        # \"local\" (Chrome JSON), \"otlp\", or \"pyroscope\"\n\
         # trace_dir = \".local/traces\"\n\
         # include_args = false\n\
         # service_name = \"zeph-agent\"\n\
         # sample_rate = 1.0\n\
         # otel_filter = \"info\"     # base EnvFilter for OTLP layer; noisy-crate exclusions always appended\n";

    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["telemetry".to_owned()],
    })
}

/// Add a commented-out `[agent.supervisor]` block if the sub-table is absent (#2883).
///
/// Appended as comments under `[agent]` so users can discover and tune supervisor limits
/// without manual hunting. Safe to call on configs that already have the section.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if `toml_src` is not valid TOML.
pub fn migrate_supervisor_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: skip if already present (either as real section or commented-out block).
    if toml_src.contains("[agent.supervisor]") || toml_src.contains("# [agent.supervisor]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Only inject the comment block when an [agent] section is already present so we don't
    // pollute configs that have no [agent] at all.
    if !doc.contains_key("agent") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n\
         # Background task supervisor tuning (optional — defaults shown, #2883).\n\
         # [agent.supervisor]\n\
         # enrichment_limit = 4\n\
         # telemetry_limit = 8\n\
         # abort_enrichment_on_turn = false\n";

    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["agent.supervisor".to_owned()],
    })
}

/// Add a commented-out `otel_filter` entry under `[telemetry]` if the key is absent (#2997).
///
/// When `[telemetry]` exists but lacks `otel_filter`, appends the key as a comment so users
/// can discover it without manual hunting. Safe to call when the key is already present
/// (real or commented-out).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if `toml_src` is not valid TOML.
pub fn migrate_otel_filter(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Idempotency: skip if key already present (real or commented-out).
    if toml_src.contains("otel_filter") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    // Only inject when [telemetry] section exists; otherwise the field will be added
    // by migrate_telemetry_config which already includes it in the commented block.
    if !doc.contains_key("telemetry") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Base EnvFilter for the OTLP tracing layer. Noisy-crate exclusions \
        (tonic=warn etc.) are always appended (#2997).\n\
        # otel_filter = \"info\"\n";
    let raw = doc.to_string();
    // Insert within [telemetry] so the comment stays adjacent to its section.
    let output = insert_after_section(&raw, "telemetry", comment);

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["telemetry.otel_filter".to_owned()],
    })
}

/// Adds a commented-out `[tools.egress]` section to configs that predate egress logging (#3058).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML source cannot be parsed.
pub fn migrate_egress_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("[tools.egress]") || toml_src.contains("tools.egress") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Egress network logging — records outbound HTTP requests to the audit log\n\
        # with per-hop correlation IDs, response metadata, and block reasons (#3058).\n\
        # [tools.egress]\n\
        # enabled = true           # set to false to disable all egress event recording\n\
        # log_blocked = true       # record scheme/domain/SSRF-blocked requests\n\
        # log_response_bytes = true\n\
        # log_hosts_to_tui = true\n";

    let mut output = toml_src.to_owned();
    output.push_str(comment);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["tools.egress".to_owned()],
    })
}

/// Adds a commented-out `[security.vigil]` section to configs that predate VIGIL (#3058).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML source cannot be parsed.
pub fn migrate_vigil_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("[security.vigil]") || toml_src.contains("security.vigil") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# VIGIL verify-before-commit intent-anchoring gate (#3058).\n\
        # Runs a regex tripwire on every tool output before it enters LLM context.\n\
        # [security.vigil]\n\
        # enabled = true          # master switch; false bypasses VIGIL entirely\n\
        # strict_mode = false     # true: block (replace with sentinel); false: truncate+annotate\n\
        # sanitize_max_chars = 2048\n\
        # extra_patterns = []     # operator-supplied additional injection patterns (max 64)\n\
        # exempt_tools = [\"memory_search\", \"read_overflow\", \"load_skill\", \"schedule_deferred\"]\n";

    let mut output = toml_src.to_owned();
    output.push_str(comment);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["security.vigil".to_owned()],
    })
}

/// Adds a commented-out `[tools.sandbox]` section to configs that predate the
/// OS subprocess sandbox wizard (#3070). Also referenced by #3077.
///
/// Idempotent: if the section (or a dotted-key form under `[tools]`) is already
/// present, OR if the commented-out block was already appended by a prior run,
/// the input is returned unchanged. Uses `toml_edit` parsing to avoid false
/// positives from comments that mention `tools.sandbox`.
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML source cannot be parsed.
pub fn migrate_sandbox_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let doc: DocumentMut = toml_src.parse()?;
    let already_present = doc
        .get("tools")
        .and_then(|t| t.as_table())
        .and_then(|t| t.get("sandbox"))
        .is_some();
    // Secondary guard: commented-out block appended by a prior run of this
    // function is not a real TOML key, so toml_edit would not detect it above.
    if already_present || toml_src.contains("# [tools.sandbox]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# OS-level subprocess sandbox for shell commands (#3070).\n\
        # macOS: sandbox-exec (Seatbelt); Linux: bwrap + Landlock + seccomp (requires `sandbox` feature).\n\
        # Applies ONLY to subprocess executors — in-process tools are unaffected.\n\
        # [tools.sandbox]\n\
        # enabled = false                 # set to true to wrap shell commands\n\
        # profile = \"workspace\"          # \"workspace\" | \"read-only\" | \"network-allow-all\" | \"off\"\n\
        # backend = \"auto\"               # \"auto\" | \"seatbelt\" | \"landlock-bwrap\" | \"noop\"\n\
        # strict = true                   # fail startup if sandbox init fails (fail-closed)\n\
        # allow_read = []                 # additional read-allowed absolute paths\n\
        # allow_write = []                # additional write-allowed absolute paths\n";

    let mut output = toml_src.to_owned();
    output.push_str(comment);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["tools.sandbox".to_owned()],
    })
}

/// Insert `denied_domains` and `fail_if_unavailable` into an existing `[tools.sandbox]`
/// section when those keys are absent (#3294).
///
/// Idempotent: if either key is already present (active or commented), the function is a no-op.
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML document cannot be parsed.
pub fn migrate_sandbox_egress_filter(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Only inject when [tools.sandbox] already exists.
    if !toml_src.contains("[tools.sandbox]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let already_has_denied =
        toml_src.contains("denied_domains") || toml_src.contains("# denied_domains");
    let already_has_fail =
        toml_src.contains("fail_if_unavailable") || toml_src.contains("# fail_if_unavailable");

    if already_has_denied && already_has_fail {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut comment = String::new();
    if !already_has_denied {
        comment.push_str(
            "# denied_domains = []       \
             # hostnames denied egress from sandboxed processes (\"pastebin.com\", \"*.evil.com\")\n",
        );
    }
    if !already_has_fail {
        comment.push_str(
            "# fail_if_unavailable = false  \
             # abort startup when no effective OS sandbox is available\n",
        );
    }

    let output = toml_src.replacen(
        "[tools.sandbox]\n",
        &format!("[tools.sandbox]\n{comment}"),
        1,
    );
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["tools.sandbox.denied_domains".to_owned()],
    })
}

/// Add a commented-out `[scheduler.daemon]` block if the config lacks it (#3332).
///
/// Introduced alongside the `zeph serve` daemon mode (#3332). All `DaemonConfig` fields
/// have defaults so existing configs parse without changes; this migration surfaces the
/// section so users can discover and configure the daemon process.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_scheduler_daemon_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src
        .lines()
        .any(|l| l.trim() == "[scheduler.daemon]" || l.trim() == "# [scheduler.daemon]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [scheduler.daemon] — daemon process config for `zeph serve` (#3332).\n\
         # [scheduler.daemon]\n\
         # pid_file = \"/tmp/zeph-scheduler.pid\"   # PID file path (must be on a local filesystem)\n\
         # log_file = \"/tmp/zeph-scheduler.log\"   # daemon log file path (append-only; rotate externally)\n\
         # tick_secs = 60                           # scheduler tick interval in seconds (clamped 5..=3600)\n\
         # shutdown_grace_secs = 30                 # grace period after SIGTERM before process exits\n\
         # catch_up = true                          # replay missed cron tasks on daemon restart\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["scheduler.daemon".to_owned()],
    })
}

/// Adds a commented-out `[telemetry.trace_metadata]` example to configs that have a
/// `[telemetry]` section but no `trace_metadata` key (#4160).
///
/// # Errors
///
/// Returns [`MigrateError`] if the TOML source cannot be parsed.
pub fn migrate_trace_metadata(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("trace_metadata") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    if !doc.contains_key("telemetry") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Custom key/value pairs attached as OpenTelemetry resource attributes (#4160).\n\
        # Appear on every exported span. Values are plaintext — do not store secrets here.\n\
        # [telemetry.trace_metadata]\n\
        # \"deployment.environment\" = \"production\"\n\
        # \"vcs.revision\" = \"abc1234\"\n";
    let raw = doc.to_string();
    let output = insert_after_section(&raw, "telemetry", comment);

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["telemetry.trace_metadata".to_owned()],
    })
}

/// Add a commented-out `[worktree]` section with defaults if absent (#4679).
///
/// All worktree fields have `#[serde(default)]` so existing configs parse without changes.
/// This step surfaces the new section for users upgrading from older configs.
///
/// Idempotent: the section header (live or commented) suppresses re-injection.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_worktree_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Check both active and commented-out headers to preserve idempotency across runs.
    // `section_header_present` handles active headers (including subtables and inline comments).
    // The second check detects the commented-out block that a previous migration run injected.
    let commented_present = toml_src.lines().any(|l| l.trim() == "# [worktree]");
    if section_header_present(toml_src, "worktree") || commented_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let _doc = toml_src.parse::<toml_edit::DocumentMut>()?;

    let block = "\n# Native worktree isolation for background sub-agents (#4679).\n\
         # [worktree]\n\
         # enabled = false\n\
         # base_ref = \"head\"\n\
         # default_branch = \"main\"\n\
         # root = \".claude/worktrees\"\n\
         # branch_prefix = \"agent/\"\n\
         # prune_branch_on_remove = false\n\
         # cleanup_on_completion = true\n\
         # bg_isolation = \"worktree\"\n";
    let output = format!("{}{}", toml_src.trim_end(), block);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["worktree".to_owned()],
    })
}

/// Add a commented-out `git_timeout_secs` field to `[worktree]` when the section
/// is present but the key is absent (#4704).
///
/// The field defaults to `30` when absent; this step surfaces it for discovery
/// so operators can tune the value for slow networks or large repositories.
/// Only runs when `[worktree]` is present — configs without the section are unchanged.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_worktree_git_timeout(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Anchored multiline pattern: matches `[worktree]` with optional inline comment,
    // followed by LF or CRLF. Does NOT match subtables (`[worktree.foo]`) so the
    // replacement target and the guard stay aligned.
    static WORKTREE_HEADER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?m)^[ \t]*\[worktree\][ \t]*(?:#[^\r\n]*)?\r?\n").expect("static pattern")
    });

    if toml_src.contains("git_timeout_secs") || !WORKTREE_HEADER_RE.is_match(toml_src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# git_timeout_secs = 30  \
        # per-command timeout for git invocations (seconds)\n";
    // Preserve the original header line (including any inline comment) and append after it.
    let output = WORKTREE_HEADER_RE
        .replacen(toml_src, 1, |caps: &regex::Captures| {
            format!("{}{comment}", &caps[0])
        })
        .into_owned();

    let changed = output != toml_src;
    Ok(MigrationResult {
        output,
        changed_count: usize::from(changed),
        sections_changed: if changed {
            vec!["worktree".to_owned()]
        } else {
            Vec::new()
        },
    })
}
