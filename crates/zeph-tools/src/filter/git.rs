use std::fmt::Write;
use std::sync::LazyLock;

use super::{
    CommandMatcher, FilterConfidence, FilterResult, GitFilterConfig, OutputFilter, make_result,
};

static GIT_MATCHER: LazyLock<CommandMatcher> =
    LazyLock::new(|| CommandMatcher::Custom(Box::new(|cmd| cmd.trim_start().starts_with("git "))));

pub struct GitFilter {
    config: GitFilterConfig,
}

impl GitFilter {
    #[must_use]
    pub fn new(config: GitFilterConfig) -> Self {
        Self { config }
    }
}

impl OutputFilter for GitFilter {
    fn name(&self) -> &'static str {
        "git"
    }

    fn matcher(&self) -> &CommandMatcher {
        &GIT_MATCHER
    }

    fn filter(&self, command: &str, raw_output: &str, _exit_code: i32) -> FilterResult {
        let subcmd = command
            .trim_start()
            .strip_prefix("git ")
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");

        match subcmd {
            "status" => filter_status(raw_output),
            "diff" => filter_diff(raw_output, self.config.max_diff_lines),
            "log" => filter_log(raw_output, self.config.max_log_entries),
            "push" => filter_push(raw_output),
            _ => make_result(
                raw_output,
                raw_output.to_owned(),
                FilterConfidence::Fallback,
            ),
        }
    }
}

fn filter_status(raw: &str) -> FilterResult {
    let mut modified = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;
    let mut untracked = 0u32;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("M ") || trimmed.starts_with("MM") || trimmed.starts_with(" M") {
            modified += 1;
        } else if trimmed.starts_with("A ") || trimmed.starts_with("AM") {
            added += 1;
        } else if trimmed.starts_with("D ") || trimmed.starts_with(" D") {
            deleted += 1;
        } else if trimmed.starts_with("??") {
            untracked += 1;
        } else if trimmed.starts_with("modified:") {
            modified += 1;
        } else if trimmed.starts_with("new file:") {
            added += 1;
        } else if trimmed.starts_with("deleted:") {
            deleted += 1;
        }
    }

    let total = modified + added + deleted + untracked;
    if total == 0 {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback);
    }

    let mut output = String::new();
    let _ = write!(
        output,
        "M  {modified} files | A  {added} files | D  {deleted} files | ??  {untracked} files"
    );
    make_result(raw, output, FilterConfidence::Full)
}

fn filter_diff(raw: &str, max_diff_lines: usize) -> FilterResult {
    let mut files: Vec<(String, i32, i32)> = Vec::new();
    let mut current_file = String::new();
    let mut additions = 0i32;
    let mut deletions = 0i32;

    for line in raw.lines() {
        if line.starts_with("diff --git ") {
            if !current_file.is_empty() {
                files.push((current_file.clone(), additions, deletions));
            }
            line.strip_prefix("diff --git a/")
                .and_then(|s| s.split(" b/").next())
                .unwrap_or("unknown")
                .clone_into(&mut current_file);
            additions = 0;
            deletions = 0;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    if !current_file.is_empty() {
        files.push((current_file, additions, deletions));
    }

    if files.is_empty() {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback);
    }

    let total_lines: usize = raw.lines().count();
    let total_add: i32 = files.iter().map(|(_, a, _)| a).sum();
    let total_del: i32 = files.iter().map(|(_, _, d)| d).sum();
    let mut output = String::new();
    for (file, add, del) in &files {
        let _ = writeln!(output, "{file}    | +{add} -{del}");
    }
    let _ = write!(
        output,
        "{} files changed, {} insertions(+), {} deletions(-)",
        files.len(),
        total_add,
        total_del
    );
    if total_lines > max_diff_lines {
        let _ = write!(output, " (truncated from {total_lines} lines)");
    }
    make_result(raw, output, FilterConfidence::Full)
}

fn filter_log(raw: &str, max_entries: usize) -> FilterResult {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= max_entries {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback);
    }

    let mut output: String = lines[..max_entries].join("\n");
    let remaining = lines.len() - max_entries;
    let _ = write!(output, "\n... and {remaining} more commits");
    make_result(raw, output, FilterConfidence::Full)
}

