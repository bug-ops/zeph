// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wire-format types for the A2A protocol.
//!
//! All types in this module are serialized using `camelCase` JSON field names to comply with
//! the A2A specification. They are re-exported from the crate root via `pub use types::*`.

use serde::{Deserialize, Serialize};

/// Lifecycle state of an A2A task.
///
/// The state machine progresses roughly as:
/// `Submitted` → `Working` → `Completed` (success) or `Failed` (error).
/// `InputRequired` pauses processing until the caller sends more data.
/// Terminal states (`Completed`, `Failed`, `Canceled`, `Rejected`) cannot be resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// Task has been received and queued but processing has not started.
    #[serde(rename = "submitted")]
    Submitted,
    /// The agent is actively processing the task.
    #[serde(rename = "working")]
    Working,
    /// Processing is paused; the agent needs more input from the caller.
    #[serde(rename = "input-required")]
    InputRequired,
    /// Task finished successfully. Terminal state.
    #[serde(rename = "completed")]
    Completed,
    /// Task encountered an unrecoverable error. Terminal state.
    #[serde(rename = "failed")]
    Failed,
    /// Task was canceled by the caller. Terminal state.
    #[serde(rename = "canceled")]
    Canceled,
    /// Task was rejected by the agent (e.g., policy violation). Terminal state.
    #[serde(rename = "rejected")]
    Rejected,
    /// The agent requires authentication before proceeding.
    #[serde(rename = "auth-required")]
    AuthRequired,
    /// State could not be determined (e.g., deserialization of a future protocol version).
    #[serde(rename = "unknown")]
    Unknown,
}

/// A unit of work dispatched to or created by an A2A agent.
///
/// Tasks are the central concept in the A2A protocol. A caller creates a task by sending
/// a [`Message`] via `message/send`. The agent processes it and returns the completed
/// [`Task`] with [`artifacts`](Task::artifacts) and final [`status`](Task::status).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Unique task identifier, assigned by the server on creation.
    pub id: String,
    /// Optional session/conversation context shared across multiple tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// Current lifecycle state plus timestamp.
    pub status: TaskStatus,
    /// Output artifacts produced by the agent for this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    /// Conversation history for this task (may be limited by `historyLength` on retrieval).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
    /// Arbitrary key-value metadata for extension without schema changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Current lifecycle state of a task, including when the state was last updated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    /// The task's current lifecycle state.
    pub state: TaskState,
    /// RFC 3339 timestamp of the last state transition.
    pub timestamp: String,
    /// Optional agent message accompanying the state transition (e.g., an error description).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

/// Participant role in a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Message originated from the human user or calling system.
    User,
    /// Message originated from the AI agent.
    Agent,
}

/// A single message in the A2A conversation, consisting of one or more [`Part`]s.
///
/// Messages carry content between the caller and the agent. Use [`Message::user_text`]
/// to construct a simple single-part text message from the user side.
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::{Message, Part, Role};
///
/// let msg = Message::user_text("Summarize this document.");
/// assert_eq!(msg.role, Role::User);
/// assert_eq!(msg.text_content(), Some("Summarize this document."));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Who sent this message.
    pub role: Role,
    /// Content parts; at least one is expected for meaningful messages.
    pub parts: Vec<Part>,
    /// Optional stable identifier for this specific message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Task this message belongs to (set by the server on responses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Conversation context shared with other tasks in the same session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// Arbitrary extension metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// A typed content part within a [`Message`] or [`Artifact`].
