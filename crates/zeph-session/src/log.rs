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
//! - INV-SP-2 (torn-append truncation): every read validates each line and drops a garbled/
//!   incomplete trailing line from the in-memory result, which can only occur as the very last
//!   line because appends are serialized through a single writer (INV-D2). Only
//!   [`SessionEventLog::open_exclusive`] additionally repairs the torn tail physically on disk —
//!   a lockless [`SessionEventLog::open`]/[`SessionEventLog::read_all`] cannot prove a "torn"
//!   line isn't a live writer's in-flight, not-yet-fsynced append, so it must never mutate the
//!   file (#5487 Finding B).
//! - INV-D2 (single writer): only the session's owning actor/agent process may hold a
//!   `SessionEventLog` for a given session directory at a time. [`SessionEventLog::open`]
//!   does not itself enforce cross-process exclusion — it is also used by read-only
//!   tooling (session export/inspection) that may legitimately run alongside a live
//!   writer. The session's owning actor/agent process should instead use
//!   [`SessionEventLog::open_exclusive`], which takes a non-blocking `flock(2)` advisory
//!   lock (Unix only) and fails with [`SessionError::AlreadyLocked`] if another writer
//!   already holds the session directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::error::SessionError;
use crate::event::{SessionEvent, SessionEventEnvelope};

const EVENTS_FILE_NAME: &str = "events.jsonl";
const LOCK_FILE_NAME: &str = "events.jsonl.lock";

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
    #[allow(dead_code)] // held only for its Drop (releases the flock, if taken)
    lock: Option<AdvisoryLock>,
}

impl SessionEventLog {
    /// Open (creating if absent) the `events.jsonl` log under `session_dir`.
    ///
    /// Validates the existing file per INV-SP-2, dropping a torn trailing line from the
    /// in-memory result, then opens the file in append mode for subsequent writes. Sets
    /// file/directory permissions to `0o700`/`0o600` on Unix (spec §4.1); a no-op on other
    /// platforms.
    ///
    /// Does not take the cross-process advisory lock, and never physically truncates the file
    /// (even if a torn tail is found) — safe for read-only tooling that may run alongside a live
    /// writer whose in-flight, not-yet-fsynced line could otherwise be mistaken for "torn" and
    /// destroyed (#5487 Finding B). The session's owning actor/agent process should use
    /// [`Self::open_exclusive`] instead, which does perform the physical repair.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the directory or file cannot be created, or
    /// [`SessionError::Serde`] surfaces only via [`Self::read_all`], never here (torn lines are
    /// discarded, not treated as fatal).
    pub async fn open(session_dir: &Path) -> Result<Self, SessionError> {
        Self::open_with_lock(session_dir, None).await
    }

    /// Open the `events.jsonl` log under `session_dir` like [`Self::open`], but additionally
    /// take a non-blocking, exclusive advisory lock (`flock(2)` on Unix, mirroring
    /// `zeph-scheduler`'s `PidFile`) enforcing INV-D2's single-writer invariant.
    ///
    /// Intended for the session's owning actor/agent process. On non-Unix targets the lock
    /// is a no-op (the workspace has no vetted cross-platform advisory-locking primitive), so
    /// this degrades to [`Self::open`]'s behavior there.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AlreadyLocked`] if another process already holds the session's
    /// write lock, or any error [`Self::open`] can return.
    pub async fn open_exclusive(session_dir: &Path) -> Result<Self, SessionError> {
        fs::create_dir_all(session_dir).await?;
        let lock = AdvisoryLock::acquire(session_dir)?;
        Self::open_with_lock(session_dir, Some(lock)).await
    }

    async fn open_with_lock(
        session_dir: &Path,
        lock: Option<AdvisoryLock>,
    ) -> Result<Self, SessionError> {
        fs::create_dir_all(session_dir).await?;
        set_permissions(session_dir, 0o700).await?;

        let events_path = session_dir.join(EVENTS_FILE_NAME);
        // Only the exclusive-lock holder may physically repair a torn tail (see
        // `read_events`'s doc comment) — a lockless `open()` cannot prove the "torn" line
        // isn't a live writer's in-flight, not-yet-fsynced append.
        let (_, max_seq) = read_events(&events_path, lock.is_some()).await?;

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
            lock,
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
        let mut file = self.writer.lock().await;

        // seq assignment MUST happen while holding the writer lock: two concurrent
        // callers assigned seq N and N+1 before the lock could still race for the
        // lock and land their physical writes in the opposite order, breaking
        // INV-SP-2's ascending-seq-order assumption (#5487).
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let envelope = SessionEventEnvelope::new(seq, turn_id, parent_seq, kind);

        let mut line = serde_json::to_vec(&envelope)?;
        line.push(b'\n');

        file.write_all(&line).await?;
        file.sync_all().await?;

        Ok(envelope)
    }

