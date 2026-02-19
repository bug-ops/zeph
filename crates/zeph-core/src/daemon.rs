//! Daemon supervisor for component lifecycle management.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::DaemonConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentStatus {
    Running,
    Failed(String),
    Stopped,
}

pub struct ComponentHandle {
    pub name: String,
    handle: JoinHandle<anyhow::Result<()>>,
    pub status: ComponentStatus,
    pub restart_count: u32,
}

impl ComponentHandle {
    #[must_use]
    pub fn new(name: impl Into<String>, handle: JoinHandle<anyhow::Result<()>>) -> Self {
        Self {
            name: name.into(),
            handle,
            status: ComponentStatus::Running,
            restart_count: 0,
        }
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

pub struct DaemonSupervisor {
    components: Vec<ComponentHandle>,
    health_interval: Duration,
    _max_backoff: Duration,
    shutdown_rx: watch::Receiver<bool>,
}

impl DaemonSupervisor {
    #[must_use]
    pub fn new(config: &DaemonConfig, shutdown_rx: watch::Receiver<bool>) -> Self {
        Self {
            components: Vec::new(),
            health_interval: Duration::from_secs(config.health_interval_secs),
            _max_backoff: Duration::from_secs(config.max_restart_backoff_secs),
            shutdown_rx,
        }
    }

    pub fn add_component(&mut self, handle: ComponentHandle) {
        self.components.push(handle);
    }

    #[must_use]
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Run the health monitoring loop until shutdown signal.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(self.health_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.check_health();
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        tracing::info!("daemon supervisor shutting down");
                        break;
                    }
                }
            }
        }
    }

    fn check_health(&mut self) {
        for component in &mut self.components {
            if component.status == ComponentStatus::Running && component.is_finished() {
                component.status = ComponentStatus::Failed("task exited".into());
                component.restart_count += 1;
                tracing::warn!(
                    component = %component.name,
                    restarts = component.restart_count,
                    "component exited unexpectedly"
                );
            }
        }
    }

    #[must_use]
    pub fn component_statuses(&self) -> Vec<(&str, &ComponentStatus)> {
        self.components
            .iter()
            .map(|c| (c.name.as_str(), &c.status))
            .collect()
    }
}

/// Write a PID file. Returns an error if the write fails.
///
/// # Errors
///
/// Returns an error if the PID file directory cannot be created or the file cannot be written.
pub fn write_pid_file(path: &str) -> std::io::Result<()> {
    let expanded = expand_tilde(path);
    let path = std::path::Path::new(&expanded);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string())
}

/// Read the PID from a PID file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the content is not a valid PID.
pub fn read_pid_file(path: &str) -> std::io::Result<u32> {
    let expanded = expand_tilde(path);
    let content = std::fs::read_to_string(&expanded)?;
    content
        .trim()
        .parse::<u32>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Remove the PID file.
///
/// # Errors
///
/// Returns an error if the file cannot be removed.
pub fn remove_pid_file(path: &str) -> std::io::Result<()> {
    let expanded = expand_tilde(path);
    match std::fs::remove_file(&expanded) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return format!("{}/{rest}", home.to_string_lossy());
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_with_home() {
        let result = expand_tilde("~/test/file.pid");
        assert!(!result.starts_with("~/"));
    }

    #[test]
    fn expand_tilde_absolute_unchanged() {
        assert_eq!(expand_tilde("/tmp/zeph.pid"), "/tmp/zeph.pid");
    }

    #[test]
    fn pid_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        let path_str = path.to_string_lossy().to_string();

        write_pid_file(&path_str).unwrap();
        let pid = read_pid_file(&path_str).unwrap();
        assert_eq!(pid, std::process::id());
        remove_pid_file(&path_str).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remove_nonexistent_pid_file_ok() {
        assert!(remove_pid_file("/tmp/nonexistent_zeph_test.pid").is_ok());
    }

    #[test]
    fn read_invalid_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.pid");
        std::fs::write(&path, "not_a_number").unwrap();
        assert!(read_pid_file(&path.to_string_lossy()).is_err());
    }

    #[tokio::test]
    async fn supervisor_tracks_components() {
        let config = DaemonConfig::default();
        let (_tx, rx) = watch::channel(false);
        let mut supervisor = DaemonSupervisor::new(&config, rx);

        let handle = tokio::spawn(async { Ok(()) });
        supervisor.add_component(ComponentHandle::new("test", handle));
        assert_eq!(supervisor.component_count(), 1);
    }

    #[tokio::test]
    async fn supervisor_detects_finished_component() {
        let config = DaemonConfig::default();
        let (_tx, rx) = watch::channel(false);
        let mut supervisor = DaemonSupervisor::new(&config, rx);

        let handle = tokio::spawn(async { Ok(()) });
        tokio::time::sleep(Duration::from_millis(10)).await;
        supervisor.add_component(ComponentHandle::new("finished", handle));
        supervisor.check_health();

        let statuses = supervisor.component_statuses();
        assert_eq!(statuses.len(), 1);
        assert!(matches!(statuses[0].1, ComponentStatus::Failed(_)));
    }

    #[tokio::test]
    async fn supervisor_shutdown() {
        let mut config = DaemonConfig::default();
        config.health_interval_secs = 1;
        let (tx, rx) = watch::channel(false);
        let mut supervisor = DaemonSupervisor::new(&config, rx);

        let run_handle = tokio::spawn(async move { supervisor.run().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), run_handle)
            .await
            .expect("supervisor should stop on shutdown")
            .expect("task should complete");
    }

    #[test]
    fn component_status_eq() {
        assert_eq!(ComponentStatus::Running, ComponentStatus::Running);
        assert_eq!(ComponentStatus::Stopped, ComponentStatus::Stopped);
        assert_ne!(ComponentStatus::Running, ComponentStatus::Stopped);
    }
}
