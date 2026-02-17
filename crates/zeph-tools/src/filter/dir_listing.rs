use std::fmt::Write;

use super::{FilterResult, OutputFilter, make_result};

const NOISE_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".next",
    ".cache",
];

pub struct DirListingFilter;

impl OutputFilter for DirListingFilter {
    fn matches(&self, command: &str) -> bool {
        let cmd = command.trim_start();
        cmd == "ls" || cmd.starts_with("ls ")
    }

    fn filter(&self, _command: &str, raw_output: &str, _exit_code: i32) -> FilterResult {
        let mut kept = Vec::new();
        let mut hidden: Vec<&str> = Vec::new();

        for line in raw_output.lines() {
            let entry = line.split_whitespace().last().unwrap_or(line);
            let name = entry.trim_end_matches('/');

            if NOISE_DIRS.contains(&name) {
                hidden.push(name);
            } else {
                kept.push(line);
            }
        }

        if hidden.is_empty() {
            return make_result(raw_output, raw_output.to_owned());
        }

        let mut output = kept.join("\n");
        let names = hidden.join(", ");
        let _ = write!(output, "\n(+ {} hidden: {names})", hidden.len());

        make_result(raw_output, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ls() {
        let f = DirListingFilter;
        assert!(f.matches("ls"));
        assert!(f.matches("ls -la"));
        assert!(f.matches("ls /tmp"));
        assert!(!f.matches("lsof"));
        assert!(!f.matches("cargo build"));
    }

    #[test]
    fn filter_hides_noise_dirs() {
        let f = DirListingFilter;
        let raw = "Cargo.toml\nsrc\ntarget\nnode_modules\nREADME.md\n.git";
        let result = f.filter("ls", raw, 0);
        assert!(result.output.contains("Cargo.toml"));
        assert!(result.output.contains("src"));
        assert!(result.output.contains("README.md"));
        assert!(!result.output.contains("\ntarget\n"));
        assert!(
            result
                .output
                .contains("(+ 3 hidden: target, node_modules, .git)")
        );
    }

    #[test]
    fn filter_no_noise_passthrough() {
        let f = DirListingFilter;
        let raw = "Cargo.toml\nsrc\nREADME.md";
        let result = f.filter("ls", raw, 0);
        assert_eq!(result.output, raw);
    }

    #[test]
    fn filter_ls_la_format() {
        let f = DirListingFilter;
        let raw = "\
drwxr-xr-x  5 user staff 160 Jan 1 12:00 src
drwxr-xr-x 20 user staff 640 Jan 1 12:00 node_modules
-rw-r--r--  1 user staff 200 Jan 1 12:00 Cargo.toml
drwxr-xr-x  8 user staff 256 Jan 1 12:00 target";
        let result = f.filter("ls -la", raw, 0);
        assert!(result.output.contains("src"));
        assert!(result.output.contains("Cargo.toml"));
        assert!(result.output.contains("(+ 2 hidden: node_modules, target)"));
    }
}
