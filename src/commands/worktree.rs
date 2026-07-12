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
            let outcome = wm
                .clean(
                    force,
                    config.worktree.prune_branch_on_remove,
                    "`zeph worktree clean --force`",
                )
                .await?;
            for warning in &outcome.warnings {
                eprintln!("{warning}");
            }
            println!("{}", zeph_worktree::format_clean_summary(&outcome));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use clap::Parser as _;
    use serial_test::serial;

    use crate::cli::{Cli, Command, WorktreeCommand};

    fn git(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git must be on PATH for this test")
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        assert!(git(&["init", "-q"], path).status.success());
        git(&["config", "user.email", "test@example.com"], path);
        git(&["config", "user.name", "Test"], path);
        std::fs::write(path.join("README.md"), "test\n").expect("write README");
        git(&["add", "."], path);
        assert!(git(&["commit", "-q", "-m", "init"], path).status.success());
        dir
    }

    /// Writes a full, valid config (not just a `[worktree]` snippet — most
    /// `Config` fields have no section-level default) with worktrees enabled,
    /// rooted at `worktrees` relative to the repo, and returns its path.
    fn write_worktree_config(dir: &std::path::Path) -> std::path::PathBuf {
        let mut cfg = zeph_core::config::Config::default();
        cfg.worktree.enabled = true;
        cfg.worktree.root = "worktrees".to_string();
        cfg.worktree.branch_prefix = "agent/".to_string();
        let toml_str = toml::to_string_pretty(&cfg).expect("serialize config");
        let config_path = dir.join("zeph-test-config.toml");
        std::fs::write(&config_path, toml_str).expect("write config");
        config_path
    }

    /// End-to-end regression test for #6077 (tester gaps B + C): calls the
    /// real production handler `handle_worktree_command(Clean)` directly
    /// against a real git repo — rather than reimplementing its skip-gating
    /// logic, as the previous integration test in
    /// `crates/zeph-worktree/tests/real_git_integration.rs` did — over a
    /// mixed stale list containing both prunable entries (directories deleted
    /// externally, simulating a crash) and non-prunable ones (directories
    /// left intact, simulating another concurrently running zeph session),
    /// with N>1 removed and M>1 skipped in the same run.
    ///
    /// `#[serial]` because this mutates the process-global current directory
    /// (`find_repo_root` resolves the repo from cwd); nextest — the sanctioned
    /// runner for this project — gives every test its own process, but plain
    /// `cargo test` would otherwise race this against other tests in this
    /// binary that also touch cwd (see `src/acp.rs`, `src/url_scheme/register.rs`).
    #[tokio::test]
    #[serial]
    async fn clean_handles_mixed_prunable_and_in_use_stale_list() {
        let repo = init_repo();
        let repo_root = repo.path().canonicalize().expect("canonicalize repo root");
        let config_path = write_worktree_config(repo.path());

        let creator = zeph_worktree::DefaultWorktreeManager::new(
            repo_root.clone(),
            zeph_config::WorktreeConfig {
                enabled: true,
                root: "worktrees".to_string(),
                branch_prefix: "agent/".to_string(),
                ..Default::default()
            },
            zeph_worktree::DefaultGitRunner::new(),
        )
        .await
        .expect("construct fixture manager");

        // Prunable: directories deleted externally (crash simulation).
        let prunable_1 = creator.create("prunable-1").await.expect("create");
        let prunable_2 = creator.create("prunable-2").await.expect("create");
        std::fs::remove_dir_all(&prunable_1.path).expect("remove prunable-1 dir");
        std::fs::remove_dir_all(&prunable_2.path).expect("remove prunable-2 dir");

        // Non-prunable: directories left intact (another live session).
        let in_use_1 = creator.create("in-use-1").await.expect("create");
        let in_use_2 = creator.create("in-use-2").await.expect("create");

        let orig_cwd = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(&repo_root).expect("set_current_dir");
        let result = super::handle_worktree_command(
            WorktreeCommand::Clean { force: false },
            Some(&config_path),
        )
        .await;
        std::env::set_current_dir(orig_cwd).expect("restore current_dir");
        result.expect("handle_worktree_command(Clean) must succeed");

        let list = git(&["worktree", "list", "--porcelain"], &repo_root);
        let list_str = String::from_utf8_lossy(&list.stdout);

        assert!(
            !list_str.contains(&*prunable_1.path.to_string_lossy()),
            "prunable-1 must be removed from the git registry: {list_str}"
        );
        assert!(
            !list_str.contains(&*prunable_2.path.to_string_lossy()),
            "prunable-2 must be removed from the git registry: {list_str}"
        );
        assert!(
            in_use_1.path.exists(),
            "in-use-1 must survive a non-force clean"
        );
        assert!(
            in_use_2.path.exists(),
            "in-use-2 must survive a non-force clean"
        );
        assert!(
            list_str.contains(&*in_use_1.path.to_string_lossy()),
            "in-use-1 must remain in the git registry: {list_str}"
        );
        assert!(
            list_str.contains(&*in_use_2.path.to_string_lossy()),
            "in-use-2 must remain in the git registry: {list_str}"
        );
    }

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
