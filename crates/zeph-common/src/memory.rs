// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared memory interface types used by both `zeph-memory` (Layer 1) and
//! `zeph-context` (Layer 1) without a cross-layer dependency.
//!
//! Moving these pure interface types here resolves the same-layer violation
//! `zeph-context → zeph-memory` (issue #3665).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ── MemoryRoute ───────────────────────────────────────────────────────────────

/// Classification of which memory backend(s) to query.
///
/// Used in routing configuration and at runtime to dispatch memory operations.
/// Serialises with `snake_case` names (`keyword`, `semantic`, `hybrid`, `graph`, `episodic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemoryRoute {
    /// Full-text search only (`SQLite` FTS5). Fast, good for keyword/exact queries.
    Keyword,
    /// Vector search only (Qdrant). Good for semantic/conceptual queries.
    Semantic,
    /// Both backends, results merged by reciprocal rank fusion.
    #[default]
    Hybrid,
    /// Graph-based retrieval via BFS traversal.
    Graph,
    /// FTS5 search with a timestamp-range filter. Used for temporal/episodic queries.
    Episodic,
}

/// Routing decision with confidence and optional LLM reasoning.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub route: MemoryRoute,
    /// Confidence in `[0, 1]`. `1.0` = certain, `0.5` = ambiguous.
    pub confidence: f32,
    /// Only populated when an LLM classifier was used.
    pub reasoning: Option<String>,
}

/// Decides which memory backend(s) to query for a given input.
pub trait MemoryRouter: Send + Sync {
    /// Route a query to the appropriate backend(s).
    fn route(&self, query: &str) -> MemoryRoute;

    /// Route with a confidence signal. Default implementation wraps `route()` with confidence 1.0.
    fn route_with_confidence(&self, query: &str) -> RoutingDecision {
        RoutingDecision {
            route: self.route(query),
            confidence: 1.0,
            reasoning: None,
        }
    }
}

/// Async extension for LLM-capable routers.
pub trait AsyncMemoryRouter: MemoryRouter {
    fn route_async<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RoutingDecision> + Send + 'a>>;
}

// ── RecallView ────────────────────────────────────────────────────────────────

/// Enrichment level for view-aware graph recall.
///
/// # Examples
///
/// ```
/// use zeph_common::memory::RecallView;
///
/// assert_eq!(RecallView::default(), RecallView::Head);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RecallView {
    /// Standard retrieval — no enrichment beyond what the base method provides.
    #[default]
    Head,
    /// Retrieval + source-message provenance.
    ZoomIn,
    /// Retrieval + 1-hop neighbor expansion.
    ZoomOut,
}

// ── CompressionLevel ─────────────────────────────────────────────────────────

/// The three abstraction levels in the compression spectrum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum CompressionLevel {
    /// Raw episodic messages — full fidelity, high token cost.
    Episodic,
    /// Abstracted procedural knowledge (how-to, tool patterns).
    Procedural,
    /// Stable declarative facts and reference material.
    Declarative,
}

impl CompressionLevel {
    /// A relative token-cost factor for budgeting purposes.
    ///
    /// `Episodic = 1.0` (baseline), `Procedural = 0.6`, `Declarative = 0.3`.
    #[must_use]
    pub const fn cost_factor(self) -> f32 {
        match self {
            Self::Episodic => 1.0,
            Self::Procedural => 0.6,
            Self::Declarative => 0.3,
        }
    }
}

// ── AnchoredSummary ───────────────────────────────────────────────────────────

/// Structured compaction summary with anchored sections.
///
/// Produced by the structured summarization path during hard compaction.
/// Replaces the free-form 9-section prose when `[memory] structured_summaries = true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
pub struct AnchoredSummary {
    /// What the user is ultimately trying to accomplish in this session.
    pub session_intent: String,
    /// File paths, function names, structs/enums touched or referenced.
    pub files_modified: Vec<String>,
    /// Architectural or implementation decisions made, with rationale.
    pub decisions_made: Vec<String>,
    /// Unresolved questions, ambiguities, or blocked items.
    pub open_questions: Vec<String>,
    /// Concrete next actions the agent should take immediately.
    pub next_steps: Vec<String>,
}

