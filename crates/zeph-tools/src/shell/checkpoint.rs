// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-scoped checkpoint stack for `/undo` and `/redo` commands.
//!
//! Each successful write command captured by the transactional subsystem records a
//! [`Checkpoint`] onto the undo stack. Checkpoints are in-memory only — they are lost
//! when the agent process exits. The undo/redo cycle uses plain [`TransactionSnapshot`]
//! capture-before-rollback ordering (C2 fix), so no new snapshot primitives are needed.

use std::path::PathBuf;

use crate::executor::CheckpointEntryView;

use super::transaction::TransactionSnapshot;

/// A single point-in-time capture of files before a write command.
#[derive(Debug)]
pub(crate) struct Checkpoint {
    /// Before-state snapshot: rolling this back restores the pre-command file state.
    pub(crate) before_snapshot: TransactionSnapshot,
    /// Human-readable label (the shell command string).
    pub(crate) command: String,
    /// Paths captured — frozen at record time, reused during undo/redo re-capture.
    pub(crate) paths: Vec<PathBuf>,
    /// Unix timestamp (seconds since epoch) when this checkpoint was recorded.
    pub(crate) captured_at_secs: u64,
}

/// In-memory undo/redo checkpoint stack.
///
/// The undo stack grows at the tail (most-recent last). The redo stack is cleared
/// whenever a new checkpoint is recorded. The cap (`max_checkpoints`) is enforced at
/// record time by evicting the oldest entry from the front.
#[derive(Debug)]
pub(crate) struct CheckpointStack {
    pub(crate) undo: Vec<Checkpoint>,
    pub(crate) redo: Vec<Checkpoint>,
    pub(crate) max_checkpoints: usize,
}

