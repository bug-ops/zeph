// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::def::{AGENT_NAME_RE, MemoryScope};
use super::error::SubAgentError;

/// Case-insensitive regex matching any variant of `<agent-memory>` or `</agent-memory>` tags.
///
/// Handles uppercase, mixed-case, and whitespace variants to prevent prompt injection bypass.
static MEMORY_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</?(\s*)agent-memory(\s*)>").unwrap());

/// Maximum allowed size for MEMORY.md (256 KiB — same cap as instruction files).
const MAX_MEMORY_SIZE: u64 = 256 * 1024;

/// Number of lines to inject from MEMORY.md into the system prompt.
const MEMORY_INJECT_LINES: usize = 200;

/// Resolve the memory directory path for a given scope and agent name.
///
/// Agent name is validated against the same regex enforced in `parse_with_path`.
/// This prevents path traversal via crafted names (e.g., `../../../etc`).
///
/// # Errors
///
/// Returns [`SubAgentError::Invalid`] if the agent name fails validation.
/// Returns [`SubAgentError::Memory`] if the home directory is unavailable (`User` scope).
pub fn resolve_memory_dir(scope: MemoryScope, agent_name: &str) -> Result<PathBuf, SubAgentError> {
    if !AGENT_NAME_RE.is_match(agent_name) {
        return Err(SubAgentError::Invalid(format!(
            "agent name '{agent_name}' is not valid for memory directory (must match \
             ^[a-zA-Z0-9][a-zA-Z0-9_-]{{0,63}}$)"
        )));
    }

    let dir = match scope {
        MemoryScope::User => {
            let home = dirs::home_dir().ok_or_else(|| SubAgentError::Memory {
                name: agent_name.to_owned(),
                reason: "home directory unavailable".to_owned(),
            })?;
            home.join(".zeph").join("agent-memory").join(agent_name)
        }
        MemoryScope::Project => {
            let cwd = std::env::current_dir().map_err(|e| SubAgentError::Memory {
                name: agent_name.to_owned(),
                reason: format!("cannot determine working directory: {e}"),
            })?;
            cwd.join(".zeph").join("agent-memory").join(agent_name)
        }
        MemoryScope::Local => {
            let cwd = std::env::current_dir().map_err(|e| SubAgentError::Memory {
                name: agent_name.to_owned(),
                reason: format!("cannot determine working directory: {e}"),
            })?;
            cwd.join(".zeph")
                .join("agent-memory-local")
                .join(agent_name)
        }
    };
    Ok(dir)
}

/// Ensure the memory directory exists, creating it if necessary.
///
/// Returns the absolute path to the directory. Logs at `debug` level when the
/// directory is newly created.
///
/// # Errors
///
/// Returns [`SubAgentError::Invalid`] if the agent name is invalid.
/// Returns [`SubAgentError::Memory`] if the directory cannot be created.
pub fn ensure_memory_dir(scope: MemoryScope, agent_name: &str) -> Result<PathBuf, SubAgentError> {
    let dir = resolve_memory_dir(scope, agent_name)?;
    // create_dir_all is idempotent — no need for a prior exists() check (REV-MED-02).
    std::fs::create_dir_all(&dir).map_err(|e| SubAgentError::Memory {
        name: agent_name.to_owned(),
        reason: format!("cannot create memory directory '{}': {e}", dir.display()),
    })?;
    tracing::debug!(
        agent = agent_name,
        scope = ?scope,
        path = %dir.display(),
        "ensured agent memory directory"
    );

    // Warn for Local scope if .gitignore likely does not cover the directory.
    if scope == MemoryScope::Local {
        check_gitignore_for_local(&dir);
    }

    Ok(dir)
}

