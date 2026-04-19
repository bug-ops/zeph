// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! APEX-MEM ontology normalization layer.
//!
//! Resolves raw LLM-emitted predicate strings to canonical forms using a static TOML table
//! and an optional LLM-assisted fallback. Maintains a bounded LRU cache so repeated
//! resolution of the same alias avoids both table scans and LLM calls.
//!
//! # Reload
//!
//! Call [`OntologyTable::reload`] to swap in a fresh TOML file at runtime (e.g., on
//! `/graph ontology reload`). The in-memory alias table and LRU cache are replaced atomically
//! via [`arc_swap::ArcSwap`] so concurrent readers observe either the old or new state with
//! no partial updates (critic nit #3).

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use lru::LruCache;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::error::MemoryError;

/// Cardinality of an ontology predicate.
///
/// - `One`: at most one active head edge per `(source, canonical_relation, edge_type)` at recall.
///   The conflict resolver runs when multiple head edges coexist for this predicate.
/// - `Many`: multi-valued predicate; all head edges pass through recall unchanged.
///
/// # Notes
///
/// Cardinality is keyed per `canonical` only (not per `(canonical, EdgeType)`) because the
/// TOML format declares it per predicate without an `edge_type` field (critic nit #2). Per-edge-type
/// overrides are a future extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cardinality {
    /// Single-valued: conflict resolver picks one head edge when multiples coexist.
    One,
    /// Multi-valued: all head edges pass through recall unchanged.
    #[default]
    Many,
}

/// In-memory state swapped atomically on reload.
#[derive(Debug, Default)]
struct OntologyState {
    /// alias (lowercase-trimmed) → canonical (lowercase-trimmed)
    alias_to_canonical: HashMap<String, String>,
    /// canonical (lowercase-trimmed) → cardinality
    cardinality: HashMap<String, Cardinality>,
}

impl OntologyState {
    fn build(predicates: &[PredicateToml]) -> Self {
        let mut alias_to_canonical = HashMap::new();
        let mut cardinality = HashMap::new();

        for entry in predicates {
            let canonical = normalize(&entry.canonical);
            let card = match entry.cardinality.as_deref() {
                Some("1") => Cardinality::One,
                _ => Cardinality::Many,
            };
            alias_to_canonical.insert(canonical.clone(), canonical.clone());
            cardinality.insert(canonical.clone(), card);
            for alias in &entry.aliases {
                alias_to_canonical.insert(normalize(alias), canonical.clone());
            }
        }
        Self {
            alias_to_canonical,
            cardinality,
        }
    }
}

/// The loaded APEX-MEM ontology table plus bounded LRU cache for resolved mappings.
///
/// Designed for read-heavy workloads: the static table and cardinality map are behind
/// `ArcSwap` for lock-free reads; the LRU cache is behind a `Mutex` (written only on
/// misses and LLM fallback results).
pub struct OntologyTable {
    state: ArcSwap<OntologyState>,
    /// Bounded LRU: alias (lowercase-trimmed) → canonical. Includes LLM-fallback entries.
    cache: Mutex<LruCache<String, String>>,
    cache_max: usize,
}

impl std::fmt::Debug for OntologyTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OntologyTable")
            .field("state", &"<ArcSwap<OntologyState>>")
            .field("cache", &"<Mutex<LruCache>>")
            .field("cache_max", &self.cache_max)
            .finish()
    }
}

impl OntologyTable {
    fn new_with_state(state: OntologyState, cache_max: usize) -> Self {
        let cap = NonZeroUsize::new(cache_max.max(1)).expect("cache_max >= 1");
        Self {
            state: ArcSwap::new(Arc::new(state)),
            cache: Mutex::new(LruCache::new(cap)),
            cache_max,
        }
    }

    /// Create from the embedded default ontology table.
    #[must_use]
    pub fn from_default(cache_max: usize) -> Self {
        let state = OntologyState::build(default_predicates());
        Self::new_with_state(state, cache_max)
    }