impl CheckpointStack {
    pub(crate) fn new(max_checkpoints: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            max_checkpoints,
        }
    }

    /// Push a new checkpoint, evicting the oldest if the cap is exceeded.
    ///
    /// Clears the redo stack — any recorded forward state is invalidated by a new mutation.
    pub(crate) fn record(&mut self, checkpoint: Checkpoint) {
        self.redo.clear();
        if self.max_checkpoints > 0 && self.undo.len() >= self.max_checkpoints {
            self.undo.remove(0);
        }
        self.undo.push(checkpoint);
    }

    /// Pop up to `n` checkpoints and perform undo.
    ///
    /// For each popped checkpoint:
    /// 1. Capture the current (after-mutation) state BEFORE restoring — this becomes the redo entry.
    /// 2. Roll back the before-state snapshot to restore files.
    /// 3. Push the after-state capture onto the redo stack.
    ///
    /// Returns a summary of what was reverted.
    pub(crate) fn undo(&mut self, n: usize, max_snapshot_bytes: u64) -> UndoRedoResult {
        let available = self.undo.len();
        let count = n.min(available);
        if count == 0 {
            return UndoRedoResult {
                reverted_commands: 0,
                restored: 0,
                deleted: 0,
                message: "Nothing to undo.".to_owned(),
            };
        }
        let mut total_restored = 0usize;
        let mut total_deleted = 0usize;
        let mut reverted = Vec::new();

        for _ in 0..count {
            let Some(cp) = self.undo.pop() else {
                break;
            };
            // Capture after-state BEFORE rollback so deleted files are still absent.
            let redo_snap = match TransactionSnapshot::capture(&cp.paths, max_snapshot_bytes) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(err = %e, "checkpoint undo: redo re-capture failed, redo entry skipped");
                    None
                }
            };
            let cmd = cp.command.clone();
            let paths = cp.paths.clone();
            let captured_at_secs = cp.captured_at_secs;
            match cp.before_snapshot.rollback() {
                Ok(report) => {
                    total_restored += report.restored_count;
                    total_deleted += report.deleted_count;
                    reverted.push(cmd.clone());
                    if let Some(snap) = redo_snap {
                        self.redo.push(Checkpoint {
                            before_snapshot: snap,
                            command: cmd,
                            paths,
                            captured_at_secs,
                        });
                    }
                }
                Err(e) => {
                    tracing::error!(err = %e, cmd = %cmd, "checkpoint undo: rollback failed");
                }
            }
        }

        let actual = reverted.len();
        let message = if actual == 0 {
            "Undo failed: rollback errors for all checkpoints.".to_owned()
        } else if available > count {
            format!(
                "Undid {} of {} available command(s); {} more available.",
                actual,
                available,
                available - count
            )
        } else {
            format!("Undid {actual} command(s).")
        };

        UndoRedoResult {
            reverted_commands: actual,
            restored: total_restored,
            deleted: total_deleted,
            message,
        }
    }

    /// Pop one redo entry and re-apply the mutation.
    ///
    /// Mirrors undo: capture current state first, roll back the redo snapshot, push to undo.
    pub(crate) fn redo(&mut self, max_snapshot_bytes: u64) -> UndoRedoResult {
        let Some(cp) = self.redo.pop() else {
            return UndoRedoResult {
                reverted_commands: 0,
                restored: 0,
                deleted: 0,
                message: "Nothing to redo.".to_owned(),
            };
        };
        // Capture current (before-state) BEFORE re-applying after-state.
        let undo_snap = match TransactionSnapshot::capture(&cp.paths, max_snapshot_bytes) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(err = %e, "checkpoint redo: undo re-capture failed, undo entry skipped");
                None
            }
        };
        let cmd = cp.command.clone();
        let paths = cp.paths.clone();
        let captured_at_secs = cp.captured_at_secs;
        match cp.before_snapshot.rollback() {
            Ok(report) => {
                if let Some(snap) = undo_snap {
                    self.undo.push(Checkpoint {
                        before_snapshot: snap,
                        command: cmd.clone(),
                        paths,
                        captured_at_secs,
                    });
                }
                UndoRedoResult {
                    reverted_commands: 1,
                    restored: report.restored_count,
                    deleted: report.deleted_count,
                    message: format!("Redone: {cmd}"),
                }
            }
            Err(e) => {
                tracing::error!(err = %e, cmd = %cmd, "checkpoint redo: rollback failed");
                UndoRedoResult {
                    reverted_commands: 0,
                    restored: 0,
                    deleted: 0,
                    message: format!("Redo failed: {e}"),
                }
            }
        }
    }

    /// Return view entries for the undo stack (index 0 = most recent).
    pub(crate) fn list_undo(&self) -> Vec<CheckpointEntryView> {
        self.undo
            .iter()
            .enumerate()
            .rev()
            .map(|(stack_idx, cp)| CheckpointEntryView {
                index: self.undo.len() - 1 - stack_idx,
                command: cp.command.clone(),
                captured_at_secs: cp.captured_at_secs,
                file_count: cp.paths.len(),
            })
            .collect()
    }

    /// Number of redo entries available.
    pub(crate) fn redo_depth(&self) -> usize {
        self.redo.len()
    }
}

/// Result of a single undo or redo operation.
#[derive(Debug)]
pub(crate) struct UndoRedoResult {
    pub(crate) reverted_commands: usize,
    pub(crate) restored: usize,
    pub(crate) deleted: usize,
    pub(crate) message: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn make_checkpoint(paths: &[PathBuf]) -> Checkpoint {
        let snap = TransactionSnapshot::capture(paths, 0).unwrap();
        Checkpoint {
            before_snapshot: snap,
            command: "echo test".to_owned(),
            paths: paths.to_vec(),
            captured_at_secs: 0,
        }
    }

    // ---- empty-stack behaviour ----

    #[test]
    fn undo_empty_stack_returns_nothing_to_undo() {
        let mut stack = CheckpointStack::new(10);
        let r = stack.undo(1, 0);
        assert_eq!(r.reverted_commands, 0);
        assert_eq!(r.message, "Nothing to undo.");
    }

