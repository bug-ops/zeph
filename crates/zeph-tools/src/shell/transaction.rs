// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use globset::GlobMatcher;

/// Summary of a completed rollback operation.
#[derive(Debug)]
pub(crate) struct RollbackReport {
    pub restored_count: usize,
    pub deleted_count: usize,
}

/// Tracks whether a snapshotted path existed before capture.
#[derive(Debug)]
enum EntryKind {
    /// File existed; backup copy is at `backup_path`.
    Existing { backup_path: PathBuf },
    /// File did not exist; rollback should delete it if present.
    New,
}

#[derive(Debug)]
struct SnapshotEntry {
    original: PathBuf,
    kind: EntryKind,
}

/// File-level snapshot for transactional rollback.
///
/// Holds copies of files captured before a write command executes.
/// On success the snapshot is simply dropped (`TempDir` auto-cleans).
/// On failure call `rollback()` to restore originals.
pub(crate) struct TransactionSnapshot {
    // Kept for its Drop impl which deletes the temp directory on success path.
    #[allow(dead_code)]
    backup_dir: tempfile::TempDir,
    entries: Vec<SnapshotEntry>,
}

impl std::fmt::Debug for TransactionSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionSnapshot")
            .field("entry_count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl TransactionSnapshot {
    /// Capture files at `paths`.
    ///
    /// Non-existent paths are recorded as "new" (rollback will delete them if created).
    /// If `max_bytes > 0` and the cumulative size of copied files exceeds `max_bytes`,
    /// an error is returned immediately.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the temp directory, any file copy fails, or the
    /// snapshot size limit is exceeded.
    pub(crate) fn capture(paths: &[PathBuf], max_bytes: u64) -> Result<Self, std::io::Error> {
        let backup_dir = tempfile::TempDir::new()?;
        let mut entries = Vec::with_capacity(paths.len());
        let mut cumulative_bytes: u64 = 0;

        for (i, original) in paths.iter().enumerate() {
            // Use symlink_metadata to avoid following symlinks — snapshot only regular files.
            match original.symlink_metadata() {
                Err(_) => {
                    // Path does not exist; record as "new" so rollback can delete it if created.
                    entries.push(SnapshotEntry {
                        original: original.clone(),
                        kind: EntryKind::New,
                    });
                    continue;
                }
                Ok(meta) if meta.file_type().is_symlink() => {
                    tracing::debug!(
                        path = %original.display(),
                        "transaction snapshot: skipping symlink"
                    );
                    continue;
                }
                Ok(_) => {}
            }

            // Mirror the relative structure inside the backup dir using an index prefix
            // to avoid collisions from files with the same name in different directories.
            let backup_path = backup_dir
                .path()
                .join(format!("{i}_{}", file_name(original)));
            std::fs::copy(original, &backup_path)?;

            // Preserve permissions
            let meta = std::fs::metadata(original)?;
            std::fs::set_permissions(&backup_path, meta.permissions())?;

            cumulative_bytes += meta.len();
            if max_bytes > 0 && cumulative_bytes > max_bytes {
                return Err(std::io::Error::other(format!(
                    "snapshot size {cumulative_bytes} exceeds limit {max_bytes}"
                )));
            }

            entries.push(SnapshotEntry {
                original: original.clone(),
                kind: EntryKind::Existing { backup_path },
            });
        }

        Ok(Self {
            backup_dir,
            entries,
        })
    }

    /// Number of files captured (existing + new).
    pub(crate) fn file_count(&self) -> usize {
        self.entries.len()
    }

    /// Total bytes stored in backup copies (new-file entries contribute 0).
    pub(crate) fn total_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter_map(|e| {
                if let EntryKind::Existing { backup_path } = &e.kind {
                    std::fs::metadata(backup_path).map(|m| m.len()).ok()
                } else {
                    None
                }
            })
            .sum()
    }

    /// Restore all captured files to their original locations.
    ///
    /// All entries are attempted even if individual restores fail.
    /// Files that did not exist before the snapshot are deleted if they now exist.
    ///
    /// # Errors
    ///
    /// Returns the first `std::io::Error` encountered after attempting all restores.
    pub(crate) fn rollback(self) -> Result<RollbackReport, std::io::Error> {
        let mut restored_count = 0usize;
        let mut deleted_count = 0usize;
        let mut first_error: Option<std::io::Error> = None;

        for entry in &self.entries {
            let result = match &entry.kind {
                EntryKind::Existing { backup_path } => {
                    let dir_result = entry
                        .original
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all);
                    dir_result
                        .and_then(|()| std::fs::copy(backup_path, &entry.original).map(|_| ()))
                }
                EntryKind::New => {
                    if entry.original.exists() {
                        std::fs::remove_file(&entry.original)
                    } else {
                        Ok(())
                    }
                }
            };
            match result {
                Ok(()) => match &entry.kind {
                    EntryKind::Existing { .. } => restored_count += 1,
                    EntryKind::New => {
                        if !entry.original.exists() {
                            deleted_count += 1;
                        }
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %entry.original.display(),
                        err = %e,
                        "rollback: failed to restore entry, continuing"
                    );
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        // backup_dir is dropped here; TempDir auto-cleans.
        if let Some(e) = first_error {
            Err(e)
        } else {
            Ok(RollbackReport {
                restored_count,
                deleted_count,
            })
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned())
}

// Shell write indicators: if the command contains any of these tokens we assume
// it may write to the filesystem and a snapshot should be taken.
// False positives are acceptable (cheap snapshot); false negatives are the risk.
const WRITE_INDICATORS: &[&str] = &[
    ">",
    ">>",
    "tee ",
    "mv ",
    "cp ",
    "rm ",
    "mkdir ",
    "touch ",
    "sed -i",
    "chmod ",
    "chown ",
    "git checkout",
    "cargo fmt",
    "patch ",
];

/// Returns true if `command` likely performs a write operation.
pub(crate) fn is_write_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    WRITE_INDICATORS.iter().any(|ind| lower.contains(ind))
}

/// Extract file paths that are targets of shell redirections.
///
/// Handles: `>`, `>>`, `2>`, `2>>`, `&>`, `&>>`
pub(crate) fn extract_redirection_targets(command: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        let is_redir = matches!(tok, ">" | ">>" | "2>" | "2>>" | "&>" | "&>>");
        // Also handle redirections glued to the next token: e.g. "2>/dev/null"
        let is_glued_redir = tok.starts_with(">>")
            || tok.starts_with("2>>")
            || tok.starts_with("&>>")
            || tok.starts_with("2>")
            || tok.starts_with("&>")
            || (tok.starts_with('>') && tok.len() > 1 && !tok.starts_with(">>"));

        if is_redir {
            if let Some(next) = tokens.get(i + 1) {
                if !next.starts_with('-') {
                    targets.push((*next).to_owned());
                }
                i += 2;
                continue;
            }
        } else if is_glued_redir {
            // Extract the path portion after the operator characters
            let path_part = tok
                .trim_start_matches("&>>")
                .trim_start_matches("2>>")
                .trim_start_matches("&>")
                .trim_start_matches("2>")
                .trim_start_matches(">>")
                .trim_start_matches('>');
            if !path_part.is_empty() && !path_part.starts_with('-') {
                targets.push(path_part.to_owned());
            }
        }
        i += 1;
    }
    targets
}

/// Combine `extract_paths()` and `extract_redirection_targets()`, deduplicate,
/// then filter through `scope` glob matchers.
///
/// If `scope` is empty, all extracted paths are eligible.
pub(crate) fn affected_paths(command: &str, scope: &[GlobMatcher]) -> Vec<PathBuf> {
    let mut raw: Vec<String> = super::extract_paths(command);
    raw.extend(extract_redirection_targets(command));
    raw.sort_unstable();
    raw.dedup();

    raw.into_iter()
        .map(PathBuf::from)
        .filter(|p| scope.is_empty() || scope.iter().any(|m| m.is_match(p)))
        .collect()
}

/// Build `GlobMatcher` list from pattern strings.
///
/// Patterns that fail to compile are silently skipped with a warning.
pub(crate) fn build_scope_matchers(patterns: &[String]) -> Vec<GlobMatcher> {
    patterns
        .iter()
        .filter_map(|pat| {
            globset::Glob::new(pat)
                .map(|g| g.compile_matcher())
                .map_err(
                    |e| tracing::warn!(pattern = %pat, err = %e, "invalid transaction_scope glob"),
                )
                .ok()
        })
        .collect()
}