    /// Read and validate every event currently in the log, dropping a torn trailing line from
    /// the result (INV-SP-2). Only physically repairs the file if this handle was opened via
    /// [`Self::open_exclusive`] — see that method's doc comment.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the file cannot be read.
    #[tracing::instrument(name = "session.log.read_all", skip_all, level = "debug")]
    pub async fn read_all(&self) -> Result<Vec<SessionEventEnvelope>, SessionError> {
        // Same repair gating as `open_with_lock`: only repair the physical file when this
        // handle holds the exclusive lock (i.e. is the session's owning writer). A read-only
        // handle (`open()`) calling `read_all()` — e.g. `sessions show --events`, the ACP HTTP
        // inspection endpoint — must never truncate a live writer's in-flight tail out from
        // under it (#5487 Finding B).
        let (events, _) = read_events(&self.events_path, self.lock.is_some()).await?;
        Ok(events)
    }
}

/// Read every valid line of `path`, dropping a garbled/incomplete trailing line from the
/// in-memory result (INV-SP-2).
///
/// When `repair` is `true`, additionally truncates that torn tail physically on disk. Only the
/// session's exclusive-lock holder (see [`SessionEventLog::open_exclusive`]) may pass `true`: it
/// is the only caller that can prove a "torn" trailing line isn't actually a live writer's
/// in-flight, not-yet-fsynced append (#5487 Finding B) — a lockless reader physically truncating
/// the file could destroy a concurrent writer's tail out from under it.
///
/// Returns the validated events and the maximum `seq` seen (`None` for an empty/absent log).
async fn read_events(
    path: &Path,
    repair: bool,
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
                // Track the true running maximum, not just the last line's value: a
                // file whose physical order doesn't match seq order (e.g. from a
                // pre-fix #5487 race) must still yield the correct next seq.
                max_seq = Some(max_seq.map_or(envelope.seq, |m: u64| m.max(envelope.seq)));
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
            repair,
            "dropped torn tail in session event log (INV-SP-2)"
        );
    }

    if repair {
        let actual_len = fs::metadata(path).await?.len();
        if valid_len < actual_len {
            let file = OpenOptions::new().write(true).open(path).await?;
            file.set_len(valid_len).await?;
        }
    }

    Ok((events, max_seq))
}

#[cfg(unix)]
async fn set_permissions(path: &Path, mode: u32) -> Result<(), SessionError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    Ok(())
}

/// Cross-process advisory lock enforcing INV-D2's single-writer invariant, held for the
/// lifetime of a [`SessionEventLog`] opened via [`SessionEventLog::open_exclusive`].
///
/// Backed by `flock(2)` on a sibling lock file (`events.jsonl.lock`) rather than
/// `events.jsonl` itself, so the lock is independent of the append-mode file handle already
/// held for writing. Mirrors `zeph-scheduler`'s `PidFile`, but — unlike a pid file — the lock
/// file is never unlinked on drop: it is a permanent sentinel, not ephemeral process identity,
/// and unlinking it would reopen an unlink/re-create race between the releasing and the next
/// acquiring process.
#[cfg(unix)]
struct AdvisoryLock(#[allow(dead_code)] rustix::fd::OwnedFd);

#[cfg(unix)]
impl AdvisoryLock {
    fn acquire(session_dir: &Path) -> Result<Self, SessionError> {
        use rustix::fs::{FlockOperation, Mode, OFlags};

        let lock_path = session_dir.join(LOCK_FILE_NAME);
        let fd = rustix::fs::open(
            &lock_path,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?;

        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                SessionError::AlreadyLocked(lock_path.display().to_string())
            } else {
                SessionError::Io(e.into())
            }
        })?;

        Ok(Self(fd))
    }
}

/// No vetted cross-platform advisory-locking primitive exists in this workspace, so
/// [`SessionEventLog::open_exclusive`] does not enforce INV-D2 on non-Unix targets.
#[cfg(not(unix))]
struct AdvisoryLock;

#[cfg(not(unix))]
impl AdvisoryLock {
    fn acquire(_session_dir: &Path) -> Result<Self, SessionError> {
        Ok(Self)
    }
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