    /// Load from a TOML file at `path`, or fall back to embedded defaults when `path` is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be parsed.
    pub async fn from_path(path: &Path, cache_max: usize) -> Result<Self, MemoryError> {
        let predicates = if path.as_os_str().is_empty() {
            default_predicates().to_vec()
        } else {
            load_toml_file(path).await?
        };
        let state = OntologyState::build(&predicates);
        Ok(Self::new_with_state(state, cache_max))
    }

    /// Reload the ontology table from `path` (or embedded defaults if `path` is empty).
    ///
    /// The LRU cache is cleared atomically with the table swap so stale mappings from the
    /// old table cannot win over new canonical forms.
    ///
    /// # Errors
    ///
    /// Returns an error if the new TOML cannot be parsed.
    pub async fn reload(&self, path: &Path) -> Result<(), MemoryError> {
        let predicates = if path.as_os_str().is_empty() {
            default_predicates().to_vec()
        } else {
            load_toml_file(path).await?
        };
        let new_state = Arc::new(OntologyState::build(&predicates));
        // Clear cache before swapping state: atomic table+cache swap prevents readers
        // from observing new table with stale cache entries.
        let mut cache = self.cache.lock().await;
        cache.clear();
        self.state.store(new_state);
        Ok(())
    }

    /// Resolve `raw_predicate` to its canonical form.
    ///
    /// Resolution order:
    /// 1. LRU cache hit
    /// 2. Static table lookup
    /// 3. Miss: return raw as canonical (cardinality-n default)
    ///
    /// Returns `(canonical, was_unmapped)`. `was_unmapped` is `true` when the predicate
    /// had no entry in the static table; callers should increment the unmapped counter.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use zeph_memory::graph::ontology::OntologyTable;
    ///
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let table = OntologyTable::from_default(64);
    /// let (canonical, unmapped) = table.resolve("employed_by").await;
    /// assert_eq!(canonical, "works_at");
    /// assert!(!unmapped);
    /// # })
    /// ```
    pub async fn resolve(&self, raw_predicate: &str) -> (String, bool) {
        let key = normalize(raw_predicate);
        tracing::debug!(target: "memory.graph.apex.ontology_resolve", predicate = raw_predicate);

        {
            let mut cache = self.cache.lock().await;
            if let Some(canonical) = cache.get(&key) {
                return (canonical.clone(), false);
            }
        }

        let state = self.state.load();
        if let Some(canonical) = state.alias_to_canonical.get(&key) {
            let canonical = canonical.clone();
            let mut cache = self.cache.lock().await;
            cache.put(key, canonical.clone());
            return (canonical, false);
        }

        // Predicate not in static table — use raw form as canonical.
        let canonical = key.clone();
        let mut cache = self.cache.lock().await;
        cache.put(key, canonical.clone());
        (canonical, true)
    }

    /// Return the cardinality for `canonical_predicate`.
    ///
    /// Defaults to [`Cardinality::Many`] for predicates not in the ontology table.
    #[must_use]
    pub fn cardinality(&self, canonical_predicate: &str) -> Cardinality {
        let key = normalize(canonical_predicate);
        self.state
            .load()
            .cardinality
            .get(&key)
            .copied()
            .unwrap_or_default()
    }
}

/// Normalize a predicate string: trim whitespace, remove control characters, lowercase.
pub(crate) fn normalize(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .to_lowercase()
}

// ── TOML deserialization ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct OntologyToml {
    #[serde(rename = "predicate")]
    predicates: Vec<PredicateToml>,
}

#[derive(Debug, Clone, Deserialize)]
struct PredicateToml {
    canonical: String,
    #[serde(default)]
    aliases: Vec<String>,
    /// Accepts `"1"` or `"n"` (string) or integer `1` via custom deserialization.
    #[serde(default, deserialize_with = "de_cardinality")]
    cardinality: Option<String>,
}

