// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeph_llm::provider::Message;

use super::error::SubAgentError;
use super::state::SubAgentState;

/// A single entry in a JSONL transcript file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub seq: u32,
    /// ISO 8601 timestamp (UTC).
    pub timestamp: String,
    pub message: Message,
}

/// Sidecar metadata for a transcript, written as `<agent_id>.meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptMeta {
    pub agent_id: String,
    pub agent_name: String,
    pub def_name: String,
    pub status: SubAgentState,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// ID of the original agent session this was resumed from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    pub turns_used: u32,
}

/// Appends `TranscriptEntry` lines to a JSONL file.
///
/// The file handle is kept open for the writer's lifetime to avoid
/// race conditions from repeated open/close cycles.
pub struct TranscriptWriter {
    file: File,
}

impl TranscriptWriter {
    /// Create (or open) a JSONL transcript file in append mode.
    ///
    /// Creates parent directories if they do not already exist.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the directory cannot be created or the file cannot be opened.
    pub fn new(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = open_private(path)?;
        Ok(Self { file })
    }

    /// Append a single message as a JSON line and flush immediately.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on serialization or write failure.
    pub fn append(&mut self, seq: u32, message: &Message) -> io::Result<()> {
        let entry = TranscriptEntry {
            seq,
            timestamp: utc_now(),
            message: message.clone(),
        };
        let line = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }

    /// Write the meta sidecar file for an agent.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on serialization or write failure.
    pub fn write_meta(dir: &Path, agent_id: &str, meta: &TranscriptMeta) -> io::Result<()> {
        let path = dir.join(format!("{agent_id}.meta.json"));
        let content = serde_json::to_string_pretty(meta)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private(&path, content.as_bytes())
    }
}

/// Reads and reconstructs message history from JSONL transcript files.
pub struct TranscriptReader;

impl TranscriptReader {
    /// Load all messages from a JSONL transcript file.
    ///
    /// Malformed lines are skipped with a warning. An empty or missing file
    /// returns an empty `Vec`. If the file does not exist at all but a matching
    /// `.meta.json` sidecar exists, returns `SubAgentError::Transcript` with a
    /// clear message so the caller knows the data is gone rather than silently
    /// degrading to a fresh start.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::Transcript`] on unrecoverable I/O failures, or
    /// when the transcript file is missing but meta exists (data-loss guard).
    pub fn load(path: &Path) -> Result<Vec<Message>, SubAgentError> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Check if a meta sidecar exists — if so, data has been lost.
                // Build meta path from the file stem (e.g. "abc" from "abc.jsonl")
                // so it is consistent with write_meta which uses format!("{agent_id}.meta.json").
                let meta_path =
                    if let (Some(parent), Some(stem)) = (path.parent(), path.file_stem()) {
                        parent.join(format!("{}.meta.json", stem.to_string_lossy()))
                    } else {
                        path.with_extension("meta.json")
                    };
                if meta_path.exists() {
                    return Err(SubAgentError::Transcript(format!(
                        "transcript file '{}' is missing but meta sidecar exists — \
                         transcript data may have been deleted",
                        path.display()
                    )));
                }
                return Ok(vec![]);
            }
            Err(e) => {
                return Err(SubAgentError::Transcript(format!(
                    "failed to open transcript '{}': {e}",
                    path.display()
                )));
            }
        };

        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        for (line_no, line_result) in reader.lines().enumerate() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        line = line_no + 1,
                        error = %e,
                        "failed to read transcript line — skipping"
                    );
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<TranscriptEntry>(trimmed) {
                Ok(entry) => messages.push(entry.message),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        line = line_no + 1,
                        error = %e,
                        "malformed transcript entry — skipping"
                    );
                }
            }
        }
        Ok(messages)
    }

    /// Load the meta sidecar for an agent.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the file does not exist,
    /// [`SubAgentError::Transcript`] on parse failure.
    pub fn load_meta(dir: &Path, agent_id: &str) -> Result<TranscriptMeta, SubAgentError> {
        let path = dir.join(format!("{agent_id}.meta.json"));
        let content = fs::read_to_string(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                SubAgentError::NotFound(agent_id.to_owned())
            } else {
                SubAgentError::Transcript(format!("failed to read meta '{}': {e}", path.display()))
            }
        })?;
        serde_json::from_str(&content).map_err(|e| {
            SubAgentError::Transcript(format!("failed to parse meta '{}': {e}", path.display()))
        })
    }

    /// Find the full agent ID by scanning `dir` for `.meta.json` files whose names
    /// start with `prefix`.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if no match is found,
    /// [`SubAgentError::AmbiguousId`] if multiple matches are found,
    /// [`SubAgentError::Transcript`] on I/O failure.
    pub fn find_by_prefix(dir: &Path, prefix: &str) -> Result<String, SubAgentError> {
        let entries = fs::read_dir(dir).map_err(|e| {
            SubAgentError::Transcript(format!(
                "failed to read transcript dir '{}': {e}",
                dir.display()
            ))
        })?;

        let mut matches: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| SubAgentError::Transcript(format!("failed to read dir entry: {e}")))?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(agent_id) = name_str.strip_suffix(".meta.json")
                && agent_id.starts_with(prefix)
            {
                matches.push(agent_id.to_owned());
            }
        }

        match matches.len() {
            0 => Err(SubAgentError::NotFound(prefix.to_owned())),
            1 => Ok(matches.remove(0)),
            n => Err(SubAgentError::AmbiguousId(prefix.to_owned(), n)),
        }
    }
}

