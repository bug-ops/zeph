// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zeph_llm::LlmError;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, Role};

use crate::error::MemoryError;

const SYSTEM_PROMPT: &str = "\
You are an entity and relationship extractor. Given a conversation message and \
its recent context, extract structured knowledge as JSON.

Rules:
1. Extract only entities that appear in natural conversational text — user statements, \
preferences, opinions, or factual claims made by a person.
2. Do NOT extract entities from: tool outputs, command results, file contents, \
configuration files, JSON/TOML/YAML data, code snippets, or error messages. \
If the message is structured data or raw command output, return empty arrays.
3. Do NOT extract structural data: config keys, file paths, tool names, TOML/JSON keys, \
programming keywords, or single-letter identifiers.
4. Entity types must be one of: person, project, tool, language, organization, concept. \
\"tool\" covers frameworks, software tools, and libraries. \
\"language\" covers programming and natural languages. \
\"concept\" covers abstract ideas, methodologies, and practices.
5. Only extract entities with clear semantic meaning about people, projects, or domain knowledge.
6. Entity names must be at least 3 characters long. Reject single characters, two-letter \
tokens (e.g. standalone \"go\", \"cd\"), URLs, and bare file paths.
7. Relations should be short verb phrases: \"prefers\", \"uses\", \"works_on\", \"knows\", \
\"created\", \"depends_on\", \"replaces\", \"configured_with\".
8. The \"fact\" field is a human-readable sentence summarizing the relationship.
9. If a message contains a temporal change (e.g., \"switched from X to Y\"), include a \
temporal_hint like \"replaced X\" or \"since January 2026\".
10. Each edge must include an \"edge_type\" field classifying the relationship:
  - \"semantic\": conceptual relationships (uses, prefers, knows, works_on, depends_on, created)
  - \"temporal\": time-ordered events (preceded_by, followed_by, happened_during, started_before)
  - \"causal\": cause-effect chains (caused, triggered, resulted_in, led_to, prevented)
  - \"entity\": identity/structural relationships (is_a, part_of, instance_of, alias_of, replaces)
  Default to \"semantic\" if the relationship type is uncertain.
11. Each edge must include a \"confidence\" field: a float in [0.0, 1.0] reflecting how \
certain you are that this relationship was explicitly stated or strongly implied by the text. \
Use 1.0 only for direct verbatim statements. Use 0.5–0.8 for clear implications. \
Use 0.3–0.5 for weak inferences. Omit or use null if you cannot assign a meaningful score.
11. Do not extract entities from greetings, filler, or meta-conversation (\"hi\", \"thanks\", \"ok\").
12. Do not extract personal identifiable information as entity names: email addresses, \
phone numbers, physical addresses, SSNs, or API keys. Use generic references instead.
13. Always output entity names and relation verbs in English. Translate if needed.
14. Return empty arrays if no entities or relationships are present.

Output JSON schema:
{
  \"entities\": [
    {\"name\": \"string\", \"type\": \"person|project|tool|language|organization|concept\", \"summary\": \"optional string\"}
  ],
  \"edges\": [
    {\"source\": \"entity name\", \"target\": \"entity name\", \"relation\": \"verb phrase\", \"fact\": \"human-readable sentence\", \"temporal_hint\": \"optional string\", \"edge_type\": \"semantic|temporal|causal|entity\", \"confidence\": 0.0}
  ]
}";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtractionResult {
    pub entities: Vec<ExtractedEntity>,
    pub edges: Vec<ExtractedEdge>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtractedEntity {
    pub name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    pub summary: Option<String>,
}

fn default_semantic() -> String {
    "semantic".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExtractedEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub fact: String,
    pub temporal_hint: Option<String>,
    /// MAGMA edge type classification. Defaults to "semantic" when omitted by the LLM.
    #[serde(default = "default_semantic")]
    pub edge_type: String,
    /// Extractor confidence in the relationship, in `[0.0, 1.0]`.
    ///
    /// Assigned by the LLM during extraction. `None` means the LLM omitted the field;
    /// callers should treat `None` as `1.0` (direct statement, commit immediately).
    /// Values below `BeliefMemConfig::promote_threshold` route the edge to
    /// `BeliefStore` for evidence accumulation instead of immediate commit.
    #[serde(default)]
    pub confidence: Option<f32>,
}

pub struct GraphExtractor {
    provider: AnyProvider,
    max_entities: usize,
    max_edges: usize,
}

impl GraphExtractor {
    #[must_use]
    pub fn new(provider: AnyProvider, max_entities: usize, max_edges: usize) -> Self {
        Self {
            provider,
            max_entities,
            max_edges,
        }
    }

    /// Extract entities and relations from a message with surrounding context.
    ///
    /// Returns `None` if the message is empty, extraction fails, or the LLM returns
    /// unparseable output. Callers should treat `None` as a graceful degradation.
    ///
    /// # Errors
    ///
    /// Returns an error only for transport-level failures (network, auth).
    /// JSON parse failures are logged and return `Ok(None)`.
    pub async fn extract(
        &self,
        message: &str,
        context_messages: &[&str],
    ) -> Result<Option<ExtractionResult>, MemoryError> {
        if message.trim().is_empty() {
            return Ok(None);
        }

        let user_prompt = build_user_prompt(message, context_messages);
        let messages = [
            Message::from_legacy(Role::System, SYSTEM_PROMPT),
            Message::from_legacy(Role::User, user_prompt),
        ];

        match self
            .provider
            .chat_typed_erased::<ExtractionResult>(&messages)
            .await
        {
            Ok(mut result) => {
                result.entities.truncate(self.max_entities);
                result.edges.truncate(self.max_edges);
                Ok(Some(result))
            }
            Err(LlmError::StructuredParse(msg)) => {
                tracing::warn!(
                    "graph extraction: LLM returned unparseable output (len={}): {:.200}",
                    msg.len(),
                    msg
                );
                Ok(None)
            }
            Err(other) => Err(MemoryError::Llm(other)),
        }
    }
}

fn build_user_prompt(message: &str, context_messages: &[&str]) -> String {
    if context_messages.is_empty() {
        format!("Current message:\n{message}\n\nExtract entities and relationships as JSON.")
    } else {
        let n = context_messages.len();
        let context = context_messages.join("\n");
        format!(
            "Context (last {n} messages):\n{context}\n\nCurrent message:\n{message}\n\nExtract entities and relationships as JSON."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entity(name: &str, entity_type: &str, summary: Option<&str>) -> ExtractedEntity {
        ExtractedEntity {
            name: name.into(),
            entity_type: entity_type.into(),
            summary: summary.map(Into::into),
        }
    }

    fn make_edge(
        source: &str,
        target: &str,
        relation: &str,
        fact: &str,
        temporal_hint: Option<&str>,
    ) -> ExtractedEdge {
        ExtractedEdge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            fact: fact.into(),
            temporal_hint: temporal_hint.map(Into::into),
            edge_type: "semantic".into(),
            confidence: None,
        }
    }

    #[test]
    fn extraction_result_deserialize_valid_json() {
        let json = r#"{"entities":[{"name":"Rust","type":"language","summary":"A systems language"}],"edges":[]}"#;
        let result: ExtractionResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "Rust");
        assert_eq!(result.entities[0].entity_type, "language");
        assert_eq!(
            result.entities[0].summary.as_deref(),
            Some("A systems language")
        );
        assert!(result.edges.is_empty());
    }

    #[test]
    fn extraction_result_deserialize_empty_arrays() {
        let json = r#"{"entities":[],"edges":[]}"#;
        let result: ExtractionResult = serde_json::from_str(json).unwrap();
        assert!(result.entities.is_empty());
        assert!(result.edges.is_empty());
    }

    #[test]
    fn extraction_result_deserialize_missing_optional_fields() {
        let json = r#"{"entities":[{"name":"Alice","type":"person","summary":null}],"edges":[{"source":"Alice","target":"Rust","relation":"uses","fact":"Alice uses Rust","temporal_hint":null}]}"#;
        let result: ExtractionResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.entities[0].summary, None);
        assert_eq!(result.edges[0].temporal_hint, None);
        // edge_type defaults to "semantic" when omitted
        assert_eq!(result.edges[0].edge_type, "semantic");
    }

    #[test]
    fn extracted_edge_type_defaults_to_semantic_when_missing() {
        // When LLM omits edge_type, serde(default) must provide "semantic".
        let json = r#"{"source":"A","target":"B","relation":"uses","fact":"A uses B"}"#;
        let edge: ExtractedEdge = serde_json::from_str(json).unwrap();
        assert_eq!(edge.edge_type, "semantic");
    }

    #[test]
    fn extracted_edge_type_parses_all_variants() {
        for et in &["semantic", "temporal", "causal", "entity"] {
            let json = format!(
                r#"{{"source":"A","target":"B","relation":"r","fact":"f","edge_type":"{et}"}}"#
            );
            let edge: ExtractedEdge = serde_json::from_str(&json).unwrap();
            assert_eq!(&edge.edge_type, et);
        }
    }

    #[test]
    fn extraction_result_with_edge_types_roundtrip() {
        let original = ExtractionResult {
            entities: vec![],
            edges: vec![
                ExtractedEdge {
                    source: "A".into(),
                    target: "B".into(),
                    relation: "caused".into(),
                    fact: "A caused B".into(),
                    temporal_hint: None,
                    edge_type: "causal".into(),
                    confidence: Some(0.9),
                },
                ExtractedEdge {
                    source: "B".into(),
                    target: "C".into(),
                    relation: "preceded_by".into(),
                    fact: "B preceded_by C".into(),
                    temporal_hint: None,
                    edge_type: "temporal".into(),
                    confidence: None,
                },
            ],
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: ExtractionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
        assert_eq!(restored.edges[0].edge_type, "causal");
        assert_eq!(restored.edges[1].edge_type, "temporal");
    }

    #[test]
    fn extracted_entity_type_field_rename() {
        let json = r#"{"name":"cargo","type":"tool","summary":null}"#;
        let entity: ExtractedEntity = serde_json::from_str(json).unwrap();
        assert_eq!(entity.entity_type, "tool");

        let serialized = serde_json::to_string(&entity).unwrap();
        assert!(serialized.contains("\"type\""));
        assert!(!serialized.contains("\"entity_type\""));
    }

    #[test]
    fn extraction_result_roundtrip() {
        let original = ExtractionResult {
            entities: vec![make_entity("Rust", "language", Some("A systems language"))],
            edges: vec![make_edge("Alice", "Rust", "uses", "Alice uses Rust", None)],
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: ExtractionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn extraction_result_json_schema() {
        let schema = schemars::schema_for!(ExtractionResult);
        let value = serde_json::to_value(&schema).unwrap();
        let schema_obj = value.as_object().unwrap();
        assert!(
            schema_obj.contains_key("title") || schema_obj.contains_key("properties"),
            "schema should have top-level keys"
        );
        let json_str = serde_json::to_string(&schema).unwrap();
        assert!(
            json_str.contains("entities"),
            "schema should contain 'entities'"
        );
        assert!(json_str.contains("edges"), "schema should contain 'edges'");
    }

    #[test]
    fn build_user_prompt_with_context() {
        let prompt = build_user_prompt("Hello Rust", &["prev message 1", "prev message 2"]);
        assert!(prompt.contains("Context (last 2 messages):"));
        assert!(prompt.contains("prev message 1\nprev message 2"));
        assert!(prompt.contains("Current message:\nHello Rust"));
        assert!(prompt.contains("Extract entities and relationships as JSON."));
    }

    #[test]
    fn build_user_prompt_without_context() {
        let prompt = build_user_prompt("Hello Rust", &[]);
        assert!(!prompt.contains("Context"));
        assert!(prompt.contains("Current message:\nHello Rust"));
        assert!(prompt.contains("Extract entities and relationships as JSON."));
    }

    mod mock_tests {
        use super::*;
        use zeph_llm::mock::MockProvider;

        fn make_entities_json(count: usize) -> String {
            let entities: Vec<String> = (0..count)
                .map(|i| format!(r#"{{"name":"entity{i}","type":"concept","summary":null}}"#))
                .collect();
            format!(r#"{{"entities":[{}],"edges":[]}}"#, entities.join(","))
        }

        fn make_edges_json(count: usize) -> String {
            let edges: Vec<String> = (0..count)
                .map(|i| {
                    format!(
                        r#"{{"source":"A","target":"B{i}","relation":"uses","fact":"A uses B{i}","temporal_hint":null}}"#
                    )
                })
                .collect();
            format!(r#"{{"entities":[],"edges":[{}]}}"#, edges.join(","))
        }

        #[tokio::test]
        async fn extract_truncates_to_max_entities() {
            let json = make_entities_json(20);
            let mock = MockProvider::with_responses(vec![json]);
            let extractor = GraphExtractor::new(zeph_llm::any::AnyProvider::Mock(mock), 5, 100);
            let result = extractor.extract("test message", &[]).await.unwrap();
            let result = result.unwrap();
            assert_eq!(result.entities.len(), 5);
        }

        #[tokio::test]
        async fn extract_truncates_to_max_edges() {
            let json = make_edges_json(15);
            let mock = MockProvider::with_responses(vec![json]);
            let extractor = GraphExtractor::new(zeph_llm::any::AnyProvider::Mock(mock), 100, 3);
            let result = extractor.extract("test message", &[]).await.unwrap();
            let result = result.unwrap();
            assert_eq!(result.edges.len(), 3);
        }

        #[tokio::test]
        async fn extract_returns_none_on_parse_failure() {
            let mock = MockProvider::with_responses(vec!["not valid json at all".into()]);
            let extractor = GraphExtractor::new(zeph_llm::any::AnyProvider::Mock(mock), 10, 10);
            let result = extractor.extract("test message", &[]).await.unwrap();
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn extract_returns_err_on_transport_failure() {
            let mock = MockProvider::default()
                .with_errors(vec![zeph_llm::LlmError::Other("connection refused".into())]);
            let extractor = GraphExtractor::new(zeph_llm::any::AnyProvider::Mock(mock), 10, 10);
            let result = extractor.extract("test message", &[]).await;
            assert!(result.is_err());
            assert!(matches!(result.unwrap_err(), MemoryError::Llm(_)));
        }

        #[tokio::test]
        async fn extract_returns_none_on_empty_message() {
            let mock = MockProvider::with_responses(vec!["should not be called".into()]);
            let extractor = GraphExtractor::new(zeph_llm::any::AnyProvider::Mock(mock), 10, 10);

            let result_empty = extractor.extract("", &[]).await.unwrap();
            assert!(result_empty.is_none());

            let result_whitespace = extractor.extract("   \t\n  ", &[]).await.unwrap();
            assert!(result_whitespace.is_none());
        }
    }
}