    /// Regression test for #5487 Finding B: a lockless `open()`/`read_all()` must never
    /// physically truncate a torn tail — it cannot distinguish a genuinely torn line from a
    /// live writer's in-flight, not-yet-fsynced append, so mutating the file could destroy that
    /// writer's data out from under it. Only `open_exclusive()` may repair.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_does_not_physically_truncate_torn_tail() {
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

        let full = tokio::fs::read(&path).await.unwrap();
        let cut = full.len() - 5;
        tokio::fs::write(&path, &full[..cut]).await.unwrap();
        let torn_len = tokio::fs::metadata(&path).await.unwrap().len();

        // Lockless open()/read_all(): in-memory result drops the torn line, but the file on
        // disk must be untouched.
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), Some(1));
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            tokio::fs::metadata(&path).await.unwrap().len(),
            torn_len,
            "open()/read_all() must never physically truncate the file"
        );
        drop(log);

        // open_exclusive(): now physically repairs the file.
        let log = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), Some(1));
        let repaired_len = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(
            repaired_len < torn_len,
            "open_exclusive() must physically truncate the torn tail"
        );
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

    /// Regression test for #5487 bug B: `read_events` must compute the true running
    /// maximum `seq`, not just take the last physical line's value. Simulates the on-disk
    /// shape a pre-fix concurrent-append race could produce: seq 7 written physically before
    /// seq 6.
    #[tokio::test]
    async fn test_max_seq_survives_out_of_order_physical_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(EVENTS_FILE_NAME);

        let make_line = |seq: u64| {
            let envelope = SessionEventEnvelope::new(
                seq,
                None,
                None,
                SessionEvent::SessionEnded { reason: "x".into() },
            );
            let mut line = serde_json::to_vec(&envelope).unwrap();
            line.push(b'\n');
            line
        };

        // Physical order is seq=7 then seq=6 — out of seq order, as a pre-fix race could
        // produce, but every line individually well-formed and fsynced.
        let mut contents = make_line(7);
        contents.extend(make_line(6));
        tokio::fs::write(&path, &contents).await.unwrap();

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(
            log.last_seq(),
            Some(7),
            "next_seq must be derived from the true max seq, not the last physical line"
        );
        let appended = log
            .append(
                None,
                None,
                SessionEvent::SessionEnded { reason: "z".into() },
            )
            .await
            .unwrap();
        assert_eq!(
            appended.seq, 8,
            "must not reuse a seq already present earlier in the file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_exclusive_rejects_second_writer() {
        let dir = tempfile::tempdir().unwrap();
        let _first = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        match SessionEventLog::open_exclusive(dir.path()).await {
            Err(SessionError::AlreadyLocked(_)) => {}
            Err(e) => panic!("expected AlreadyLocked, got different error: {e}"),
            Ok(_) => panic!("expected AlreadyLocked, but second open_exclusive succeeded"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_exclusive_allows_reacquire_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _first = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        }
        // Lock released when the first handle dropped — must not still be held.
        let _second = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_is_not_blocked_by_open_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let _writer = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        // Read-only `open()` must still succeed while a writer holds the exclusive lock.
        let _reader = SessionEventLog::open(dir.path()).await.unwrap();
    }

    /// Regression test for #5487 bug A: drives genuine concurrent `append()` calls (on real
    /// OS threads, not just cooperative interleaving) against one shared `SessionEventLog` and
    /// asserts seq assignment and physical write order never diverge. Before the fix, `seq`
    /// was assigned via `fetch_add` before acquiring the writer lock, so a task could win a
    /// low seq but lose the race for the lock, landing its line after a higher-seq task's line.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_concurrent_append_preserves_seq_order() {
        const N: u64 = 100;

        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(SessionEventLog::open(dir.path()).await.unwrap());

        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..N {
            let log = log.clone();
            tasks.spawn(async move {
                log.append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("msg-{i}"),
                        image_refs: vec![],
                    },
                )
                .await
                .unwrap()
                .seq
            });
        }

        let mut assigned_seqs: Vec<u64> = tasks.join_all().await;
        assigned_seqs.sort_unstable();
        assert_eq!(
            assigned_seqs,
            (0..N).collect::<Vec<_>>(),
            "every seq in 0..{N} must be assigned exactly once, with no gaps or duplicates"
        );

        // Physical order on disk must match seq order: seq assignment and the write it
        // guards must never diverge under contention (#5487 fix 2).
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), usize::try_from(N).unwrap());
        for (i, envelope) in events.iter().enumerate() {
            assert_eq!(
                envelope.seq, i as u64,
                "physical line {i} must carry seq {i}; seq and write order diverged"
            );
        }
    }
}
