// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core graph types for the MAGMA / GAAMA knowledge graph.

use std::fmt;
use std::str::FromStr;

use crate::types::MessageId;

/// MAGMA edge type: the semantic category of a relationship between two entities.
///
/// Four orthogonal relation categories from the MAGMA multi-graph architecture:
/// - `Semantic`: conceptual relationships (`uses`, `knows`, `prefers`, `depends_on`, `works_on`)
/// - `Temporal`: time-ordered events (`preceded_by`, `followed_by`, `happened_during`)
/// - `Causal`: cause-effect chains (`caused`, `triggered`, `resulted_in`, `led_to`)
/// - `Entity`: identity/structural (`is_a`, `part_of`, `instance_of`, `alias_of`)
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    #[default]
    Semantic,
    Temporal,
    Causal,
    Entity,
}

impl EdgeType {
    /// Return the canonical lowercase string for this edge type.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_memory::EdgeType;
    ///
    /// assert_eq!(EdgeType::Causal.as_str(), "causal");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Temporal => "temporal",
            Self::Causal => "causal",
            Self::Entity => "entity",
        }
    }
}

impl fmt::Display for EdgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EdgeType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "semantic" => Ok(Self::Semantic),
            "temporal" => Ok(Self::Temporal),
            "causal" => Ok(Self::Causal),
            "entity" => Ok(Self::Entity),
            other => Err(format!("unknown edge type: {other}")),
        }
    }
}

/// Domain category of a graph entity.
///
/// Used by the LLM extractor to classify extracted named entities into coarse types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    /// A human or AI agent.
    Person,
    /// A CLI tool, library, or framework.
    Tool,
    /// An abstract idea or technical concept.
    Concept,
    /// A software project or repository.
    Project,
    /// A programming language.
    Language,
    /// A file or directory path.
    File,
    /// A configuration file or settings key.
    Config,
    /// A company, team, or open-source organization.
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

/// A named entity in the knowledge graph.
///
/// Entities are the nodes; [`Edge`]s connect them with typed, factual relationships.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    /// `SQLite` row ID.
    pub id: i64,
    /// Raw extracted name as it appeared in the source text.
    pub name: String,
    /// Normalized canonical name (lowercase, de-aliased).
    pub canonical_name: String,
    /// Coarse semantic category.
    pub entity_type: EntityType,
    /// Optional LLM-generated summary describing the entity.
    pub summary: Option<String>,
    /// ISO 8601 timestamp when the entity was first extracted.
    pub first_seen_at: String,
    /// ISO 8601 timestamp when the entity was last seen in a conversation.
    pub last_seen_at: String,
    /// Qdrant point ID for the entity's embedding, if stored.
    pub qdrant_point_id: Option<String>,
}

/// An alternative name or spelling for an [`Entity`].
#[derive(Debug, Clone, PartialEq)]
pub struct EntityAlias {
    /// `SQLite` row ID.
    pub id: i64,
    /// The entity this alias resolves to.
    pub entity_id: i64,
    /// The alternate name string.
    pub alias_name: String,
    /// ISO 8601 timestamp when the alias was recorded.
    pub created_at: String,
}

/// A directed, typed relationship between two entities in the knowledge graph.
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
    /// Message-level provenance: the message that caused this edge to be created.
    /// Stored as `episode_id` in the DB column (legacy name); renamed here to avoid
    /// confusion with the GAAMA conversation-level `graph_episodes` table.
    pub source_message_id: Option<MessageId>,
    pub qdrant_point_id: Option<String>,
    pub edge_type: EdgeType,
    /// Number of times this edge was traversed during graph recall (A-MEM link weight evolution).
    pub retrieval_count: i32,
    /// Unix timestamp of the last retrieval. `None` if never retrieved.
    pub last_retrieved_at: Option<i64>,
    /// ID of the edge that superseded this one during Kumiho belief revision.
    /// `None` for active edges and for edges invalidated by legacy exact-match dedup.
    pub superseded_by: Option<i64>,
    /// Canonical (ontology-normalized) relation. Equals `relation` when no ontology is loaded.
    /// Added by APEX-MEM migration 075.
    pub canonical_relation: String,
    /// ID of the prior active edge that this edge replaced in the supersede chain.
    /// `None` for the chain root. Added by APEX-MEM migration 075.
    pub supersedes: Option<i64>,
}

