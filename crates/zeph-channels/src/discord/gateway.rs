//! Discord Gateway WebSocket client with heartbeat and reconnect.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub channel_id: String,
    pub content: String,
    pub author_id: String,
    pub author_roles: Vec<String>,
}

// Intents: GUILD_MESSAGES (1<<9) | MESSAGE_CONTENT (1<<15) | DIRECT_MESSAGES (1<<12)
const INTENTS: u64 = (1 << 9) | (1 << 15) | (1 << 12);

#[derive(Serialize)]
struct Identify {
    op: u8,
    d: IdentifyData,
}

#[derive(Serialize)]
struct IdentifyData {
    token: String,
    intents: u64,
    properties: IdentifyProperties,
}

#[derive(Serialize)]
struct IdentifyProperties {
    os: String,
    browser: String,
    device: String,
}

#[derive(Serialize)]
struct Heartbeat {
    op: u8,
    d: Option<u64>,
}

/// Spawn the gateway connection loop, returning a receiver of incoming messages.
#[must_use]
pub fn spawn_gateway(token: String) -> mpsc::Receiver<IncomingMessage> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(gateway_loop(token, tx));
    rx
}

async fn gateway_loop(token: String, tx: mpsc::Sender<IncomingMessage>) {
    loop {
        match run_session(&token, &tx).await {
            Ok(()) => {
                tracing::info!("discord gateway session ended, reconnecting...");
            }
            Err(e) => {
                tracing::warn!("discord gateway error: {e:#}, reconnecting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_session(
    token: &str,
    tx: &mpsc::Sender<IncomingMessage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _): (WsStream, _) = connect_async(GATEWAY_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    // Wait for Hello (op 10)
    let hello_text = read_next_text(&mut read).await?;
    let hello: Value = serde_json::from_str(&hello_text)?;
    let op = hello.get("op").and_then(Value::as_u64).unwrap_or(0);
    if op != 10 {
        return Err(format!("expected Hello (op 10), got op {op}").into());
    }

    let heartbeat_interval = hello
        .get("d")
        .and_then(|d| d.get("heartbeat_interval"))
        .and_then(Value::as_u64)
        .unwrap_or(41250);

    // Send Identify
    let identify = Identify {
        op: 2,
        d: IdentifyData {
            token: token.to_owned(),
            intents: INTENTS,
            properties: IdentifyProperties {
                os: "linux".into(),
                browser: "zeph".into(),
                device: "zeph".into(),
            },
        },
    };
    let json = serde_json::to_string(&identify)?;
    write.send(WsMessage::Text(json.into())).await?;

    let mut sequence: Option<u64> = None;
    let mut heartbeat_timer = tokio::time::interval(Duration::from_millis(heartbeat_interval));

    loop {
        tokio::select! {
            _ = heartbeat_timer.tick() => {
                let hb = Heartbeat { op: 1, d: sequence };
                let json = serde_json::to_string(&hb)?;
                write.send(WsMessage::Text(json.into())).await?;
            }
            msg = read.next() => {
                let Some(msg) = msg else { return Ok(()); };
                let text = match msg? {
                    WsMessage::Text(t) => t,
                    WsMessage::Close(_) => return Ok(()),
                    _ => continue,
                };
                let payload: Value = serde_json::from_str(&text)?;
                let op = payload.get("op").and_then(Value::as_u64).unwrap_or(0);
                if let Some(s) = payload.get("s").and_then(Value::as_u64) {
                    sequence = Some(s);
                }
                match op {
                    0 if payload.get("t").and_then(Value::as_str) == Some("MESSAGE_CREATE") => {
                        if let Some(incoming) = payload.get("d").and_then(parse_message_create) {
                            let _ = tx.send(incoming).await;
                        }
                    }
                    7 | 9 => return Ok(()), // Reconnect / Invalid Session
                    _ => {}
                }
            }
        }
    }
}

fn parse_message_create(d: &Value) -> Option<IncomingMessage> {
    let content = d.get("content")?.as_str()?.to_owned();
    let author = d.get("author")?;
    let author_id = author.get("id")?.as_str()?.to_owned();

    if author.get("bot").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }

    let channel_id = d.get("channel_id")?.as_str()?.to_owned();
    let author_roles: Vec<String> = d
        .get("member")
        .and_then(|m| m.get("roles"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Some(IncomingMessage {
        channel_id,
        content,
        author_id,
        author_roles,
    })
}

async fn read_next_text<S>(read: &mut S) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    S: futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let Some(msg) = read.next().await else {
            return Err("gateway connection closed".into());
        };
        match msg? {
            WsMessage::Text(t) => return Ok(t.to_string()),
            WsMessage::Close(_) => return Err("gateway closed".into()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_create_valid() {
        let d: Value = serde_json::json!({
            "content": "hello world",
            "author": { "id": "123", "bot": false },
            "channel_id": "456",
            "member": { "roles": ["admin", "mod"] }
        });
        let msg = parse_message_create(&d).unwrap();
        assert_eq!(msg.content, "hello world");
        assert_eq!(msg.author_id, "123");
        assert_eq!(msg.channel_id, "456");
        assert_eq!(msg.author_roles, vec!["admin", "mod"]);
    }

    #[test]
    fn parse_message_create_skips_bot() {
        let d: Value = serde_json::json!({
            "content": "bot msg",
            "author": { "id": "123", "bot": true },
            "channel_id": "456"
        });
        assert!(parse_message_create(&d).is_none());
    }

    #[test]
    fn parse_message_create_missing_content() {
        let d: Value = serde_json::json!({
            "author": { "id": "123" },
            "channel_id": "456"
        });
        assert!(parse_message_create(&d).is_none());
    }

    #[test]
    fn parse_message_create_missing_author() {
        let d: Value = serde_json::json!({
            "content": "hello",
            "channel_id": "456"
        });
        assert!(parse_message_create(&d).is_none());
    }

    #[test]
    fn parse_message_create_no_member_roles() {
        let d: Value = serde_json::json!({
            "content": "hello",
            "author": { "id": "123" },
            "channel_id": "456"
        });
        let msg = parse_message_create(&d).unwrap();
        assert!(msg.author_roles.is_empty());
    }

    #[test]
    fn intents_value() {
        assert_eq!(INTENTS, (1 << 9) | (1 << 15) | (1 << 12));
    }

    #[test]
    fn spawn_gateway_returns_receiver() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _rx = rt.block_on(async { spawn_gateway("invalid-token".into()) });
    }

    #[test]
    fn incoming_message_clone() {
        let msg = IncomingMessage {
            channel_id: "ch".into(),
            content: "text".into(),
            author_id: "user".into(),
            author_roles: vec!["role".into()],
        };
        let cloned = msg.clone();
        assert_eq!(cloned.channel_id, "ch");
        assert_eq!(cloned.content, "text");
    }
}
