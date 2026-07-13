// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Input, Select};
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

    println!(
        "\n-- Worktree disk quota + auto-reconcile (#5924) --\n\
         Cap concurrent worktrees and/or total disk usage, and optionally run a periodic \
         reconcile sweep that reclaims worktrees git itself reports as abandoned. Leave a \
         field blank to disable that particular cap.\n"
    );

    let max_worktrees_raw: String = Input::new()
        .with_prompt("Maximum concurrent worktrees (blank = unlimited)")
        .allow_empty(true)
        .validate_with(|s: &String| -> Result<(), String> {
            parse_optional_nonzero::<usize>(s).map(|_| ())
        })
        .interact_text()?;
    state.worktree_max_worktrees =
        parse_optional_nonzero(&max_worktrees_raw).map_err(|e| anyhow::anyhow!(e))?;

    let disk_quota_raw: String = Input::new()
        .with_prompt("Disk quota across all worktrees, in MB (blank = no accounting)")
        .allow_empty(true)
        .validate_with(|s: &String| -> Result<(), String> {
            parse_optional_nonzero::<u64>(s).map(|_| ())
        })
        .interact_text()?;
    state.worktree_disk_quota_mb =
        parse_optional_nonzero(&disk_quota_raw).map_err(|e| anyhow::anyhow!(e))?;

    state.worktree_auto_reconcile_secs = Input::new()
        .with_prompt("Periodic reconcile+quota sweep interval, in seconds (0 = disabled, or >= 60)")
        .default(0_u64)
        .validate_with(|v: &u64| -> Result<(), String> {
            if (1..60).contains(v) {
                Err(
                    "must be 0 (disabled) or >= 60 — a short interval runs a full filesystem \
                     walk in a tight loop"
                        .to_owned(),
                )
            } else {
                Ok(())
            }
        })
        .interact_text()?;

    println!();
    Ok(())
}

/// Parses an optional positive integer field: blank input means "unset" (`None`); a `0`
/// is rejected (matches `Config::validate`'s rejection of `Some(0)` for `max_worktrees`/
/// `disk_quota_mb` — see `crates/zeph-config/src/loader.rs`) so the wizard cannot emit a
/// self-contradictory config.
fn parse_optional_nonzero<T>(input: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr + PartialEq + Default,
{
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: T = trimmed
        .parse()
        .map_err(|_| format!("'{trimmed}' is not a valid positive integer"))?;
    if value == T::default() {
        return Err("must be > 0, or blank for unlimited/no accounting".to_owned());
    }
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_optional_nonzero_blank_is_none() {
        assert_eq!(parse_optional_nonzero::<usize>(""), Ok(None));
        assert_eq!(parse_optional_nonzero::<usize>("   "), Ok(None));
    }

    #[test]
    fn parse_optional_nonzero_positive_value_ok() {
        assert_eq!(parse_optional_nonzero::<usize>("5"), Ok(Some(5)));
        assert_eq!(parse_optional_nonzero::<u64>("2048"), Ok(Some(2048)));
    }

    #[test]
    fn parse_optional_nonzero_rejects_zero() {
        assert!(parse_optional_nonzero::<usize>("0").is_err());
        assert!(parse_optional_nonzero::<u64>("0").is_err());
    }

    #[test]
    fn parse_optional_nonzero_rejects_non_numeric() {
        assert!(parse_optional_nonzero::<usize>("abc").is_err());
    }
}
