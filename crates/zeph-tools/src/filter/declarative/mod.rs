// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Declarative TOML-based output filter engine.
//!
//! Loads filter rules from a TOML file and compiles them into [`OutputFilter`]
//! implementations at startup.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use super::{
    CommandMatcher, FilterConfidence, FilterResult, OutputFilter, make_result, sanitize_output,
};

// ---------------------------------------------------------------------------
// Deserialization types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct DeclarativeFilterFile {
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

#[derive(Deserialize)]
pub(crate) struct RuleConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    pub strategy: StrategyConfig,
    #[serde(default = "super::default_true")]
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct MatchConfig {
    pub exact: Option<String>,
    pub prefix: Option<String>,
    pub regex: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct NormalizeEntry {
    pub pattern: String,
    pub replacement: String,
}

fn default_head() -> usize {
    20
}

fn default_tail() -> usize {
    20
}

fn default_long_threshold() -> usize {
    30
}

fn default_keep_head() -> usize {
    10
}

fn default_keep_tail() -> usize {
    5
}

fn default_max_failures() -> usize {
    10
}

fn default_truncate_stack_trace() -> usize {
    50
}

fn default_max_diff_lines() -> usize {
    500
}

fn default_max_unique() -> usize {
    10_000
}

fn default_normalize_patterns() -> Vec<NormalizeEntry> {
    vec![
        NormalizeEntry {
            pattern: r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}([.\d]*)?([Z+-][\d:]*)?".into(),
            replacement: "<TS>".into(),
        },
        NormalizeEntry {
            pattern: r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}".into(),
            replacement: "<UUID>".into(),
        },
        NormalizeEntry {
            pattern: r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}".into(),
            replacement: "<IP>".into(),
        },
        NormalizeEntry {
            pattern: r"(?:port|pid|PID)[=: ]+\d+".into(),
            replacement: "<N>".into(),
        },
    ]
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StrategyConfig {
    StripNoise {
        patterns: Vec<String>,
    },
    Truncate {
        max_lines: usize,
        #[serde(default = "default_head")]
        head: usize,
        #[serde(default = "default_tail")]
        tail: usize,
    },
    KeepMatching {
        patterns: Vec<String>,
    },
    StripAnnotated {
        patterns: Vec<String>,
        #[serde(default)]
        summary_pattern: Option<String>,
        #[serde(default = "default_long_threshold")]
        long_output_threshold: usize,
        #[serde(default = "default_keep_head")]
        keep_head: usize,
        #[serde(default = "default_keep_tail")]
        keep_tail: usize,
    },
    TestSummary {
        #[serde(default = "default_max_failures")]
        max_failures: usize,
        #[serde(default = "default_truncate_stack_trace")]
        truncate_stack_trace: usize,
    },
    GroupByRule {
        location_pattern: String,
        rule_pattern: String,
    },
    GitStatus {},
    GitDiff {
        #[serde(default = "default_max_diff_lines")]
        max_diff_lines: usize,
    },
    Dedup {
        #[serde(default = "default_normalize_patterns")]
        normalize_patterns: Vec<NormalizeEntry>,
        #[serde(default = "default_max_unique")]
        max_unique_patterns: usize,
    },
}

// ---------------------------------------------------------------------------
// Compiled runtime types
// ---------------------------------------------------------------------------

pub(crate) enum CompiledStrategy {
    StripNoise {
        patterns: Vec<Regex>,
    },
    Truncate {
        max_lines: usize,
        head: usize,
        tail: usize,
    },
    KeepMatching {
        patterns: Vec<Regex>,
    },
    StripAnnotated {
        patterns: Vec<Regex>,
        summary_pattern: Option<Regex>,
        long_output_threshold: usize,
        keep_head: usize,
        keep_tail: usize,
    },
    TestSummary {
        max_failures: usize,
        truncate_stack_trace: usize,
    },
    GroupByRule {
        location_re: Regex,
        rule_re: Regex,
    },
    GitStatus,
    GitDiff {
        max_diff_lines: usize,
    },
    Dedup {
        normalize_patterns: Vec<(Regex, String)>,
        max_unique_patterns: usize,
    },
}

