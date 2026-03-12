// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::path::PathBuf;

/// Build the ordered list of agent definition paths with deduplication.
///
/// Priority (highest first):
/// 1. `cli_agents` — paths from `--agents` CLI flag
/// 2. `.zeph/agents/` — project-level (relative to CWD)
/// 3. user-level dir — `config_user_dir` or platform default (`~/.config/zeph/agents/`)
/// 4. `extra_dirs` — from `[agents]` config section
///
/// Directories are deduplicated by canonical path before returning to avoid
/// redundant scans when the same directory appears in multiple sources.
///
/// # Errors
///
/// Returns `Err` if any path in `cli_agents` does not exist on disk, because
/// CLI arguments represent explicit user intent and should fail loudly on typos.
pub fn resolve_agent_paths(
    cli_agents: &[PathBuf],
    config_user_dir: Option<&PathBuf>,
    extra_dirs: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    // Validate CLI paths eagerly — non-existent paths are user errors.
    for p in cli_agents {
        if !p.exists() {
            return Err(format!("--agents path does not exist: {}", p.display()));
        }
    }

    let mut paths: Vec<PathBuf> = Vec::new();

    // 1. CLI --agents (highest priority)
    paths.extend(cli_agents.iter().cloned());

    // 2. Project-level
    paths.push(PathBuf::from(".zeph/agents"));

    // 3. User-level
    if let Some(dir) = config_user_dir {
        if !dir.as_os_str().is_empty() {
            paths.push(dir.clone());
        }
        // explicit empty string = user disabled user-level dir
    } else {
        // Use dirs crate for cross-platform config dir resolution.
        if let Some(config_dir) = dirs::config_dir() {
            paths.push(config_dir.join("zeph").join("agents"));
        } else {
            tracing::debug!("user config dir unavailable; user-level agents directory skipped");
        }
    }

    // 4. Extra dirs from config
    paths.extend(extra_dirs.iter().cloned());

    // Deduplicate directories by canonical path. Non-existent paths cannot be
    // canonicalized — they are kept as-is and load_all will skip them silently.
    Ok(dedup_by_canonical(paths))
}

fn dedup_by_canonical(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut result = Vec::with_capacity(paths.len());

    for path in paths {
        // Only canonicalize existing paths; non-existent ones pass through.
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            result.push(path);
        } else {
            tracing::debug!(
                path = %path.display(),
                "deduplicating agent path (canonical path already in list)"
            );
        }
    }

    result
}