///
/// The A2A spec uses a tagged union (`"kind"` discriminant) so that clients and agents
/// can safely ignore part types they do not understand. Use [`Part::text`] to construct
/// the most common variant without boilerplate.
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::{Part};
///
/// let text_part = Part::text("Hello!");
/// assert!(matches!(text_part, Part::Text { .. }));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Part {
    /// Plain or markdown text content.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// Binary or URI-referenced file attachment.
    File {
        file: FileContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// Arbitrary structured JSON data (e.g., tool call results, structured output).
    Data {
        data: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

/// File attachment within a [`Part::File`], specified either as inline base64 bytes or a URI.
///
/// Exactly one of `file_with_bytes` or `file_with_uri` should be set. If both are present,
/// the server's behavior is unspecified by the protocol — prefer one field per message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    /// Human-readable filename (e.g., `"report.pdf"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// MIME type of the file (e.g., `"application/pdf"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Standard base64-encoded file content for inline transfer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_with_bytes: Option<String>,
    /// URL referencing the file for out-of-band retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_with_uri: Option<String>,
}

/// A named output produced by an agent during task processing.
///
/// Artifacts are the primary mechanism for agents to return results. They can contain
/// text, files, or structured data, and are accumulated on the [`Task`] as the agent runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// Unique artifact identifier within the task.
    pub artifact_id: String,
    /// Optional human-readable label for the artifact (e.g., `"generated_report"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Content parts composing the artifact.
    pub parts: Vec<Part>,
    /// Arbitrary extension metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Capability advertisement document served at `/.well-known/agent.json`.
///
/// [`AgentCard`] describes an agent's identity, endpoint, skills, and protocol capabilities.
/// It is the primary discovery mechanism — callers fetch the card before sending messages.
///
/// Prefer constructing cards via [`AgentCardBuilder`](crate::AgentCardBuilder) to get correct
/// defaults (including the current [`A2A_PROTOCOL_VERSION`](crate::A2A_PROTOCOL_VERSION)).
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::AgentCardBuilder;
///
/// let card = AgentCardBuilder::new("my-agent", "http://localhost:8080", "0.1.0")
///     .description("An AI assistant")
///     .streaming(true)
///     .build();
///
/// assert_eq!(card.name, "my-agent");
/// assert!(card.capabilities.streaming);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    /// Human-readable agent name.
    pub name: String,
    /// Short description of the agent's purpose.
    pub description: String,
    /// Base URL of the A2A endpoint (without path suffix).
    pub url: String,
    /// Agent software version string (semver recommended).
    pub version: String,
    /// A2A protocol version the agent implements (see [`A2A_PROTOCOL_VERSION`](crate::A2A_PROTOCOL_VERSION)).
    pub protocol_version: String,
    /// Optional organization that built or operates the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    /// Flags indicating which A2A capabilities the agent supports.
    pub capabilities: AgentCapabilities,
    /// MIME types or mode identifiers the agent accepts as input (e.g., `"text/plain"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_input_modes: Vec<String>,
    /// MIME types or mode identifiers the agent can produce as output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_output_modes: Vec<String>,
    /// Discrete skills the agent exposes, each with its own examples and mode overrides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<AgentSkill>,
}

/// Organization that built or operates an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvider {
    /// Name of the organization (e.g., `"Acme Corp"`).
    pub organization: String,
    /// Optional URL for the organization's public homepage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Boolean flags advertising which A2A protocol extensions an agent supports.
///
/// The three protocol-defined fields (`streaming`, `push_notifications`,
/// `state_transition_history`) are part of the A2A specification. The modality fields
/// (`images`, `audio`, `files`) are Zeph forward-compatible extensions — they default to
/// `false` so that peers that do not understand them can safely ignore the fields via
/// `#[serde(default)]`. If the A2A spec standardises different names for these capabilities
/// in a future revision, a follow-up PR can add the canonical names without breaking
/// existing serialised cards.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct AgentCapabilities {
    /// Agent supports `message/stream` for real-time SSE output.
    #[serde(default)]
    pub streaming: bool,
    /// Agent supports server-initiated push notifications.
    #[serde(default)]
    pub push_notifications: bool,
    /// Agent includes full state-transition history in task responses.
    #[serde(default)]
    pub state_transition_history: bool,
    /// Agent can receive and send `Part::File` entries with `image/*` media types (#3326).
    ///
    /// Defaults to `false`. Set via [`AgentCardBuilder::images`](crate::AgentCardBuilder::images).
    #[serde(default)]
    pub images: bool,
    /// Agent can receive and send `Part::File` entries with `audio/*` media types (#3326).
    ///
    /// Defaults to `false`. Set via [`AgentCardBuilder::audio`](crate::AgentCardBuilder::audio).
    #[serde(default)]
    pub audio: bool,
    /// Agent can receive and send non-media file attachments via `Part::File` (#3326).
    ///
    /// Defaults to `false`. Set via [`AgentCardBuilder::files`](crate::AgentCardBuilder::files).
    #[serde(default)]
    pub files: bool,
}