pub(crate) struct DeclarativeFilter {
    name: &'static str,
    matcher: CommandMatcher,
    strategy: CompiledStrategy,
}

impl DeclarativeFilter {
    pub fn compile(rule: RuleConfig) -> Result<Self, String> {
        let name: &'static str = Box::leak(rule.name.into_boxed_str());
        let matcher = compile_match(&rule.match_config)?;
        let strategy = compile_strategy(rule.strategy)?;
        Ok(Self {
            name,
            matcher,
            strategy,
        })
    }
}

fn compile_regex(pattern: &str) -> Result<Regex, String> {
    if pattern.len() > 512 {
        return Err(format!("pattern '{pattern}': exceeds 512 character limit"));
    }
    RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| format!("pattern '{pattern}': {e}"))
}

fn compile_match(m: &MatchConfig) -> Result<CommandMatcher, String> {
    if let Some(ref exact) = m.exact {
        let s: &'static str = Box::leak(exact.clone().into_boxed_str());
        Ok(CommandMatcher::Exact(s))
    } else if let Some(ref prefix) = m.prefix {
        let s: &'static str = Box::leak(prefix.clone().into_boxed_str());
        Ok(CommandMatcher::Prefix(s))
    } else if let Some(ref regex) = m.regex {
        if regex.len() > 512 {
            return Err("regex pattern exceeds 512 character limit".into());
        }
        let re = RegexBuilder::new(regex)
            .size_limit(1 << 20)
            .build()
            .map_err(|e| format!("invalid regex: {e}"))?;
        Ok(CommandMatcher::Regex(re))
    } else {
        Err("match config must have exactly one of: exact, prefix, regex".into())
    }
}

fn contains_unescaped_dollar(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next(); // skip escaped char
        } else if c == '$' {
            return true;
        }
    }
    false
}

fn compile_patterns(patterns: &[String]) -> Result<Vec<Regex>, String> {
    patterns
        .iter()
        .map(|p| compile_regex(p))
        .collect::<Result<Vec<_>, _>>()
}

fn compile_dedup_entry(e: NormalizeEntry) -> Result<(Regex, String), String> {
    if contains_unescaped_dollar(&e.replacement) {
        return Err(format!(
            "replacement '{}': unescaped '$' is not allowed (use plain text like <TS>)",
            e.replacement
        ));
    }
    compile_regex(&e.pattern).map(|re| (re, e.replacement))
}