fn filter_push(raw: &str) -> FilterResult {
    let mut output = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.contains("->") || trimmed.starts_with("To ") || trimmed.starts_with("Branch") {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(trimmed);
        }
    }
    if output.is_empty() {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback);
    }
    make_result(raw, output, FilterConfidence::Full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filter() -> GitFilter {
        GitFilter::new(GitFilterConfig::default())
    }

    #[test]
    fn matches_git_commands() {
        let f = make_filter();
        assert!(f.matcher().matches("git status"));
        assert!(f.matcher().matches("git diff --stat"));
        assert!(f.matcher().matches("git log --oneline"));
        assert!(f.matcher().matches("git push origin main"));
        assert!(!f.matcher().matches("cargo build"));
        assert!(!f.matcher().matches("github-cli"));
    }

    #[test]
    fn filter_status_summarizes() {
        let f = make_filter();
        let raw = " M src/main.rs\n M src/lib.rs\n?? new_file.txt\nA  added.rs\n";
        let result = f.filter("git status --short", raw, 0);
        assert!(result.output.contains("M  2 files"));
        assert!(result.output.contains("??  1 files"));
        assert!(result.output.contains("A  1 files"));
        assert_eq!(result.confidence, FilterConfidence::Full);
    }

    #[test]
    fn filter_diff_compresses() {
        let f = make_filter();
        let raw = "\
diff --git a/src/main.rs b/src/main.rs
index abc..def 100644
--- a/src/main.rs
+++ b/src/main.rs
+new line 1
+new line 2
-old line 1
diff --git a/src/lib.rs b/src/lib.rs
index ghi..jkl 100644
--- a/src/lib.rs
+++ b/src/lib.rs
+added
";
        let result = f.filter("git diff", raw, 0);
        assert!(result.output.contains("src/main.rs"));
        assert!(result.output.contains("src/lib.rs"));
        assert!(result.output.contains("2 files changed"));
        assert!(result.output.contains("3 insertions(+)"));
        assert!(result.output.contains("1 deletions(-)"));
    }

    #[test]
    fn filter_log_truncates() {
        let f = make_filter();
        let lines: Vec<String> = (0..50)
            .map(|i| format!("abc{i:04} feat: commit {i}"))
            .collect();
        let raw = lines.join("\n");
        let result = f.filter("git log --oneline", &raw, 0);
        assert!(result.output.contains("abc0000"));
        assert!(result.output.contains("abc0019"));
        assert!(!result.output.contains("abc0020"));
        assert!(result.output.contains("and 30 more commits"));
        assert_eq!(result.confidence, FilterConfidence::Full);
    }

    #[test]
    fn filter_log_short_passthrough() {
        let f = make_filter();
        let raw = "abc1234 feat: something\ndef5678 fix: other";
        let result = f.filter("git log --oneline", raw, 0);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }

    #[test]
    fn filter_push_extracts_summary() {
        let f = make_filter();
        let raw = "\
Enumerating objects: 5, done.
Counting objects: 100% (5/5), done.
Delta compression using up to 10 threads
Compressing objects: 100% (3/3), done.
Writing objects: 100% (3/3), 1.20 KiB | 1.20 MiB/s, done.
Total 3 (delta 2), reused 0 (delta 0)
To github.com:user/repo.git
   abc1234..def5678  main -> main
";
        let result = f.filter("git push origin main", raw, 0);
        assert!(result.output.contains("main -> main"));
        assert!(result.output.contains("To github.com"));
        assert!(!result.output.contains("Enumerating"));
    }

    #[test]
    fn filter_status_long_form() {
        let f = make_filter();
        let raw = "\
On branch main
Changes not staged for commit:
        modified:   src/main.rs
        modified:   src/lib.rs
        deleted:    old_file.rs

Untracked files:
        new_file.txt
";
        let result = f.filter("git status", raw, 0);
        assert!(result.output.contains("M  2 files"));
        assert!(result.output.contains("D  1 files"));
    }

    #[test]
    fn filter_diff_empty_passthrough() {
        let f = make_filter();
        let raw = "";
        let result = f.filter("git diff", raw, 0);
        assert_eq!(result.output, raw);
    }

    #[test]
    fn filter_unknown_subcommand_passthrough() {
        let f = make_filter();
        let raw = "some output";
        let result = f.filter("git stash list", raw, 0);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }
}
