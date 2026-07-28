// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Input, Select};

use super::WizardState;

/// Returns `(can_enable, platform_description)` for the current build.
///
/// When `can_enable` is false the wizard must refuse to set `sandbox_enabled = true`.
pub(crate) fn sandbox_platform_support() -> (bool, &'static str) {
    #[cfg(target_os = "macos")]
    {
        (true, "macOS (sandbox-exec / Seatbelt)")
    }
    #[cfg(all(target_os = "linux", feature = "sandbox"))]
    {
        (true, "Linux (bwrap + Landlock + seccomp)")
    }
    #[cfg(all(target_os = "linux", not(feature = "sandbox")))]
    {
        (
            false,
            "Linux build without the `sandbox` cargo feature — rebuild with --features sandbox",
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        (false, "unsupported OS (only macOS and Linux are supported)")
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn step_sandbox(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== OS Subprocess Sandbox ==\n");
    println!(
        "Wraps shell commands in an OS-level sandbox (macOS Seatbelt or Linux bwrap+Landlock).\n\
         Applies ONLY to subprocess executors (shell). In-process tools are unaffected.\n"
    );

    let (supported, platform_desc) = sandbox_platform_support();
    println!("Platform: {platform_desc}");
    if !supported {
        println!(
            "\nSandbox is not available in this build. Leaving enabled=false.\n\
             Re-run with a supported build to turn the sandbox on.\n"
        );
        state.sandbox_enabled = false;
        println!();
        return Ok(());
    }

    state.sandbox_enabled = Confirm::new()
        .with_prompt(
            "Enable OS subprocess sandbox? (recommended when running untrusted shell output)",
        )
        .default(false)
        .interact()?;

    if !state.sandbox_enabled {
        println!();
        return Ok(());
    }

    // "off" is intentionally NOT offered — enabled=true + profile=off produces an
    // "OS sandbox enabled" log line with zero enforcement.
    // Users who want to disable the sandbox answer "No" to the prior prompt.
    let profiles = &[
        "workspace (read/write to configured paths, no network)",
        "read-only (read configured paths, no writes, no network)",
        "network-allow-all (workspace + unrestricted network)",
    ];
    let idx = Select::new()
        .with_prompt("Sandbox profile")
        .items(profiles)
        .default(0)
        .interact()?;
    state.sandbox_profile = match idx {
        1 => "read-only".into(),
        2 => "network-allow-all".into(),
        _ => "workspace".into(),
    };

    #[cfg(target_os = "macos")]
    let (backends, backend_values): (&[&str], &[&str]) = (
        &[
            "auto (recommended)",
            "seatbelt (macOS sandbox-exec)",
            "noop (no enforcement; test-only)",
        ],
        &["auto", "seatbelt", "noop"],
    );
    #[cfg(target_os = "linux")]
    let (backends, backend_values): (&[&str], &[&str]) = (
        &[
            "auto (recommended)",
            "landlock-bwrap (Linux)",
            "noop (no enforcement; test-only)",
        ],
        &["auto", "landlock-bwrap", "noop"],
    );
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let (backends, backend_values): (&[&str], &[&str]) = (&["auto", "noop"], &["auto", "noop"]);

    let bidx = Select::new()
        .with_prompt("Backend")
        .items(backends)
        .default(0)
        .interact()?;
    state.sandbox_backend = match backend_values[bidx] {
        "seatbelt" => zeph_config::SandboxBackend::Seatbelt,
        "landlock-bwrap" => zeph_config::SandboxBackend::LandlockBwrap,
        "noop" => zeph_config::SandboxBackend::Noop,
        _ => zeph_config::SandboxBackend::Auto,
    };

    state.sandbox_strict = Confirm::new()
        .with_prompt(
            "Strict mode? (true: fail startup when sandbox init fails; false: warn and run without isolation)",
        )
        .default(true)
        .interact()?;

    state.sandbox_allow_read = prompt_abs_paths("Additional read-allowed paths")?;
    state.sandbox_allow_write = prompt_abs_paths("Additional write-allowed paths")?;

    // Sandbox egress filter (#3294).
    println!("\n-- Sandbox egress filter --");
    println!(
        "Deny network egress to specific hostnames from sandboxed shell commands.\n\
         Patterns: exact hostname (\"pastebin.com\") or single-level wildcard (\"*.evil.com\").\n\
         Enforcement is per-backend: macOS uses Seatbelt rules; Linux overlays /etc/hosts.\n"
    );
    let raw_denied: String = Input::new()
        .with_prompt("Denied domains (comma-separated, empty = none)")
        .allow_empty(true)
        .interact_text()?;
    state.sandbox_denied_domains = raw_denied
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    if !state.sandbox_denied_domains.is_empty() {
        state.sandbox_fail_if_unavailable = Confirm::new()
            .with_prompt(
                "Abort startup when no effective OS sandbox is available? \
                 (Recommended when denied_domains must be enforced)",
            )
            .default(false)
            .interact()?;
    }

    println!();
    Ok(())
}

/// Prompt for a comma-separated list of absolute paths. `~/…` is expanded via
/// `dirs::home_dir`. Non-absolute entries trigger a reprompt; non-existent
/// paths warn but are accepted (sandbox canonicalisation drops them at load).
fn prompt_abs_paths(label: &str) -> anyhow::Result<Vec<String>> {
    loop {
        let raw: String = Input::new()
            .with_prompt(format!(
                "{label} (comma-separated absolute paths, empty = none)"
            ))
            .allow_empty(true)
            .interact_text()?;
        let expanded: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                if let Some(rest) = s.strip_prefix("~/") {
                    dirs::home_dir()
                        .map_or_else(|| s.to_owned(), |h| h.join(rest).display().to_string())
                } else {
                    s.to_owned()
                }
            })
            .collect();
        let bad: Vec<&String> = expanded
            .iter()
            .filter(|p| !std::path::Path::new(p).is_absolute())
            .collect();
        if !bad.is_empty() {
            eprintln!("  not absolute: {bad:?} — please retry");
            continue;
        }
        for p in &expanded {
            if !std::path::Path::new(p).exists() {
                eprintln!("  warning: {p} does not exist (will be dropped at startup)");
            }
        }
        return Ok(expanded);
    }
}

/// Prompt for `[tools.shell] risk_chain_window_turns` (#6603): the number of turns a recorded
/// tool call stays "live" for `RiskChainAccumulator` cross-turn multi-step chain detection.
///
/// Falls back to [`zeph_tools::risk_chain::DEFAULT_CROSS_TURN_WINDOW_TURNS`] on unparseable
/// input, matching the pre-filled default shown in the prompt.
fn prompt_risk_chain_window_turns() -> anyhow::Result<u64> {
    let default_window = zeph_tools::risk_chain::DEFAULT_CROSS_TURN_WINDOW_TURNS;
    let window_str: String = Input::new()
        .with_prompt(
            "Multi-step attack-chain detection window, in turns? (how long a sensitive read \
             stays \"live\" before a later network egress call is still blocked as a chain; \
             narrower than [security.trajectory] window_turns because this feeds a hard block)",
        )
        .default(default_window.to_string())
        .interact_text()?;
    Ok(window_str.trim().parse::<u64>().unwrap_or(default_window))
}

/// Parse a wizard-entered USD amount into cents, clamped to `[0, u32::MAX]`.
///
/// Non-numeric input falls back to the wizard's suggested default (2500 cents = $25.00),
/// matching the pre-filled prompt value rather than silently accepting a typo as `0`.
/// Negative values clamp to `0` (unlimited), consistent with `max_daily_cents`'s meaning.
fn parse_daily_cap_dollars(s: &str) -> u32 {
    s.trim().parse::<f64>().map_or(2500, |dollars| {
        let cents = (dollars.max(0.0) * 100.0).round();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            cents.min(f64::from(u32::MAX)) as u32
        }
    })
}