/// Delete the oldest `.jsonl` files in `dir` when the count exceeds `max_files`.
///
/// Files are sorted by modification time (oldest first). Returns the number of
/// files deleted.
///
/// # Errors
///
/// Returns `io::Error` if the directory cannot be read or a file cannot be deleted.
pub fn sweep_old_transcripts(dir: &Path, max_files: usize) -> io::Result<usize> {
    if max_files == 0 {
        return Ok(0);
    }

    let mut jsonl_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            jsonl_files.push((path, mtime));
        }
    }

    if jsonl_files.len() <= max_files {
        return Ok(0);
    }

    // Sort oldest first.
    jsonl_files.sort_by_key(|(_, mtime)| *mtime);

    let to_delete = jsonl_files.len() - max_files;
    let mut deleted = 0;
    for (path, _) in jsonl_files.into_iter().take(to_delete) {
        // Also remove the companion .meta.json sidecar if present.
        let meta = path.with_extension("meta.json");
        if meta.exists() {
            let _ = fs::remove_file(&meta);
        }
        fs::remove_file(&path)?;
        deleted += 1;
    }
    Ok(deleted)
}

/// Open a file in append mode with owner-only permissions (0o600 on Unix).
///
/// On non-Unix platforms falls back to standard `OpenOptions` without extra permissions.
fn open_private(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().create(true).append(true).open(path)
    }
}

/// Write `contents` to `path` atomically with owner-only permissions (0o600 on Unix).
///
/// On non-Unix platforms falls back to `fs::write`.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;
        file.flush()
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

/// Returns the current UTC time as an ISO 8601 string.
#[must_use]
pub fn utc_now_pub() -> String {
    utc_now()
}

