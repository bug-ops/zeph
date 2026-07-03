// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;
use std::path::Path;

use similar::{ChangeTag, TextDiff};
use zeph_core::config::migrate::{ConfigMigrator, MIGRATIONS, MigrationResult};

/// Aggregated totals from a `migrate-config` run, returned by [`handle_migrate_config`]
/// so callers (and tests) can observe the counts behind the printed summary line
/// without capturing stdout/stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MigrateSummary {
    /// Total entries added or renamed, summed across every named [`MIGRATIONS`] step
    /// plus the final `ConfigMigrator` catch-all pass.
    pub(crate) total_changed_count: usize,
    /// Number of distinct sections touched across every named step plus the catch-all
    /// pass (a section touched by both is counted once).
    pub(crate) total_sections_changed: usize,
}

/// Handle the `zeph migrate-config` command.
///
/// Applies all registered migration steps from [`MIGRATIONS`] in chronological order,
/// followed by the `ConfigMigrator` pass that adds missing keys as commented-out entries.
///
/// # Errors
///
/// Returns an error if the config file cannot be read, any migration step fails, or the
/// in-place write fails.
pub(crate) fn handle_migrate_config(
    config_path: &Path,
    in_place: bool,
    diff: bool,
) -> anyhow::Result<MigrateSummary> {
    let input = if config_path.exists() {
        std::fs::read_to_string(config_path)?
    } else {
        String::new()
    };

    // Apply all registered migration steps in order, collecting results for diff reporting.
    let mut current = input.clone();
    let mut step_results: Vec<(&str, MigrationResult)> = Vec::with_capacity(MIGRATIONS.len());
    for migration in MIGRATIONS.iter() {
        let result = migration.apply(&current)?;
        current.clone_from(&result.output);
        step_results.push((migration.name(), result));
    }

    // Final pass: add missing default keys as commented-out entries.
    let migrator = ConfigMigrator::new();
    let result = migrator.migrate(&current)?;

    // Aggregate totals across every named step plus the final catch-all pass, so the
    // summary line reflects all changes, not just the catch-all pass alone.
    let (total_changed_count, total_sections_changed) = aggregate_totals(&step_results, &result);
    let total_sections_len = total_sections_changed.len();

    if diff {
        print_diff(&input, &result.output);
        for (name, step_result) in &step_results {
            if step_result.changed_count > 0 {
                eprintln!(
                    "{}: {} change(s) (sections: {})",
                    name,
                    step_result.changed_count,
                    if step_result.sections_changed.is_empty() {
                        "none".to_owned()
                    } else {
                        step_result.sections_changed.join(", ")
                    }
                );
            }
        }
        eprintln!(
            "Migration would add {total_changed_count} entries ({total_sections_len} sections)."
        );
    } else if in_place {
        atomic_write(config_path, &result.output)?;
        eprintln!(
            "Config migrated in-place: {} ({} entries added, sections: {})",
            config_path.display(),
            total_changed_count,
            if total_sections_changed.is_empty() {
                "none".to_owned()
            } else {
                total_sections_changed
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    } else {
        print!("{}", result.output);
    }

    Ok(MigrateSummary {
        total_changed_count,
        total_sections_changed: total_sections_len,
    })
}

/// Sum `changed_count` and union `sections_changed` across every named migration step
/// and the final [`ConfigMigrator`] catch-all pass, so the printed summary reflects all
/// changes rather than only the catch-all pass. Sections touched by both a named step
/// and the catch-all pass are counted once.
fn aggregate_totals<'a>(
    step_results: &'a [(&str, MigrationResult)],
    final_result: &'a MigrationResult,
) -> (usize, BTreeSet<&'a str>) {
    let total_changed_count = step_results
        .iter()
        .map(|(_, r)| r.changed_count)
        .sum::<usize>()
        + final_result.changed_count;
    let total_sections_changed: BTreeSet<&str> = step_results
        .iter()
        .flat_map(|(_, r)| r.sections_changed.iter().map(String::as_str))
        .chain(final_result.sections_changed.iter().map(String::as_str))
        .collect();
    (total_changed_count, total_sections_changed)
}

/// Print a unified-style diff between `old` and `new`.
fn print_diff(old: &str, new: &str) {
    let diff = TextDiff::from_lines(old, new);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => print!(" {change}"),
            ChangeTag::Insert => print!("+{change}"),
            ChangeTag::Delete => print!("-{change}"),
        }
    }
}

