// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use similar::{ChangeTag, TextDiff};
use zeph_core::config::migrate::ConfigMigrator;

/// Handle the `zeph migrate-config` command.
///
/// # Errors
///
/// Returns an error if the config file cannot be read, the migration fails, or the
/// in-place write fails.
pub(crate) fn handle_migrate_config(
    config_path: &Path,
    in_place: bool,
    diff: bool,
) -> anyhow::Result<()> {
    let input = if config_path.exists() {
        std::fs::read_to_string(config_path)?
    } else {
        String::new()
    };

    let migrator = ConfigMigrator::new();
    let result = migrator.migrate(&input)?;

    if diff {
        print_diff(&input, &result.output);
        eprintln!(
            "Migration would add {} entries ({} sections).",
            result.added_count,
            result.sections_added.len()
        );
    } else if in_place {
        atomic_write(config_path, &result.output)?;
        eprintln!(
            "Config migrated in-place: {} ({} entries added, sections: {})",
            config_path.display(),
            result.added_count,
            if result.sections_added.is_empty() {
                "none".to_owned()
            } else {
                result.sections_added.join(", ")
            }
        );
    } else {
        print!("{}", result.output);
    }

    Ok(())
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
