use std::future::Future;
use std::pin::Pin;

use semver::Version;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::error::SchedulerError;
use crate::task::TaskHandler;

const GITHUB_RELEASES_URL: &str = "https://api.github.com/repos/bug-ops/zeph/releases/latest";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub struct UpdateCheckHandler {
    current_version: &'static str,
    notify_tx: mpsc::Sender<String>,
    http_client: reqwest::Client,
}

#[derive(Deserialize)]
struct ReleaseInfo {
    tag_name: Option<String>,
}

impl UpdateCheckHandler {
    /// Create a new handler.
    ///
    /// `current_version` should be `env!("CARGO_PKG_VERSION")`.
    /// Notifications are sent as formatted strings via `notify_tx`.
    ///
    /// # Panics
    ///
    /// Panics if the underlying `reqwest` client cannot be constructed (unreachable in practice).
    #[must_use]
    pub fn new(current_version: &'static str, notify_tx: mpsc::Sender<String>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent(format!("zeph/{current_version}"))
            .build()
            .expect("reqwest client builder should not fail with timeout and user_agent");
        Self {
            current_version,
            notify_tx,
            http_client,
        }
    }

    /// Extract and compare versions; returns `Some(remote_version_str)` when remote > current.
    fn newer_version(current: &str, tag_name: &str) -> Option<String> {
        let remote_str = tag_name.trim_start_matches('v');
        if remote_str.is_empty() {
            return None;
        }
        let current_v = Version::parse(current).ok()?;
        let remote_v = Version::parse(remote_str).ok()?;
        if remote_v > current_v {
            Some(remote_str.to_owned())
        } else {
            None
        }
    }
}

impl TaskHandler for UpdateCheckHandler {
    fn execute(
        &self,
        _config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + '_>> {
        Box::pin(async move {
            let resp = self
                .http_client
                .get(GITHUB_RELEASES_URL)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("update check request failed: {e}");
                    return Ok(());
                }
            };

            if !resp.status().is_success() {
                tracing::warn!("update check: HTTP {}", resp.status());
                return Ok(());
            }

            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("update check: failed to read response body: {e}");
                    return Ok(());
                }
            };
            if bytes.len() > MAX_RESPONSE_BYTES {
                tracing::warn!(
                    "update check: response body too large ({} bytes), skipping",
                    bytes.len()
                );
                return Ok(());
            }
            let info: ReleaseInfo = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("update check response parse failed: {e}");
                    return Ok(());
                }
            };

            let Some(tag_name) = info.tag_name else {
                tracing::warn!("update check: missing tag_name in response");
                return Ok(());
            };

            match Self::newer_version(self.current_version, &tag_name) {
                Some(remote) => {
                    let msg = format!(
                        "New version available: v{remote} (current: v{}).\nUpdate: https://github.com/bug-ops/zeph/releases/tag/v{remote}",
                        self.current_version
                    );
                    tracing::debug!("update available: {remote}");
                    let _ = self.notify_tx.send(msg).await;
                }
                None => {
                    tracing::debug!(
                        current = self.current_version,
                        remote = tag_name,
                        "no update available"
                    );
                }
            }

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_version_detects_upgrade() {
        assert_eq!(
            UpdateCheckHandler::newer_version("0.11.0", "v0.12.0"),
            Some("0.12.0".to_owned())
        );
    }

    #[test]
    fn newer_version_same_version_no_notify() {
        assert_eq!(UpdateCheckHandler::newer_version("0.11.0", "v0.11.0"), None);
    }

    #[test]
    fn newer_version_older_remote_no_notify() {
        assert_eq!(UpdateCheckHandler::newer_version("0.11.0", "v0.10.0"), None);
    }

    #[test]
    fn newer_version_strips_v_prefix() {
        assert_eq!(
            UpdateCheckHandler::newer_version("1.0.0", "v2.0.0"),
            Some("2.0.0".to_owned())
        );
        assert_eq!(
            UpdateCheckHandler::newer_version("1.0.0", "2.0.0"),
            Some("2.0.0".to_owned())
        );
    }

    #[test]
    fn newer_version_invalid_current_returns_none() {
        assert_eq!(
            UpdateCheckHandler::newer_version("not-semver", "v1.0.0"),
            None
        );
    }

    #[test]
    fn newer_version_invalid_remote_returns_none() {
        assert_eq!(
            UpdateCheckHandler::newer_version("1.0.0", "v-garbage"),
            None
        );
    }

    #[test]
    fn newer_version_empty_tag_returns_none() {
        assert_eq!(UpdateCheckHandler::newer_version("1.0.0", ""), None);
    }

    // Prerelease versions (e.g. 0.12.0-rc.1) compare as greater than 0.11.0 per semver spec.
    // This is intentional: users should be notified of release candidates if they appear
    // on the GitHub releases/latest endpoint (which typically only returns stable releases).
    #[test]
    fn newer_version_prerelease_is_notified() {
        assert_eq!(
            UpdateCheckHandler::newer_version("0.11.0", "v0.12.0-rc.1"),
            Some("0.12.0-rc.1".to_owned())
        );
    }
}