impl AnchoredSummary {
    /// Returns true if the mandatory sections (`session_intent`, `next_steps`) are populated.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.session_intent.trim().is_empty() && !self.next_steps.is_empty()
    }

    /// Render as Markdown for context injection into the LLM.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("[anchored summary]\n");
        out.push_str("## Session Intent\n");
        out.push_str(&self.session_intent);
        out.push('\n');

        if !self.files_modified.is_empty() {
            out.push_str("\n## Files Modified\n");
            for entry in &self.files_modified {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        if !self.decisions_made.is_empty() {
            out.push_str("\n## Decisions Made\n");
            for entry in &self.decisions_made {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        if !self.open_questions.is_empty() {
            out.push_str("\n## Open Questions\n");
            for entry in &self.open_questions {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        if !self.next_steps.is_empty() {
            out.push_str("\n## Next Steps\n");
            for entry in &self.next_steps {
                let clean = entry.trim_start_matches("- ");
                out.push_str("- ");
                out.push_str(clean);
                out.push('\n');
            }
        }

        out
    }

    /// Validate per-field length limits to guard against bloated LLM output.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a descriptive message if any field exceeds its limit.
    #[must_use = "validation result must be checked"]
    pub fn validate(&self) -> Result<(), String> {
        const MAX_INTENT: usize = 2_000;
        const MAX_ENTRY: usize = 500;
        const MAX_VEC_LEN: usize = 50;

        if self.session_intent.len() > MAX_INTENT {
            return Err(format!(
                "session_intent exceeds {MAX_INTENT} chars (got {})",
                self.session_intent.len()
            ));
        }
        for (field, entries) in [
            ("files_modified", &self.files_modified),
            ("decisions_made", &self.decisions_made),
            ("open_questions", &self.open_questions),
            ("next_steps", &self.next_steps),
        ] {
            if entries.len() > MAX_VEC_LEN {
                return Err(format!(
                    "{field} has {} entries (max {MAX_VEC_LEN})",
                    entries.len()
                ));
            }
            for entry in entries {
                if entry.len() > MAX_ENTRY {
                    return Err(format!(
                        "{field} entry exceeds {MAX_ENTRY} chars (got {})",
                        entry.len()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Serialize to JSON for storage in `summaries.content`.
    ///
    /// # Panics
    ///
    /// Panics if serialization fails. Since all fields are `String`/`Vec<String>`,
    /// serialization is infallible in practice.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("AnchoredSummary serialization is infallible")
    }
}

// ── SpreadingActivationParams ─────────────────────────────────────────────────

/// Parameters for spreading activation graph retrieval.
#[derive(Debug, Clone)]
pub struct SpreadingActivationParams {
    pub decay_lambda: f32,
    pub max_hops: u32,
    pub activation_threshold: f32,
    pub inhibition_threshold: f32,
    pub max_activated_nodes: usize,
    pub temporal_decay_rate: f64,
    /// Weight of structural score in hybrid seed ranking. Range: `[0.0, 1.0]`. Default: `0.4`.
    pub seed_structural_weight: f32,
    /// Maximum seeds per community ID. `0` = unlimited. Default: `3`.
    pub seed_community_cap: usize,
    /// SYNAPSE blend coefficient for Benna-Fusi fast/slow variables (#3709).
    ///
    /// Blends `confidence_fast` and `confidence_slow` for edge weight in spreading activation:
    /// `blended = alpha * fast + (1 - alpha) * slow`.
    /// Range: `[0.0, 1.0]`. Default: `0.3` (favors the stable slow variable).
    pub alpha: f32,
}

// ── EdgeType ──────────────────────────────────────────────────────────────────

/// MAGMA edge type: the semantic category of a relationship between two entities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
    /// use zeph_common::memory::EdgeType;
    ///
    /// assert_eq!(EdgeType::Causal.as_str(), "causal");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
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
        f.pad(self.as_str())
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

// ── FunctionalType ────────────────────────────────────────────────────────────

/// MemGuard-inspired functional-role classification of a memory source (spec 004-16, #6086).
///
/// Each variant names one of the memory sources composed during context assembly
/// (`schedule_context_fetchers` in `zeph-context`) — not a storage tier
/// ([`CompressionLevel`]) and not a routing backend ([`MemoryRoute`]). The two axes are
/// orthogonal: a `zeph_conversations` vector is `Episodic`-tier *and* the `Episodic`
/// functional type, while a `Semantic`-tier consolidated fact lives under the `UserFact`
/// functional type. Placed here (rather than in `zeph-memory`) because `zeph-context` — the
/// crate that gates fetchers by this type — deliberately has no `zeph-memory` dependency
/// (see the module doc above and issue #3665); `zeph-memory` re-exports this type at its
/// crate root for taxonomy discoverability.
///
/// `#[non_exhaustive]`: additional functional sources may be added later without a breaking
/// change; an unrecognised variant is always-composed until explicitly gated (never silently
/// dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FunctionalType {
    /// Raw episodic conversation recall (`fetch_semantic_recall` → `zeph_conversations`).
    Episodic,
    /// User preference/attribute facts (`fetch_persona_facts` → SQL `persona_memory`,
    /// **not** the `zeph_key_facts` collection — that surface is out of scope, see spec 004-16 §4).
    UserFact,
    /// Past user corrections (`fetch_corrections` → `zeph_corrections`).
    ///
    /// Safety-critical: this type is never gated out by type-aware composition — context
    /// assembly always schedules `fetch_corrections` regardless of the active set.
    BehavioralRule,
    /// Distilled `ReasoningBank` strategies (`fetch_reasoning_strategies` → `reasoning_strategies`).
    ReasoningStrategy,
    /// Cross-session summaries (`fetch_summaries` / `fetch_cross_session` → `zeph_session_summaries`).
    CrossSessionSummary,
    /// Knowledge graph facts (`fetch_graph_facts` → `zeph_graph_entities`).
    GraphFact,
}

impl FunctionalType {
    /// Return the canonical lowercase string for this functional type.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::memory::FunctionalType;
    ///
    /// assert_eq!(FunctionalType::UserFact.as_str(), "user_fact");
    /// assert_eq!(FunctionalType::BehavioralRule.as_str(), "behavioral_rule");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Episodic => "episodic",
            Self::UserFact => "user_fact",
            Self::BehavioralRule => "behavioral_rule",
            Self::ReasoningStrategy => "reasoning_strategy",
            Self::CrossSessionSummary => "cross_session_summary",
            Self::GraphFact => "graph_fact",
        }
    }
}

impl fmt::Display for FunctionalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for FunctionalType {
    type Err = String;

    /// Strict parse: an unrecognised string is a hard error, never a silent fallback.
    ///
    /// This is deliberate (spec 004-16 §4, critic finding S4): a config typo in
    /// `default_compose_types` must fail config load, not silently widen to "all types".
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "episodic" => Ok(Self::Episodic),
            "user_fact" => Ok(Self::UserFact),
            "behavioral_rule" => Ok(Self::BehavioralRule),
            "reasoning_strategy" => Ok(Self::ReasoningStrategy),
            "cross_session_summary" => Ok(Self::CrossSessionSummary),
            "graph_fact" => Ok(Self::GraphFact),
            other => Err(format!("unknown functional memory type: {other}")),
        }
    }
}

// ── Marker constants ──────────────────────────────────────────────────────────

/// MAGMA causal edge markers used by `classify_graph_subgraph`.
pub const CAUSAL_MARKERS: &[&str] = &[
    "why",
    "because",
    "caused",
    "cause",
    "reason",
    "result",
    "led to",
    "consequence",
    "trigger",
    "effect",
    "blame",
    "fault",
];

/// MAGMA temporal edge markers for subgraph classification.
pub const TEMPORAL_MARKERS: &[&str] = &[
    "before", "after", "first", "then", "timeline", "sequence", "preceded", "followed", "started",
    "ended", "during", "prior",
];

/// MAGMA entity/structural markers.
pub const ENTITY_MARKERS: &[&str] = &[
    "is a",
    "type of",
    "kind of",
    "part of",
    "instance",
    "same as",
    "alias",
    "subtype",
    "subclass",
    "belongs to",
];

/// Single-word temporal tokens that require word-boundary checking.
pub const WORD_BOUNDARY_TEMPORAL: &[&str] = &["ago"];

/// Classify a query into MAGMA edge types to use for subgraph-scoped BFS retrieval.
///
/// Pure heuristic, zero latency — no LLM call. Returns a prioritised list of [`EdgeType`]s.
///
/// # Example
///
/// ```
/// use zeph_common::memory::{classify_graph_subgraph, EdgeType};
///
/// let types = classify_graph_subgraph("why did X happen");
/// assert!(types.contains(&EdgeType::Causal));
/// assert!(types.contains(&EdgeType::Semantic));
/// ```
#[must_use]
pub fn classify_graph_subgraph(query: &str) -> Vec<EdgeType> {
    let lower = query.to_ascii_lowercase();
    let mut types: Vec<EdgeType> = Vec::new();

    if CAUSAL_MARKERS.iter().any(|m| lower.contains(m)) {
        types.push(EdgeType::Causal);
    }
    if TEMPORAL_MARKERS.iter().any(|m| lower.contains(m)) {
        types.push(EdgeType::Temporal);
    }
    if ENTITY_MARKERS.iter().any(|m| lower.contains(m)) {
        types.push(EdgeType::Entity);
    }

    if !types.contains(&EdgeType::Semantic) {
        types.push(EdgeType::Semantic);
    }

    types
}

/// Parse a route name string into a [`MemoryRoute`], falling back to `fallback` on unknown values.
///
/// # Examples
///
/// ```
/// use zeph_common::memory::{parse_route_str, MemoryRoute};
///
/// assert_eq!(parse_route_str("semantic", MemoryRoute::Hybrid), MemoryRoute::Semantic);
/// assert_eq!(parse_route_str("unknown", MemoryRoute::Hybrid), MemoryRoute::Hybrid);
/// ```
#[must_use]
pub fn parse_route_str(s: &str, fallback: MemoryRoute) -> MemoryRoute {
    match s {
        "keyword" => MemoryRoute::Keyword,
        "semantic" => MemoryRoute::Semantic,
        "hybrid" => MemoryRoute::Hybrid,
        "graph" => MemoryRoute::Graph,
        "episodic" => MemoryRoute::Episodic,
        _ => fallback,
    }
}

// ── TokenCounting trait ───────────────────────────────────────────────────────

/// Minimal token-counting interface used by `zeph-context` for budget enforcement.
///
/// Defined here in Layer 0 so `zeph-context` can accept a `&dyn TokenCounting`
/// without importing `zeph-memory`. `zeph-memory::TokenCounter` implements this trait.
pub trait TokenCounting: Send + Sync {
    /// Count tokens in a plain text string.
    fn count_tokens(&self, text: &str) -> usize;
    /// Count tokens for a JSON schema value (tool definitions).
    fn count_tool_schema_tokens(&self, schema: &serde_json::Value) -> usize;
}

// ── Context memory DTOs ───────────────────────────────────────────────────────
//
// Plain data-transfer structs used by `ContextMemoryBackend`. They mirror the
// fields that `zeph-context::assembler` actually reads from `zeph-memory` row
// types. Keeping them here (Layer 0) allows `zeph-context` (Layer 1) to depend
// only on `zeph-common` rather than `zeph-memory`.

/// A persona fact row projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemPersonaFact {
    /// Fact category label (e.g. `"preference"`, `"domain"`).
    pub category: String,
    /// Fact content injected into the system prompt.
    pub content: String,
}

/// A memory tree node projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemTreeNode {
    /// Node content injected into the system prompt.
    pub content: String,
}

/// A conversation summary projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemSummary {
    /// Row ID of the first message covered by this summary, if known.
    pub first_message_id: Option<i64>,
    /// Row ID of the last message covered by this summary, if known.
    pub last_message_id: Option<i64>,
    /// Summary text.
    pub content: String,
}

/// A reasoning strategy projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemReasoningStrategy {
    /// Unique strategy identifier (used by `mark_reasoning_used`).
    pub id: String,
    /// Outcome label (e.g. `"success"`, `"failure"`).
    pub outcome: String,
    /// Distilled strategy summary injected into the system prompt.
    pub summary: String,
}

