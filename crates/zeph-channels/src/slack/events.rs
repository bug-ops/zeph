//! Slack Events API webhook handler with request signature verification.

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct SlackFile {
    pub url: String,
    pub filename: Option<String>,
    pub mimetype: String,
}

#[derive(Clone)]
pub struct IncomingMessage {
    pub channel_id: String,
    pub text: String,
    pub user_id: String,
    pub files: Vec<SlackFile>,
}

#[derive(Clone)]
struct EventState {
    signing_secret: String,
    tx: mpsc::Sender<IncomingMessage>,
    bot_user_id: String,
    allowed_user_ids: Vec<String>,
    allowed_channel_ids: Vec<String>,
}

/// Spawn the Slack Events API webhook server.
#[must_use]
pub fn spawn_event_server(
    host: String,
    port: u16,
    signing_secret: String,
    bot_user_id: String,
    allowed_user_ids: Vec<String>,
    allowed_channel_ids: Vec<String>,
) -> mpsc::Receiver<IncomingMessage> {
    let (tx, rx) = mpsc::channel(64);
    let state = EventState {
        signing_secret,
        tx,
        bot_user_id,
        allowed_user_ids,
        allowed_channel_ids,
    };

    tokio::spawn(async move {
        let app = Router::new()
            .route("/slack/events", post(handle_event))
            .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
            .with_state(state);
        let listener = match tokio::net::TcpListener::bind(format!("{host}:{port}")).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("failed to bind slack events server on port {port}: {e}");
                return;
            }
        };
        tracing::info!("slack events server listening on port {port}");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("slack events server error: {e}");
        }
    });

    rx
}

async fn handle_event(
    State(state): State<EventState>,
    headers: HeaderMap,
    body: String,
) -> Result<String, StatusCode> {
    verify_signature(&state.signing_secret, &headers, &body)?;

    let payload: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;

    let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "url_verification" => {
            let challenge = payload
                .get("challenge")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Ok(challenge.to_owned())
        }
        "event_callback" => {
            if let Some(event) = payload.get("event") {
                let subtype = event.get("subtype").and_then(|v| v.as_str());
                let event_type = event.get("type").and_then(|v| v.as_str());

                if event_type == Some("message")
                    && (subtype.is_none() || subtype == Some("file_share"))
                {
                    let user = event.get("user").and_then(|v| v.as_str()).unwrap_or("");
                    let channel = event.get("channel").and_then(|v| v.as_str()).unwrap_or("");
                    let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");

                    // Skip bot's own messages
                    if !state.bot_user_id.is_empty() && user == state.bot_user_id {
                        return Ok(String::new());
                    }

                    // Authorization checks
                    if !state.allowed_channel_ids.is_empty()
                        && !state.allowed_channel_ids.iter().any(|c| c == channel)
                    {
                        return Ok(String::new());
                    }
                    if !state.allowed_user_ids.is_empty()
                        && !state.allowed_user_ids.iter().any(|u| u == user)
                    {
                        return Ok(String::new());
                    }

                    let files = parse_audio_files(event);

                    let _ = state
                        .tx
                        .send(IncomingMessage {
                            channel_id: channel.to_owned(),
                            text: text.to_owned(),
                            user_id: user.to_owned(),
                            files,
                        })
                        .await;
                }
            }
            Ok(String::new())
        }
        _ => Ok(String::new()),
    }
}

fn is_audio_mime(mime: &str) -> bool {
    mime.starts_with("audio/") || mime == "video/webm"
}