/// A Louvain-detected community (cluster) of related entities.
///
/// Communities provide coarse-grained grouping for graph eviction and summarization.
#[derive(Debug, Clone, PartialEq)]
pub struct Community {
    /// `SQLite` row ID.
    pub id: i64,
    /// Short name for the community (e.g. `"Rust toolchain"`).
    pub name: String,
    /// LLM-generated summary of what the community's entities share.
    pub summary: String,
    /// IDs of all entities assigned to this community.
    pub entity_ids: Vec<i64>,
    /// Content fingerprint used to detect stale communities after membership changes.
    pub fingerprint: Option<String>,
    /// ISO 8601 timestamp when the community was detected.
    pub created_at: String,
    /// ISO 8601 timestamp when the community summary was last updated.
    pub updated_at: String,
}

/// A GAAMA episode node — one per conversation.
///
/// Groups entities observed during a single conversation context. Enables
/// episode-boundary-aware retrieval: facts from the current episode are
/// more salient than facts from older episodes.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub id: i64,
    pub conversation_id: i64,
    pub created_at: String,
    pub closed_at: Option<String>,
}

/// Entity with its match score from hybrid seed selection.
#[derive(Debug, Clone)]
pub struct ScoredEntity {
    pub entity: Entity,
    pub fts_score: f32,
    pub structural_score: f32,
    pub community_id: Option<i64>,
}

/// A recalled fact from the knowledge graph, ready for context injection.
///
/// Produced by graph retrieval (BFS, spreading activation) and consumed by
/// `SemanticMemory::recall` to inject graph knowledge into the LLM context.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphFact {
    /// Source entity name.
    pub entity_name: String,
    /// Relation label (e.g. `"uses"`, `"caused"`, `"is_a"`).
    pub relation: String,
    /// Target entity name.
    pub target_name: String,
    /// Full fact sentence (e.g. `"Rust uses LLVM for code generation"`).
    pub fact: String,
    /// BM25/vector similarity score for the seed entity match.
    pub entity_match_score: f32,
    /// BFS hop distance from the seed entity (0 = direct match).
    pub hop_distance: u32,
    /// Edge confidence in `[0, 1]`.
    pub confidence: f32,
    /// `SQLite` datetime string when the edge became valid (e.g. `"2026-03-14 12:00:00"`).
    /// Used for optional temporal recency scoring. `None` when not populated.
    pub valid_from: Option<String>,
    /// MAGMA edge classification for this fact.
    pub edge_type: EdgeType,
    /// Number of times this edge was traversed (A-MEM link weight evolution).
    pub retrieval_count: i32,
}

/// Compute A-MEM evolved edge weight.
///
/// Applies a logarithmic boost to base confidence based on retrieval count.
/// Uses a 0.2 dampening factor to prevent saturation at low counts.
///
/// Formula: `confidence * (1.0 + 0.2 * ln(1.0 + count)).min(1.0)`
///
/// - `count=0`: returns `confidence` (identity)
/// - `count=1`: ~1.14x boost
/// - `count=10`: ~1.48x boost (capped at 1.0 if confidence is high)
#[must_use]
pub fn evolved_weight(retrieval_count: i32, base_confidence: f32) -> f32 {
    let count = f64::from(retrieval_count.max(0));
    let boost = 1.0 + 0.2 * (1.0 + count).ln();
    // cast f64 -> f32: boost is bounded, truncation is acceptable
    #[allow(clippy::cast_possible_truncation)]
    let boost_f32 = boost as f32;
    (base_confidence * boost_f32).min(1.0)
}

