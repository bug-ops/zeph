use std::path::PathBuf;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::mpsc;

pub enum SkillEvent {
    Changed,
}

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
    pub fn start(paths: &[PathBuf], tx: mpsc::Sender<SkillEvent>) -> anyhow::Result<Self> {
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
