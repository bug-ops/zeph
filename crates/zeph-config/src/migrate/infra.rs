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
    if section_header_present(toml_src, "agent.supervisor")
        || toml_src.contains("# [agent.supervisor]")
    {
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
    if section_header_present(toml_src, "tools.egress") || toml_src.contains("# [tools.egress]") {
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
    if section_header_present(toml_src, "security.vigil") || toml_src.contains("# [security.vigil]")
    {
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
    if !section_header_present(toml_src, "tools.sandbox") {
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
    if section_header_present(toml_src, "scheduler.daemon")
        || toml_src.lines().any(|l| l.trim() == "# [scheduler.daemon]")
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

/// Add commented-out `max_worktrees`, `disk_quota_mb`, `auto_reconcile_secs`, and
/// `reconcile_on_startup` fields to `[worktree]` when the section is present but the
/// keys are absent (#5924).
///
/// Mirrors [`migrate_worktree_git_timeout`]'s idempotency guard and header-anchored
/// insertion. `reconcile_on_startup` defaults to `true` in code even though the
/// migration surfaces it as a comment — the comment documents the value so upgrading
/// operators can discover and override it, without the migration itself silently
/// changing already-loaded behavior (all four fields are `#[serde(default)]`, so an
/// existing config without them already resolves to the code defaults before and
/// after this migration runs). Only runs when `[worktree]` is present — configs
/// without the section are unchanged.
///
/// # Errors
///
/// This function is infallible in practice; the `Result` return type matches the
/// migration function convention for use in chained pipelines.
pub fn migrate_worktree_quota_fields(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    static WORKTREE_HEADER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?m)^[ \t]*\[worktree\][ \t]*(?:#[^\r\n]*)?\r?\n").expect("static pattern")
    });

    if toml_src.contains("auto_reconcile_secs") || !WORKTREE_HEADER_RE.is_match(toml_src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# max_worktrees = 10  # cap on concurrent worktrees under root; unset = unlimited\n\
        # disk_quota_mb = 5120  # soft total-disk-usage threshold (MB) across all worktrees; unset = no accounting\n\
        # auto_reconcile_secs = 3600  # periodic reconcile+quota sweep interval; 0 = disabled\n\
        # reconcile_on_startup = true  # run one reconcile+quota sweep at bootstrap (default: true)\n";
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

/// Add the `[durable]` execution-layer section with all defaults, commented out and default-off.
///
/// Purely additive and idempotent: skips if either an active or a previously-injected commented
/// `[durable]` header is present, so running `--migrate-config` twice does not duplicate it. Existing
/// configs gain the section as comments (no behavior change — `enabled = false`). Part of spec-064
/// (durable execution layer, #4949).
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_durable_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let commented_present = toml_src.lines().any(|l| l.trim() == "# [durable]");
    if section_header_present(toml_src, "durable") || commented_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let _doc = toml_src.parse::<DocumentMut>()?;

    let block = "\n# Native durable execution layer (spec-064, #4949). Opt-in, default-off.\n\
         # [durable]\n\
         # enabled = false\n\
         # backend = \"local\"\n\
         # encrypt_payload = true\n\
         # shared_db = false\n\
         # agent_turns = true\n\
         # orchestration = true\n\
         # scheduler = true\n\
         # subagent = true\n\
         # journal_flush_interval_ms = 10\n\
         # journal_ack_timeout_ms = 5000\n\
         # max_steps_per_execution = 10000\n\
         # max_payload_bytes = 1048576\n\
         # promise_poll_interval_secs = 2\n\
         # max_parked_promises = 1000\n\
         #\n\
         # [durable.retention]\n\
         # ttl_completed_secs = 604800\n\
         # ttl_failed_secs = 2592000\n\
         # max_executions = 10000\n\
         # max_journal_bytes = 1073741824\n\
         # prune_batch_size = 500\n\
         # prune_interval_secs = 3600\n";
    let output = format!("{}{}", toml_src.trim_end(), block);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["durable".to_owned()],
    })
}

/// Whether `durable.encrypt_payload = false` is active while `shared_db` is left unset (#6042).
///
/// `shared_db` is purely operator-declared (INV-8 `encryption_gate` cannot infer it from the
/// filesystem), so this combination silently satisfies the gate's local-only override even when
/// the durable journal database actually lives on a network-shared mount. A commented-out
/// `# shared_db = ...` line does not set the value, so this stays `true` until the operator
/// declares the field explicitly.
pub(crate) fn is_unsafe_shared_topology(doc: &DocumentMut) -> bool {
    let durable = doc.get("durable").and_then(toml_edit::Item::as_table);
    let encrypt_payload_disabled = durable
        .and_then(|t| t.get("encrypt_payload"))
        .and_then(toml_edit::Item::as_bool)
        == Some(false);
    let shared_db_declared = durable.is_some_and(|t| t.contains_key("shared_db"));
    encrypt_payload_disabled && !shared_db_declared
}

/// Warns the operator when [`is_unsafe_shared_topology`] detects the dangerous combination.
///
/// # Errors
///
/// Returns [`MigrateError`] if `toml_src` is not valid TOML.
fn warn_if_unsafe_shared_topology(toml_src: &str) -> Result<(), MigrateError> {
    let doc: DocumentMut = toml_src.parse()?;
    if is_unsafe_shared_topology(&doc) {
        let msg = "durable.encrypt_payload = false with shared_db unset: confirm your \
            deployment topology before proceeding. If the durable journal database (see \
            memory.sqlite_path / the durable db path) is reachable from more than one \
            process or client (e.g. a network-shared mount), set shared_db = true explicitly — \
            leaving it unset silently passes the INV-8 encryption_gate's local-only override \
            even on unsafe shared storage (#6042).";
        tracing::warn!("{msg}");
        eprintln!("WARNING: {msg}");
    }

    Ok(())
}

/// Adds a commented `# shared_db = false` advisory line to an existing active `[durable]` table
/// that lacks the field: the operator-declared flag the INV-8 `encryption_gate` uses to forbid
/// `encrypt_payload = false` on a multi-process/client journal database (#5996).
///
/// Purely additive with a safe default (`false`, matching current unflagged behavior) — no-op
/// when `[durable]` is absent (covered by [`migrate_durable_config`] instead) or `shared_db`
/// (active or commented) is already present.
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_durable_shared_db(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Anchored multiline pattern: matches `[durable]` with optional inline comment, followed by
    // LF or CRLF. Does NOT match `[durable.retention]` so the replacement target stays aligned.
    static DURABLE_HEADER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?m)^[ \t]*\[durable\][ \t]*(?:#[^\r\n]*)?\r?\n").expect("static pattern")
    });

    if !section_header_present(toml_src, "durable") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    warn_if_unsafe_shared_topology(toml_src)?;

    let already_present = toml_src.lines().any(|l| {
        l.trim()
            .trim_start_matches('#')
            .trim()
            .starts_with("shared_db")
    });
    if already_present || !DURABLE_HEADER_RE.is_match(toml_src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# shared_db = false  # journal DB reachable by more than one process/client \
        (INV-8 encryption_gate, #5996)\n";
    let output = DURABLE_HEADER_RE
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
            vec!["durable.shared_db".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Adds a commented-out `[security.content_isolation.nli]` section to configs that predate the
/// SONAR NLI entailment check stage (#5438). Idempotent: no-op when the real or commented
/// `[security.content_isolation.nli]` header is already present, so running `--migrate-config`
/// twice does not duplicate it. Existing configs gain the section as comments (no behavior
/// change — `enabled = false`).
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_nli_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let commented_present = toml_src
        .lines()
        .any(|l| l.trim() == "# [security.content_isolation.nli]");
    if section_header_present(toml_src, "security.content_isolation.nli") || commented_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let _doc = toml_src.parse::<DocumentMut>()?;

    let block = "\n# SONAR NLI entailment check: probabilistic injection detection, observe-only\n\
         # (never blocks). Complements the regex sanitizer above (#5438). Opt-in, default-off.\n\
         # [security.content_isolation.nli]\n\
         # enabled = false\n\
         # provider = \"\"\n\
         # threshold = 0.75\n\
         # timeout_ms = 5000\n\
         # max_content_len = 2048\n";
    let output = format!("{}{}", toml_src.trim_end(), block);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["security.content_isolation.nli".to_owned()],
    })
}