/// A discrete skill or capability advertised by an agent in its [`AgentCard`].
///
/// Skills allow callers to discover what a specific agent is good at before sending a task,
/// enabling smarter agent routing and delegation decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    /// Machine-readable skill identifier (e.g., `"code-review"`).
    pub id: String,
    /// Human-readable skill name.
    pub name: String,
    /// Explanation of what this skill does and when to use it.
    pub description: String,
    /// Searchable labels for capability-based routing (e.g., `["rust", "security"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Example prompts or queries that invoke this skill well.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    /// Input mode overrides for this skill (falls back to card-level defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,
    /// Output mode overrides for this skill (falls back to card-level defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,
}

/// SSE event emitted by the server when a task's [`TaskStatus`] changes.
///
/// Delivered over the `POST /a2a/stream` SSE channel. The `is_final` flag signals
/// that the stream will not emit further events after this one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    /// Always `"status-update"` — used by clients to distinguish event types.
    #[serde(default = "kind_status_update")]
    pub kind: String,
    /// The task whose status changed.
    pub task_id: String,
    /// Conversation context for the task, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// New status value including state and timestamp.
    pub status: TaskStatus,
    /// If `true`, this is the last event in the stream.
    #[serde(rename = "final", default)]
    pub is_final: bool,
}

fn kind_status_update() -> String {
    "status-update".into()
}

/// SSE event emitted by the server when a new [`Artifact`] is produced or updated.
///
/// Delivered over the `POST /a2a/stream` SSE channel alongside [`TaskStatusUpdateEvent`]s.
/// The `is_final` flag on the artifact event indicates that the artifact is complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    /// Always `"artifact-update"` — used by clients to distinguish event types.
    #[serde(default = "kind_artifact_update")]
    pub kind: String,
    /// The task that produced this artifact.
    pub task_id: String,
    /// Conversation context for the task, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// The artifact content (may be a partial chunk if `is_final` is `false`).
    pub artifact: Artifact,
    /// If `true`, the artifact is fully produced and no further chunks will follow.
    #[serde(rename = "final", default)]
    pub is_final: bool,
}

fn kind_artifact_update() -> String {
    "artifact-update".into()
}

impl Part {
    /// Construct a plain-text [`Part`] with no metadata.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::Part;
    ///
    /// let p = Part::text("Hello, world!");
    /// assert!(matches!(p, Part::Text { ref text, .. } if text == "Hello, world!"));
    /// ```
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text {
            text: s.into(),
            metadata: None,
        }
    }
}

impl Message {
    /// Construct a single-part user text message.
    ///
    /// This is the most common way to build an outgoing message when calling a peer agent.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::{Message, Role};
    ///
    /// let msg = Message::user_text("Please summarize this.");
    /// assert_eq!(msg.role, Role::User);
    /// assert_eq!(msg.text_content(), Some("Please summarize this."));
    /// ```
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