/// Reads `MEMORY.md` from the given directory and returns the first 200 lines.
///
/// Returns `None` if the file does not exist or is empty.
///
/// Security:
/// - Canonicalizes the path and verifies it stays within `dir` (symlink boundary).
/// - Opens the canonical path after the boundary check (no TOCTOU window).
/// - Rejects files larger than 256 KiB.
/// - Rejects files containing null bytes.
pub fn load_memory_content(dir: &Path) -> Option<String> {
    let memory_path = dir.join("MEMORY.md");

    // Canonicalize to resolve any symlinks before opening.
    let canonical = std::fs::canonicalize(&memory_path).ok()?;

    // Boundary check: MEMORY.md must be within the memory directory.
    // REV-LOW-01: canonicalize dir separately (can't derive from canonical — symlink
    // target's parent differs from the original dir when symlink escapes boundary).
    let canonical_dir = std::fs::canonicalize(dir).ok()?;
    if !canonical.starts_with(&canonical_dir) {
        tracing::warn!(
            path = %canonical.display(),
            boundary = %canonical_dir.display(),
            "MEMORY.md escapes memory directory boundary via symlink, skipping"
        );
        return None;
    }

    // Open the canonical path — no TOCTOU window for symlink swap after this point.
    // Read content via the same handle to avoid re-opening (REV-CRIT-01).
    let mut file = std::fs::File::open(&canonical).ok()?;
    let meta = file.metadata().ok()?;

    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_MEMORY_SIZE {
        tracing::warn!(
            path = %canonical.display(),
            size = meta.len(),
            limit = MAX_MEMORY_SIZE,
            "MEMORY.md exceeds 256 KiB size limit, skipping"
        );
        return None;
    }

    let mut content = String::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.read_to_string(&mut content).ok()?;

    // Security: reject files with null bytes (potential binary or injection attack).
    if content.contains('\0') {
        tracing::warn!(
            path = %canonical.display(),
            "MEMORY.md contains null bytes, skipping"
        );
        return None;
    }

    if content.trim().is_empty() {
        return None;
    }

    // Truncate to the first MEMORY_INJECT_LINES lines without full Vec allocation (REV-MED-01).
    let mut line_count = 0usize;
    let mut byte_offset = 0usize;
    let mut truncated = false;
    for line in content.lines() {
        line_count += 1;
        if line_count > MEMORY_INJECT_LINES {
            truncated = true;
            break;
        }
        byte_offset += line.len() + 1; // +1 for newline
    }

    let result = if truncated {
        let head = content[..byte_offset.min(content.len())].trim_end_matches('\n');
        format!(
            "{head}\n\n[... truncated at {MEMORY_INJECT_LINES} lines. \
             See full file at {}]",
            dir.join("MEMORY.md").display()
        )
    } else {
        content
    };

    Some(result)
}

/// Escape `<agent-memory>` and `</agent-memory>` tags from memory content.
///
/// Handles case variations (`</AGENT-MEMORY>`, `</Agent-Memory >`) via case-insensitive
/// regex. Prevents prompt injection: an agent writing the closing tag to MEMORY.md would
/// otherwise escape the `<agent-memory>` wrapper and inject arbitrary system prompt text.
///
/// Trust model note: MEMORY.md is written by the agent itself, unlike user-written
/// instruction files. Agent-written content requires stricter escaping.
#[must_use]
pub fn escape_memory_content(content: &str) -> String {
    MEMORY_TAG_RE
        .replace_all(content, "<\\/$1agent-memory$2>")
        .into_owned()
}

