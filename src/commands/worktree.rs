// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use zeph_worktree::{DefaultGitRunner, DefaultWorktreeManager, probe_capabilities};

use crate::bootstrap::{find_repo_root, resolve_config_path};
use crate::cli::WorktreeCommand;

/// Dispatch a `zeph worktree` subcommand.
///
/// # Errors
///
/// Returns an error if config cannot be loaded, the git repo cannot be found,
/// git capabilities probe fails, or the worktree manager initialisation fails.
pub(crate) async fn handle_worktree_command(
    cmd: WorktreeCommand,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).map_err(|e| {
        anyhow::anyhow!("failed to load config from {}: {e}", config_file.display())
    })?;

    if !config.worktree.enabled {
        anyhow::bail!(
            "worktree subsystem is disabled. Set `worktree.enabled = true` in your config."
        );
    }

    let repo_root = find_repo_root().ok_or_else(|| {
        anyhow::anyhow!("Not inside a git repository. Worktree commands require a git repo.")
    })?;

    let runner = DefaultGitRunner::with_timeout(std::time::Duration::from_secs(
        config.worktree.git_timeout_secs,
    ));
    probe_capabilities(&runner, &repo_root).await?;
    let wm = DefaultWorktreeManager::new(repo_root, config.worktree.clone(), runner).await?;

    match cmd {
        WorktreeCommand::List => {
            let stale = wm.reconcile().await?;
            let active = wm.list();
            if active.is_empty() && stale.is_empty() {
                println!("No active worktrees.");
            } else {
                if !active.is_empty() {
                    println!("{:<36}  PATH", "AGENT ID");
                    for handle in &active {
                        println!("{:<36}  {}", handle.subagent_id, handle.path.display());
                    }
                }
                if !stale.is_empty() {
                    println!("\nStale (on disk but not tracked):");
                    for stale_wt in &stale {
                        match &stale_wt.prunable_reason {
                            Some(reason) => {
                                println!(
                                    "  {}  [prunable: {reason}]",
                                    stale_wt.handle.path.display()
                                );
                            }
                            None => println!(
                                "  {}  [in use — not marked prunable by git; may belong to another session]",
                                stale_wt.handle.path.display()
                            ),
                        }
                    }
                }
            }
        }
        WorktreeCommand::Clean { force } => {
            let stale = wm.reconcile().await?;
            let mut removed = 0usize;
            let mut skipped = 0usize;
            for stale_wt in stale {
                if !force && !stale_wt.is_safe_to_force_remove() {
                    eprintln!(
                        "warning: skipping {} — directory exists and git does not report it as \
                         prunable; it may be in active use by another zeph session. \
                         Re-run with `zeph worktree clean --force` if you are certain it is abandoned.",
                        stale_wt.handle.path.display()
                    );
                    skipped += 1;
                    continue;
                }
                if let Err(e) = wm
                    .remove(&stale_wt.handle, config.worktree.prune_branch_on_remove)
                    .await
                {
                    eprintln!(
                        "warning: failed to remove {}: {e}",
                        stale_wt.handle.path.display()
                    );
                } else {
                    removed += 1;
                }
            }
            // FR-CLEANUP-04: clear any remaining stale administrative entries
            // (e.g. worktrees deleted outside Zeph) from the git registry.
            if let Err(e) = wm.prune().await {
                eprintln!("warning: failed to prune worktree registry: {e}");
            }
            println!("Removed {removed} stale worktree(s), skipped {skipped} in-use candidate(s).");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use clap::Parser as _;

    use crate::cli::{Cli, Command, WorktreeCommand};

    /// Regression test for #4701: `handle_worktree_command` must propagate a config parse
    /// error instead of silently falling back to `Config::default()`.
    #[tokio::test]
    async fn invalid_config_returns_error_not_default() {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(b"[[[[invalid toml}}}").expect("write");
        let path = f.path().to_owned();

        let result =
            super::handle_worktree_command(crate::cli::WorktreeCommand::List, Some(&path)).await;

        assert!(
            result.is_err(),
            "expected an error for invalid config, got Ok"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("failed to load config"),
            "error must mention config load failure, got: {msg}"
        );
    }

    /// Regression test for #6055: `zeph worktree clean --force` must parse
    /// with `force == true`; plain `zeph worktree clean` must default to
    /// `force == false` so `clean` never force-removes an in-use worktree
    /// without an explicit operator opt-in.
    #[test]
    fn worktree_clean_force_flag_parses() {
        let cli = Cli::try_parse_from(["zeph", "worktree", "clean", "--force"]).unwrap();
        let Some(Command::Worktree {
            command: WorktreeCommand::Clean { force },
        }) = cli.command
        else {
            panic!("expected Worktree(Clean) command");
        };
        assert!(force, "--force must set force = true");
    }

    #[test]
    fn worktree_clean_defaults_to_no_force() {
        let cli = Cli::try_parse_from(["zeph", "worktree", "clean"]).unwrap();
        let Some(Command::Worktree {
            command: WorktreeCommand::Clean { force },
        }) = cli.command
        else {
            panic!("expected Worktree(Clean) command");
        };
        assert!(
            !force,
            "clean without --force must default to force = false"
        );
    }
}
