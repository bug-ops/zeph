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
    /// Base URL for the GitHub releases API. Configurable for testing.
    base_url: String,
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
            base_url: GITHUB_RELEASES_URL.to_owned(),
        }
    }

    /// Override the releases API URL. Intended for tests only.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
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
                .get(&self.base_url)
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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn make_handler(
        current_version: &'static str,
        tx: mpsc::Sender<String>,
        server_url: &str,
    ) -> UpdateCheckHandler {
        UpdateCheckHandler::new(current_version, tx).with_base_url(server_url)
    }

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

    #[tokio::test]
    async fn test_execute_newer_version_sends_notification() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v99.0.0"
            })))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        handler
            .execute(&serde_json::Value::Null)
            .await
            .expect("handler must not return an error");

        let msg = rx.try_recv().expect("notification must be sent");
        assert!(
            msg.contains("99.0.0"),
            "notification should mention new version"
        );
        assert!(
            msg.contains("0.11.0"),
            "notification should mention current version"
        );
    }

    #[tokio::test]
    async fn test_execute_same_version_no_notification() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v0.11.0"
            })))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        handler
            .execute(&serde_json::Value::Null)
            .await
            .expect("handler must not return an error");

        assert!(
            rx.try_recv().is_err(),
            "no notification expected for same version"
        );
    }

    #[tokio::test]
    async fn test_execute_http_404_no_notification_no_panic() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        let result = handler.execute(&serde_json::Value::Null).await;
        assert!(result.is_ok(), "handler must return Ok on 404");
        assert!(rx.try_recv().is_err(), "no notification expected on 404");
    }

    #[tokio::test]
    async fn test_execute_http_429_rate_limit_graceful() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        let result = handler.execute(&serde_json::Value::Null).await;
        assert!(result.is_ok(), "handler must return Ok on 429");
        assert!(rx.try_recv().is_err(), "no notification expected on 429");
    }

    #[tokio::test]
    async fn test_execute_http_500_server_error_graceful() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        let result = handler.execute(&serde_json::Value::Null).await;
        assert!(result.is_ok(), "handler must return Ok on 500");
        assert!(rx.try_recv().is_err(), "no notification expected on 500");
    }

    #[tokio::test]
    async fn test_execute_malformed_json_graceful() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("this is not json {{{"))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        let result = handler.execute(&serde_json::Value::Null).await;
        assert!(result.is_ok(), "handler must return Ok on malformed JSON");
        assert!(
            rx.try_recv().is_err(),
            "no notification expected for malformed JSON"
        );
    }

    #[tokio::test]
    async fn test_execute_missing_tag_name_graceful() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "Latest Release",
                "published_at": "2024-01-01"
            })))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        let result = handler.execute(&serde_json::Value::Null).await;
        assert!(result.is_ok(), "handler must return Ok on missing tag_name");
        assert!(
            rx.try_recv().is_err(),
            "no notification expected for missing tag_name"
        );
    }

    #[tokio::test]
    async fn test_execute_oversized_body_graceful() {
        let server = MockServer::start().await;
        // Body larger than MAX_RESPONSE_BYTES (64 KB): 65 537 bytes
        let large_body = "x".repeat(MAX_RESPONSE_BYTES + 1);
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(large_body))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(1);
        let handler = make_handler("0.11.0", tx, &server.uri());

        let result = handler.execute(&serde_json::Value::Null).await;
        assert!(result.is_ok(), "handler must return Ok for oversized body");
        assert!(
            rx.try_recv().is_err(),
            "no notification expected for oversized body"
        );
    }
}
