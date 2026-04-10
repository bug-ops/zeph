// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Filesystem hot-reload for skill directories.
//!
//! [`SkillWatcher`] wraps the `notify` crate behind a 500 ms debounce window
//! and forwards coalesced change events over a [`tokio::sync::mpsc`] channel.
//! Only `SKILL.md` file changes trigger an event; other filesystem activity is
//! silently ignored.
//!
//! The watcher task runs for as long as the [`SkillWatcher`] handle is alive.
//! Dropping the handle aborts the background task and stops all watches.
//!
//! # Examples
//!
//! ```rust,no_run
//! use tokio::sync::mpsc;
//! use zeph_skills::watcher::{SkillEvent, SkillWatcher};
//! use std::path::PathBuf;
//!
//! # async fn run() -> Result<(), zeph_skills::SkillError> {
//! let (tx, mut rx) = mpsc::channel(16);
//! let _watcher = SkillWatcher::start(&[PathBuf::from("/path/to/skills")], tx)?;
//!
//! while let Some(event) = rx.recv().await {
//!     match event {
//!         SkillEvent::Changed => println!("skills changed, reloading"),
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::mpsc;

use crate::error::SkillError;

/// Events emitted by [`SkillWatcher`] when a `SKILL.md` file is created, modified, or removed.
#[derive(Clone)]
pub enum SkillEvent {
    /// At least one `SKILL.md` file changed within the watched directories.
    ///
    /// Multiple rapid filesystem changes are coalesced into a single `Changed` event
    /// by the 500 ms debounce window.
    Changed,
}

/// Handle to the background filesystem watcher task.
///
/// Dropping this value aborts the watcher task and releases all filesystem watches.
pub struct SkillWatcher {
    _handle: tokio::task::JoinHandle<()>,
}

impl SkillWatcher {
    /// Start watching directories for SKILL.md changes.
    ///
    /// Sends `SkillEvent::Changed` on any filesystem change (debounced 500ms).
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem watcher cannot be initialized.
    pub fn start(paths: &[PathBuf], tx: mpsc::Sender<SkillEvent>) -> Result<Self, SkillError> {
        let (notify_tx, mut notify_rx) = mpsc::channel(16);

        let mut debouncer = new_debouncer(
            Duration::from_millis(500),
            move |events: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                let events = match events {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!("watcher error: {e}");
                        return;
                    }
                };

                let has_skill_change = events.iter().any(|e| {
                    e.kind == DebouncedEventKind::Any
                        && e.path.file_name().is_some_and(|n| n == "SKILL.md")
                });

                if has_skill_change {
                    let _ = notify_tx.blocking_send(());
                }
            },
        )?;

        for path in paths {
            debouncer
                .watcher()
                .watch(path, notify::RecursiveMode::Recursive)?;
        }

        let handle = tokio::spawn(async move {
            let _debouncer = debouncer;
            while notify_rx.recv().await.is_some() {
                if tx.send(SkillEvent::Changed).await.is_err() {
                    break;
                }
            }
        });

        Ok(Self { _handle: handle })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_with_valid_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let watcher = SkillWatcher::start(&[dir.path().to_path_buf()], tx);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn start_with_multiple_directories() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let watcher =
            SkillWatcher::start(&[dir1.path().to_path_buf(), dir2.path().to_path_buf()], tx);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn start_with_nonexistent_directory_fails() {
        let (tx, _rx) = mpsc::channel(16);
        let result = SkillWatcher::start(&[PathBuf::from("/nonexistent/path/xyz")], tx);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn start_with_empty_paths() {
        let (tx, _rx) = mpsc::channel(16);
        let watcher = SkillWatcher::start(&[], tx);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn detects_skill_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let _watcher = SkillWatcher::start(&[dir.path().to_path_buf()], tx).unwrap();

        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(&skill_path, "initial").unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        std::fs::write(&skill_path, "updated content").unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await;
        assert!(
            result.is_ok(),
            "expected SkillEvent::Changed within timeout"
        );
    }

    #[tokio::test]
    async fn ignores_non_skill_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let _watcher = SkillWatcher::start(&[dir.path().to_path_buf()], tx).unwrap();

        let other_path = dir.path().join("README.md");
        std::fs::write(&other_path, "content").unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_millis(1500), rx.recv()).await;
        assert!(result.is_err(), "should not receive event for non-SKILL.md");
    }
}
