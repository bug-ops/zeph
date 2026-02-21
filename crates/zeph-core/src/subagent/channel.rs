use tokio::sync::mpsc;
use zeph_a2a::types::{Message, TaskArtifactUpdateEvent, TaskStatusUpdateEvent};

/// Messages exchanged between the orchestrator and a sub-agent over the in-process channel.
#[derive(Debug)]
pub enum A2aMessage {
    SendMessage(Message),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
    Cancel,
}

/// Orchestrator-side half of the sub-agent channel.
pub struct OrchestratorHalf {
    pub tx: mpsc::Sender<A2aMessage>,
    pub rx: mpsc::Receiver<A2aMessage>,
}

/// Sub-agent-side half of the in-process A2A channel.
pub struct AgentHalf {
    pub tx: mpsc::Sender<A2aMessage>,
    pub rx: mpsc::Receiver<A2aMessage>,
}

/// Create a bidirectional in-process channel between orchestrator and sub-agent.
#[must_use]
pub fn new_channel(buffer: usize) -> (OrchestratorHalf, AgentHalf) {
    let (orch_tx, agent_rx) = mpsc::channel::<A2aMessage>(buffer);
    let (agent_tx, orch_rx) = mpsc::channel::<A2aMessage>(buffer);
    (
        OrchestratorHalf {
            tx: orch_tx,
            rx: orch_rx,
        },
        AgentHalf {
            tx: agent_tx,
            rx: agent_rx,
        },
    )
}

#[cfg(test)]
mod tests {
    use zeph_a2a::types::{Message, Part, Role, TaskState, TaskStatus, TaskStatusUpdateEvent};

    use super::*;

    #[tokio::test]
    async fn send_receive_round_trip() {
        let (mut orch, mut agent) = new_channel(8);

        let msg = A2aMessage::StatusUpdate(TaskStatusUpdateEvent {
            kind: "status-update".into(),
            task_id: "task-1".into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Working,
                timestamp: "2026-01-01T00:00:00Z".into(),
                message: None,
            },
            is_final: false,
        });
        orch.tx.send(msg).await.unwrap();

        let received = agent.rx.recv().await.unwrap();
        assert!(matches!(received, A2aMessage::StatusUpdate(_)));

        let reply = A2aMessage::SendMessage(Message {
            role: Role::Agent,
            parts: vec![Part::Text {
                text: "hello".into(),
                metadata: None,
            }],
            message_id: None,
            task_id: None,
            context_id: None,
            metadata: None,
        });
        agent.tx.send(reply).await.unwrap();

        let back = orch.rx.recv().await.unwrap();
        assert!(matches!(back, A2aMessage::SendMessage(_)));
    }
}