/// A user correction projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemCorrection {
    /// The correction text to inject into the system prompt.
    pub correction_text: String,
}

/// A recalled message projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemRecalledMessage {
    /// Message role: `"user"`, `"assistant"`, or `"system"`.
    pub role: String,
    /// Message content.
    pub content: String,
    /// Similarity score in `[0, 1]`.
    pub score: f32,
}

/// A neighbor fact in a graph recall result.
#[derive(Debug, Clone)]
pub struct MemGraphNeighbor {
    /// Neighbor fact text.
    pub fact: String,
    /// Confidence score in `[0, 1]`.
    pub confidence: f32,
}

/// A graph fact projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemGraphFact {
    /// Fact text.
    pub fact: String,
    /// Confidence score in `[0, 1]`.
    pub confidence: f32,
    /// Spreading-activation score, if applicable.
    pub activation_score: Option<f32>,
    /// `ZoomOut` 1-hop neighbors, if view-aware expansion was requested.
    pub neighbors: Vec<MemGraphNeighbor>,
    /// `ZoomIn` provenance snippet, if view-aware provenance was requested.
    pub provenance_snippet: Option<String>,
}

/// A cross-session summary search result used by context assembly.
#[derive(Debug, Clone)]
pub struct MemSessionSummary {
    /// Summary text from the matched session.
    pub summary_text: String,
    /// Similarity score in `[0, 1]`.
    pub score: f32,
}

