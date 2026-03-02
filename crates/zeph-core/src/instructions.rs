// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::config::ProviderKind;

/// Maximum size of a single instruction file. Files exceeding this limit are skipped.
const MAX_FILE_SIZE: u64 = 512 * 1024; // 512 KB

/// A loaded instruction block from a single file.
#[derive(Debug, Clone)]
pub struct InstructionBlock {
    /// Absolute path of the source file.
    pub source: PathBuf,
    /// UTF-8 text content of the file.
    pub content: String,
}

/// Load instruction blocks from provider-specific and explicit files.
///
/// `base_dir` is resolved as the process working directory at startup via
/// `std::env::current_dir()`. This matches the directory from which the user
/// launches `zeph` and is therefore the most natural project root for file
/// discovery. Non-git projects are fully supported; git root is not used.
///
/// Candidate paths are collected in this order:
/// 1. Always: `base_dir/zeph.md` and `base_dir/.zeph/zeph.md`.
/// 2. If `auto_detect`, per-provider paths from `detection_paths()` for each kind.
/// 3. `explicit_files` as provided (trusted — user controls config.toml).
///
/// Deduplication uses `fs::canonicalize`. Paths that do not exist are silently
/// skipped; canonicalize fails on nonexistent paths, so they cannot be deduped
/// via symlinks against existing paths — this is an acceptable edge case documented here.
pub fn load_instructions(
    base_dir: &Path,
    provider_kinds: &[ProviderKind],
    explicit_files: &[PathBuf],
    auto_detect: bool,
) -> Vec<InstructionBlock> {
    let canonical_base = match std::fs::canonicalize(base_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %base_dir.display(), error = %e, "failed to canonicalize base_dir, skipping all instruction files");
            return Vec::new();
        }
    };

    let mut candidates: Vec<PathBuf> = Vec::new();

    // zeph.md is always checked regardless of provider or auto_detect setting.
    candidates.push(base_dir.join("zeph.md"));
    candidates.push(base_dir.join(".zeph").join("zeph.md"));

    if auto_detect {
        for &kind in provider_kinds {
            candidates.extend(detection_paths(kind, base_dir));
        }
    }

    // Explicit files are trusted (user controls config). Resolve relative to base_dir.
    for p in explicit_files {
        if p.is_absolute() {
            candidates.push(p.clone());
        } else {
            candidates.push(base_dir.join(p));
        }
    }

    // Deduplicate by canonical path. Only existing paths can be canonicalized.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut result: Vec<InstructionBlock> = Vec::new();

    for path in candidates {
        let Ok(file) = std::fs::File::open(&path) else {
            // Nonexistent or unreadable paths are silently skipped.
            continue;
        };

        let canonical = match std::fs::canonicalize(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to canonicalize instruction file path, skipping");
                continue;
            }
        };

        if !canonical.starts_with(&canonical_base) {
            tracing::warn!(path = %canonical.display(), "instruction file escapes project root, skipping");
            continue;
        }

        if !seen.insert(canonical) {
            // Already loaded this path via a different candidate or symlink.
            continue;
        }

        let meta = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read instruction file metadata, skipping");
                continue;
            }
        };

        if !meta.is_file() {
            continue;
        }

        if meta.len() > MAX_FILE_SIZE {
            tracing::warn!(
                path = %path.display(),
                size = meta.len(),
                limit = MAX_FILE_SIZE,
                "instruction file exceeds 512 KB size limit, skipping"
            );
            continue;
        }

        let mut content = String::new();
        match std::io::BufReader::new(file).read_to_string(&mut content) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read instruction file, skipping");
                continue;
            }
        }

        if content.contains('\0') {
            tracing::warn!(path = %path.display(), "instruction file contains null bytes, skipping");
            continue;
        }

        if content.is_empty() {
            tracing::debug!(path = %path.display(), "instruction file is empty, skipping");
            continue;
        }

        tracing::debug!(path = %path.display(), bytes = content.len(), "loaded instruction file");
        result.push(InstructionBlock {
            source: path,
            content,
        });
    }

    result
}