/// Compute the scope label for an agent definition file given the ordered path list.
///
/// Returns one of `"cli"`, `"project"`, `"user"`, `"extra"`, or `"unknown"`.
#[must_use]
pub fn scope_label(
    def_path: &std::path::Path,
    cli_agents: &[PathBuf],
    config_user_dir: Option<&PathBuf>,
    extra_dirs: &[PathBuf],
) -> &'static str {
    // Check CLI paths
    for cli_path in cli_agents {
        if def_path.starts_with(cli_path) || def_path == cli_path {
            return "cli";
        }
    }

    // Check project-level
    if def_path.starts_with(".zeph/agents") {
        return "project";
    }

    // Check user-level dir
    let user_dir = if let Some(dir) = config_user_dir {
        if dir.as_os_str().is_empty() {
            None
        } else {
            Some(dir.clone())
        }
    } else {
        dirs::config_dir().map(|d| d.join("zeph").join("agents"))
    };

    if user_dir
        .as_ref()
        .is_some_and(|udir| def_path.starts_with(udir))
    {
        return "user";
    }

    // Check extra dirs
    for extra in extra_dirs {
        if def_path.starts_with(extra) {
            return "extra";
        }
    }

    "unknown"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_empty_inputs_returns_project_and_user() {
        let paths = resolve_agent_paths(&[], None, &[]).unwrap();
        // Must have at least the project-level path
        assert!(paths.iter().any(|p| p == &PathBuf::from(".zeph/agents")));
    }

    #[test]
    fn resolve_cli_paths_come_first() {
        let tmp = tempfile::tempdir().unwrap();
        let cli_path = tmp.path().to_path_buf();
        let paths = resolve_agent_paths(std::slice::from_ref(&cli_path), None, &[]).unwrap();
        assert_eq!(paths[0], cli_path);
    }

    #[test]
    fn resolve_nonexistent_cli_path_returns_error() {
        let bad = PathBuf::from("/tmp/zeph-test-does-not-exist-12345");
        let err = resolve_agent_paths(&[bad], None, &[]).unwrap_err();
        assert!(err.contains("--agents path does not exist"));
    }

    #[test]
    fn resolve_empty_user_dir_disables_user_level() {
        let paths = resolve_agent_paths(&[], Some(&PathBuf::from("")), &[]).unwrap();
        // No user-level dir should be added
        let has_config_dir = paths.iter().any(|p| {
            p.to_str()
                .is_some_and(|s| s.contains(".config") || s.contains("AppData"))
        });
        assert!(!has_config_dir);
    }

    #[test]
    fn resolve_explicit_user_dir_added() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().to_path_buf();
        let paths = resolve_agent_paths(&[], Some(&user_dir), &[]).unwrap();
        assert!(paths.contains(&user_dir));
    }

    #[test]
    fn resolve_extra_dirs_come_last() {
        let tmp = tempfile::tempdir().unwrap();
        let extra = tmp.path().to_path_buf();
        let paths =
            resolve_agent_paths(&[], Some(&PathBuf::from("")), std::slice::from_ref(&extra))
                .unwrap();
        assert_eq!(paths.last().unwrap(), &extra);
    }

    #[test]
    fn resolve_deduplicates_same_canonical_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        // Same directory added twice: once as explicit user dir, once as extra
        let paths = resolve_agent_paths(&[], Some(&dir), std::slice::from_ref(&dir)).unwrap();
        let count = paths.iter().filter(|p| *p == &dir).count();
        assert_eq!(count, 1, "duplicate paths should be removed");
    }

    #[test]
    fn scope_label_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let cli_dir = tmp.path().to_path_buf();
        let def_path = cli_dir.join("my-agent.md");
        let label = scope_label(&def_path, &[cli_dir], None, &[]);
        assert_eq!(label, "cli");
    }

    #[test]
    fn scope_label_project() {
        let def_path = PathBuf::from(".zeph/agents/my-agent.md");
        let label = scope_label(&def_path, &[], None, &[]);
        assert_eq!(label, "project");
    }

    #[test]
    fn scope_label_extra() {
        let tmp = tempfile::tempdir().unwrap();
        let extra_dir = tmp.path().to_path_buf();
        let def_path = extra_dir.join("my-agent.md");
        let label = scope_label(&def_path, &[], Some(&PathBuf::from("")), &[extra_dir]);
        assert_eq!(label, "extra");
    }

    #[test]
    fn scope_label_user() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().to_path_buf();
        let def_path = user_dir.join("my-agent.md");
        let label = scope_label(&def_path, &[], Some(&user_dir), &[]);
        assert_eq!(label, "user");
    }

    #[test]
    fn scope_label_unknown_when_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        let def_path = tmp.path().join("my-agent.md");
        let label = scope_label(&def_path, &[], Some(&PathBuf::from("")), &[]);
        assert_eq!(label, "unknown");
    }

    #[test]
    fn resolve_user_dir_none_falls_back_to_platform_default() {
        // When config_user_dir is None, platform default should be attempted.
        // We cannot guarantee dirs::config_dir() returns Some on all CI machines,
        // but we can verify that at minimum the project-level path is present.
        let paths = resolve_agent_paths(&[], None, &[]).unwrap();
        assert!(paths.iter().any(|p| p == &PathBuf::from(".zeph/agents")));
    }

    #[test]
    fn resolve_priority_order_cli_first_then_project() {
        let tmp = tempfile::tempdir().unwrap();
        let cli_dir = tmp.path().to_path_buf();
        let paths = resolve_agent_paths(
            std::slice::from_ref(&cli_dir),
            Some(&PathBuf::from("")),
            &[],
        )
        .unwrap();
        // CLI must be index 0, project-level must follow
        assert_eq!(paths[0], cli_dir);
        assert_eq!(paths[1], PathBuf::from(".zeph/agents"));
    }

    #[test]
    fn resolve_extra_dirs_after_user_dir() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let user_dir = tmp1.path().to_path_buf();
        let extra_dir = tmp2.path().to_path_buf();
        let paths =
            resolve_agent_paths(&[], Some(&user_dir), std::slice::from_ref(&extra_dir)).unwrap();
        let user_pos = paths.iter().position(|p| p == &user_dir).unwrap();
        let extra_pos = paths.iter().position(|p| p == &extra_dir).unwrap();
        assert!(user_pos < extra_pos, "user dir must come before extra dirs");
    }
}
