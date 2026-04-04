// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Input, Select};

use super::WizardState;

pub(super) fn step_security(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Security ==\n");
    println!(
        "Memory write validation is enabled by default (size limits, forbidden patterns, entity PII scan).\n"
    );
    state.pii_filter_enabled = Confirm::new()
        .with_prompt(
            "Enable PII filter? (scrubs emails, phone numbers, SSNs, and credit card numbers from tool outputs before LLM context and debug dumps)",
        )
        .default(false)
        .interact()?;
    state.rate_limit_enabled = Confirm::new()
        .with_prompt(
            "Enable tool rate limiter? (sliding-window per-category limits: shell 30/min, web 20/min, memory 60/min)",
        )
        .default(false)
        .interact()?;
    state.skill_scan_on_load = Confirm::new()
        .with_prompt(
            "Scan skill content for injection patterns on load? (advisory — logs warnings, does not block; recommended)",
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

    println!();
    Ok(())
}