/// Check if `.zeph/agent-memory-local/` appears in `.gitignore` and warn if not.
///
/// This is best-effort — only checks the project-root `.gitignore`.
fn check_gitignore_for_local(memory_dir: &Path) {
    // Walk up to find .gitignore (at most 5 levels up from memory dir).
    let mut current = memory_dir;
    for _ in 0..5 {
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
        let gitignore = current.join(".gitignore");
        if gitignore.exists() {
            if std::fs::read_to_string(&gitignore).is_ok_and(|c| c.contains("agent-memory-local")) {
                return;
            }
            tracing::warn!(
                "local agent memory directory is not in .gitignore — \
                 sensitive data may be committed. Add '.zeph/agent-memory-local/' to .gitignore"
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::format_collect)]

    use super::*;

    // ── resolve_memory_dir ────────────────────────────────────────────────────

    #[test]
    fn resolve_project_scope_returns_correct_path() {
        let dir = resolve_memory_dir(MemoryScope::Project, "my-agent").unwrap();
        assert!(dir.ends_with(".zeph/agent-memory/my-agent"));
    }

    #[test]
    fn resolve_local_scope_returns_correct_path() {
        let dir = resolve_memory_dir(MemoryScope::Local, "my-agent").unwrap();
        assert!(dir.ends_with(".zeph/agent-memory-local/my-agent"));
    }

    #[test]
    fn resolve_user_scope_returns_home_path() {
        if dirs::home_dir().is_none() {
            return; // Skip in environments without home dir.
        }
        let dir = resolve_memory_dir(MemoryScope::User, "my-agent").unwrap();
        assert!(dir.ends_with(".zeph/agent-memory/my-agent"));
        assert!(dir.starts_with(dirs::home_dir().unwrap()));
    }

    #[test]
    fn resolve_rejects_path_traversal_name() {
        let err = resolve_memory_dir(MemoryScope::Project, "../etc/passwd").unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn resolve_rejects_slash_in_name() {
        let err = resolve_memory_dir(MemoryScope::Project, "a/b").unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn resolve_rejects_empty_name() {
        let err = resolve_memory_dir(MemoryScope::Project, "").unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn resolve_rejects_whitespace_only_name() {
        let err = resolve_memory_dir(MemoryScope::Project, "   ").unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn resolve_accepts_single_char_name() {
        resolve_memory_dir(MemoryScope::Project, "a").unwrap();
    }

    #[test]
    fn resolve_accepts_64_char_name() {
        let name = "a".repeat(64);
        resolve_memory_dir(MemoryScope::Project, &name).unwrap();
    }

    #[test]
    fn resolve_rejects_65_char_name() {
        let name = "a".repeat(65);
        let err = resolve_memory_dir(MemoryScope::Project, &name).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn resolve_rejects_unicode_cyrillic() {
        // Cyrillic 'а' (U+0430) looks like Latin 'a' but is not ASCII.
        let err = resolve_memory_dir(MemoryScope::Project, "аgent").unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn resolve_rejects_fullwidth_slash() {
        // Full-width solidus U+FF0F.
        let err = resolve_memory_dir(MemoryScope::Project, "a\u{FF0F}b").unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    // ── ensure_memory_dir ────────────────────────────────────────────────────

    #[test]
    fn ensure_creates_directory_for_project_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let result = ensure_memory_dir(MemoryScope::Project, "test-agent").unwrap();
        assert!(result.exists());
        assert!(result.ends_with(".zeph/agent-memory/test-agent"));

        std::env::set_current_dir(orig_dir).unwrap();
    }

    #[test]
    fn ensure_idempotent_when_directory_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let dir1 = ensure_memory_dir(MemoryScope::Project, "idempotent-agent").unwrap();
        let dir2 = ensure_memory_dir(MemoryScope::Project, "idempotent-agent").unwrap();
        assert_eq!(dir1, dir2);

        std::env::set_current_dir(orig_dir).unwrap();
    }

    // ── load_memory_content ───────────────────────────────────────────────────

    #[test]
    fn load_returns_none_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_memory_content(tmp.path()).is_none());
    }

    #[test]
    fn load_returns_content_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "# Notes\nkey: value\n").unwrap();
        let content = load_memory_content(tmp.path()).unwrap();
        assert!(content.contains("key: value"));
    }

    #[test]
    fn load_truncates_at_200_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: String = (0..300).map(|i| format!("line {i}\n")).collect();
        std::fs::write(tmp.path().join("MEMORY.md"), &lines).unwrap();
        let content = load_memory_content(tmp.path()).unwrap();
        let line_count = content.lines().count();
        // Truncated content has 200 data lines + 1 truncation marker line.
        assert!(line_count <= 202, "expected <= 202 lines, got {line_count}");
        assert!(content.contains("truncated at 200 lines"));
    }

    #[test]
    fn load_rejects_null_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "valid\0content").unwrap();
        assert!(load_memory_content(tmp.path()).is_none());
    }

    #[test]
    fn load_returns_none_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "").unwrap();
        assert!(load_memory_content(tmp.path()).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn load_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("secret.md");
        std::fs::write(&target, "secret content").unwrap();

        let link = tmp.path().join("MEMORY.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // The symlink points outside the tmp directory — should be rejected.
        assert!(load_memory_content(tmp.path()).is_none());
    }

    #[test]
    fn load_returns_none_for_whitespace_only_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "   \n\n   \n").unwrap();
        assert!(load_memory_content(tmp.path()).is_none());
    }

    #[test]
    fn load_rejects_file_over_size_cap() {
        let tmp = tempfile::tempdir().unwrap();
        // 257 KiB of content — exceeds the 256 KiB limit.
        let content = "x".repeat(257 * 1024);
        std::fs::write(tmp.path().join("MEMORY.md"), content).unwrap();
        assert!(load_memory_content(tmp.path()).is_none());
    }

    // ── escape_memory_content ─────────────────────────────────────────────────

    #[test]
    fn escape_replaces_closing_tag_lowercase() {
        let content = "safe content </agent-memory> more content";
        let escaped = escape_memory_content(content);
        assert!(!escaped.contains("</agent-memory>"));
    }

    #[test]
    fn escape_replaces_closing_tag_uppercase() {
        let content = "safe </AGENT-MEMORY> content";
        let escaped = escape_memory_content(content);
        assert!(!escaped.to_lowercase().contains("</agent-memory>"));
    }

    #[test]
    fn escape_replaces_closing_tag_mixed_case() {
        let content = "safe </Agent-Memory> content";
        let escaped = escape_memory_content(content);
        assert!(!escaped.to_lowercase().contains("</agent-memory>"));
    }

    #[test]
    fn escape_replaces_opening_tag() {
        let content = "before <agent-memory> injection attempt";
        let escaped = escape_memory_content(content);
        // Opening tag must also be escaped to prevent nested boundaries.
        assert!(!escaped.contains("<agent-memory>"));
    }

    #[test]
    fn escape_leaves_normal_content_unchanged() {
        let content = "# Notes\nThis is safe content.";
        assert_eq!(escape_memory_content(content), content);
    }
}