/// Adds a commented-out `[security.content_isolation.secret_masking]` section to configs that
/// predate the PAAC secret placeholder masking registry (#5437). Idempotent: no-op when the
/// real or commented `[security.content_isolation.secret_masking]` header is already present.
/// Existing configs gain the section as comments (no behavior change — `enabled = false`).
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_secret_masking_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let commented_present = toml_src
        .lines()
        .any(|l| l.trim() == "# [security.content_isolation.secret_masking]");
    if section_header_present(toml_src, "security.content_isolation.secret_masking")
        || commented_present
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let _doc = toml_src.parse::<DocumentMut>()?;

    let block = "\n# PAAC secret placeholder masking: substitutes vault-resolved secrets with opaque\n\
         # per-session placeholders before outbound LLM calls (#5437). Opt-in, default-off.\n\
         # [security.content_isolation.secret_masking]\n\
         # enabled = false\n\
         # min_secret_len = 8\n";
    let output = format!("{}{}", toml_src.trim_end(), block);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["security.content_isolation.secret_masking".to_owned()],
    })
}

/// Adds a commented `# filter_names = false` advisory line to an existing active
/// `[security.pii_filter]` table that lacks the field: a capitalized-word-sequence heuristic
/// that compensates for weak NER-model recall on free-text personal names (#5530).
///
/// Opt-in by design (defaults to `false`) — unlike the other `pii_filter` flags, this heuristic
/// also flags common two-word technical/product terms (e.g. `"Docker Compose"`, `"Pull
/// Request"`) as candidate names, so it is surfaced as a commented advisory rather than
/// force-enabled on existing installs (#5530 review S1). No-op when `[security.pii_filter]` is
/// absent, or `filter_names` (active or commented) is already present.
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_pii_filter_names(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    // Anchored multiline pattern: matches `[security.pii_filter]` with optional inline comment,
    // followed by LF or CRLF. Does NOT match subtables so the replacement target stays aligned.
    static PII_FILTER_HEADER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?m)^[ \t]*\[security\.pii_filter\][ \t]*(?:#[^\r\n]*)?\r?\n")
            .expect("static pattern")
    });

    if !section_header_present(toml_src, "security.pii_filter") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let already_present = toml_src.lines().any(|l| {
        let t = l.trim().trim_start_matches('#').trim();
        t.starts_with("filter_names")
    });
    if already_present || !PII_FILTER_HEADER_RE.is_match(toml_src) {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "# filter_names = false  # capitalized-word-sequence name heuristic \
        (opt-in; also flags technical/product terms like \"Docker Compose\", #5530)\n";
    let output = PII_FILTER_HEADER_RE
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
            vec!["security.pii_filter".to_owned()]
        } else {
            Vec::new()
        },
    })
}