    /// Return the text of the first [`Part::Text`] in this message, if any.
    ///
    /// For messages that may contain multiple text parts, prefer [`all_text_content`](Self::all_text_content).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::Message;
    ///
    /// let msg = Message::user_text("hello");
    /// assert_eq!(msg.text_content(), Some("hello"));
    /// ```
    #[must_use]
    pub fn text_content(&self) -> Option<&str> {
        self.parts.iter().find_map(|p| match p {
            Part::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// Collect and concatenate all `Part::Text` entries in order.
    ///
    /// Unlike `text_content` which returns only the first text part, this method
    /// preserves the full message when an agent sends multiple text parts.
    /// Returns an empty string if the message contains no text parts.
    #[must_use]
    pub fn all_text_content(&self) -> String {
        let parts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_serde() {
        let states = [
            (TaskState::Submitted, "\"submitted\""),
            (TaskState::Working, "\"working\""),
            (TaskState::InputRequired, "\"input-required\""),
            (TaskState::Completed, "\"completed\""),
            (TaskState::Failed, "\"failed\""),
            (TaskState::Canceled, "\"canceled\""),
            (TaskState::Rejected, "\"rejected\""),
            (TaskState::AuthRequired, "\"auth-required\""),
            (TaskState::Unknown, "\"unknown\""),
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
        assert_eq!(
            part,
            Part::Text {
                text: "hello".into(),
                metadata: None
            }
        );
    }

    #[test]
    fn part_kind_serde() {
        let text_part = Part::text("hello");
        let json = serde_json::to_string(&text_part).unwrap();
        assert!(json.contains("\"kind\":\"text\""));
        assert!(json.contains("\"text\":\"hello\""));
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(back, text_part);

        let file_part = Part::File {
            file: FileContent {
                name: Some("doc.pdf".into()),
                media_type: None,
                file_with_bytes: None,
                file_with_uri: Some("https://example.com/doc.pdf".into()),
            },
            metadata: None,
        };
        let json = serde_json::to_string(&file_part).unwrap();
        assert!(json.contains("\"kind\":\"file\""));
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(back, file_part);

        let data_part = Part::Data {
            data: serde_json::json!({"key": "value"}),
            metadata: None,
        };
        let json = serde_json::to_string(&data_part).unwrap();
        assert!(json.contains("\"kind\":\"data\""));
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data_part);
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
                state: TaskState::Submitted,
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
            protocol_version: "0.2.1".into(),
            provider: Some(AgentProvider {
                organization: "TestOrg".into(),
                url: Some("https://test.org".into()),
            }),
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: false,
                images: false,
                audio: false,
                files: false,
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
    fn task_status_update_event_serde() {
        let event = TaskStatusUpdateEvent {
            kind: "status-update".into(),
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
        assert!(json.contains("\"kind\":\"status-update\""));
        let back: TaskStatusUpdateEvent = serde_json::from_str(&json).unwrap();
        assert!(back.is_final);
        assert_eq!(back.kind, "status-update");
    }

    #[test]
    fn task_artifact_update_event_serde() {
        let event = TaskArtifactUpdateEvent {
            kind: "artifact-update".into(),
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
        assert!(json.contains("\"kind\":\"artifact-update\""));
        let back: TaskArtifactUpdateEvent = serde_json::from_str(&json).unwrap();
        assert!(!back.is_final);
        assert_eq!(back.kind, "artifact-update");
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

    #[test]
    fn all_text_content_single_part() {
        let msg = Message::user_text("hello world");
        assert_eq!(msg.all_text_content(), "hello world");
    }

    #[test]
    fn all_text_content_multiple_parts_joined() {
        let msg = Message {
            role: Role::User,
            parts: vec![
                Part::text("first"),
                Part::text("second"),
                Part::text("third"),
            ],
            message_id: None,
            task_id: None,
            context_id: None,
            metadata: None,
        };
        assert_eq!(msg.all_text_content(), "first\n\nsecond\n\nthird");
    }

    #[test]
    fn all_text_content_no_text_parts_returns_empty() {
        let msg = Message {
            role: Role::User,
            parts: vec![],
            message_id: None,
            task_id: None,
            context_id: None,
            metadata: None,
        };
        assert_eq!(msg.all_text_content(), "");
    }

    #[test]
    fn all_text_content_skips_non_text_parts() {
        let msg = Message {
            role: Role::User,
            parts: vec![
                Part::text("text-only"),
                Part::Data {
                    data: serde_json::json!({"key": "val"}),
                    metadata: None,
                },
            ],
            message_id: None,
            task_id: None,
            context_id: None,
            metadata: None,
        };
        assert_eq!(msg.all_text_content(), "text-only");
    }

    #[test]
    fn agent_capabilities_default_has_no_modalities() {
        let caps = AgentCapabilities::default();
        assert!(!caps.images);
        assert!(!caps.audio);
        assert!(!caps.files);
    }

    #[test]
    fn agent_capabilities_modality_fields_serialize() {
        let caps = AgentCapabilities {
            streaming: false,
            push_notifications: false,
            state_transition_history: false,
            images: false,
            audio: false,
            files: false,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("\"images\":false"));
        assert!(json.contains("\"audio\":false"));
        assert!(json.contains("\"files\":false"));
    }

    #[test]
    fn deserialize_legacy_capabilities_uses_modality_defaults() {
        // Old-format JSON with only the core A2A fields — modality fields must default to false.
        let json = r#"{"streaming": true}"#;
        let caps: AgentCapabilities = serde_json::from_str(json).unwrap();
        assert!(caps.streaming);
        assert!(!caps.images);
        assert!(!caps.audio);
        assert!(!caps.files);
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
