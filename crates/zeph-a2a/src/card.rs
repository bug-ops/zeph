// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Builder for [`AgentCard`] capability advertisement documents.

use crate::types::{AgentCapabilities, AgentCard, AgentProvider, AgentSkill};

/// Builder for [`AgentCard`] — the capability document served at `/.well-known/agent.json`.
///
/// [`AgentCard`] has many optional fields and nested structs. `AgentCardBuilder` provides
/// sensible defaults (streaming disabled, no skills, current protocol version) and a
/// fluent API so callers only specify what they need.
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::{AgentCardBuilder, types::AgentSkill};
///
/// let card = AgentCardBuilder::new("my-agent", "http://localhost:8080", "1.0.0")
///     .description("An AI agent that answers questions")
///     .streaming(true)
///     .provider("Acme Corp", Some("https://acme.example.com".into()))
///     .default_input_modes(vec!["text/plain".into()])
///     .default_output_modes(vec!["text/plain".into()])
///     .skill(AgentSkill {
///         id: "qa".into(),
///         name: "Q&A".into(),
///         description: "Answers questions".into(),
///         tags: vec!["questions".into()],
///         examples: vec!["What is Rust?".into()],
///         input_modes: vec![],
///         output_modes: vec![],
///     })
///     .build();
///
/// assert_eq!(card.name, "my-agent");
/// assert!(card.capabilities.streaming);
/// assert_eq!(card.skills.len(), 1);
/// ```
pub struct AgentCardBuilder {
    name: String,
    description: String,
    url: String,
    version: String,
    protocol_version: String,
    capabilities: AgentCapabilities,
    skills: Vec<AgentSkill>,
    provider: Option<AgentProvider>,
    input_modes: Vec<String>,
    output_modes: Vec<String>,
}