/// A document chunk search result used by context assembly.
#[derive(Debug, Clone)]
pub struct MemDocumentChunk {
    /// Chunk text extracted from the `"text"` payload key.
    pub text: String,
}

/// A trajectory entry projection used by context assembly.
#[derive(Debug, Clone)]
pub struct MemTrajectoryEntry {
    /// Intent description for the trajectory entry.
    pub intent: String,
    /// Outcome description.
    pub outcome: String,
    /// Confidence score in `[0, 1]`.
    pub confidence: f64,
}

/// Graph traversal algorithm selector for a graph recall call.
///
/// Mirrors `zeph_config::memory::GraphRetrievalStrategy` so that `zeph-context` (Layer 1)
/// can pass a strategy selection through [`GraphRecallParams`] without depending on
/// `zeph-config`'s concrete enum. Concrete dispatch onto `SemanticMemory` methods happens
/// in `zeph-agent-context::memory_backend` (Layer 4).
///
/// # Examples
///
/// ```
/// use zeph_common::memory::GraphRetrievalStrategy;
///
/// assert_eq!(GraphRetrievalStrategy::default(), GraphRetrievalStrategy::Synapse);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphRetrievalStrategy {
    /// SYNAPSE spreading activation (default, existing behavior).
    #[default]
    Synapse,
    /// Hop-limited BFS traversal (pre-SYNAPSE behavior).
    Bfs,
    /// A* shortest-path traversal.
    AStar,
    /// Concentric BFS expanding outward from seed nodes.
    WaterCircles,
    /// Beam search: keep top-K candidates per hop.
    BeamSearch,
    /// Dynamic: LLM classifier selects strategy per query.
    Hybrid,
}

