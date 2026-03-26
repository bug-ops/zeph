// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
    pub canonical_name: String,
    pub entity_type: EntityType,
    pub summary: Option<String>,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub qdrant_point_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntityAlias {
    pub id: i64,
    pub entity_id: i64,
    pub alias_name: String,
    pub created_at: String,
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
    pub edge_type: EdgeType,
    /// Number of times this edge was traversed during graph recall (A-MEM link weight evolution).
    pub retrieval_count: i32,
    /// Unix timestamp of the last retrieval. `None` if never retrieved.
    pub last_retrieved_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Community {
    pub id: i64,
    pub name: String,
    pub summary: String,
    pub entity_ids: Vec<i64>,
    pub fingerprint: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Entity with its match score from hybrid seed selection.
#[derive(Debug, Clone)]
pub struct ScoredEntity {
    pub entity: Entity,
    pub fts_score: f32,
    pub structural_score: f32,
    pub community_id: Option<i64>,
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

impl GraphFact {
    /// Base composite score with A-MEM evolved edge weight.
    ///
    /// Formula: `entity_match_score * (1 / (1 + hop_distance)) * evolved_weight(retrieval_count, confidence)`
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn composite_score(&self) -> f32 {
        let w = evolved_weight(self.retrieval_count, self.confidence);
        self.entity_match_score * (1.0 / (1.0 + self.hop_distance as f32)) * w
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
}
