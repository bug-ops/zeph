use std::fmt::Write;
use std::path::{Path, PathBuf};

const PROJECT_CONFIG_FILES: &[&str] = &["ZEPH.md", ".zeph/config.md"];

/// Walk up from `start` to filesystem root, collecting all ZEPH.md files.
/// Returns paths ordered from most general (ancestor) to most specific (cwd).
#[must_use]
pub fn discover_project_configs(start: &Path) -> Vec<PathBuf> {
    let mut configs = Vec::new();
    let mut current = start.to_path_buf();

    loop {
        for filename in PROJECT_CONFIG_FILES {
            let candidate = current.join(filename);
            if candidate.is_file() {
                configs.push(candidate);
            }
        }
        if !current.pop() {
            break;
        }
    }

    configs.reverse();
    configs
}

/// Load and concatenate project configs into a prompt section.
#[must_use]
pub fn load_project_context(configs: &[PathBuf]) -> String {
    if configs.is_empty() {
        return String::new();
    }

    let mut out = String::from("<project_context>\n");
    for path in configs {
        if let Ok(content) = std::fs::read_to_string(path) {
            let source = path.display();
            let _ = write!(
                out,
                "  <config source=\"{source}\">\n{content}\n  </config>\n"
            );
        }
    }
    out.push_str("</project_context>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_project_configs_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let configs = discover_project_configs(dir.path());
        assert!(configs.is_empty());
    }

    #[test]
    fn discover_project_configs_finds_zeph_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ZEPH.md"), "# Project").unwrap();
        let configs = discover_project_configs(dir.path());
        assert_eq!(configs.len(), 1);
        assert!(configs[0].ends_with("ZEPH.md"));
    }

    #[test]
    fn discover_project_configs_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("sub");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(dir.path().join("ZEPH.md"), "# Parent").unwrap();
        std::fs::write(child.join("ZEPH.md"), "# Child").unwrap();

        let configs = discover_project_configs(&child);
        assert!(configs.len() >= 2);
        // Parent should come before child (reversed order)
        let parent_idx = configs
            .iter()
            .position(|p| p.parent().unwrap() == dir.path())
            .unwrap();
        let child_idx = configs
            .iter()
            .position(|p| p.parent().unwrap() == child)
            .unwrap();
        assert!(parent_idx < child_idx);
    }

    #[test]
    fn load_project_context_empty() {
        let result = load_project_context(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn load_project_context_concatenates() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("ZEPH.md");
        let f2 = dir.path().join("other.md");
        std::fs::write(&f1, "config 1").unwrap();
        std::fs::write(&f2, "config 2").unwrap();

        let result = load_project_context(&[f1, f2]);
        assert!(result.starts_with("<project_context>"));
        assert!(result.ends_with("</project_context>"));
        assert!(result.contains("config 1"));
        assert!(result.contains("config 2"));
        assert!(result.contains("<config source="));
    }
}
