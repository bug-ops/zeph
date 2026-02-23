use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use zeph_core::channel::{ChannelMessage, LoopbackChannel, LoopbackHandle};

use crate::error::AcpError;

const MAX_SESSIONS: usize = 100;

pub struct AcpSession {
    pub session_id: String,
    pub input_tx: tokio::sync::mpsc::Sender<ChannelMessage>,
    pub cancel_tx: tokio::sync::watch::Sender<bool>,
}

pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, AcpSession>>>,
}

impl SessionManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new session and return the [`LoopbackChannel`] for the agent loop
    /// and the [`LoopbackHandle`] for the bridge.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::Other`] if the session limit is reached.
    pub async fn create(
        &self,
        session_id: String,
    ) -> Result<
        (
            LoopbackChannel,
            LoopbackHandle,
            tokio::sync::watch::Receiver<bool>,
        ),
        AcpError,
    > {
        let (channel, handle) = LoopbackChannel::pair(64);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let session = AcpSession {
            session_id: session_id.clone(),
            input_tx: handle.input_tx.clone(),
            cancel_tx,
        };

        let mut sessions = self.sessions.write().await;
        if sessions.len() >= MAX_SESSIONS {
            return Err(AcpError::Other("session limit reached".to_owned()));
        }
        sessions.insert(session_id, session);

        Ok((channel, handle, cancel_rx))
    }

    /// # Errors
    ///
    /// Returns [`AcpError::SessionNotFound`] if the session does not exist.
    /// Returns [`AcpError::Transport`] if the session input channel is closed.
    pub async fn send_message(
        &self,
        session_id: &str,
        message: ChannelMessage,
    ) -> Result<(), AcpError> {
        let tx = {
            let sessions = self.sessions.read().await;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| AcpError::SessionNotFound(session_id.to_owned()))?;
            session.input_tx.clone()
        };

        tx.send(message)
            .await
            .map_err(|_| AcpError::Transport("session input channel closed".to_owned()))
    }

    /// # Errors
    ///
    /// Returns [`AcpError::SessionNotFound`] if the session does not exist.
    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| AcpError::SessionNotFound(session_id.to_owned()))?;

        let _ = session.cancel_tx.send(true);
        Ok(())
    }

    pub async fn remove(&self, session_id: &str) {
        self.sessions.write().await.remove(session_id);
    }

    pub async fn exists(&self, session_id: &str) -> bool {
        self.sessions.read().await.contains_key(session_id)
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_session_returns_linked_channels() {
        let mgr = SessionManager::new();
        let (channel, handle, _cancel_rx) = mgr.create("sess-1".to_owned()).await.unwrap();
        drop(channel);
        drop(handle);
        assert!(mgr.exists("sess-1").await);
    }

    #[tokio::test]
    async fn send_message_delivers_to_loopback() {
        let mgr = SessionManager::new();
        let (_channel, handle, _cancel_rx) = mgr.create("sess-2".to_owned()).await.unwrap();

        mgr.send_message(
            "sess-2",
            ChannelMessage {
                text: "hello".to_owned(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();

        let msg = handle.input_tx.reserve().await;
        // Channel is reachable — just verify send didn't error above
        drop(msg);
    }

    #[tokio::test]
    async fn send_message_not_found() {
        let mgr = SessionManager::new();
        let result = mgr
            .send_message(
                "nonexistent",
                ChannelMessage {
                    text: "x".to_owned(),
                    attachments: vec![],
                },
            )
            .await;
        assert!(matches!(result, Err(AcpError::SessionNotFound(_))));
    }

    #[tokio::test]
    async fn cancel_not_found() {
        let mgr = SessionManager::new();
        let result = mgr.cancel("nonexistent").await;
        assert!(matches!(result, Err(AcpError::SessionNotFound(_))));
    }

    #[tokio::test]
    async fn cancel_signals_watch() {
        let mgr = SessionManager::new();
        let (_channel, _handle, mut cancel_rx) = mgr.create("sess-3".to_owned()).await.unwrap();
        assert!(!*cancel_rx.borrow());

        mgr.cancel("sess-3").await.unwrap();
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    #[tokio::test]
    async fn remove_session() {
        let mgr = SessionManager::new();
        let (_channel, _handle, _cancel_rx) = mgr.create("sess-4".to_owned()).await.unwrap();
        assert!(mgr.exists("sess-4").await);
        mgr.remove("sess-4").await;
        assert!(!mgr.exists("sess-4").await);
    }

    #[tokio::test]
    async fn create_session_limit_enforced() {
        let mgr = SessionManager::new();
        for i in 0..MAX_SESSIONS {
            mgr.create(format!("sess-{i}")).await.unwrap();
        }
        let result = mgr.create("sess-overflow".to_owned()).await;
        assert!(matches!(result, Err(AcpError::Other(_))));
    }
}