// ── GraphRecallParams ─────────────────────────────────────────────────────────

/// Parameters for a graph-view recall call, used by [`ContextMemoryBackend::recall_graph_facts`].
#[derive(Debug)]
pub struct GraphRecallParams<'a> {
    /// Maximum number of graph facts to return.
    pub limit: usize,
    /// Enrichment view (head, zoom-in, zoom-out).
    pub view: RecallView,
    /// Cap on `ZoomOut` neighbor expansion.
    pub zoom_out_neighbor_cap: usize,
    /// Maximum BFS hops during graph traversal.
    pub max_hops: u32,
    /// Rate at which older facts are downweighted.
    pub temporal_decay_rate: f64,
    /// Edge type filters for subgraph-scoped BFS.
    pub edge_types: &'a [EdgeType],
    /// Spreading activation parameters. `None` disables spreading activation.
    pub spreading_activation: Option<SpreadingActivationParams>,
    /// Graph traversal algorithm to use.
    pub retrieval_strategy: GraphRetrievalStrategy,
    /// Beam width for the `BeamSearch` strategy.
    pub beam_width: usize,
    /// Per-ring fact cap for the `WaterCircles` strategy. `0` = auto.
    pub ring_limit: usize,
}

// ── ContextMemoryBackend trait ────────────────────────────────────────────────

