// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::str::FromStr;

use crate::types::MessageId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Person,
    Tool,
    Concept,
    Project,
    Language,
    File,
    Config,
    Organization,
}

impl EntityType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Tool => "tool",
            Self::Concept => "concept",
            Self::Project => "project",
            Self::Language => "language",
            Self::File => "file",
            Self::Config => "config",
            Self::Organization => "organization",
        }
    }
}

impl fmt::Display for EntityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EntityType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "person" => Ok(Self::Person),
            "tool" => Ok(Self::Tool),
            "concept" => Ok(Self::Concept),
            "project" => Ok(Self::Project),
            "language" => Ok(Self::Language),
            "file" => Ok(Self::File),
            "config" => Ok(Self::Config),
            "organization" => Ok(Self::Organization),
            other => Err(format!("unknown entity type: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: i64,
    pub name: String,
    pub entity_type: EntityType,
    pub summary: Option<String>,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub qdrant_point_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub id: i64,
    pub source_entity_id: i64,
    pub target_entity_id: i64,
    pub relation: String,
    pub fact: String,
    pub confidence: f32,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub created_at: String,
    pub expired_at: Option<String>,
    pub episode_id: Option<MessageId>,
    pub qdrant_point_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Community {
    pub id: i64,
    pub name: String,
    pub summary: String,
    pub entity_ids: Vec<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphFact {
    pub entity_name: String,
    pub relation: String,
    pub target_name: String,
    pub fact: String,
    pub entity_match_score: f32,
    pub hop_distance: u32,
    pub confidence: f32,
}

impl GraphFact {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn composite_score(&self) -> f32 {
        self.entity_match_score * (1.0 / (1.0 + self.hop_distance as f32)) * self.confidence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_type_from_str_all_variants() {
        assert_eq!("person".parse::<EntityType>().unwrap(), EntityType::Person);
        assert_eq!("tool".parse::<EntityType>().unwrap(), EntityType::Tool);
        assert_eq!(
            "concept".parse::<EntityType>().unwrap(),
            EntityType::Concept
        );
        assert_eq!(
            "project".parse::<EntityType>().unwrap(),
            EntityType::Project
        );
        assert_eq!(
            "language".parse::<EntityType>().unwrap(),
            EntityType::Language
        );
        assert_eq!("file".parse::<EntityType>().unwrap(), EntityType::File);
        assert_eq!("config".parse::<EntityType>().unwrap(), EntityType::Config);
        assert_eq!(
            "organization".parse::<EntityType>().unwrap(),
            EntityType::Organization
        );
    }

    #[test]
    fn entity_type_from_str_unknown_rejected() {
        assert!("unknown".parse::<EntityType>().is_err());
        assert!("Person".parse::<EntityType>().is_err());
        assert!("".parse::<EntityType>().is_err());
    }

    #[test]
    fn entity_type_display_round_trip() {
        for et in [
            EntityType::Person,
            EntityType::Tool,
            EntityType::Concept,
            EntityType::Project,
            EntityType::Language,
            EntityType::File,
            EntityType::Config,
            EntityType::Organization,
        ] {
            let s = et.to_string();
            assert_eq!(s.parse::<EntityType>().unwrap(), et);
        }
    }

    #[test]
    fn graph_fact_composite_score() {
        let fact = GraphFact {
            entity_name: "A".into(),
            relation: "knows".into(),
            target_name: "B".into(),
            fact: "A knows B".into(),
            entity_match_score: 1.0,
            hop_distance: 0,
            confidence: 1.0,
        };
        // 1.0 * (1/(1+0)) * 1.0 = 1.0
        assert!((fact.composite_score() - 1.0).abs() < 1e-6);

        let fact2 = GraphFact {
            hop_distance: 1,
            confidence: 0.8,
            entity_match_score: 0.9,
            ..fact.clone()
        };
        // 0.9 * (1/2) * 0.8 = 0.36
        assert!((fact2.composite_score() - 0.36).abs() < 1e-5);
    }
}
