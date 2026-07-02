// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The append-only JSONL event log: [`SessionEventLog`].
//!
//! Mirrors the append + fsync pattern of `zeph-durable`'s `JournalWriter`
//! (`crates/zeph-durable/src/writer.rs`) at the conversation-semantics level, but persists to a
//! plain JSONL file rather than a `SQLite`-backed journal (spec-068 §3, §14).
//!
//! # Invariants
//!
//! - INV-SP-1 (log-first ordering): callers must append to this log before updating any
//!   downstream projection (`SQLite` `messages`, `acp_sessions.last_seq`).
//! - INV-SP-2 (torn-append truncation): [`SessionEventLog::open`] validates every line on open
//!   and truncates a garbled/incomplete trailing line, which can only occur as the very last line
//!   because appends are serialized through a single writer (INV-D2).
//! - INV-D2 (single writer): only the session's owning actor/agent process may hold a
//!   `SessionEventLog` for a given session directory at a time. This module does not itself
//!   enforce cross-process exclusion; callers are responsible for the single-writer guarantee.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::error::SessionError;
use crate::event::{SessionEvent, SessionEventEnvelope};

const EVENTS_FILE_NAME: &str = "events.jsonl";

/// Append-only JSONL log for one conversation-session's `events.jsonl`.
///
/// # Examples
///
/// ```
/// use tempfile::tempdir;
/// use zeph_session::event::SessionEvent;
/// use zeph_session::log::SessionEventLog;
///
/// # #[tokio::main]
/// # async fn main() {
/// let dir = tempdir().unwrap();
/// let log = SessionEventLog::open(dir.path()).await.unwrap();
/// log.append(None, None, SessionEvent::SessionEnded { reason: "user_quit".to_owned() })
///     .await
///     .unwrap();
/// assert_eq!(log.last_seq(), Some(0));
/// # }
/// ```
pub struct SessionEventLog {
    events_path: PathBuf,
    writer: Mutex<File>,
    next_seq: AtomicU64,
}

impl SessionEventLog {
    /// Open (creating if absent) the `events.jsonl` log under `session_dir`.
    ///
    /// Validates the existing file per INV-SP-2, truncating a torn trailing write, then opens
    /// the file in append mode for subsequent writes. Sets file/directory permissions to
    /// `0o700`/`0o600` on Unix (spec §4.1); a no-op on other platforms.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the directory or file cannot be created, or
    /// [`SessionError::Serde`] surfaces only via [`Self::read_all`], never here (torn lines are
    /// discarded, not treated as fatal).
    pub async fn open(session_dir: &Path) -> Result<Self, SessionError> {
        fs::create_dir_all(session_dir).await?;
        set_permissions(session_dir, 0o700).await?;

        let events_path = session_dir.join(EVENTS_FILE_NAME);
        let (_, max_seq) = read_and_truncate(&events_path).await?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .await?;
        set_permissions(&events_path, 0o600).await?;

        let next_seq = max_seq.map_or(0, |seq| seq + 1);
        Ok(Self {
            events_path,
            writer: Mutex::new(file),
            next_seq: AtomicU64::new(next_seq),
        })
    }

    /// The path to this session's `events.jsonl` file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.events_path
    }

    /// The highest `seq` durably appended so far, or `None` if the log is empty.
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        let next = self.next_seq.load(Ordering::SeqCst);
        next.checked_sub(1)
    }

    /// Append one event, assigning it the next monotonic `seq`, and `fsync` before returning.
    ///
    /// The single `write_all` + `sync_all` pair is the atomicity boundary INV-SP-2 relies on: a
    /// crash mid-write can only ever corrupt this one trailing line.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Serde`] if the event cannot be JSON-encoded, or
    /// [`SessionError::Io`] if the write or fsync fails.
    #[tracing::instrument(name = "session.log.append", skip_all, level = "debug")]
    pub async fn append(
        &self,
        turn_id: Option<u64>,
        parent_seq: Option<u64>,
        kind: SessionEvent,
    ) -> Result<SessionEventEnvelope, SessionError> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let envelope = SessionEventEnvelope::new(seq, turn_id, parent_seq, kind);

        let mut line = serde_json::to_vec(&envelope)?;
        line.push(b'\n');

        let mut file = self.writer.lock().await;
        file.write_all(&line).await?;
        file.sync_all().await?;

        Ok(envelope)
    }

    /// Read and validate every event currently in the log, applying INV-SP-2 truncation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the file cannot be read.
    #[tracing::instrument(name = "session.log.read_all", skip_all, level = "debug")]
    pub async fn read_all(&self) -> Result<Vec<SessionEventEnvelope>, SessionError> {
        let (events, _) = read_and_truncate(&self.events_path).await?;
        Ok(events)
    }
}