    #[test]
    fn redo_empty_stack_returns_nothing_to_redo() {
        let mut stack = CheckpointStack::new(10);
        let r = stack.redo(0);
        assert_eq!(r.reverted_commands, 0);
        assert_eq!(r.message, "Nothing to redo.");
    }

    // ---- cap eviction + redo-clear ----

    #[test]
    fn record_at_cap_evicts_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "v0").unwrap();

        let mut stack = CheckpointStack::new(2);

        let cp0 = make_checkpoint(std::slice::from_ref(&p));
        let cp1 = make_checkpoint(std::slice::from_ref(&p));
        let cp2 = make_checkpoint(std::slice::from_ref(&p));
        stack.record(cp0);
        stack.record(cp1);
        // At cap; recording cp2 must evict cp0 (oldest).
        stack.record(cp2);
        assert_eq!(stack.undo.len(), 2);
    }

    #[test]
    fn record_clears_redo_stack() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "v0").unwrap();

        let mut stack = CheckpointStack::new(10);
        stack.record(make_checkpoint(std::slice::from_ref(&p)));
        stack.undo(1, 0); // moves entry to redo
        assert_eq!(stack.redo.len(), 1);

        // A new record must clear redo.
        std::fs::write(&p, "v1").unwrap();
        stack.record(make_checkpoint(std::slice::from_ref(&p)));
        assert_eq!(stack.redo.len(), 0);
    }

    // ---- undo(n > available) clamping ----

    #[test]
    fn undo_n_greater_than_available_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "v0").unwrap();

        let mut stack = CheckpointStack::new(10);
        stack.record(make_checkpoint(std::slice::from_ref(&p)));
        // Request more than available — must not panic.
        let r = stack.undo(99, 0);
        assert_eq!(r.reverted_commands, 1);
    }

    // ---- list_undo ordering ----

    #[test]
    fn list_undo_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "v0").unwrap();

        let mut stack = CheckpointStack::new(10);

        let mut cp_a = make_checkpoint(std::slice::from_ref(&p));
        cp_a.command = "cmd_a".to_owned();
        cp_a.captured_at_secs = 1;

        let mut cp_b = make_checkpoint(std::slice::from_ref(&p));
        cp_b.command = "cmd_b".to_owned();
        cp_b.captured_at_secs = 2;

        stack.record(cp_a);
        stack.record(cp_b);

        let list = stack.list_undo();
        // Most recent (cp_b) must be first.
        assert_eq!(list[0].command, "cmd_b");
        assert_eq!(list[1].command, "cmd_a");
    }

    // ---- C2: undo + redo round-trip on real files ----

    #[test]
    fn undo_redo_roundtrip_restores_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("target.txt");
        std::fs::write(&p, "before").unwrap();

        let mut stack = CheckpointStack::new(10);
        stack.record(make_checkpoint(std::slice::from_ref(&p)));

        // Simulate the write that followed the snapshot.
        std::fs::write(&p, "after").unwrap();

        // Undo: should restore "before".
        let r = stack.undo(1, 0);
        assert_eq!(r.reverted_commands, 1);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "before");

        // Redo: should re-apply "after".
        let r2 = stack.redo(0);
        assert_eq!(r2.reverted_commands, 1);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "after");
    }

    // ---- C2: redo-of-delete (undo recreates, redo re-deletes) ----

    #[test]
    fn redo_of_delete_removes_file_again() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("to_delete.txt");
        std::fs::write(&p, "exists").unwrap();

        let mut stack = CheckpointStack::new(10);
        // Capture before deletion (file exists).
        stack.record(make_checkpoint(std::slice::from_ref(&p)));
        // Simulate deletion.
        std::fs::remove_file(&p).unwrap();

        // Undo: file should be restored.
        stack.undo(1, 0);
        assert!(p.exists(), "undo must restore the deleted file");

        // Redo: file should be deleted again.
        stack.redo(0);
        assert!(!p.exists(), "redo must re-delete the restored file");
    }
}