/// Edge-type weight multipliers for BFS scoring and spreading activation.
///
/// Applied as a multiplicative factor on the composite score to reflect the
/// relative signal quality of each MAGMA edge type during traversal:
/// - `Causal`: high-signal (cause→effect chains are precise and informative).
/// - `Semantic`: baseline (default relationship type).
/// - `Temporal`: slightly lower than semantic (ordering is useful but less precise than causality).
/// - `Entity`: lowest (structural/identity edges are graph skeleton, not recall signal).
#[must_use]
pub fn edge_type_weight(et: EdgeType) -> f32 {
    match et {
        EdgeType::Causal => 1.2,
        EdgeType::Semantic => 1.0, // baseline
        EdgeType::Temporal => 0.9,
        EdgeType::Entity => 0.8,
    }
}

impl GraphFact {
    /// Base composite score with A-MEM evolved edge weight and MAGMA edge-type weight.
    ///
    /// Formula: `entity_match_score * (1 / (1 + hop_distance)) * evolved_weight(retrieval_count, confidence) * edge_type_weight(edge_type)`
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn composite_score(&self) -> f32 {
        let w = evolved_weight(self.retrieval_count, self.confidence);
        let type_w = edge_type_weight(self.edge_type);
        self.entity_match_score * (1.0 / (1.0 + self.hop_distance as f32)) * w * type_w
    }

    /// Composite score with an optional additive temporal recency boost.
    ///
    /// When `temporal_decay_rate > 0`, a recency boost is computed as
    /// `1 / (1 + days_old * decay_rate)` and blended additively with the base score
    /// (capped at 2x base) so that hop distance remains the dominant factor.
    ///
    /// With `temporal_decay_rate = 0.0` (the default) the result equals `composite_score()`.
    ///
    /// # Parameters
    ///
    /// - `temporal_decay_rate`: non-negative decay rate in units of 1/day. Default 0.0.
    /// - `now_secs`: current Unix timestamp in seconds (seconds since epoch).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn score_with_decay(&self, temporal_decay_rate: f64, now_secs: i64) -> f32 {
        let base = self.composite_score();
        if temporal_decay_rate <= 0.0 {
            return base;
        }
        let boost = self
            .valid_from
            .as_deref()
            .and_then(parse_sqlite_datetime_to_unix)
            .map_or(0.0_f64, |valid_from_secs| {
                let age_secs = (now_secs - valid_from_secs).max(0);
                // cast i64 → f64: precision loss acceptable for age-in-seconds computation
                #[allow(clippy::cast_precision_loss)]
                let age_days = age_secs as f64 / 86_400.0;
                1.0_f64 / (1.0 + age_days * temporal_decay_rate)
            });
        // boost is in [0.0, 1.0]; cast to f32 is safe (no truncation risk).
        #[allow(clippy::cast_possible_truncation)]
        let boost_f32 = boost as f32;
        // Additive blend: base * (1 + boost_fraction), capped at 2x base.
        base * (1.0 + boost_f32).min(2.0)
    }
}