/// Abstraction over `SemanticMemory` that `zeph-context` uses for all memory
/// operations during context assembly.
///
/// Defined in Layer 0 (`zeph-common`) so that `zeph-context` (Layer 1) can hold
/// `Option<Arc<dyn ContextMemoryBackend>>` without importing `zeph-memory`.
/// `zeph-core` (Layer 4) provides the concrete implementation that wraps
/// `SemanticMemory`.
///
/// All async methods use `Pin<Box<dyn Future<...>>>` for dyn-compatibility.
#[allow(clippy::type_complexity)]
pub trait ContextMemoryBackend: Send + Sync {
    /// Load persona facts with at least `min_confidence`.
    fn load_persona_facts<'a>(
        &'a self,
        min_confidence: f64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<MemPersonaFact>, Box<dyn std::error::Error + Send + Sync>>,
                > + Send
                + 'a,
        >,
    >;

    /// Load `top_k` trajectory entries for the given `tier` filter (e.g. `"procedural"`).
    fn load_trajectory_entries<'a>(
        &'a self,
        tier: Option<&'a str>,
        top_k: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<MemTrajectoryEntry>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Load `top_k` memory tree nodes at the given level.
    fn load_tree_nodes<'a>(
        &'a self,
        level: u32,
        top_k: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<MemTreeNode>, Box<dyn std::error::Error + Send + Sync>>,
                > + Send
                + 'a,
        >,
    >;

    /// Load all summaries for the given conversation (raw row ID).
    fn load_summaries<'a>(
        &'a self,
        conversation_id: i64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<MemSummary>, Box<dyn std::error::Error + Send + Sync>>,
                > + Send
                + 'a,
        >,
    >;

    /// Retrieve the top-`top_k` reasoning strategies for `query`.
    fn retrieve_reasoning_strategies<'a>(
        &'a self,
        query: &'a str,
        top_k: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<MemReasoningStrategy>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Mark reasoning strategies as used (fire-and-forget; best-effort).
    fn mark_reasoning_used<'a>(
        &'a self,
        ids: &'a [String],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>
                + Send
                + 'a,
        >,
    >;

    /// Retrieve corrections similar to `query`, up to `limit` with `min_score`.
    fn retrieve_corrections<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        min_score: f32,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<MemCorrection>, Box<dyn std::error::Error + Send + Sync>>,
                > + Send
                + 'a,
        >,
    >;

    /// Recall semantically similar messages for `query`, up to `limit`.
    fn recall<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        router: Option<&'a dyn AsyncMemoryRouter>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<MemRecalledMessage>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Recall graph facts for `query` with view-aware enrichment.
    fn recall_graph_facts<'a>(
        &'a self,
        query: &'a str,
        params: GraphRecallParams<'a>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<MemGraphFact>, Box<dyn std::error::Error + Send + Sync>>,
                > + Send
                + 'a,
        >,
    >;

