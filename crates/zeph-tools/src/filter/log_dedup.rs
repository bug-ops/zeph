use std::collections::HashMap;
use std::fmt::Write;
use std::sync::LazyLock;

use regex::Regex;

use super::{
    CommandMatcher, FilterConfidence, FilterResult, LogDedupFilterConfig, OutputFilter, make_result,
};

const MAX_UNIQUE_PATTERNS: usize = 10_000;

static LOG_DEDUP_MATCHER: LazyLock<CommandMatcher> = LazyLock::new(|| {
    CommandMatcher::Custom(Box::new(|cmd| {
        let c = cmd.to_lowercase();
        c.contains("journalctl")
            || c.contains("tail -f")
            || c.contains("docker logs")
            || (c.contains("cat ") && c.contains(".log"))
    }))
});

static TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}([.\d]*)?([Z+-][\d:]*)?").unwrap()
});
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap()
});
static IP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}").unwrap());
static PORT_PID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:port|pid|PID)[=: ]+\d+").unwrap());

pub struct LogDedupFilter;

impl LogDedupFilter {
    #[must_use]
    pub fn new(_config: LogDedupFilterConfig) -> Self {
        Self
    }
}

impl OutputFilter for LogDedupFilter {
    fn name(&self) -> &'static str {
        "log_dedup"
    }

    fn matcher(&self) -> &CommandMatcher {
        &LOG_DEDUP_MATCHER
    }

    fn filter(&self, _command: &str, raw_output: &str, _exit_code: i32) -> FilterResult {
        let lines: Vec<&str> = raw_output.lines().collect();
        if lines.len() < 3 {
            return make_result(
                raw_output,
                raw_output.to_owned(),
                FilterConfidence::Fallback,
            );
        }

        let mut pattern_counts: HashMap<String, (usize, String)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        let mut capped = false;
        for line in &lines {
            let normalized = normalize(line);
            if let Some(entry) = pattern_counts.get_mut(&normalized) {
                entry.0 += 1;
            } else if pattern_counts.len() < MAX_UNIQUE_PATTERNS {
                order.push(normalized.clone());
                pattern_counts.insert(normalized, (1, (*line).to_owned()));
            } else {
                capped = true;
            }
        }

        let unique = order.len();
        let total = lines.len();

        if unique == total && !capped {
            return make_result(
                raw_output,
                raw_output.to_owned(),
                FilterConfidence::Fallback,
            );
        }

        let mut output = String::new();
        for key in &order {
            let (count, example) = &pattern_counts[key];
            if *count > 1 {
                let _ = writeln!(output, "{example} (x{count})");
            } else {
                let _ = writeln!(output, "{example}");
            }
        }
        let _ = write!(output, "{unique} unique patterns ({total} total lines)");
        if capped {
            let _ = write!(output, " (capped at {MAX_UNIQUE_PATTERNS})");
        }

        make_result(raw_output, output, FilterConfidence::Full)
    }
}

fn normalize(line: &str) -> String {
    let s = TIMESTAMP_RE.replace_all(line, "<TS>");
    let s = UUID_RE.replace_all(&s, "<UUID>");
    let s = IP_RE.replace_all(&s, "<IP>");
    PORT_PID_RE.replace_all(&s, "<N>").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filter() -> LogDedupFilter {
        LogDedupFilter::new(LogDedupFilterConfig::default())
    }

    #[test]
    fn matches_log_commands() {
        let f = make_filter();
        assert!(f.matcher().matches("journalctl -u nginx"));
        assert!(f.matcher().matches("tail -f /var/log/syslog"));
        assert!(f.matcher().matches("docker logs -f container"));
        assert!(f.matcher().matches("cat /var/log/app.log"));
        assert!(!f.matcher().matches("cat file.txt"));
        assert!(!f.matcher().matches("cargo build"));
    }

    #[test]
    fn filter_deduplicates() {
        let f = make_filter();
        let raw = "\
2024-01-15T12:00:01Z INFO request handled path=/api/health
2024-01-15T12:00:02Z INFO request handled path=/api/health
2024-01-15T12:00:03Z INFO request handled path=/api/health
2024-01-15T12:00:04Z WARN connection timeout addr=10.0.0.1
2024-01-15T12:00:05Z WARN connection timeout addr=10.0.0.2
2024-01-15T12:00:06Z ERROR database unreachable
";
        let result = f.filter("journalctl -u app", raw, 0);
        assert!(result.output.contains("(x3)"));
        assert!(result.output.contains("(x2)"));
        assert!(result.output.contains("3 unique patterns (6 total lines)"));
        assert!(result.savings_pct() > 20.0);
        assert_eq!(result.confidence, FilterConfidence::Full);
    }

    #[test]
    fn filter_all_unique_passthrough() {
        let f = make_filter();
        let raw = "line one\nline two\nline three";
        let result = f.filter("cat app.log", raw, 0);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }

    #[test]
    fn filter_short_passthrough() {
        let f = make_filter();
        let raw = "single line";
        let result = f.filter("cat app.log", raw, 0);
        assert_eq!(result.output, raw);
        assert_eq!(result.confidence, FilterConfidence::Fallback);
    }

    #[test]
    fn normalize_replaces_patterns() {
        let line = "2024-01-15T12:00:00Z req=abc12345-1234-1234-1234-123456789012 addr=192.168.1.1 pid=1234";
        let n = normalize(line);
        assert!(n.contains("<TS>"));
        assert!(n.contains("<UUID>"));
        assert!(n.contains("<IP>"));
        assert!(n.contains("<N>"));
    }
}