/// Read every valid line of `path`, truncating a garbled/incomplete trailing line (INV-SP-2).
///
/// Returns the validated events and the maximum `seq` seen (`None` for an empty/absent log).
async fn read_and_truncate(
    path: &Path,
) -> Result<(Vec<SessionEventEnvelope>, Option<u64>), SessionError> {
    let file = match File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(e) => return Err(e.into()),
    };

    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut max_seq = None;
    let mut valid_len: u64 = 0;
    let mut offset: u64 = 0;
    let mut line = String::new();
    let mut torn = false;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await? as u64;
        if bytes_read == 0 {
            break;
        }

        let is_terminated = line.ends_with('\n');
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            offset += bytes_read;
            if is_terminated {
                valid_len = offset;
            }
            continue;
        }

        match serde_json::from_str::<SessionEventEnvelope>(trimmed) {
            Ok(envelope) if is_terminated => {
                max_seq = Some(envelope.seq);
                events.push(envelope);
                offset += bytes_read;
                valid_len = offset;
            }
            _ => {
                torn = true;
                break;
            }
        }
    }
    drop(reader);

    if torn {
        tracing::warn!(
            path = %path.display(),
            valid_len,
            "truncating torn tail in session event log (INV-SP-2)"
        );
    }

    let actual_len = fs::metadata(path).await?.len();
    if valid_len < actual_len {
        let file = OpenOptions::new().write(true).open(path).await?;
        file.set_len(valid_len).await?;
    }

    Ok((events, max_seq))
}

#[cfg(unix)]
async fn set_permissions(path: &Path, mode: u32) -> Result<(), SessionError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_permissions(_path: &Path, _mode: u32) -> Result<(), SessionError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_append_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();

        for i in 0..5u64 {
            log.append(
                Some(i),
                None,
                SessionEvent::UserMessage {
                    text: format!("msg-{i}"),
                    image_refs: vec![],
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(log.last_seq(), Some(4));
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 5);
        for (i, envelope) in events.iter().enumerate() {
            assert_eq!(envelope.seq, i as u64);
        }
    }

    #[tokio::test]
    async fn test_reopen_resumes_seq() {
        let dir = tempfile::tempdir().unwrap();
        {
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            log.append(
                None,
                None,
                SessionEvent::SessionEnded { reason: "x".into() },
            )
            .await
            .unwrap();
        }
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), Some(0));
        let appended = log
            .append(
                None,
                None,
                SessionEvent::SessionEnded { reason: "y".into() },
            )
            .await
            .unwrap();
        assert_eq!(appended.seq, 1);
    }

    #[tokio::test]
    async fn test_torn_write_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path;
        {
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            for i in 0..3u64 {
                log.append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("msg-{i}"),
                        image_refs: vec![],
                    },
                )
                .await
                .unwrap();
            }
            path = log.path().to_path_buf();
        }

        // Simulate a torn write: truncate the file mid-way through the last line.
        let full = tokio::fs::read(&path).await.unwrap();
        let cut = full.len() - 5;
        tokio::fs::write(&path, &full[..cut]).await.unwrap();

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(
            log.last_seq(),
            Some(1),
            "torn last line must be dropped cleanly"
        );
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn test_torn_write_truncation_various_offsets() {
        for cut_from_end in [1usize, 3, 10, 20] {
            let dir = tempfile::tempdir().unwrap();
            let path;
            {
                let log = SessionEventLog::open(dir.path()).await.unwrap();
                for i in 0..4u64 {
                    log.append(
                        None,
                        None,
                        SessionEvent::UserMessage {
                            text: format!("event-number-{i}"),
                            image_refs: vec![],
                        },
                    )
                    .await
                    .unwrap();
                }
                path = log.path().to_path_buf();
            }
            let full = tokio::fs::read(&path).await.unwrap();
            let cut = full.len().saturating_sub(cut_from_end);
            tokio::fs::write(&path, &full[..cut]).await.unwrap();

            // Must not panic and must never see more than the 4 originally-committed events.
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            let events = log.read_all().await.unwrap();
            assert!(events.len() <= 4);
        }
    }

    #[tokio::test]
    async fn test_empty_log_read_all() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), None);
        assert!(log.read_all().await.unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_file_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let meta = tokio::fs::metadata(log.path()).await.unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
}