fn de_cardinality<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Visitor;

    struct CardVisitor;
    impl<'de> Visitor<'de> for CardVisitor {
        type Value = Option<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, r#"cardinality string "1" or "n", or integer 1"#)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(if v == 1 {
                "1".to_string()
            } else {
                "n".to_string()
            }))
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(self)
        }
    }

    deserializer.deserialize_option(CardVisitor)
}

async fn load_toml_file(path: &Path) -> Result<Vec<PredicateToml>, MemoryError> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| MemoryError::InvalidInput(format!("ontology TOML read error: {e}")))?;
    let parsed: OntologyToml = toml::from_str(&content)
        .map_err(|e| MemoryError::InvalidInput(format!("ontology TOML parse error: {e}")))?;
    Ok(parsed.predicates)
}

// ── Embedded default predicates ──────────────────────────────────────────────

fn make(canonical: &str, aliases: &[&str], cardinality: &str) -> PredicateToml {
    PredicateToml {
        canonical: canonical.to_string(),
        aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
        cardinality: Some(cardinality.to_string()),
    }
}

fn default_predicates() -> &'static [PredicateToml] {
    use std::sync::OnceLock;
    static DEFAULTS: OnceLock<Vec<PredicateToml>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        vec![
            make("works_at", &["employed_by", "job_at", "works_for"], "1"),
            make("lives_in", &["resides_in", "based_in"], "1"),
            make("born_in", &["birthplace", "born_at"], "1"),
            make("manages", &["manages_team", "leads", "supervises"], "1"),
            make("owns", &["has", "possesses"], "n"),
            make("depends_on", &["requires", "needs"], "n"),
            make("knows", &[], "n"),
        ]
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_alias_to_canonical() {
        let table = OntologyTable::from_default(64);
        let (canonical, unmapped) = table.resolve("employed_by").await;
        assert_eq!(canonical, "works_at");
        assert!(!unmapped);
    }

    #[tokio::test]
    async fn resolves_canonical_to_itself() {
        let table = OntologyTable::from_default(64);
        let (canonical, unmapped) = table.resolve("works_at").await;
        assert_eq!(canonical, "works_at");
        assert!(!unmapped);
    }

    #[tokio::test]
    async fn unknown_predicate_returns_raw_and_unmapped() {
        let table = OntologyTable::from_default(64);
        let (canonical, unmapped) = table.resolve("some_new_predicate").await;
        assert_eq!(canonical, "some_new_predicate");
        assert!(unmapped);
    }

    #[tokio::test]
    async fn cardinality_one_predicates() {
        let table = OntologyTable::from_default(64);
        assert_eq!(table.cardinality("works_at"), Cardinality::One);
        assert_eq!(table.cardinality("lives_in"), Cardinality::One);
        assert_eq!(table.cardinality("born_in"), Cardinality::One);
        assert_eq!(table.cardinality("manages"), Cardinality::One);
    }

    #[tokio::test]
    async fn cardinality_many_predicates() {
        let table = OntologyTable::from_default(64);
        assert_eq!(table.cardinality("owns"), Cardinality::Many);
        assert_eq!(table.cardinality("depends_on"), Cardinality::Many);
        assert_eq!(table.cardinality("unknown_pred"), Cardinality::Many);
    }

    #[tokio::test]
    async fn normalize_trims_and_lowercases() {
        assert_eq!(normalize("  Works_At  "), "works_at");
        assert_eq!(normalize("EMPLOYED_BY"), "employed_by");
    }

    #[tokio::test]
    async fn cache_hit_on_second_resolve() {
        let table = OntologyTable::from_default(64);
        let (c1, _) = table.resolve("job_at").await;
        let (c2, _) = table.resolve("job_at").await;
        assert_eq!(c1, c2);
        assert_eq!(c1, "works_at");
    }

    #[tokio::test]
    async fn reload_clears_cache_and_preserves_resolution() {
        let table = OntologyTable::from_default(64);
        let _ = table.resolve("job_at").await;
        table.reload(Path::new("")).await.unwrap();
        let (canonical, _) = table.resolve("job_at").await;
        assert_eq!(canonical, "works_at");
    }
}