impl AgentCardBuilder {
    /// Create a new builder with the three required fields.
    ///
    /// All optional fields default to empty/disabled. The `protocol_version` is set to
    /// [`A2A_PROTOCOL_VERSION`](crate::A2A_PROTOCOL_VERSION) automatically.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            url: url.into(),
            version: version.into(),
            protocol_version: crate::A2A_PROTOCOL_VERSION.to_owned(),
            capabilities: AgentCapabilities::default(),
            skills: Vec::new(),
            provider: None,
            input_modes: Vec::new(),
            output_modes: Vec::new(),
        }
    }

    /// Set the agent's human-readable description.
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Advertise whether the agent supports `message/stream` (SSE streaming).
    #[must_use]
    pub fn streaming(mut self, enabled: bool) -> Self {
        self.capabilities.streaming = enabled;
        self
    }

    /// Advertise whether the agent supports server-initiated push notifications.
    #[must_use]
    pub fn push_notifications(mut self, enabled: bool) -> Self {
        self.capabilities.push_notifications = enabled;
        self
    }

    /// Declare image-modality capability on the card.
    ///
    /// When `on` is `true`, the card advertises that the agent can receive and send
    /// `Part::File` entries whose `media_type` is in the `image/*` family.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::AgentCardBuilder;
    ///
    /// let card = AgentCardBuilder::new("vision-agent", "http://localhost:8080", "1.0.0")
    ///     .images(true)
    ///     .build();
    ///
    /// assert!(card.capabilities.images);
    /// ```
    #[must_use]
    pub fn images(mut self, on: bool) -> Self {
        self.capabilities.images = on;
        self
    }

    /// Declare audio-modality capability on the card.
    ///
    /// When `on` is `true`, the card advertises that the agent can receive and send
    /// `Part::File` entries whose `media_type` is in the `audio/*` family.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::AgentCardBuilder;
    ///
    /// let card = AgentCardBuilder::new("audio-agent", "http://localhost:8080", "1.0.0")
    ///     .audio(true)
    ///     .build();
    ///
    /// assert!(card.capabilities.audio);
    /// ```
    #[must_use]
    pub fn audio(mut self, on: bool) -> Self {
        self.capabilities.audio = on;
        self
    }

    /// Declare file-attachment capability on the card.
    ///
    /// When `on` is `true`, the card advertises that the agent can receive and send
    /// non-media file attachments via `Part::File` (e.g., documents, archives).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::AgentCardBuilder;
    ///
    /// let card = AgentCardBuilder::new("file-agent", "http://localhost:8080", "1.0.0")
    ///     .files(true)
    ///     .build();
    ///
    /// assert!(card.capabilities.files);
    /// ```
    #[must_use]
    pub fn files(mut self, on: bool) -> Self {
        self.capabilities.files = on;
        self
    }

    /// Add a skill to the card. Can be called multiple times to add multiple skills.
    #[must_use]
    pub fn skill(mut self, skill: AgentSkill) -> Self {
        self.skills.push(skill);
        self
    }

    /// Set the organization that built or operates this agent.
    ///
    /// Pass `None` for the URL if no public URL exists.
    #[must_use]
    pub fn provider(mut self, org: impl Into<String>, url: impl Into<Option<String>>) -> Self {
        self.provider = Some(AgentProvider {
            organization: org.into(),
            url: url.into(),
        });
        self
    }

    /// Set the default input modes supported by this agent (e.g., `["text/plain"]`).
    #[must_use]
    pub fn default_input_modes(mut self, modes: Vec<String>) -> Self {
        self.input_modes = modes;
        self
    }

    /// Set the default output modes supported by this agent (e.g., `["text/plain"]`).
    #[must_use]
    pub fn default_output_modes(mut self, modes: Vec<String>) -> Self {
        self.output_modes = modes;
        self
    }

    /// Consume the builder and produce the final [`AgentCard`].
    #[must_use]
    pub fn build(self) -> AgentCard {
        AgentCard {
            name: self.name,
            description: self.description,
            url: self.url,
            version: self.version,
            protocol_version: self.protocol_version,
            provider: self.provider,
            capabilities: self.capabilities,
            default_input_modes: self.input_modes,
            default_output_modes: self.output_modes,
            skills: self.skills,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_minimal() {
        let card = AgentCardBuilder::new("agent", "http://localhost", "0.1.0").build();
        assert_eq!(card.name, "agent");
        assert_eq!(card.url, "http://localhost");
        assert_eq!(card.version, "0.1.0");
        assert!(card.description.is_empty());
        assert!(!card.capabilities.streaming);
        assert!(card.skills.is_empty());
    }

    #[test]
    fn builder_full() {
        let card = AgentCardBuilder::new("zeph", "http://localhost:8080", "0.5.0")
            .description("AI agent")
            .streaming(true)
            .push_notifications(false)
            .provider("TestOrg", Some("https://test.org".into()))
            .default_input_modes(vec!["text".into()])
            .default_output_modes(vec!["text".into()])
            .skill(AgentSkill {
                id: "s1".into(),
                name: "Skill One".into(),
                description: "Does things".into(),
                tags: vec!["test".into()],
                examples: vec![],
                input_modes: vec![],
                output_modes: vec![],
            })
            .build();

        assert_eq!(card.description, "AI agent");
        assert!(card.capabilities.streaming);
        assert!(!card.capabilities.push_notifications);
        assert_eq!(card.provider.as_ref().unwrap().organization, "TestOrg");
        assert_eq!(
            card.provider.as_ref().unwrap().url.as_deref(),
            Some("https://test.org")
        );
        assert_eq!(card.default_input_modes, vec!["text"]);
        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].id, "s1");
    }

    #[test]
    fn builder_card_serializes() {
        let card = AgentCardBuilder::new("test", "http://example.com", "1.0.0")
            .description("test agent")
            .build();
        let json = serde_json::to_string(&card).unwrap();
        assert!(json.contains("\"name\":\"test\""));
        assert!(json.contains("\"defaultInputModes\"").not());
    }

    #[test]
    fn builder_includes_protocol_version() {
        let card = AgentCardBuilder::new("agent", "http://localhost", "0.1.0").build();
        let json = serde_json::to_string(&card).unwrap();
        assert!(json.contains("\"protocolVersion\""));
        assert!(json.contains(crate::A2A_PROTOCOL_VERSION));
        assert_eq!(card.protocol_version, crate::A2A_PROTOCOL_VERSION);
    }

    #[test]
    fn builder_sets_image_capability() {
        let card = AgentCardBuilder::new("agent", "http://localhost", "0.1.0")
            .images(true)
            .build();
        assert!(card.capabilities.images);
        assert!(!card.capabilities.audio);
        assert!(!card.capabilities.files);
    }

    #[test]
    fn builder_sets_audio_capability() {
        let card = AgentCardBuilder::new("agent", "http://localhost", "0.1.0")
            .audio(true)
            .build();
        assert!(card.capabilities.audio);
        assert!(!card.capabilities.images);
        assert!(!card.capabilities.files);
    }

    #[test]
    fn builder_sets_file_capability() {
        let card = AgentCardBuilder::new("agent", "http://localhost", "0.1.0")
            .files(true)
            .build();
        assert!(card.capabilities.files);
        assert!(!card.capabilities.images);
        assert!(!card.capabilities.audio);
    }

    #[test]
    fn card_serializes_modality_fields() {
        let card = AgentCardBuilder::new("agent", "http://localhost", "0.1.0")
            .images(true)
            .build();
        let json = serde_json::to_string(&card).unwrap();
        // images was set to true
        assert!(json.contains("\"images\":true"));
        // audio and files default to false and must be present
        assert!(json.contains("\"audio\":false"));
        assert!(json.contains("\"files\":false"));
    }

    #[test]
    fn deserialize_legacy_card_uses_defaults() {
        // A card from a peer that predates modality fields: modalities must default to false.
        let json = r#"{"name":"old","description":"","url":"http://x","version":"1","protocolVersion":"0.2.1","capabilities":{"streaming":true}}"#;
        let card: crate::types::AgentCard = serde_json::from_str(json).unwrap();
        assert!(card.capabilities.streaming);
        assert!(!card.capabilities.images);
        assert!(!card.capabilities.audio);
        assert!(!card.capabilities.files);
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