/// Parse a `SQLite` `datetime('now')` string to Unix seconds.
///
/// Accepts:
/// - `"YYYY-MM-DD HH:MM:SS"` (19 chars, standard `SQLite` format)
/// - `"YYYY-MM-DD HH:MM:SS.fff"` (fractional seconds — truncated, not rounded)
/// - `"YYYY-MM-DD HH:MM:SSZ"` or `"YYYY-MM-DD HH:MM:SS+HH:MM"` (timezone suffix — treated as UTC)
///
/// Returns `None` if the string cannot be parsed.
#[must_use]
fn parse_sqlite_datetime_to_unix(s: &str) -> Option<i64> {
    // Minimum: "YYYY-MM-DD HH:MM:SS" (19 chars)
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    // Only parse the base seconds; ignore fractional seconds and timezone suffix.
    let sec: i64 = s[17..19].parse().ok()?;

    // Days since Unix epoch (1970-01-01) via civil calendar algorithm.
    // Reference: https://howardhinnant.github.io/date_algorithms.html#days_from_civil
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_type_weight_causal_highest() {
        assert!(edge_type_weight(EdgeType::Causal) > edge_type_weight(EdgeType::Semantic));
        assert!(edge_type_weight(EdgeType::Causal) > edge_type_weight(EdgeType::Temporal));
        assert!(edge_type_weight(EdgeType::Causal) > edge_type_weight(EdgeType::Entity));
    }

    #[test]
    fn edge_type_weight_entity_lowest() {
        assert!(edge_type_weight(EdgeType::Entity) < edge_type_weight(EdgeType::Semantic));
        assert!(edge_type_weight(EdgeType::Entity) < edge_type_weight(EdgeType::Temporal));
        assert!(edge_type_weight(EdgeType::Entity) < edge_type_weight(EdgeType::Causal));
    }

    #[test]
    fn edge_type_weight_semantic_is_baseline() {
        assert!((edge_type_weight(EdgeType::Semantic) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn composite_score_causal_higher_than_semantic_same_hop() {
        let base = GraphFact {
            entity_name: "A".into(),
            relation: "rel".into(),
            target_name: "B".into(),
            fact: "A rel B".into(),
            entity_match_score: 1.0,
            hop_distance: 0,
            confidence: 1.0,
            valid_from: None,
            retrieval_count: 0,
            edge_type: EdgeType::Semantic,
        };
        let causal = GraphFact {
            edge_type: EdgeType::Causal,
            ..base.clone()
        };
        assert!(
            causal.composite_score() > base.composite_score(),
            "causal edge must score higher than semantic at same hop distance"
        );
    }

    #[test]
    fn edge_type_from_str_all_variants() {
        assert_eq!("semantic".parse::<EdgeType>().unwrap(), EdgeType::Semantic);
        assert_eq!("temporal".parse::<EdgeType>().unwrap(), EdgeType::Temporal);
        assert_eq!("causal".parse::<EdgeType>().unwrap(), EdgeType::Causal);
        assert_eq!("entity".parse::<EdgeType>().unwrap(), EdgeType::Entity);
    }

    #[test]
    fn edge_type_from_str_unknown_rejected() {
        assert!("unknown".parse::<EdgeType>().is_err());
        assert!("Semantic".parse::<EdgeType>().is_err());
        assert!("sematic".parse::<EdgeType>().is_err());
        assert!("".parse::<EdgeType>().is_err());
    }

    #[test]
    fn edge_type_display_round_trip() {
        for et in [
            EdgeType::Semantic,
            EdgeType::Temporal,
            EdgeType::Causal,
            EdgeType::Entity,
        ] {
            let s = et.to_string();
            assert_eq!(s.parse::<EdgeType>().unwrap(), et);
        }
    }

    #[test]
    fn edge_type_as_str_values() {
        assert_eq!(EdgeType::Semantic.as_str(), "semantic");
        assert_eq!(EdgeType::Temporal.as_str(), "temporal");
        assert_eq!(EdgeType::Causal.as_str(), "causal");
        assert_eq!(EdgeType::Entity.as_str(), "entity");
    }

    #[test]
    fn edge_type_default_is_semantic() {
        assert_eq!(EdgeType::default(), EdgeType::Semantic);
    }

    #[test]
    fn edge_type_serde_roundtrip() {
        for et in [
            EdgeType::Semantic,
            EdgeType::Temporal,
            EdgeType::Causal,
            EdgeType::Entity,
        ] {
            let json = serde_json::to_string(&et).unwrap();
            let restored: EdgeType = serde_json::from_str(&json).unwrap();
            assert_eq!(et, restored);
        }
    }

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
            valid_from: None,
            edge_type: EdgeType::Semantic,
            retrieval_count: 0,
        };
        // retrieval_count=0 → evolved_weight = confidence = 1.0
        // 1.0 * (1/(1+0)) * 1.0 = 1.0
        assert!((fact.composite_score() - 1.0).abs() < 1e-6);

        let fact2 = GraphFact {
            hop_distance: 1,
            confidence: 0.8,
            entity_match_score: 0.9,
            retrieval_count: 0,
            ..fact.clone()
        };
        // retrieval_count=0: evolved_weight = 0.8; 0.9 * (1/2) * 0.8 = 0.36
        assert!((fact2.composite_score() - 0.36).abs() < 1e-5);
    }

    #[test]
    fn evolved_weight_identity_at_zero() {
        let w = evolved_weight(0, 0.8);
        assert!(
            (w - 0.8).abs() < 1e-6,
            "count=0 must return base confidence"
        );
    }

    #[test]
    fn evolved_weight_capped_at_one() {
        // High confidence + many retrievals should not exceed 1.0
        let w = evolved_weight(1000, 0.9);
        assert!(w <= 1.0, "evolved_weight must not exceed 1.0");
        assert!(w > 0.9, "evolved_weight must boost above base confidence");
    }

    #[test]
    fn evolved_weight_slow_growth() {
        // Verify 0.2 dampening: count=1 should give modest boost
        let w1 = evolved_weight(1, 0.5);
        let w10 = evolved_weight(10, 0.5);
        // Both must be in (0.5, 1.0]
        assert!(w1 > 0.5 && w1 <= 1.0);
        assert!(w10 > w1, "more retrievals → higher weight");
    }

    #[test]
    fn evolved_weight_negative_count_treated_as_zero() {
        let w_neg = evolved_weight(-5, 0.7);
        let w_zero = evolved_weight(0, 0.7);
        assert!((w_neg - w_zero).abs() < 1e-6);
    }

    #[test]
    fn composite_score_boosted_by_retrieval_count() {
        let base_fact = GraphFact {
            entity_name: "A".into(),
            relation: "knows".into(),
            target_name: "B".into(),
            fact: "A knows B".into(),
            entity_match_score: 1.0,
            hop_distance: 0,
            confidence: 0.7,
            valid_from: None,
            edge_type: EdgeType::Semantic,
            retrieval_count: 0,
        };
        let retrieved_fact = GraphFact {
            retrieval_count: 5,
            ..base_fact.clone()
        };
        assert!(
            retrieved_fact.composite_score() > base_fact.composite_score(),
            "frequently-retrieved fact must score higher"
        );
    }

    #[test]
    fn score_with_decay_zero_rate_equals_composite() {
        let fact = GraphFact {
            entity_name: "A".into(),
            relation: "uses".into(),
            target_name: "B".into(),
            fact: "A uses B".into(),
            entity_match_score: 1.0,
            hop_distance: 1,
            confidence: 0.8,
            valid_from: Some("2026-01-01 00:00:00".into()),
            edge_type: EdgeType::Semantic,
            retrieval_count: 0,
        };
        let base = fact.composite_score();
        let with_decay = fact.score_with_decay(0.0, 1_752_000_000);
        assert!((base - with_decay).abs() < 1e-6);
    }

    #[test]
    fn score_with_decay_recent_edge_boosted() {
        // Edge created just now — boost should be near 1.0 (near-zero age).
        let now_secs: i64 = 1_752_000_000;
        // valid_from = "2026-01-01 00:00:00" = 1_735_689_600 seconds
        let fact = GraphFact {
            entity_name: "A".into(),
            relation: "uses".into(),
            target_name: "B".into(),
            fact: "A uses B".into(),
            entity_match_score: 1.0,
            hop_distance: 0,
            confidence: 1.0,
            valid_from: Some("2026-01-01 00:00:00".into()),
            edge_type: EdgeType::Semantic,
            retrieval_count: 0,
        };
        let base = fact.composite_score();
        let boosted = fact.score_with_decay(0.01, now_secs);
        // With nonzero age the boost < 1, so score drops slightly below base * 2.
        // But the boosted value must be >= base (additive boost).
        assert!(
            boosted >= base,
            "expected boosted >= base: {boosted} >= {base}"
        );
    }

    #[test]
    fn parse_sqlite_datetime_known_epoch() {
        // 1970-01-01 00:00:00 UTC = Unix epoch
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00"),
            Some(0)
        );
        // 1970-01-02 00:00:00 UTC = 86400
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-02 00:00:00"),
            Some(86_400)
        );
    }

    #[test]
    fn parse_sqlite_datetime_invalid_returns_none() {
        assert_eq!(parse_sqlite_datetime_to_unix("not-a-date"), None);
        assert_eq!(parse_sqlite_datetime_to_unix(""), None);
    }

    #[test]
    fn parse_sqlite_datetime_fractional_seconds_truncated() {
        // Fractional seconds should be ignored (truncated), not cause parse failure.
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00.999"),
            Some(0)
        );
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-02 00:00:00.123"),
            Some(86_400)
        );
    }

    #[test]
    fn parse_sqlite_datetime_timezone_suffix_treated_as_utc() {
        // Timezone suffixes are ignored — input is treated as UTC.
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00Z"),
            Some(0)
        );
        // +HH:MM suffix: only base 19 chars are parsed.
        assert_eq!(
            parse_sqlite_datetime_to_unix("1970-01-01 00:00:00+05:30"),
            Some(0)
        );
    }

    #[test]
    fn edge_type_weight_exact_values() {
        assert!((edge_type_weight(EdgeType::Causal) - 1.2).abs() < 1e-6);
        assert!((edge_type_weight(EdgeType::Semantic) - 1.0).abs() < 1e-6);
        assert!((edge_type_weight(EdgeType::Temporal) - 0.9).abs() < 1e-6);
        assert!((edge_type_weight(EdgeType::Entity) - 0.8).abs() < 1e-6);
    }

    #[test]
    fn composite_score_applies_non_baseline_type_weight() {
        // With entity_match_score=1.0, hop=0, confidence=1.0, retrieval_count=0:
        // evolved_weight = 1.0; composite = 1.0 * 1.0 * 1.0 * type_w = type_w.
        let fact = |et: EdgeType| GraphFact {
            entity_name: "A".into(),
            relation: "rel".into(),
            target_name: "B".into(),
            fact: "A rel B".into(),
            entity_match_score: 1.0,
            hop_distance: 0,
            confidence: 1.0,
            valid_from: None,
            edge_type: et,
            retrieval_count: 0,
        };
        assert!((fact(EdgeType::Causal).composite_score() - 1.2).abs() < 1e-5);
        assert!((fact(EdgeType::Temporal).composite_score() - 0.9).abs() < 1e-5);
        assert!((fact(EdgeType::Entity).composite_score() - 0.8).abs() < 1e-5);
    }

    #[test]
    fn composite_score_entity_lower_than_temporal_lower_than_causal() {
        let fact = |et: EdgeType| GraphFact {
            entity_name: "X".into(),
            relation: "r".into(),
            target_name: "Y".into(),
            fact: "X r Y".into(),
            entity_match_score: 0.8,
            hop_distance: 1,
            confidence: 0.9,
            valid_from: None,
            edge_type: et,
            retrieval_count: 0,
        };
        let causal = fact(EdgeType::Causal).composite_score();
        let temporal = fact(EdgeType::Temporal).composite_score();
        let entity = fact(EdgeType::Entity).composite_score();
        assert!(causal > temporal, "causal score must exceed temporal");
        assert!(temporal > entity, "temporal score must exceed entity");
    }
}