fn parse_audio_files(event: &Value) -> Vec<SlackFile> {
    event
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let mime = f.get("mimetype")?.as_str()?;
                    if !is_audio_mime(mime) {
                        return None;
                    }
                    Some(SlackFile {
                        url: f.get("url_private_download")?.as_str()?.to_owned(),
                        filename: f.get("name").and_then(|v| v.as_str()).map(String::from),
                        mimetype: mime.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn verify_signature(
    signing_secret: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<(), StatusCode> {
    let timestamp = headers
        .get("X-Slack-Request-Timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // SEC-002: Reject requests older than 5 minutes to prevent replay attacks
    if let Ok(ts) = timestamp.parse::<i64>() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().cast_signed())
            .unwrap_or(0);
        if (now - ts).abs() > 300 {
            return Err(StatusCode::UNAUTHORIZED);
        }
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let provided_sig = headers
        .get("X-Slack-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let base_string = format!("v0:{timestamp}:{body}");
    let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mac.update(base_string.as_bytes());
    let result = mac.finalize().into_bytes();
    let hex = result.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    });
    let expected = format!("v0={hex}");

    if expected.as_bytes().ct_eq(provided_sig.as_bytes()).into() {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use hmac::Mac;

    fn current_timestamp() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string()
    }

    fn compute_signature(secret: &str, timestamp: &str, body: &str) -> String {
        let base_string = format!("v0:{timestamp}:{body}");
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(base_string.as_bytes());
        let result = mac.finalize().into_bytes();
        let hex = result.iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
        format!("v0={hex}")
    }

    #[test]
    fn verify_signature_valid() {
        let secret = "test-secret";
        let timestamp = current_timestamp();
        let body = r#"{"type":"url_verification"}"#;
        let sig = compute_signature(secret, &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        assert!(verify_signature(secret, &headers, body).is_ok());
    }

    #[test]
    fn verify_signature_invalid() {
        let timestamp = current_timestamp();
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_static("v0=deadbeef"));

        let result = verify_signature("secret", &headers, "body");
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_signature_missing_timestamp() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Slack-Signature", HeaderValue::from_static("v0=abc"));

        let result = verify_signature("secret", &headers, "body");
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_signature_missing_signature() {
        let timestamp = current_timestamp();
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );

        let result = verify_signature("secret", &headers, "body");
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_signature_rejects_stale_timestamp() {
        let secret = "test-secret";
        let stale_ts = "1234567890";
        let body = "test";
        let sig = compute_signature(secret, stale_ts, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_static(stale_ts),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let result = verify_signature(secret, &headers, body);
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn handle_event_url_verification() {
        let (tx, _rx) = mpsc::channel(16);
        let state = EventState {
            signing_secret: "secret".into(),
            tx,
            bot_user_id: String::new(),
            allowed_user_ids: vec![],
            allowed_channel_ids: vec![],
        };

        let body = r#"{"type":"url_verification","challenge":"test-challenge"}"#;
        let timestamp = current_timestamp();
        let sig = compute_signature("secret", &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let result = handle_event(State(state), headers, body.to_owned()).await;
        assert_eq!(result.unwrap(), "test-challenge");
    }

    #[tokio::test]
    async fn handle_event_message_dispatched() {
        let (tx, mut rx) = mpsc::channel(16);
        let state = EventState {
            signing_secret: "secret".into(),
            tx,
            bot_user_id: String::new(),
            allowed_user_ids: vec![],
            allowed_channel_ids: vec![],
        };

        let body = r#"{"type":"event_callback","event":{"type":"message","user":"U123","channel":"C456","text":"hi"}}"#;
        let timestamp = current_timestamp();
        let sig = compute_signature("secret", &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let result = handle_event(State(state), headers, body.to_owned()).await;
        assert!(result.is_ok());

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.user_id, "U123");
        assert_eq!(msg.channel_id, "C456");
        assert_eq!(msg.text, "hi");
        assert!(msg.files.is_empty());
    }

    #[tokio::test]
    async fn handle_event_filters_bot_messages() {
        let (tx, mut rx) = mpsc::channel(16);
        let state = EventState {
            signing_secret: "secret".into(),
            tx,
            bot_user_id: "BOT".into(),
            allowed_user_ids: vec![],
            allowed_channel_ids: vec![],
        };

        let body = r#"{"type":"event_callback","event":{"type":"message","user":"BOT","channel":"C1","text":"bot msg"}}"#;
        let timestamp = current_timestamp();
        let sig = compute_signature("secret", &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let _ = handle_event(State(state), headers, body.to_owned()).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_event_filters_by_allowed_user() {
        let (tx, mut rx) = mpsc::channel(16);
        let state = EventState {
            signing_secret: "secret".into(),
            tx,
            bot_user_id: String::new(),
            allowed_user_ids: vec!["U_ALLOWED".into()],
            allowed_channel_ids: vec![],
        };

        let body = r#"{"type":"event_callback","event":{"type":"message","user":"U_OTHER","channel":"C1","text":"hi"}}"#;
        let timestamp = current_timestamp();
        let sig = compute_signature("secret", &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let _ = handle_event(State(state), headers, body.to_owned()).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_event_filters_by_allowed_channel() {
        let (tx, mut rx) = mpsc::channel(16);
        let state = EventState {
            signing_secret: "secret".into(),
            tx,
            bot_user_id: String::new(),
            allowed_user_ids: vec![],
            allowed_channel_ids: vec!["C_ALLOWED".into()],
        };

        let body = r#"{"type":"event_callback","event":{"type":"message","user":"U1","channel":"C_OTHER","text":"hi"}}"#;
        let timestamp = current_timestamp();
        let sig = compute_signature("secret", &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let _ = handle_event(State(state), headers, body.to_owned()).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_event_skips_message_with_subtype() {
        let (tx, mut rx) = mpsc::channel(16);
        let state = EventState {
            signing_secret: "secret".into(),
            tx,
            bot_user_id: String::new(),
            allowed_user_ids: vec![],
            allowed_channel_ids: vec![],
        };

        let body = r#"{"type":"event_callback","event":{"type":"message","subtype":"message_changed","user":"U1","channel":"C1","text":"hi"}}"#;
        let timestamp = current_timestamp();
        let sig = compute_signature("secret", &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let _ = handle_event(State(state), headers, body.to_owned()).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_event_file_share_with_audio() {
        let (tx, mut rx) = mpsc::channel(16);
        let state = EventState {
            signing_secret: "secret".into(),
            tx,
            bot_user_id: String::new(),
            allowed_user_ids: vec![],
            allowed_channel_ids: vec![],
        };

        let body = r#"{"type":"event_callback","event":{"type":"message","subtype":"file_share","user":"U1","channel":"C1","text":"","files":[{"name":"voice.webm","mimetype":"audio/webm","url_private_download":"https://files.slack.com/voice.webm"}]}}"#;
        let timestamp = current_timestamp();
        let sig = compute_signature("secret", &timestamp, body);

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Slack-Request-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert("X-Slack-Signature", HeaderValue::from_str(&sig).unwrap());

        let result = handle_event(State(state), headers, body.to_owned()).await;
        assert!(result.is_ok());

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.files.len(), 1);
        assert_eq!(msg.files[0].mimetype, "audio/webm");
        assert_eq!(msg.files[0].filename.as_deref(), Some("voice.webm"));
    }

    #[test]
    fn parse_audio_files_filters_non_audio() {
        let event: Value = serde_json::from_str(
            r#"{"files":[
                {"name":"img.png","mimetype":"image/png","url_private_download":"https://x/img"},
                {"name":"voice.ogg","mimetype":"audio/ogg","url_private_download":"https://x/voice"}
            ]}"#,
        )
        .unwrap();
        let files = parse_audio_files(&event);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].mimetype, "audio/ogg");
    }

    #[test]
    fn parse_audio_files_accepts_video_webm() {
        let event: Value = serde_json::from_str(
            r#"{"files":[{"name":"v.webm","mimetype":"video/webm","url_private_download":"https://x/v"}]}"#,
        )
        .unwrap();
        let files = parse_audio_files(&event);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn parse_audio_files_empty_when_no_files() {
        let event: Value = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert!(parse_audio_files(&event).is_empty());
    }

    #[test]
    fn is_audio_mime_cases() {
        assert!(is_audio_mime("audio/webm"));
        assert!(is_audio_mime("audio/mpeg"));
        assert!(is_audio_mime("video/webm"));
        assert!(!is_audio_mime("video/mp4"));
        assert!(!is_audio_mime("image/png"));
    }
}
