use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskState {
    Pending,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_with_bytes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_with_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    pub capabilities: AgentCapabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_input_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<AgentSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvider {
    pub organization: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
    #[serde(default)]
    pub state_transition_history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: TaskStatus,
    #[serde(rename = "final", default)]
    pub is_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub artifact: Artifact,
    #[serde(rename = "final", default)]
    pub is_final: bool,
}

impl Part {
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: Some(s.into()),
            file: None,
            metadata: None,
        }
    }
}

impl Message {
    #[must_use]
    pub fn user_text(s: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            parts: vec![Part::text(s)],
            message_id: None,
            task_id: None,
            context_id: None,
            metadata: None,
        }
    }

    #[must_use]
    pub fn text_content(&self) -> Option<&str> {
        self.parts.iter().find_map(|p| p.text.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_serde_camel_case() {
        let states = [
            (TaskState::Pending, "\"pending\""),
            (TaskState::Working, "\"working\""),
            (TaskState::InputRequired, "\"inputRequired\""),
            (TaskState::Completed, "\"completed\""),
            (TaskState::Failed, "\"failed\""),
            (TaskState::Canceled, "\"canceled\""),
            (TaskState::Rejected, "\"rejected\""),
        ];
        for (state, expected) in states {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, expected, "serialization mismatch for {state:?}");
            let back: TaskState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn role_serde_lowercase() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Role::Agent).unwrap(), "\"agent\"");
    }

    #[test]
    fn part_text_constructor() {
        let part = Part::text("hello");
        assert_eq!(part.text.as_deref(), Some("hello"));
        assert!(part.file.is_none());
    }

    #[test]
    fn message_user_text_constructor() {
        let msg = Message::user_text("test input");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text_content(), Some("test input"));
    }

    #[test]
    fn message_serde_round_trip() {
        let msg = Message::user_text("hello agent");
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, Role::User);
        assert_eq!(back.text_content(), Some("hello agent"));
    }

    #[test]
    fn task_serde_round_trip() {
        let task = Task {
            id: "task-1".into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Working,
                timestamp: "2025-01-01T00:00:00Z".into(),
                message: None,
            },
            artifacts: vec![],
            history: vec![Message::user_text("do something")],
            metadata: None,
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"contextId\"").not());
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "task-1");
        assert_eq!(back.status.state, TaskState::Working);
        assert_eq!(back.history.len(), 1);
    }

    #[test]
    fn task_skips_empty_vecs_and_none() {
        let task = Task {
            id: "t".into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Pending,
                timestamp: "ts".into(),
                message: None,
            },
            artifacts: vec![],
            history: vec![],
            metadata: None,
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(!json.contains("artifacts"));
        assert!(!json.contains("history"));
        assert!(!json.contains("metadata"));
        assert!(!json.contains("contextId"));
    }

    #[test]
    fn artifact_serde_round_trip() {
        let artifact = Artifact {
            artifact_id: "art-1".into(),
            name: Some("result.txt".into()),
            parts: vec![Part::text("file content")],
            metadata: None,
        };
        let json = serde_json::to_string(&artifact).unwrap();
        assert!(json.contains("\"artifactId\""));
        let back: Artifact = serde_json::from_str(&json).unwrap();
        assert_eq!(back.artifact_id, "art-1");
    }

    #[test]
    fn agent_card_serde_round_trip() {
        let card = AgentCard {
            name: "test-agent".into(),
            description: "A test agent".into(),
            url: "http://localhost:8080".into(),
            version: "0.1.0".into(),
            provider: Some(AgentProvider {
                organization: "TestOrg".into(),
                url: Some("https://test.org".into()),
            }),
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: false,
            },
            default_input_modes: vec!["text".into()],
            default_output_modes: vec!["text".into()],
            skills: vec![AgentSkill {
                id: "skill-1".into(),
                name: "Test Skill".into(),
                description: "Does testing".into(),
                tags: vec!["test".into()],
                examples: vec![],
                input_modes: vec![],
                output_modes: vec![],
            }],
        };
        let json = serde_json::to_string_pretty(&card).unwrap();
        let back: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test-agent");
        assert!(back.capabilities.streaming);
        assert_eq!(back.skills.len(), 1);
    }

    #[test]
    fn task_status_update_event_final_rename() {
        let event = TaskStatusUpdateEvent {
            task_id: "t-1".into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: "ts".into(),
                message: None,
            },
            is_final: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"final\":true"));
        assert!(!json.contains("isFinal"));
        let back: TaskStatusUpdateEvent = serde_json::from_str(&json).unwrap();
        assert!(back.is_final);
    }

    #[test]
    fn task_artifact_update_event_final_rename() {
        let event = TaskArtifactUpdateEvent {
            task_id: "t-1".into(),
            context_id: None,
            artifact: Artifact {
                artifact_id: "a-1".into(),
                name: None,
                parts: vec![Part::text("data")],
                metadata: None,
            },
            is_final: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"final\":false"));
        let back: TaskArtifactUpdateEvent = serde_json::from_str(&json).unwrap();
        assert!(!back.is_final);
    }

    #[test]
    fn file_content_serde() {
        let fc = FileContent {
            name: Some("doc.pdf".into()),
            media_type: Some("application/pdf".into()),
            file_with_bytes: Some("base64data==".into()),
            file_with_uri: None,
        };
        let json = serde_json::to_string(&fc).unwrap();
        assert!(json.contains("\"mediaType\""));
        assert!(json.contains("\"fileWithBytes\""));
        assert!(!json.contains("fileWithUri"));
        let back: FileContent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name.as_deref(), Some("doc.pdf"));
    }

    trait Not {
        fn not(&self) -> bool;
    }
    impl Not for bool {
        fn not(&self) -> bool {
            !*self
        }
    }
}
