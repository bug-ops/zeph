use std::path::Path;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::mpsc;

pub enum ConfigEvent {
    Changed,
}

pub struct ConfigWatcher {
    _handle: tokio::task::JoinHandle<()>,
}

impl ConfigWatcher {
    /// Start watching a config file for changes.
    ///
    /// Watches the parent directory and filters for the target filename.
    /// Sends `ConfigEvent::Changed` on any modification (debounced 500ms).
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem watcher cannot be initialized
    /// or the config file path has no parent directory.
    pub fn start(path: &Path, tx: mpsc::Sender<ConfigEvent>) -> anyhow::Result<Self> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?
            .to_path_buf();
        let filename = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("config path has no filename"))?
            .to_os_string();

        let (notify_tx, mut notify_rx) = mpsc::channel(16);

        let mut debouncer = new_debouncer(
            Duration::from_millis(500),
            move |events: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                let events = match events {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!("config watcher error: {e}");
                        return;
                    }
                };

                let has_change = events.iter().any(|e| {
                    e.kind == DebouncedEventKind::Any
                        && e.path.file_name().is_some_and(|n| n == filename)
                });

                if has_change {
                    let _ = notify_tx.blocking_send(());
                }
            },
        )?;

        debouncer
            .watcher()
            .watch(&dir, notify::RecursiveMode::NonRecursive)?;

        let handle = tokio::spawn(async move {
            let _debouncer = debouncer;
            while notify_rx.recv().await.is_some() {
                if tx.send(ConfigEvent::Changed).await.is_err() {
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
    async fn start_with_valid_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "key = 1").unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let watcher = ConfigWatcher::start(&config_path, tx);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn start_with_nonexistent_parent_fails() {
        let (tx, _rx) = mpsc::channel(16);
        let result = ConfigWatcher::start(Path::new("/nonexistent/dir/config.toml"), tx);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn detects_config_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "initial = true").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let _watcher = ConfigWatcher::start(&config_path, tx).unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&config_path, "updated = true").unwrap();

        let result = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(
            result.is_ok(),
            "expected ConfigEvent::Changed within timeout"
        );
    }

    #[tokio::test]
    async fn ignores_other_files_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "key = 1").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let _watcher = ConfigWatcher::start(&config_path, tx).unwrap();

        let other_path = dir.path().join("other.txt");
        std::fs::write(&other_path, "content").unwrap();

        let result = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await;
        assert!(
            result.is_err(),
            "should not receive event for non-config file"
        );
    }
}
