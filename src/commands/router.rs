// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::router::thompson::ThompsonState;

use crate::cli::RouterCommand;

/// Handle `zeph router <subcommand>`.
///
/// # Errors
///
/// Returns an error if state file I/O fails.
pub(crate) fn handle_router_command(cmd: RouterCommand) -> anyhow::Result<()> {
    match cmd {
        RouterCommand::Stats { state_path } => {
            let path = state_path.unwrap_or_else(ThompsonState::default_path);
            let thompson_state = ThompsonState::load(&path);
            let stats = thompson_state.provider_stats();
            if stats.is_empty() {
                println!("No Thompson state found at: {}", path.display());
                println!("(File missing or empty — uniform priors will be used on next run)");
            } else {
                println!("Thompson Sampling state: {}", path.display());
                println!(
                    "{:<30} {:>8} {:>8} {:>12}",
                    "Provider", "alpha", "beta", "Mean%"
                );
                println!("{}", "-".repeat(62));
                let total_mean: f64 = stats.iter().map(|(_, a, b)| a / (a + b)).sum();
                for (name, alpha, beta) in &stats {
                    let mean = alpha / (alpha + beta);
                    let pct = if total_mean > 0.0 {
                        mean / total_mean * 100.0
                    } else {
                        0.0
                    };
                    println!("{name:<30} {alpha:>8.2} {beta:>8.2} {pct:>11.1}%");
                }
            }
            Ok(())
        }
        RouterCommand::Reset { state_path } => {
            let path = state_path.unwrap_or_else(ThompsonState::default_path);
            match std::fs::remove_file(&path) {
                Ok(()) => println!("Thompson state reset: {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("No state file at: {} (nothing to reset)", path.display());
                }
                Err(e) => return Err(e.into()),
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn stats_missing_file_succeeds() {
        let cmd = RouterCommand::Stats {
            state_path: Some(PathBuf::from("/tmp/zeph-router-test-nonexistent.json")),
        };
        // Should not error — missing file is treated as empty state.
        assert!(handle_router_command(cmd).is_ok());
    }

    #[test]
    fn reset_missing_file_succeeds() {
        let cmd = RouterCommand::Reset {
            state_path: Some(PathBuf::from("/tmp/zeph-router-test-nonexistent.json")),
        };
        // Should not error — ENOENT is handled gracefully.
        assert!(handle_router_command(cmd).is_ok());
    }

    #[test]
    fn reset_existing_file_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let mut state = ThompsonState::default();
        state.update("p", true);
        state.save(&path).unwrap();
        assert!(path.exists());

        let cmd = RouterCommand::Reset {
            state_path: Some(path.clone()),
        };
        handle_router_command(cmd).unwrap();
        assert!(!path.exists(), "state file must be deleted after reset");
    }
}
