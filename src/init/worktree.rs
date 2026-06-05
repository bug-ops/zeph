// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Select};
use zeph_config::{BgIsolation, WorktreeBaseRef};

use super::WizardState;

/// Interactive wizard step that configures `[worktree]` in the generated config.
///
/// When the user opts in, prompts for `base_ref` and `bg_isolation`. When the
/// user opts out, the fields remain at their defaults (`enabled = false`) and
/// the step returns immediately.
///
/// # Errors
///
/// Returns an error when a `dialoguer` prompt fails (e.g., non-interactive TTY).
pub(super) fn step_worktree(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Worktree Isolation ==\n");
    println!(
        "Enables per-subagent git worktree isolation. Each subagent that opts in \
         receives a dedicated branch, preventing concurrent agents from conflicting \
         with the working copy.\n"
    );

    state.worktree_enabled = Confirm::new()
        .with_prompt("Enable git worktree isolation for sub-agents?")
        .default(false)
        .interact()?;

    if !state.worktree_enabled {
        println!();
        return Ok(());
    }

    let base_ref_items = &[
        "head (branch from local HEAD — no network required)",
        "fresh (fetch origin/<default_branch> first — starts from latest remote state)",
    ];
    let base_ref_idx = Select::new()
        .with_prompt("Base ref for new worktree branches")
        .items(base_ref_items)
        .default(0)
        .interact()?;
    state.worktree_base_ref = match base_ref_idx {
        1 => {
            println!(
                "  Note: adjust worktree.default_branch in config.toml if your default branch is not 'main'."
            );
            WorktreeBaseRef::Fresh
        }
        _ => WorktreeBaseRef::Head,
    };

    let bg_isolation_items = &[
        "worktree (background agents get dedicated git worktrees — recommended)",
        "none (background agents edit the working copy directly)",
    ];
    let bg_isolation_idx = Select::new()
        .with_prompt("Background agent isolation mode")
        .items(bg_isolation_items)
        .default(0)
        .interact()?;
    state.worktree_bg_isolation = match bg_isolation_idx {
        1 => BgIsolation::None,
        _ => BgIsolation::Worktree,
    };

    println!();
    Ok(())
}
