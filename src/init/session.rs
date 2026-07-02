// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wizard steps for durable session persistence and `zeph serve-sessions` (spec-068, #5343, P4).
//!
//! Two independent steps: [`step_session`] configures `[session]` (the durable JSONL event log
//! every channel dual-writes to), [`step_serve`] configures `[serve]` (`zeph serve-sessions`'s
//! HTTP/SSE API defaults) — the latter's settings apply whether or not the user ever actually
//! runs `zeph serve-sessions`, so it is gated behind its own "customize now?" confirmation rather
//! than an "enable" toggle (there is no `[serve]` `enabled` field; the command is opt-in by
//! virtue of being a separate CLI subcommand).

use dialoguer::{Confirm, Input};

use super::WizardState;

/// Collect `[session]` durable persistence settings (spec-068 §4).
///
/// # Errors
///
/// Returns an error if a prompt cannot be read.
pub(super) fn step_session(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Session Persistence ==\n");
    println!("Maintains a durable, replayable JSONL event log per conversation-session, enabling");
    println!("crash-safe resume and `/conv resume`/`/conv fork` (spec-068).\n");

    state.session_persistence_enabled = Confirm::new()
        .with_prompt("Enable durable session persistence?")
        .default(true)
        .interact()?;

    if state.session_persistence_enabled {
        state.session_data_dir = Input::new()
            .with_prompt("Event log directory")
            .default(state.session_data_dir.clone())
            .interact_text()?;
    }

    println!();
    Ok(())
}

/// Collect `[serve]` settings for `zeph serve-sessions` (spec-068 §9).
///
/// # Errors
///
/// Returns an error if a prompt cannot be read.
pub(super) fn step_serve(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== zeph serve-sessions (HTTP/SSE Session API) ==\n");
    println!("Exposes durable sessions over HTTP/SSE. These settings apply whenever you run");
    println!("`zeph serve-sessions`, independent of whether you use it now.\n");

    let customize = Confirm::new()
        .with_prompt("Customize `zeph serve-sessions` settings now? (sensible defaults otherwise)")
        .default(false)
        .interact()?;
    if !customize {
        println!();
        return Ok(());
    }

    state.serve_http_addr = Input::new()
        .with_prompt("Bind address")
        .default(state.serve_http_addr.clone())
        .interact_text()?;

    state.serve_require_auth = Confirm::new()
        .with_prompt("Require a bearer token on /sessions* endpoints? (/health is always open)")
        .default(state.serve_require_auth)
        .interact()?;

    if state.serve_require_auth {
        state.serve_auth_token_vault_key = Input::new()
            .with_prompt("Vault key name to resolve the bearer token from")
            .default(state.serve_auth_token_vault_key.clone())
            .interact_text()?;
        println!(
            "  (set the actual token with: zeph vault set {} <token>)",
            state.serve_auth_token_vault_key
        );
    }

    state.serve_max_sessions = Input::new()
        .with_prompt("Maximum concurrent live sessions")
        .default(state.serve_max_sessions)
        .interact_text()?;

    state.serve_session_idle_ttl_secs = Input::new()
        .with_prompt("Idle session eviction TTL (seconds)")
        .default(state.serve_session_idle_ttl_secs)
        .interact_text()?;

    println!();
    Ok(())
}
