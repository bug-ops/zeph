use crate::types::{AgentCapabilities, AgentCard, AgentProvider, AgentSkill};

pub struct AgentCardBuilder {
    name: String,
    description: String,
    url: String,
    version: String,
    capabilities: AgentCapabilities,
    skills: Vec<AgentSkill>,
    provider: Option<AgentProvider>,
    input_modes: Vec<String>,
    output_modes: Vec<String>,
}

impl AgentCardBuilder {
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
            capabilities: AgentCapabilities::default(),
            skills: Vec::new(),
            provider: None,
            input_modes: Vec::new(),
            output_modes: Vec::new(),
        }
    }

    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    #[must_use]
    pub fn streaming(mut self, enabled: bool) -> Self {
        self.capabilities.streaming = enabled;
        self
    }

    #[must_use]
    pub fn push_notifications(mut self, enabled: bool) -> Self {
        self.capabilities.push_notifications = enabled;
        self
    }

    #[must_use]
    pub fn skill(mut self, skill: AgentSkill) -> Self {
        self.skills.push(skill);
        self
    }

    #[must_use]
    pub fn provider(mut self, org: impl Into<String>, url: impl Into<Option<String>>) -> Self {
        self.provider = Some(AgentProvider {
            organization: org.into(),
            url: url.into(),
        });
        self
    }

    #[must_use]
    pub fn default_input_modes(mut self, modes: Vec<String>) -> Self {
        self.input_modes = modes;
        self
    }

    #[must_use]
    pub fn default_output_modes(mut self, modes: Vec<String>) -> Self {
        self.output_modes = modes;
        self
    }

    #[must_use]
    pub fn build(self) -> AgentCard {
        AgentCard {
            name: self.name,
            description: self.description,
            url: self.url,
            version: self.version,
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

    trait Not {
        fn not(&self) -> bool;
    }
    impl Not for bool {
        fn not(&self) -> bool {
            !*self
        }
    }
}