fn compile_strategy(s: StrategyConfig) -> Result<CompiledStrategy, String> {
    match s {
        StrategyConfig::StripNoise { patterns } => {
            if patterns.is_empty() {
                tracing::warn!("rule has empty patterns list");
                return Err("strip_noise rule has empty patterns list".into());
            }
            Ok(CompiledStrategy::StripNoise {
                patterns: compile_patterns(&patterns)?,
            })
        }
        StrategyConfig::Truncate {
            max_lines,
            head,
            tail,
        } => {
            if head + tail > max_lines {
                return Err("head + tail must not exceed max_lines".into());
            }
            Ok(CompiledStrategy::Truncate {
                max_lines,
                head,
                tail,
            })
        }
        StrategyConfig::KeepMatching { patterns } => {
            if patterns.is_empty() {
                tracing::warn!("rule has empty patterns list");
                return Err("keep_matching rule has empty patterns list".into());
            }
            Ok(CompiledStrategy::KeepMatching {
                patterns: compile_patterns(&patterns)?,
            })
        }
        StrategyConfig::StripAnnotated {
            patterns,
            summary_pattern,
            long_output_threshold,
            keep_head,
            keep_tail,
        } => {
            if patterns.is_empty() {
                tracing::warn!("rule has empty patterns list");
                return Err("strip_annotated rule has empty patterns list".into());
            }
            let summary_re = summary_pattern.as_deref().map(compile_regex).transpose()?;
            Ok(CompiledStrategy::StripAnnotated {
                patterns: compile_patterns(&patterns)?,
                summary_pattern: summary_re,
                long_output_threshold,
                keep_head,
                keep_tail,
            })
        }
        StrategyConfig::TestSummary {
            max_failures,
            truncate_stack_trace,
        } => Ok(CompiledStrategy::TestSummary {
            max_failures,
            truncate_stack_trace,
        }),
        StrategyConfig::GroupByRule {
            location_pattern,
            rule_pattern,
        } => {
            let location_re = compile_regex(&location_pattern)?;
            let rule_re = compile_regex(&rule_pattern)?;
            Ok(CompiledStrategy::GroupByRule {
                location_re,
                rule_re,
            })
        }
        StrategyConfig::GitStatus {} => Ok(CompiledStrategy::GitStatus),
        StrategyConfig::GitDiff { max_diff_lines } => {
            Ok(CompiledStrategy::GitDiff { max_diff_lines })
        }
        StrategyConfig::Dedup {
            normalize_patterns,
            max_unique_patterns,
        } => {
            let compiled = normalize_patterns
                .into_iter()
                .map(compile_dedup_entry)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CompiledStrategy::Dedup {
                normalize_patterns: compiled,
                max_unique_patterns,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// is_cargo_noise helper (used by GroupByRule)
// ---------------------------------------------------------------------------

const CARGO_NOISE_PREFIXES: &[&str] = &[
    "Compiling ",
    "Downloading ",
    "Downloaded ",
    "Updating ",
    "Fetching ",
    "Fresh ",
    "Packaging ",
    "Verifying ",
    "Archiving ",
    "Locking ",
    "Adding ",
    "Removing ",
    "Checking ",
    "Documenting ",
    "Running ",
    "Loaded ",
    "Blocking ",
    "Unpacking ",
    "Finished ",
];

pub(crate) fn is_cargo_noise(line: &str) -> bool {
    let trimmed = line.trim_start();
    CARGO_NOISE_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

// ---------------------------------------------------------------------------
// Strategy implementations
// ---------------------------------------------------------------------------

fn apply_strip_annotated(
    raw: &str,
    patterns: &[Regex],
    summary_pattern: Option<&Regex>,
    long_output_threshold: usize,
    keep_head: usize,
    keep_tail: usize,
    exit_code: i32,
) -> FilterResult {
    let clean = sanitize_output(raw);
    let mut noise_count = 0usize;
    let mut kept: Vec<&str> = Vec::new();
    let mut summary_line: Option<String> = None;

    for line in clean.lines() {
        if summary_pattern.is_some_and(|sp| sp.is_match(line)) {
            summary_line = Some(line.trim_start().to_owned());
            noise_count += 1;
            continue;
        }
        if patterns.iter().any(|p| p.is_match(line)) {
            noise_count += 1;
        } else {
            kept.push(line);
        }
    }

    if noise_count == 0 {
        if exit_code != 0 {
            return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
        }
        let lines: Vec<&str> = clean.lines().collect();
        if lines.len() > long_output_threshold {
            return truncate_kept(raw, &lines, keep_head, keep_tail, FilterConfidence::Partial);
        }
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
    }

    let mut output = String::new();
    if let Some(ref fin) = summary_line {
        let _ = writeln!(output, "{fin}");
    }
    let _ = writeln!(output, "({noise_count} noise lines removed)");
    if !kept.is_empty() {
        output.push('\n');
        if kept.len() > long_output_threshold {
            let actual_head = keep_head.min(kept.len());
            let actual_tail = keep_tail.min(kept.len().saturating_sub(actual_head));
            let omitted = kept.len() - actual_head - actual_tail;
            for line in &kept[..actual_head] {
                let _ = writeln!(output, "{line}");
            }
            let _ = writeln!(output, "\n... ({omitted} lines omitted) ...\n");
            for line in &kept[kept.len() - actual_tail..] {
                let _ = writeln!(output, "{line}");
            }
        } else {
            for line in &kept {
                let _ = writeln!(output, "{line}");
            }
        }
    }
    make_result(
        raw,
        output.trim_end().to_owned(),
        FilterConfidence::Full,
        vec![],
    )
}

fn truncate_kept(
    raw: &str,
    lines: &[&str],
    keep_head: usize,
    keep_tail: usize,
    confidence: FilterConfidence,
) -> FilterResult {
    let total = lines.len();
    let omitted = total - keep_head - keep_tail;
    let mut output = String::new();
    for line in &lines[..keep_head] {
        let _ = writeln!(output, "{line}");
    }
    let _ = writeln!(output, "\n... ({omitted} lines omitted) ...\n");
    for line in &lines[total - keep_tail..] {
        let _ = writeln!(output, "{line}");
    }
    let kept_indices: Vec<usize> = (0..keep_head).chain(total - keep_tail..total).collect();
    make_result(raw, output.trim_end().to_owned(), confidence, kept_indices)
}

fn apply_test_summary(
    raw: &str,
    exit_code: i32,
    max_failures: usize,
    truncate_stack_trace: usize,
) -> FilterResult {
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut ignored = 0u64;
    let mut filtered_out = 0u64;
    let mut failure_blocks: Vec<String> = Vec::new();
    let mut in_failure_block = false;
    let mut current_block = String::new();
    let mut has_summary = false;

    for line in raw.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("FAIL [") || trimmed.starts_with("FAIL  [") {
            failed += 1;
            continue;
        }
        if trimmed.starts_with("PASS [") || trimmed.starts_with("PASS  [") {
            passed += 1;
            continue;
        }

        if trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----") {
            in_failure_block = true;
            current_block.clear();
            current_block.push_str(line);
            current_block.push('\n');
            continue;
        }

        if in_failure_block {
            current_block.push_str(line);
            current_block.push('\n');
            if trimmed == "failures:" || trimmed.starts_with("---- ") {
                failure_blocks.push(current_block.clone());
                in_failure_block = trimmed.starts_with("---- ");
                if in_failure_block {
                    current_block.clear();
                    current_block.push_str(line);
                    current_block.push('\n');
                }
            }
            continue;
        }

        if trimmed == "failures:" && !current_block.is_empty() {
            failure_blocks.push(current_block.clone());
            current_block.clear();
        }

        if trimmed.starts_with("test result:") {
            has_summary = true;
            for part in trimmed.split(';') {
                let part = part.trim();
                if let Some(n) = extract_count(part, "passed") {
                    passed += n;
                } else if let Some(n) = extract_count(part, "failed") {
                    failed += n;
                } else if let Some(n) = extract_count(part, "ignored") {
                    ignored += n;
                } else if let Some(n) = extract_count(part, "filtered out") {
                    filtered_out += n;
                }
            }
        }

        if trimmed.contains("tests run:") {
            has_summary = true;
        }
    }

    if in_failure_block && !current_block.is_empty() {
        failure_blocks.push(current_block);
    }

    if !has_summary && passed == 0 && failed == 0 {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
    }

    let mut output = String::new();

    if exit_code != 0 && !failure_blocks.is_empty() {
        output.push_str("FAILURES:\n\n");
        for block in failure_blocks.iter().take(max_failures) {
            let lines: Vec<&str> = block.lines().collect();
            if lines.len() > truncate_stack_trace {
                for line in &lines[..truncate_stack_trace] {
                    output.push_str(line);
                    output.push('\n');
                }
                let remaining = lines.len() - truncate_stack_trace;
                let _ = writeln!(output, "... ({remaining} more lines)");
            } else {
                output.push_str(block);
            }
            output.push('\n');
        }
        if failure_blocks.len() > max_failures {
            let _ = writeln!(
                output,
                "... and {} more failure(s)",
                failure_blocks.len() - max_failures
            );
        }
    }

    let status = if failed > 0 { "FAILED" } else { "ok" };
    let _ = write!(
        output,
        "test result: {status}. {passed} passed; {failed} failed; \
         {ignored} ignored; {filtered_out} filtered out"
    );

    make_result(raw, output, FilterConfidence::Full, vec![])
}

fn extract_count(s: &str, label: &str) -> Option<u64> {
    let idx = s.find(label)?;
    let before = s[..idx].trim();
    let num_str = before.rsplit_once(' ').map_or(before, |(_, n)| n);
    let num_str = num_str.trim_end_matches('.');
    let num_str = num_str.rsplit('.').next().unwrap_or(num_str).trim();
    num_str.parse().ok()
}

fn apply_group_by_rule(
    raw: &str,
    exit_code: i32,
    location_re: &Regex,
    rule_re: &Regex,
) -> FilterResult {
    let has_error = raw.contains("error[") || raw.contains("error:");
    if has_error && exit_code != 0 {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
    }

    let mut warnings: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut pending_location: Option<String> = None;
    let mut kept_indices: Vec<usize> = Vec::new();

    for (idx, line) in raw.lines().enumerate() {
        if let Some(caps) = location_re.captures(line) {
            pending_location = Some(caps[1].to_owned());
            kept_indices.push(idx);
        }
        if let Some(caps) = rule_re.captures(line) {
            let rule = caps[1].to_owned();
            if let Some(loc) = pending_location.take() {
                warnings.entry(rule).or_default().push(loc);
            }
        }
    }

    if warnings.is_empty() {
        let all_lines: Vec<&str> = raw.lines().collect();
        let kept: Vec<(usize, &str)> = all_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !is_cargo_noise(l))
            .map(|(i, l)| (i, *l))
            .collect();
        if kept.len() < all_lines.len() {
            let output = kept.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n");
            let ki: Vec<usize> = kept.iter().map(|(i, _)| *i).collect();
            return make_result(raw, output, FilterConfidence::Partial, ki);
        }
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
    }

    let total: usize = warnings.values().map(Vec::len).sum();
    let rules = warnings.len();
    let mut output = String::new();

    for (rule, locations) in &warnings {
        let count = locations.len();
        let label = if count == 1 { "warning" } else { "warnings" };
        let _ = writeln!(output, "{rule} ({count} {label}):");
        for loc in locations {
            let _ = writeln!(output, "  {loc}");
        }
        output.push('\n');
    }
    let _ = write!(output, "{total} warnings total ({rules} rules)");

    make_result(raw, output, FilterConfidence::Full, kept_indices)
}

fn apply_git_status(raw: &str) -> FilterResult {
    let mut modified = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;
    let mut untracked = 0u32;
    let mut kept_indices: Vec<usize> = Vec::new();

    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("M ") || trimmed.starts_with("MM") || trimmed.starts_with(" M") {
            modified += 1;
            kept_indices.push(idx);
        } else if trimmed.starts_with("A ") || trimmed.starts_with("AM") {
            added += 1;
            kept_indices.push(idx);
        } else if trimmed.starts_with("D ") || trimmed.starts_with(" D") {
            deleted += 1;
            kept_indices.push(idx);
        } else if trimmed.starts_with("??") {
            untracked += 1;
            kept_indices.push(idx);
        } else if trimmed.starts_with("modified:") {
            modified += 1;
            kept_indices.push(idx);
        } else if trimmed.starts_with("new file:") {
            added += 1;
            kept_indices.push(idx);
        } else if trimmed.starts_with("deleted:") {
            deleted += 1;
            kept_indices.push(idx);
        }
    }

    let total = modified + added + deleted + untracked;
    if total == 0 {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
    }

    let mut output = String::new();
    let _ = write!(
        output,
        "M  {modified} files | A  {added} files | D  {deleted} files | ??  {untracked} files"
    );
    make_result(raw, output, FilterConfidence::Full, kept_indices)
}

fn apply_git_diff(raw: &str, max_diff_lines: usize) -> FilterResult {
    let mut files: Vec<(String, i32, i32)> = Vec::new();
    let mut current_file = String::new();
    let mut additions = 0i32;
    let mut deletions = 0i32;
    let mut kept_indices: Vec<usize> = Vec::new();

    for (idx, line) in raw.lines().enumerate() {
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
            kept_indices.push(idx);
        } else if line.starts_with("@@ ") {
            kept_indices.push(idx);
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
            kept_indices.push(idx);
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
            kept_indices.push(idx);
        }
    }
    if !current_file.is_empty() {
        files.push((current_file, additions, deletions));
    }

    if files.is_empty() {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
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
    make_result(raw, output, FilterConfidence::Full, kept_indices)
}

fn apply_dedup(
    raw: &str,
    normalize_patterns: &[(Regex, String)],
    max_unique_patterns: usize,
) -> FilterResult {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() < 3 {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
    }

    let mut pattern_counts: HashMap<String, (usize, String, usize)> =
        HashMap::with_capacity(max_unique_patterns.min(4096));
    let mut order: Vec<String> = Vec::new();
    let mut capped = false;

    for (idx, line) in lines.iter().enumerate() {
        let normalized = dedup_normalize(line, normalize_patterns);
        if let Some(entry) = pattern_counts.get_mut(&normalized) {
            entry.0 += 1;
        } else if pattern_counts.len() < max_unique_patterns {
            order.push(normalized.clone());
            pattern_counts.insert(normalized, (1, (*line).to_owned(), idx));
        } else {
            capped = true;
        }
    }

    let unique = order.len();
    let total = lines.len();

    if unique == total && !capped {
        return make_result(raw, raw.to_owned(), FilterConfidence::Fallback, vec![]);
    }

    let mut output = String::new();
    let mut kept_indices: Vec<usize> = Vec::new();
    for key in &order {
        let (count, example, first_idx) = &pattern_counts[key];
        kept_indices.push(*first_idx);
        if *count > 1 {
            let _ = writeln!(output, "{example} (x{count})");
        } else {
            let _ = writeln!(output, "{example}");
        }
    }
    let _ = write!(output, "{unique} unique patterns ({total} total lines)");
    if capped {
        let _ = write!(output, " (capped at {max_unique_patterns})");
    }

    make_result(raw, output, FilterConfidence::Full, kept_indices)
}

fn dedup_normalize(line: &str, patterns: &[(Regex, String)]) -> String {
    let mut s = line.to_owned();
    for (re, replacement) in patterns {
        s = re.replace_all(&s, replacement.as_str()).into_owned();
    }
    s
}

fn apply_truncate(
    raw_output: &str,
    clean: String,
    max_lines: usize,
    head: usize,
    tail: usize,
) -> FilterResult {
    let lines: Vec<&str> = clean.lines().collect();
    if lines.len() <= max_lines {
        return make_result(raw_output, clean, FilterConfidence::Fallback, vec![]);
    }
    let total = lines.len();
    let omitted = total - head - tail;
    let mut output = String::new();
    for line in &lines[..head] {
        output.push_str(line);
        output.push('\n');
    }
    let _ = write!(output, "\n... ({omitted} lines omitted) ...\n\n");
    for line in &lines[total - tail..] {
        output.push_str(line);
        output.push('\n');
    }
    let kept_indices: Vec<usize> = (0..head).chain(total - tail..total).collect();
    make_result(
        raw_output,
        output.trim_end().to_owned(),
        FilterConfidence::Partial,
        kept_indices,
    )
}

// ---------------------------------------------------------------------------
// OutputFilter impl
// ---------------------------------------------------------------------------

impl OutputFilter for DeclarativeFilter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn matcher(&self) -> &CommandMatcher {
        &self.matcher
    }

    fn filter(&self, _command: &str, raw_output: &str, exit_code: i32) -> FilterResult {
        let clean = sanitize_output(raw_output);
        match &self.strategy {
            CompiledStrategy::StripNoise { patterns } => {
                let raw_lines: Vec<&str> = clean.lines().collect();
                let kept_indices: Vec<usize> = raw_lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| !patterns.iter().any(|p| p.is_match(line)))
                    .map(|(i, _)| i)
                    .collect();
                let filtered: String = kept_indices
                    .iter()
                    .map(|&i| raw_lines[i])
                    .collect::<Vec<_>>()
                    .join("\n");
                if filtered.len() < clean.len() {
                    make_result(raw_output, filtered, FilterConfidence::Full, kept_indices)
                } else {
                    make_result(raw_output, clean, FilterConfidence::Fallback, vec![])
                }
            }
            CompiledStrategy::Truncate {
                max_lines,
                head,
                tail,
            } => apply_truncate(raw_output, clean, *max_lines, *head, *tail),
            CompiledStrategy::KeepMatching { patterns } => {
                let raw_lines: Vec<&str> = clean.lines().collect();
                let kept_indices: Vec<usize> = raw_lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| patterns.iter().any(|p| p.is_match(line)))
                    .map(|(i, _)| i)
                    .collect();
                if kept_indices.is_empty() {
                    return make_result(raw_output, clean, FilterConfidence::Fallback, vec![]);
                }
                let kept: Vec<&str> = kept_indices.iter().map(|&i| raw_lines[i]).collect();
                make_result(
                    raw_output,
                    kept.join("\n"),
                    FilterConfidence::Full,
                    kept_indices,
                )
            }
            CompiledStrategy::StripAnnotated {
                patterns,
                summary_pattern,
                long_output_threshold,
                keep_head,
                keep_tail,
            } => apply_strip_annotated(
                raw_output,
                patterns,
                summary_pattern.as_ref(),
                *long_output_threshold,
                *keep_head,
                *keep_tail,
                exit_code,
            ),
            CompiledStrategy::TestSummary {
                max_failures,
                truncate_stack_trace,
            } => apply_test_summary(raw_output, exit_code, *max_failures, *truncate_stack_trace),
            CompiledStrategy::GroupByRule {
                location_re,
                rule_re,
            } => apply_group_by_rule(raw_output, exit_code, location_re, rule_re),
            CompiledStrategy::GitStatus => apply_git_status(raw_output),
            CompiledStrategy::GitDiff { max_diff_lines } => {
                apply_git_diff(raw_output, *max_diff_lines)
            }
            CompiledStrategy::Dedup {
                normalize_patterns,
                max_unique_patterns,
            } => apply_dedup(raw_output, normalize_patterns, *max_unique_patterns),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load declarative filters from `config_dir/filters.toml`, falling back to
/// embedded defaults when the file is absent or `config_dir` is `None`.
pub(crate) fn load_declarative_filters(config_dir: Option<&Path>) -> Vec<Box<dyn OutputFilter>> {
    let file_content = if let Some(dir) = config_dir {
        let path = dir.join("filters.toml");
        let load_result = std::fs::metadata(&path)
            .map_err(|e| e.to_string())
            .and_then(|meta| {
                if meta.len() >= 1_048_576 {
                    Err(format!(
                        "filters.toml exceeds 1 MiB limit ({} bytes)",
                        meta.len()
                    ))
                } else {
                    std::fs::read_to_string(&path).map_err(|e| e.to_string())
                }
            });
        match load_result {
            Ok(content) => {
                tracing::debug!(path = %path.display(), "loaded user filters.toml");
                content
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "failed to load filters.toml: {e}");
                include_str!("default-filters.toml").to_owned()
            }
        }
    } else {
        include_str!("default-filters.toml").to_owned()
    };

    let parsed: DeclarativeFilterFile = match toml::from_str(&file_content) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("failed to parse filters.toml: {e}");
            return Vec::new();
        }
    };

    let mut filters: Vec<Box<dyn OutputFilter>> = Vec::new();
    for rule in parsed.rules {
        if !rule.enabled {
            continue;
        }
        let name = rule.name.clone();
        match DeclarativeFilter::compile(rule) {
            Ok(f) => filters.push(Box::new(f)),
            Err(e) => tracing::warn!("skipping rule '{name}': {e}"),
        }
    }
    filters
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
