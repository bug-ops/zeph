// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::Confirm;

use super::WizardState;

/// Interactive wizard step that configures `[tools.ask_user]` and `[llm.cache_keepalive]`.
///
/// Both features are opt-in; the numeric limits keep their defaults and are documented in the
/// generated `config.toml`.
///
/// # Errors
///
/// Returns an error when a `dialoguer` prompt fails (e.g., non-interactive TTY).
pub(super) fn step_interaction(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Interaction ==\n");
    state.ask_user_enabled = Confirm::new()
        .with_prompt(
            "Let the model ask you clarifying questions mid-turn? (adds the ask_user tool)",
        )
        .default(false)
        .interact()?;
    state.cache_keepalive_enabled = Confirm::new()
        .with_prompt(
            "Keep the provider prompt cache warm while idle? (sends periodic pings that cost \
             tokens; toggle later with /keepalive on|off)",
        )
        .default(false)
        .interact()?;
    println!();
    Ok(())
}