    /// Search cross-session summaries for `query`, excluding `current_conversation_id`.
    fn search_session_summaries<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        current_conversation_id: Option<i64>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<MemSessionSummary>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Search a named document collection for `query`, returning `top_k` chunks.
    fn search_document_collection<'a>(
        &'a self,
        collection: &'a str,
        query: &'a str,
        top_k: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<MemDocumentChunk>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send
                + 'a,
        >,
    >;
}

#[cfg(test)]
mod tests {
    use super::{EdgeType, FunctionalType, MemoryRoute};
    use std::str::FromStr;

    /// Locks in the `f.pad` fix (#6066): `f.write_str` ignores width/fill/align flags.
    /// `f.pad` must reproduce the same padding a plain `&str` would get under an
    /// identical width specifier.
    #[test]
    fn edge_type_display_respects_width() {
        assert_eq!(
            format!("{:<10}", EdgeType::Causal),
            format!("{:<10}", "causal")
        );
        assert_eq!(
            format!("{:>10}", EdgeType::Semantic),
            format!("{:>10}", "semantic")
        );
    }

    #[test]
    fn memory_route_serde_roundtrip() {
        let cases = [
            ("\"keyword\"", MemoryRoute::Keyword),
            ("\"semantic\"", MemoryRoute::Semantic),
            ("\"hybrid\"", MemoryRoute::Hybrid),
            ("\"graph\"", MemoryRoute::Graph),
            ("\"episodic\"", MemoryRoute::Episodic),
        ];
        for (json_str, expected) in cases {
            let got: MemoryRoute = serde_json::from_str(json_str).unwrap();
            assert_eq!(got, expected);
            let serialized = serde_json::to_string(&got).unwrap();
            let roundtrip: MemoryRoute = serde_json::from_str(&serialized).unwrap();
            assert_eq!(roundtrip, expected);
        }
    }

    #[test]
    fn memory_route_default_is_hybrid() {
        assert_eq!(MemoryRoute::default(), MemoryRoute::Hybrid);
    }

    #[test]
    fn functional_type_from_str_round_trips_every_variant() {
        let all = [
            FunctionalType::Episodic,
            FunctionalType::UserFact,
            FunctionalType::BehavioralRule,
            FunctionalType::ReasoningStrategy,
            FunctionalType::CrossSessionSummary,
            FunctionalType::GraphFact,
        ];
        for variant in all {
            assert_eq!(FunctionalType::from_str(variant.as_str()), Ok(variant));
        }
    }

    #[test]
    fn functional_type_from_str_rejects_unknown_string() {
        // S4: unknown/typo'd type strings must fail closed, not silently widen.
        assert!(FunctionalType::from_str("user_facts").is_err());
        assert!(FunctionalType::from_str("").is_err());
    }

    #[test]
    fn functional_type_serde_roundtrip() {
        let cases = [
            ("\"episodic\"", FunctionalType::Episodic),
            ("\"user_fact\"", FunctionalType::UserFact),
            ("\"behavioral_rule\"", FunctionalType::BehavioralRule),
            ("\"reasoning_strategy\"", FunctionalType::ReasoningStrategy),
            (
                "\"cross_session_summary\"",
                FunctionalType::CrossSessionSummary,
            ),
            ("\"graph_fact\"", FunctionalType::GraphFact),
        ];
        for (json_str, expected) in cases {
            let got: FunctionalType = serde_json::from_str(json_str).unwrap();
            assert_eq!(got, expected);
            let serialized = serde_json::to_string(&got).unwrap();
            let roundtrip: FunctionalType = serde_json::from_str(&serialized).unwrap();
            assert_eq!(roundtrip, expected);
        }
    }

    #[test]
    fn functional_type_serde_rejects_unknown_variant() {
        let result: Result<FunctionalType, _> = serde_json::from_str("\"user_facts\"");
        assert!(result.is_err());
    }

    #[test]
    fn functional_type_display_respects_width() {
        assert_eq!(
            format!("{:<12}", FunctionalType::UserFact),
            format!("{:<12}", "user_fact")
        );
    }
}