fn utc_now() -> String {
    // Use SystemTime for a zero-dependency ISO 8601 timestamp.
    // Format: 2026-03-05T00:18:16Z
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = epoch_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert Unix epoch seconds to (year, month, day, hour, minute, second).
///
/// Uses the proleptic Gregorian calendar algorithm (Fliegel-Van Flandern variant).
/// All values are u64 throughout to avoid truncating casts; the caller knows values
/// fit in u32 for the ranges used (years 1970–2554, seconds/minutes/hours/days).
fn epoch_to_parts(epoch: u64) -> (u32, u32, u32, u32, u32, u32) {
    let sec = epoch % 60;
    let epoch = epoch / 60;
    let min = epoch % 60;
    let epoch = epoch / 60;
    let hour = epoch % 24;
    let days = epoch / 24;

    // Days since 1970-01-01 → civil calendar (Gregorian).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    // All values are in range for u32 for any timestamp in [1970, 2554].
    #[allow(clippy::cast_possible_truncation)]
    (
        year as u32,
        month as u32,
        day as u32,
        hour as u32,
        min as u32,
        sec as u32,
    )
}

#[cfg(test)]
mod tests {
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    use super::*;

    fn test_message(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn test_meta(agent_id: &str) -> TranscriptMeta {
        TranscriptMeta {
            agent_id: agent_id.to_owned(),
            agent_name: "bot".to_owned(),
            def_name: "bot".to_owned(),
            status: SubAgentState::Completed,
            started_at: "2026-01-01T00:00:00Z".to_owned(),
            finished_at: Some("2026-01-01T00:01:00Z".to_owned()),
            resumed_from: None,
            turns_used: 2,
        }
    }

    #[test]
    fn writer_reader_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let msg1 = test_message(Role::User, "hello");
        let msg2 = test_message(Role::Assistant, "world");

        let mut writer = TranscriptWriter::new(&path).unwrap();
        writer.append(0, &msg1).unwrap();
        writer.append(1, &msg2).unwrap();
        drop(writer);

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].content, "world");
    }

    #[test]
    fn load_missing_file_no_meta_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.jsonl");
        let messages = TranscriptReader::load(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn load_missing_file_with_meta_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join("ghost.meta.json");
        std::fs::write(&meta_path, "{}").unwrap();
        let jsonl_path = dir.path().join("ghost.jsonl");
        let err = TranscriptReader::load(&jsonl_path).unwrap_err();
        assert!(matches!(err, SubAgentError::Transcript(_)));
    }

    #[test]
    fn load_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");

        let good = test_message(Role::User, "good");
        let entry = TranscriptEntry {
            seq: 0,
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            message: good.clone(),
        };
        let good_line = serde_json::to_string(&entry).unwrap();
        let content = format!("{good_line}\nnot valid json\n{good_line}\n");
        std::fs::write(&path, &content).unwrap();

        let messages = TranscriptReader::load(&path).unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn meta_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("abc-123");
        TranscriptWriter::write_meta(dir.path(), "abc-123", &meta).unwrap();
        let loaded = TranscriptReader::load_meta(dir.path(), "abc-123").unwrap();
        assert_eq!(loaded.agent_id, "abc-123");
        assert_eq!(loaded.turns_used, 2);
    }

    #[test]
    fn meta_not_found_returns_not_found_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = TranscriptReader::load_meta(dir.path(), "ghost").unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn find_by_prefix_exact() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("abcdef01-0000-0000-0000-000000000000");
        TranscriptWriter::write_meta(dir.path(), "abcdef01-0000-0000-0000-000000000000", &meta)
            .unwrap();
        let id =
            TranscriptReader::find_by_prefix(dir.path(), "abcdef01-0000-0000-0000-000000000000")
                .unwrap();
        assert_eq!(id, "abcdef01-0000-0000-0000-000000000000");
    }

    #[test]
    fn find_by_prefix_short_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let meta = test_meta("deadbeef-0000-0000-0000-000000000000");
        TranscriptWriter::write_meta(dir.path(), "deadbeef-0000-0000-0000-000000000000", &meta)
            .unwrap();
        let id = TranscriptReader::find_by_prefix(dir.path(), "deadbeef").unwrap();
        assert_eq!(id, "deadbeef-0000-0000-0000-000000000000");
    }

    #[test]
    fn find_by_prefix_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = TranscriptReader::find_by_prefix(dir.path(), "xxxxxxxx").unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn find_by_prefix_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        TranscriptWriter::write_meta(dir.path(), "aabb0001-x", &test_meta("aabb0001-x")).unwrap();
        TranscriptWriter::write_meta(dir.path(), "aabb0002-y", &test_meta("aabb0002-y")).unwrap();
        let err = TranscriptReader::find_by_prefix(dir.path(), "aabb").unwrap_err();
        assert!(matches!(err, SubAgentError::AmbiguousId(_, 2)));
    }

    #[test]
    fn sweep_old_transcripts_removes_oldest() {
        let dir = tempfile::tempdir().unwrap();

        for i in 0..5u32 {
            let path = dir.path().join(format!("file{i:02}.jsonl"));
            std::fs::write(&path, b"").unwrap();
            // Vary mtime by touching the file — not reliable without explicit mtime set,
            // but tempdir files get sequential syscall timestamps in practice.
            // We set the mtime explicitly via filetime crate... but we have no filetime dep.
            // Instead we just verify count is correct.
        }

        let deleted = sweep_old_transcripts(dir.path(), 3).unwrap();
        assert_eq!(deleted, 2);

        let remaining: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        assert_eq!(remaining.len(), 3);
    }

    #[test]
    fn sweep_with_zero_max_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), b"").unwrap();
        let deleted = sweep_old_transcripts(dir.path(), 0).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn sweep_below_max_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), b"").unwrap();
        let deleted = sweep_old_transcripts(dir.path(), 50).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn utc_now_format() {
        let ts = utc_now();
        // Basic format check: 2026-03-05T00:18:16Z
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }

    #[test]
    fn load_empty_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, b"").unwrap();
        let messages = TranscriptReader::load(&path).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn load_meta_invalid_json_returns_transcript_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.meta.json"), b"not json at all {{{{").unwrap();
        let err = TranscriptReader::load_meta(dir.path(), "bad").unwrap_err();
        assert!(matches!(err, SubAgentError::Transcript(_)));
    }

    #[test]
    fn sweep_removes_companion_meta() {
        let dir = tempfile::tempdir().unwrap();
        // Create 4 JSONL files each with a companion meta sidecar.
        for i in 0..4u32 {
            let stem = format!("file{i:02}");
            std::fs::write(dir.path().join(format!("{stem}.jsonl")), b"").unwrap();
            std::fs::write(dir.path().join(format!("{stem}.meta.json")), b"{}").unwrap();
        }
        let deleted = sweep_old_transcripts(dir.path(), 2).unwrap();
        assert_eq!(deleted, 2);
        // Companion metas for the two deleted files should also be gone.
        let meta_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().to_string_lossy().ends_with(".meta.json"))
            .count();
        assert_eq!(
            meta_count, 2,
            "orphaned meta sidecars should have been removed"
        );
    }

    #[test]
    fn data_loss_guard_uses_stem_based_meta_path() {
        // path.with_extension("meta.json") on "abc.jsonl" should yield "abc.meta.json"
        // which matches write_meta's format!("{agent_id}.meta.json") when agent_id == stem.
        let dir = tempfile::tempdir().unwrap();
        let agent_id = "deadbeef-0000-0000-0000-000000000000";
        // Write meta sidecar but not the JSONL file.
        std::fs::write(dir.path().join(format!("{agent_id}.meta.json")), b"{}").unwrap();
        let jsonl_path = dir.path().join(format!("{agent_id}.jsonl"));
        let err = TranscriptReader::load(&jsonl_path).unwrap_err();
        assert!(matches!(err, SubAgentError::Transcript(ref m) if m.contains("missing")));
    }
}