/// Write `content` to `path` atomically using a temporary file in the same directory,
/// preserving the original file's permissions before renaming into place.
fn atomic_write(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::io::Write;

    let original_perms = if path.exists() {
        Some(std::fs::metadata(path)?.permissions())
    } else {
        None
    };

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;

    if let Some(perms) = original_perms {
        std::fs::set_permissions(tmp.path(), perms)?;
    }

    tmp.persist(path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::NamedTempFile;

    use super::*;

    fn result(changed_count: usize, sections: &[&str]) -> MigrationResult {
        MigrationResult {
            output: String::new(),
            changed_count,
            sections_changed: sections.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn aggregate_totals_sums_named_step_changes_when_catchall_is_a_no_op() {
        // Reproduces #5429: named migration steps make real changes but the final
        // `ConfigMigrator` catch-all pass finds nothing left to add.
        let step_results = vec![
            ("rename_embed_provider", result(2, &["memory"])),
            ("add_goals_section", result(1, &["goals"])),
        ];
        let final_result = result(0, &[]);

        let (total_changed, total_sections) = aggregate_totals(&step_results, &final_result);

        assert_eq!(total_changed, 3);
        assert_eq!(total_sections.len(), 2);
        assert!(total_sections.contains("memory"));
        assert!(total_sections.contains("goals"));
    }

    #[test]
    fn aggregate_totals_deduplicates_sections_touched_by_both_passes() {
        let step_results = vec![("rename_embed_provider", result(2, &["memory"]))];
        let final_result = result(1, &["memory", "goals"]);

        let (total_changed, total_sections) = aggregate_totals(&step_results, &final_result);

        // Entry count is a plain sum (each entry change is distinct)...
        assert_eq!(total_changed, 3);
        // ...but "memory" is counted once even though both passes touched it.
        assert_eq!(total_sections.len(), 2);
        assert!(total_sections.contains("memory"));
        assert!(total_sections.contains("goals"));
    }

    #[test]
    fn aggregate_totals_empty_when_nothing_changed() {
        let final_result = result(0, &[]);
        let (total_changed, total_sections) = aggregate_totals(&[], &final_result);

        assert_eq!(total_changed, 0);
        assert!(total_sections.is_empty());
    }

    #[test]
    fn handle_migrate_config_in_place_writes_migrated_output() {
        let mut file = NamedTempFile::new().expect("create temp config file");
        file.write_all(b"").expect("write empty config");
        let path = file.path().to_path_buf();

        let summary = handle_migrate_config(&path, true, false).expect("migration succeeds");

        assert!(summary.total_changed_count > 0);
        assert!(summary.total_sections_changed > 0);
        let migrated = std::fs::read_to_string(&path).expect("read migrated config");
        assert!(
            !migrated.is_empty(),
            "migration should add content to an empty config"
        );
    }

    #[test]
    fn handle_migrate_config_diff_mode_does_not_modify_the_file() {
        let mut file = NamedTempFile::new().expect("create temp config file");
        file.write_all(b"").expect("write empty config");
        let path = file.path().to_path_buf();

        let summary = handle_migrate_config(&path, false, true).expect("migration succeeds");

        assert!(summary.total_changed_count > 0);
        let unchanged = std::fs::read_to_string(&path).expect("read config after diff");
        assert!(unchanged.is_empty(), "diff mode must not write to disk");
    }

    #[test]
    fn handle_migrate_config_summary_includes_named_step_contributions() {
        // Pins #5429 at the `handle_migrate_config` API surface: a regression back to
        // reading `changed_count` from only the final `ConfigMigrator` catch-all pass
        // (discarding every named `MIGRATIONS` step's contribution) must fail this
        // assertion, because `catchall_only` below is exactly that buggy value.
        let raw = "[index]\nembed_provider = \"openai\"\n";
        let mut file = NamedTempFile::new().expect("create temp config file");
        file.write_all(raw.as_bytes()).expect("write config");
        let path = file.path().to_path_buf();

        // Replicate what the final catch-all pass alone would report once the named
        // steps (e.g. the `embed_provider` -> `embedding_provider` rename) have run —
        // this is the pre-fix value `handle_migrate_config` used to return.
        let mut current = raw.to_owned();
        for migration in MIGRATIONS.iter() {
            current = migration
                .apply(&current)
                .expect("named step applies")
                .output;
        }
        let catchall_only = ConfigMigrator::new()
            .migrate(&current)
            .expect("catch-all pass migrates");

        let summary = handle_migrate_config(&path, true, false).expect("migration succeeds");

        assert!(
            summary.total_changed_count > catchall_only.changed_count,
            "summary total ({}) must exceed the catch-all-only count ({}); named step \
             contributions (e.g. the embed_provider rename) must be included",
            summary.total_changed_count,
            catchall_only.changed_count,
        );
    }
}