pub(super) fn step_security(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Security ==\n");
    println!(
        "Memory write validation is enabled by default (size limits, forbidden patterns, entity PII scan).\n"
    );
    state.pii_filter_enabled = Confirm::new()
        .with_prompt(
            "Enable PII filter? (scrubs emails, phone numbers, SSNs, and credit card numbers from tool outputs before LLM context and debug dumps; recommended)",
        )
        .default(true)
        .interact()?;
    state.rate_limit_enabled = Confirm::new()
        .with_prompt(
            "Enable tool rate limiter? (sliding-window per-category limits: shell 30/min, web 20/min, memory 60/min; recommended)",
        )
        .default(true)
        .interact()?;

    let daily_cap_dollars: String = Input::new()
        .with_prompt(
            "Daily LLM cost cap in USD (guards against a runaway loop burning API spend; 0 = unlimited)",
        )
        .default("25.00".to_owned())
        .interact_text()?;
    state.max_daily_cents = parse_daily_cap_dollars(&daily_cap_dollars);
    state.skill_scan_on_load = Confirm::new()
        .with_prompt(
            "Scan skill content for injection patterns on load? (advisory — logs warnings, does not block; recommended)",
        )
        .default(true)
        .interact()?;
    state.skill_require_integrity_check_on_promote = Confirm::new()
        .with_prompt(
            "Automatically require a per-invocation integrity re-check when promoting a skill to trusted/verified? (re-hashes SKILL.md before every dispatch; override per-command with --no-require-check; recommended)",
        )
        .default(true)
        .interact()?;
    state.skill_capability_escalation_check = Confirm::new()
        .with_prompt(
            "Check skill capability escalation on load? (warns if skills declare tools exceeding their trust level)",
        )
        .default(false)
        .interact()?;
    state.pre_execution_verify_enabled = Confirm::new()
        .with_prompt(
            "Enable pre-execution verification? (blocks destructive commands like rm -rf / and injection patterns before tool execution; recommended)",
        )
        .default(true)
        .interact()?;
    if state.pre_execution_verify_enabled {
        println!("  Shell tools checked: bash, shell, terminal (configurable in config.toml)");
        let paths_input: String = Input::new()
            .with_prompt(
                "Allowed paths for destructive commands (comma-separated, empty = deny all)",
            )
            .allow_empty(true)
            .interact_text()?;
        state.pre_execution_verify_allowed_paths = paths_input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    state.shell_transactional = Confirm::new()
        .with_prompt(
            "Enable transactional shell? (snapshots files before write commands and rolls back on failure; see transaction_scope, auto_rollback_exit_codes in config.toml)",
        )
        .default(false)
        .interact()?;
    if state.shell_transactional {
        state.shell_auto_rollback = Confirm::new()
            .with_prompt(
                "Auto-rollback on shell failure? (restores files when exit code >= 2; set auto_rollback_exit_codes in config.toml for exact codes)",
            )
            .default(false)
            .interact()?;
    }
    state.shell_checkpoints_enabled = Confirm::new()
        .with_prompt(
            "Enable /undo and /redo checkpoints? (captures file state before write commands for session-scoped undo/redo; requires [tools.shell] checkpoints_enabled = true)",
        )
        .default(false)
        .interact()?;
    if state.shell_checkpoints_enabled {
        let max_str: String = dialoguer::Input::new()
            .with_prompt("Maximum number of undo checkpoints (0 = unlimited)")
            .default("20".to_owned())
            .interact_text()?;
        state.shell_max_checkpoints = max_str.trim().parse::<usize>().unwrap_or(20);
    }
    state.risk_chain_window_turns = prompt_risk_chain_window_turns()?;

    let deny_raw: String = dialoguer::Input::new()
        .with_prompt(
            "File read deny patterns (comma-separated globs, e.g. /etc/shadow,/root/*, empty = no restrictions)",
        )
        .allow_empty(true)
        .interact_text()?;
    state.file_deny_read = deny_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if !state.file_deny_read.is_empty() {
        let allow_raw: String = dialoguer::Input::new()
            .with_prompt("File read allow overrides (comma-separated globs, empty = none)")
            .allow_empty(true)
            .interact_text()?;
        state.file_allow_read = allow_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    {
        state.guardrail_enabled = Confirm::new()
            .with_prompt(
                "Enable LLM-based guardrail? (prompt injection pre-screening via a dedicated safety model, e.g. llama-guard)",
            )
            .default(false)
            .interact()?;

        if state.guardrail_enabled {
            let provider_options = &["ollama", "claude", "openai", "compatible"];
            let provider_idx = Select::new()
                .with_prompt("Guardrail provider")
                .items(provider_options)
                .default(0)
                .interact()?;
            provider_options[provider_idx].clone_into(&mut state.guardrail_provider);

            state.guardrail_model = dialoguer::Input::new()
                .with_prompt("Guardrail model")
                .default(if state.guardrail_provider == "ollama" {
                    "llama-guard-3:1b".to_owned()
                } else {
                    String::new()
                })
                .allow_empty(true)
                .interact_text()?;

            let action_options = &["block", "warn"];
            let action_idx = Select::new()
                .with_prompt("Action on flagged input")
                .items(action_options)
                .default(0)
                .interact()?;
            action_options[action_idx].clone_into(&mut state.guardrail_action);

            let timeout_str: String = dialoguer::Input::new()
                .with_prompt("Guardrail timeout (ms)")
                .default("500".to_owned())
                .interact_text()?;
            state.guardrail_timeout_ms = timeout_str.parse().unwrap_or(500);
        }
    }

    {
        state.nli_enabled = Confirm::new()
            .with_prompt(
                "Enable SONAR NLI entailment check? (probabilistic injection detection via LLM entailment scoring on tool output; observe-only, never blocks — complements the regex sanitizer)",
            )
            .default(false)
            .interact()?;

        if state.nli_enabled {
            let provider_options = &[
                "(primary provider)",
                "ollama",
                "claude",
                "openai",
                "compatible",
            ];
            let provider_idx = Select::new()
                .with_prompt("NLI provider (prefer a fast/cheap model — runs on every tool output)")
                .items(provider_options)
                .default(0)
                .interact()?;
            state.nli_provider = if provider_idx == 0 {
                String::new()
            } else {
                provider_options[provider_idx].to_owned()
            };
        }
    }

    {
        state.secret_masking_enabled = Confirm::new()
            .with_prompt(
                "Enable secret placeholder masking? (substitutes vault-resolved secrets — API keys, tokens, DB URLs — with opaque per-session placeholders before outbound LLM calls; resolved back only at tool execution; recommended)",
            )
            .default(true)
            .interact()?;
    }

    #[cfg(feature = "classifiers")]
    {
        state.classifiers_enabled = Confirm::new()
            .with_prompt(
                "Enable ML classifiers? (injection detection and PII detection via candle inference)",
            )
            .default(false)
            .interact()?;

        if state.classifiers_enabled {
            state.pii_enabled = Confirm::new()
                .with_prompt("Enable PII detection? (NER-based scan of assistant responses)")
                .default(false)
                .interact()?;
        }
    }

    state.egress_logging_enabled = Confirm::new()
        .with_prompt(
            "Enable egress network logging? (records outbound HTTP requests to audit log with correlation IDs; default: on)",
        )
        .default(true)
        .interact()?;

    state.vigil_enabled = Confirm::new()
        .with_prompt(
            "Enable VIGIL intent-anchoring gate? (regex tripwire that checks tool outputs for injection patterns before LLM context; recommended)",
        )
        .default(true)
        .interact()?;

    if state.vigil_enabled {
        state.vigil_strict_mode = Confirm::new()
            .with_prompt(
                "VIGIL strict mode? (true: block and replace with sentinel; false: truncate and annotate, then continue to ContentSanitizer)",
            )
            .default(false)
            .interact()?;
    }

    println!();
    Ok(())
}

/// Wizard step for spec 050 trajectory sentinel thresholds.
pub(super) fn step_trajectory(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Trajectory Risk Sentinel (spec 050) ==\n");
    println!(
        "Accumulates cross-turn risk signals and downgrades tool calls to Deny when the\n\
         score reaches the Critical threshold.\n"
    );

    let critical_raw: String = Input::new()
        .with_prompt("Critical threshold (score at which all Allow decisions are denied)")
        .default("10.0".to_owned())
        .interact_text()?;
    state.trajectory_critical_at = critical_raw.parse().unwrap_or(10.0);

    let recover_raw: String = Input::new()
        .with_prompt("Auto-recover after N consecutive Critical turns (minimum 4)")
        .default("16".to_owned())
        .interact_text()?;
    state.trajectory_auto_recover = recover_raw.parse().unwrap_or(16).max(4);

    println!();
    Ok(())
}

pub(super) fn step_policy(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Policy Enforcer ==\n");
    println!(
        "Declarative tool call authorization via TOML rules (requires policy-enforcer feature).\n"
    );

    state.policy_enforcer_enabled = Confirm::new()
        .with_prompt("Enable policy enforcer?")
        .default(false)
        .interact()?;

    if state.policy_enforcer_enabled {
        let provider: String = Input::new()
            .with_prompt(
                "Provider name for LLM-assisted policy checks \
                 (leave blank to use rules-only mode)",
            )
            .default(String::new())
            .allow_empty(true)
            .interact_text()?;
        state.policy_provider = provider;
    }

    let window: String = Input::new()
        .with_prompt(
            "Consecutive low-utility tool calls before hard-stopping the loop \
             (0 = disabled, tools.utility_window)",
        )
        .default("0".to_owned())
        .interact_text()?;
    state.utility_window = window.parse::<usize>().unwrap_or(0);

    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn sandbox_platform_support_macos_returns_true() {
        let (can_enable, desc) = sandbox_platform_support();
        assert!(can_enable);
        assert!(desc.contains("macOS"));
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn sandbox_platform_support_unsupported_returns_false() {
        let (can_enable, _) = sandbox_platform_support();
        assert!(!can_enable);
    }

    // --- parse_daily_cap_dollars (issue #6469) ---

    #[test]
    fn parse_daily_cap_dollars_zero_means_unlimited() {
        assert_eq!(parse_daily_cap_dollars("0"), 0);
    }

    #[test]
    fn parse_daily_cap_dollars_default_value() {
        assert_eq!(parse_daily_cap_dollars("25.00"), 2500);
    }

    #[test]
    fn parse_daily_cap_dollars_fractional_rounds_to_nearest_cent() {
        assert_eq!(parse_daily_cap_dollars("12.344"), 1234);
        assert_eq!(parse_daily_cap_dollars("12.346"), 1235);
    }

    #[test]
    fn parse_daily_cap_dollars_non_numeric_falls_back_to_default() {
        assert_eq!(parse_daily_cap_dollars("not a number"), 2500);
        assert_eq!(parse_daily_cap_dollars(""), 2500);
    }

    #[test]
    fn parse_daily_cap_dollars_negative_clamps_to_zero() {
        assert_eq!(parse_daily_cap_dollars("-5"), 0);
    }

    #[test]
    fn parse_daily_cap_dollars_trims_whitespace() {
        assert_eq!(parse_daily_cap_dollars("  25.00  "), 2500);
    }
}