/// Adds a commented-out `[security.shadow_sentinel]` section to configs that predate the
/// `ShadowSentinel` Phase 2 defence-in-depth safety probe (spec 050, #5934). Idempotent: no-op
/// when the real or commented `[security.shadow_sentinel]` header is already present, so running
/// `--migrate-config` twice does not duplicate it. Existing configs gain the section as comments
/// (no behavior change — `enabled = false`).
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_shadow_sentinel_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let commented_present = toml_src
        .lines()
        .any(|l| l.trim() == "# [security.shadow_sentinel]");
    if section_header_present(toml_src, "security.shadow_sentinel") || commented_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let _doc = toml_src.parse::<DocumentMut>()?;

    let block = "\n# ShadowSentinel Phase 2: persistent safety event stream + LLM pre-execution probe\n\
         # (spec 050, #5934). Defence-in-depth only — PolicyGateExecutor and TrajectorySentinel\n\
         # remain the primary enforcement mechanisms. Opt-in, default-off.\n\
         # [security.shadow_sentinel]\n\
         # enabled = false\n\
         # probe_provider = \"\"\n\
         # max_context_events = 50\n\
         # probe_timeout_ms = 2000\n\
         # max_probes_per_turn = 3\n\
         # probe_patterns = [\"builtin:shell\", \"builtin:write\", \"builtin:edit\", \"*write*\", \"*edit*\", \"*delete*\", \"*exec*\"]\n\
         # deny_on_timeout = false\n";
    let output = format!("{}{}", toml_src.trim_end(), block);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["security.shadow_sentinel".to_owned()],
    })
}

/// Adds a commented-out `card_trust_policy`/`trusted_agent_keys` advisory block for
/// `[a2a_client]` to configs that predate A2A Agent Card signature verification (#5928).
/// Idempotent: no-op when the advisory marker is already present. Existing configs gain
/// the block as comments only (no behavior change — `card_trust_policy` already defaults
/// to `"ignore"` via `#[serde(default)]` when absent).
///
/// # Errors
///
/// Returns [`MigrateError`] if the source is not valid TOML.
pub fn migrate_a2a_card_trust_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    let commented_present = toml_src.contains("# card_trust_policy");
    let doc = toml_src.parse::<DocumentMut>()?;
    let active_present = doc
        .get("a2a_client")
        .and_then(|t| t.get("card_trust_policy"))
        .is_some();
    if commented_present || active_present {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let block = "\n# A2A Agent Card signature + URL-origin trust policy (A2A 1.0.0 §8.4, #5928).\n\
         # Requires the `card-signing` feature (see the `a2a` feature in the root Cargo.toml)\n\
         # for `\"require\"` to be accepted at config load — see zeph-a2a::card_signing module\n\
         # docs for the ES256-only, out-of-band-key-store trust model.\n\
         # [a2a_client]\n\
         # card_trust_policy = \"ignore\"  # \"ignore\" | \"prefer\" | \"require\"\n\
         # [[a2a_client.trusted_agent_keys]]\n\
         # kid = \"key-1\"\n\
         # alg = \"ES256\"\n\
         # jwk_or_pem = \"\"\n";
    let output = format!("{}{}", toml_src.trim_end(), block);
    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["a2a_client".to_owned()],
    })
}