/// Returns candidate file paths for a given provider.
///
/// Uses an exhaustive match — adding a new `ProviderKind` variant will cause
/// a compile error here, forcing the developer to update the detection table.
fn detection_paths(kind: ProviderKind, base: &Path) -> Vec<PathBuf> {
    match kind {
        ProviderKind::Claude => {
            let mut paths = vec![
                base.join("CLAUDE.md"),
                base.join(".claude").join("CLAUDE.md"),
            ];
            // Collect .claude/rules/*.md sorted by name for deterministic order.
            let rules_dir = base.join(".claude").join("rules");
            if let Ok(entries) = std::fs::read_dir(&rules_dir) {
                let mut rule_files: Vec<PathBuf> = entries
                    .filter_map(std::result::Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
                    .collect();
                rule_files.sort();
                paths.extend(rule_files);
            }
            paths
        }
        ProviderKind::OpenAi => {
            vec![base.join("AGENTS.override.md"), base.join("AGENTS.md")]
        }
        ProviderKind::Compatible | ProviderKind::Ollama | ProviderKind::Candle => {
            vec![base.join("AGENTS.md")]
        }
        // Router and Orchestrator delegate to their sub-providers; detection
        // is handled by the caller collecting sub-provider kinds separately.
        ProviderKind::Router | ProviderKind::Orchestrator => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn zeph_md_loaded_even_when_auto_detect_disabled() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "zeph.md", "some content");
        let blocks = load_instructions(dir.path(), &[], &[], false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "some content");
    }

    #[test]
    fn empty_when_no_auto_detect_and_no_explicit_and_no_zeph_md() {
        let dir = TempDir::new().unwrap();
        let blocks = load_instructions(dir.path(), &[], &[], false);
        assert!(blocks.is_empty());
    }

    #[test]
    fn finds_zeph_md_in_base_dir() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "zeph.md", "zeph instructions");
        let blocks = load_instructions(dir.path(), &[], &[], true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "zeph instructions");
    }

    #[test]
    fn finds_dot_zeph_zeph_md() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), ".zeph/zeph.md", "nested zeph instructions");
        let blocks = load_instructions(dir.path(), &[], &[], true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "nested zeph instructions");
    }

    #[test]
    fn detection_paths_claude() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "CLAUDE.md", "# Claude");
        make_file(dir.path(), ".claude/CLAUDE.md", "# Dot Claude");
        make_file(dir.path(), ".claude/rules/a.md", "rule a");
        make_file(dir.path(), ".claude/rules/b.md", "rule b");

        let blocks = load_instructions(dir.path(), &[ProviderKind::Claude], &[], true);
        let sources: Vec<_> = blocks
            .iter()
            .map(|b| b.source.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(sources.contains(&"CLAUDE.md"));
        assert!(sources.contains(&"a.md"));
        assert!(sources.contains(&"b.md"));
    }

    #[test]
    fn detection_paths_openai() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "AGENTS.md", "# Agents");

        let paths = detection_paths(ProviderKind::OpenAi, dir.path());
        assert!(paths.iter().any(|p| p.file_name().unwrap() == "AGENTS.md"));
        assert!(
            paths
                .iter()
                .any(|p| p.file_name().unwrap() == "AGENTS.override.md")
        );
    }

    #[test]
    fn detection_paths_ollama_and_compatible_and_candle() {
        let dir = TempDir::new().unwrap();
        for kind in [
            ProviderKind::Ollama,
            ProviderKind::Compatible,
            ProviderKind::Candle,
        ] {
            let paths = detection_paths(kind, dir.path());
            assert_eq!(paths.len(), 1);
            assert_eq!(paths[0].file_name().unwrap(), "AGENTS.md");
        }
    }

    #[test]
    fn detection_paths_router_and_orchestrator_empty() {
        let dir = TempDir::new().unwrap();
        assert!(detection_paths(ProviderKind::Router, dir.path()).is_empty());
        assert!(detection_paths(ProviderKind::Orchestrator, dir.path()).is_empty());
    }

    #[test]
    fn deduplication_by_canonical_path() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "AGENTS.md", "content");

        // Both Ollama and Compatible resolve to AGENTS.md — should appear once.
        let blocks = load_instructions(
            dir.path(),
            &[ProviderKind::Ollama, ProviderKind::Compatible],
            &[],
            true,
        );
        let agents_count = blocks
            .iter()
            .filter(|b| b.source.file_name().unwrap() == "AGENTS.md")
            .count();
        assert_eq!(agents_count, 1);
    }

    #[test]
    fn skips_files_exceeding_size_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.md");
        // Write slightly more than 512 KB.
        let big = vec![b'x'; 513 * 1024];
        fs::write(&path, &big).unwrap();
        let blocks = load_instructions(dir.path(), &[], &[path], false);
        assert!(blocks.is_empty());
    }

    #[test]
    fn skips_empty_files() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "zeph.md", "");
        let blocks = load_instructions(dir.path(), &[], &[], true);
        assert!(blocks.is_empty());
    }

    #[test]
    fn nonexistent_paths_are_silently_skipped() {
        let dir = TempDir::new().unwrap();
        let nonexistent = dir.path().join("does_not_exist.md");
        let blocks = load_instructions(dir.path(), &[], &[nonexistent], false);
        assert!(blocks.is_empty());
    }

    #[test]
    fn explicit_relative_path_resolved_against_base_dir() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "custom.md", "custom content");
        let blocks = load_instructions(dir.path(), &[], &[PathBuf::from("custom.md")], false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "custom content");
    }

    #[test]
    fn invalid_utf8_file_is_skipped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.md");
        // Write bytes that are not valid UTF-8.
        fs::write(&path, b"\xff\xfe invalid utf8 \x80\x81").unwrap();
        let blocks = load_instructions(dir.path(), &[], &[path], false);
        assert!(blocks.is_empty());
    }

    #[test]
    fn multiple_providers_union_without_overlap() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "CLAUDE.md", "claude content");
        make_file(dir.path(), "AGENTS.md", "agents content");

        let blocks = load_instructions(
            dir.path(),
            &[ProviderKind::Claude, ProviderKind::OpenAi],
            &[],
            true,
        );
        let names: Vec<_> = blocks
            .iter()
            .map(|b| b.source.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"CLAUDE.md"), "Claude file missing");
        assert!(names.contains(&"AGENTS.md"), "OpenAI file missing");
    }

    #[test]
    fn zeph_md_always_loaded_with_provider_auto_detect() {
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "zeph.md", "zeph rules");
        // OpenAI provider has no AGENTS.md present, only zeph.md.
        let blocks = load_instructions(dir.path(), &[ProviderKind::OpenAi], &[], true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "zeph rules");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_deduplication() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        make_file(dir.path(), "CLAUDE.md", "claude content");
        symlink(
            dir.path().join("CLAUDE.md"),
            dir.path().join("CLAUDE_link.md"),
        )
        .unwrap();

        // Load the original and the symlink — should appear only once after dedup.
        let blocks = load_instructions(
            dir.path(),
            &[ProviderKind::Claude],
            &[PathBuf::from("CLAUDE_link.md")],
            true,
        );
        let claude_count = blocks
            .iter()
            .filter(|b| b.content == "claude content")
            .count();
        assert_eq!(claude_count, 1, "symlink should be deduped with original");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_project_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        let inside = TempDir::new().unwrap();
        make_file(outside.path(), "secret.md", "secret content");

        // Create a symlink inside the project dir pointing outside.
        let link = inside.path().join("evil.md");
        symlink(outside.path().join("secret.md"), &link).unwrap();

        let blocks = load_instructions(inside.path(), &[], &[link], false);
        assert!(
            blocks.is_empty(),
            "file escaping project root must be rejected"
        );
    }

    #[test]
    fn file_with_null_bytes_is_skipped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("null.md");
        fs::write(&path, b"content\x00more").unwrap();
        let blocks = load_instructions(dir.path(), &[], &[path], false);
        assert!(blocks.is_empty());
    }
}
