// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use chrono::{DateTime, Duration, Utc};

use crate::graph::EdgeType;

/// Classification of which memory backend(s) to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRoute {
    /// Full-text search only (`SQLite` FTS5). Fast, good for keyword/exact queries.
    Keyword,
    /// Vector search only (Qdrant). Good for semantic/conceptual queries.
    Semantic,
    /// Both backends, results merged by reciprocal rank fusion.
    Hybrid,
    /// Graph-based retrieval via BFS traversal. Good for relationship queries.
    /// When the `graph-memory` feature is disabled, callers treat this as `Hybrid`.
    Graph,
    /// FTS5 search with a timestamp-range filter. Used for temporal/episodic queries
    /// ("what did we discuss yesterday", "last week's conversation about Rust").
    ///
    /// Known trade-off (MVP): skips vector search entirely for speed. Semantically similar
    /// but lexically different messages may be missed. Use `Hybrid` route when semantic
    /// precision matters more than temporal filtering.
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
    ///
    /// Override this in routers that can express ambiguity (e.g. `HeuristicRouter`)
    /// so that `HybridRouter` can escalate uncertain decisions to LLM.
    fn route_with_confidence(&self, query: &str) -> RoutingDecision {
        RoutingDecision {
            route: self.route(query),
            confidence: 1.0,
            reasoning: None,
        }
    }
}

/// Resolved datetime boundaries for a temporal query.
///
/// Both fields use `SQLite` datetime format (`YYYY-MM-DD HH:MM:SS`, UTC).
/// `None` means "no bound" on that side.
///
/// Note: All timestamps are UTC. The `created_at` column in the `messages` table
/// defaults to `datetime('now')` which is also UTC, so comparisons are consistent.
/// Users in non-UTC timezones may get slightly unexpected results for "yesterday"
/// queries (e.g. at 01:00 UTC+5 the user's local yesterday differs from UTC yesterday).
/// This is an accepted approximation for the heuristic-only MVP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalRange {
    /// Exclusive lower bound: `created_at > after`.
    pub after: Option<String>,
    /// Exclusive upper bound: `created_at < before`.
    pub before: Option<String>,
}

/// Temporal patterns that indicate an episodic / time-scoped recall query.
///
/// Multi-word patterns are preferred over single-word ones to reduce false positives.
/// Single-word patterns that can appear inside other words (e.g. "ago" in "Chicago")
/// must be checked with `contains_word()` to enforce word-boundary semantics.
///
/// Omitted on purpose: "before", "after", "since", "during", "earlier", "recently"
/// — these are too ambiguous in technical contexts ("before the function returns",
/// "since you asked", "during compilation"). They are not in this list.
const TEMPORAL_PATTERNS: &[&str] = &[
    // relative day
    "yesterday",
    "today",
    "this morning",
    "tonight",
    "last night",
    // relative week
    "last week",
    "this week",
    "past week",
    // relative month
    "last month",
    "this month",
    "past month",
    // temporal questions
    "when did",
    "remember when",
    "last time",
    "how long ago",
    // relative phrases requiring word-boundary check
    // (checked separately via `contains_word` to avoid matching "a few days ago" substring in longer words)
    "few days ago",
    "few hours ago",
    "earlier today",
];

/// Single-word temporal tokens that require word-boundary checking.
/// These are NOT in `TEMPORAL_PATTERNS` to avoid substring false positives.
const WORD_BOUNDARY_TEMPORAL: &[&str] = &["ago"];

/// MAGMA causal edge markers.
///
/// Shared between [`HeuristicRouter`] and [`classify_graph_subgraph`] to prevent
/// pattern-list drift between the two classifiers (critic suggestion).
pub(crate) const CAUSAL_MARKERS: &[&str] = &[
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
///
/// Shared between [`HeuristicRouter`] and [`classify_graph_subgraph`].
/// Note: these are distinct from `TEMPORAL_PATTERNS` (which drive `Episodic` routing).
/// `TEMPORAL_MARKERS` detect edges whose *semantics* are temporal (sequencing/ordering),
/// while `TEMPORAL_PATTERNS` detect queries that ask about *when* events occurred.
pub(crate) const TEMPORAL_MARKERS: &[&str] = &[
    "before", "after", "first", "then", "timeline", "sequence", "preceded", "followed", "started",
    "ended", "during", "prior",
];

/// MAGMA entity/structural markers.
pub(crate) const ENTITY_MARKERS: &[&str] = &[
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

/// Classify a query into the MAGMA edge types to use for subgraph-scoped BFS retrieval.
///
/// Pure heuristic, zero latency — no LLM call. Returns a prioritised list of [`EdgeType`]s.
///
/// Rules (checked in order):
/// 1. Causal markers → include `Causal`
/// 2. Temporal markers → include `Temporal`
/// 3. Entity/structural markers → include `Entity`
/// 4. `Semantic` is always included as fallback to guarantee recall >= current untyped BFS.
///
/// Multiple markers may match, producing a union of detected types.
///
/// # Example
///
/// ```
/// # use zeph_memory::router::classify_graph_subgraph;
/// # use zeph_memory::EdgeType;
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

    // Semantic is always included as fallback — recall cannot be worse than untyped BFS.
    if !types.contains(&EdgeType::Semantic) {
        types.push(EdgeType::Semantic);
    }

    types
}

/// Heuristic-based memory router.
///
/// Decision logic (in priority order):
/// 1. Temporal patterns → `Episodic`
/// 2. Relationship patterns → `Graph`
/// 3. Code-like patterns (paths, `::`) without question word → `Keyword`
/// 4. Long NL query or question word → `Semantic`
/// 5. Short non-question query → `Keyword`
/// 6. Default → `Hybrid`
pub struct HeuristicRouter;

const QUESTION_WORDS: &[&str] = &[
    "what", "how", "why", "when", "where", "who", "which", "explain", "describe",
];

/// Simple substrings that signal a relationship query (checked via `str::contains`).
/// Only used when the `graph-memory` feature is enabled.
const RELATIONSHIP_PATTERNS: &[&str] = &[
    "related to",
    "relates to",
    "connection between",
    "relationship",
    "opinion on",
    "thinks about",
    "preference for",
    "history of",
    "know about",
];

/// Returns true if `text` contains `word` as a whole word (word-boundary semantics).
///
/// A "word boundary" here means the character before and after `word` (if present)
/// is not an ASCII alphanumeric character or underscore.
fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    let wbytes = word.as_bytes();
    let wlen = wbytes.len();
    if wlen > bytes.len() {
        return false;
    }
    for start in 0..=(bytes.len() - wlen) {
        if bytes[start..start + wlen].eq_ignore_ascii_case(wbytes) {
            let before_ok =
                start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
            let after_ok = start + wlen == bytes.len()
                || !bytes[start + wlen].is_ascii_alphanumeric() && bytes[start + wlen] != b'_';
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// Returns true if the lowercased query contains any temporal cue that indicates
/// an episodic / time-scoped recall request.
fn has_temporal_cue(lower: &str) -> bool {
    if TEMPORAL_PATTERNS.iter().any(|p| lower.contains(p)) {
        return true;
    }
    WORD_BOUNDARY_TEMPORAL
        .iter()
        .any(|w| contains_word(lower, w))
}

/// Temporal patterns sorted longest-first for stripping. Initialized once via `LazyLock`
/// to avoid allocating and sorting on every call to `strip_temporal_keywords`.
static SORTED_TEMPORAL_PATTERNS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| {
        let mut v: Vec<&str> = TEMPORAL_PATTERNS.to_vec();
        v.sort_by_key(|p| std::cmp::Reverse(p.len()));
        v
    });

/// Strip matched temporal keywords from a query string before passing to FTS5.
///
/// Temporal keywords are routing metadata, not search terms. Passing them to FTS5
/// causes BM25 score distortion — messages that literally mention "yesterday" get
/// boosted regardless of actual content relevance.
///
/// All occurrences of each pattern are removed (not just the first), preventing
/// score distortion from repeated temporal tokens in edge cases like
/// "yesterday I mentioned yesterday's bug".
///
/// # Example
/// ```
/// # use zeph_memory::router::strip_temporal_keywords;
/// let cleaned = strip_temporal_keywords("what did we discuss yesterday about Rust");
/// assert_eq!(cleaned, "what did we discuss about Rust");
/// ```
#[must_use]
pub fn strip_temporal_keywords(query: &str) -> String {
    // Lowercase once for pattern matching; track removal positions in the original string.
    // We operate on the lowercased copy for matching, then remove spans from `result`
    // by rebuilding via byte indices (both strings have identical byte lengths because
    // to_ascii_lowercase is a 1:1 byte mapping for ASCII).
    let lower = query.to_ascii_lowercase();
    // Collect all (start, end) spans to remove, then rebuild the string in one pass.
    let mut remove: Vec<(usize, usize)> = Vec::new();

    for pattern in SORTED_TEMPORAL_PATTERNS.iter() {
        let plen = pattern.len();
        let mut search_from = 0;
        while let Some(pos) = lower[search_from..].find(pattern) {
            let abs = search_from + pos;
            remove.push((abs, abs + plen));
            search_from = abs + plen;
        }
    }

    // Strip word-boundary tokens (single-word, e.g. "ago") — all occurrences.
    for word in WORD_BOUNDARY_TEMPORAL {
        let wlen = word.len();
        let lbytes = lower.as_bytes();
        let mut i = 0;
        while i + wlen <= lower.len() {
            if lower[i..].starts_with(*word) {
                let before_ok =
                    i == 0 || !lbytes[i - 1].is_ascii_alphanumeric() && lbytes[i - 1] != b'_';
                let after_ok = i + wlen == lower.len()
                    || !lbytes[i + wlen].is_ascii_alphanumeric() && lbytes[i + wlen] != b'_';
                if before_ok && after_ok {
                    remove.push((i, i + wlen));
                    i += wlen;
                    continue;
                }
            }
            i += 1;
        }
    }

    if remove.is_empty() {
        // Fast path: no patterns found — return the original string.
        return query.split_whitespace().collect::<Vec<_>>().join(" ");
    }

    // Merge overlapping/adjacent spans and remove them from the original string.
    remove.sort_unstable_by_key(|r| r.0);
    let bytes = query.as_bytes();
    let mut result = Vec::with_capacity(query.len());
    let mut cursor = 0;
    for (start, end) in remove {
        if start > cursor {
            result.extend_from_slice(&bytes[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    if cursor < bytes.len() {
        result.extend_from_slice(&bytes[cursor..]);
    }

    // Collapse multiple spaces and trim.
    // SAFETY: We only removed ASCII byte spans; remaining bytes are still valid UTF-8.
    let s = String::from_utf8(result).unwrap_or_default();
    s.split_whitespace()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve temporal keywords in `query` to a `(after, before)` datetime boundary pair.
///
/// Returns `None` when no specific range can be computed (the episodic path then falls
/// back to FTS5 without a time filter, relying on temporal decay for recency boosting).
///
/// The `now` parameter is injectable for deterministic unit testing. Production callers
/// should pass `chrono::Utc::now()`.
///
/// All datetime strings are in `SQLite` format: `YYYY-MM-DD HH:MM:SS` (UTC).
#[must_use]
pub fn resolve_temporal_range(query: &str, now: DateTime<Utc>) -> Option<TemporalRange> {
    let lower = query.to_ascii_lowercase();

    // yesterday: the full calendar day before today (UTC)
    if lower.contains("yesterday") {
        let yesterday = now.date_naive() - Duration::days(1);
        return Some(TemporalRange {
            after: Some(format!("{yesterday} 00:00:00")),
            before: Some(format!("{yesterday} 23:59:59")),
        });
    }

    // last night: 18:00 yesterday to 06:00 today (UTC approximation)
    if lower.contains("last night") {
        let yesterday = now.date_naive() - Duration::days(1);
        let today = now.date_naive();
        return Some(TemporalRange {
            after: Some(format!("{yesterday} 18:00:00")),
            before: Some(format!("{today} 06:00:00")),
        });
    }

    // tonight: 18:00 today onwards
    if lower.contains("tonight") {
        let today = now.date_naive();
        return Some(TemporalRange {
            after: Some(format!("{today} 18:00:00")),
            before: None,
        });
    }

    // this morning: midnight to noon today
    if lower.contains("this morning") {
        let today = now.date_naive();
        return Some(TemporalRange {
            after: Some(format!("{today} 00:00:00")),
            before: Some(format!("{today} 12:00:00")),
        });
    }

    // today / earlier today: midnight to now.
    // Note: "earlier today" always contains "today", so a separate branch would be
    // dead code — the "today" check subsumes it.
    if lower.contains("today") {
        let today = now.date_naive();
        return Some(TemporalRange {
            after: Some(format!("{today} 00:00:00")),
            before: None,
        });
    }

    // last week / past week / this week: 7-day lookback
    if lower.contains("last week") || lower.contains("past week") || lower.contains("this week") {
        let start = now - Duration::days(7);
        return Some(TemporalRange {
            after: Some(start.format("%Y-%m-%d %H:%M:%S").to_string()),
            before: None,
        });
    }

    // last month / past month / this month: 30-day lookback (approximate)
    if lower.contains("last month") || lower.contains("past month") || lower.contains("this month")
    {
        let start = now - Duration::days(30);
        return Some(TemporalRange {
            after: Some(start.format("%Y-%m-%d %H:%M:%S").to_string()),
            before: None,
        });
    }

    // "few days ago" / "few hours ago": 3-day lookback
    if lower.contains("few days ago") {
        let start = now - Duration::days(3);
        return Some(TemporalRange {
            after: Some(start.format("%Y-%m-%d %H:%M:%S").to_string()),
            before: None,
        });
    }
    if lower.contains("few hours ago") {
        let start = now - Duration::hours(6);
        return Some(TemporalRange {
            after: Some(start.format("%Y-%m-%d %H:%M:%S").to_string()),
            before: None,
        });
    }

    // "ago" (word-boundary): generic recent lookback (24h)
    if contains_word(&lower, "ago") {
        let start = now - Duration::hours(24);
        return Some(TemporalRange {
            after: Some(start.format("%Y-%m-%d %H:%M:%S").to_string()),
            before: None,
        });
    }

    // Generic temporal cues without a specific range ("when did", "remember when",
    // "last time", "how long ago") — fall back to FTS5-only with temporal decay.
    None
}

fn starts_with_question(words: &[&str]) -> bool {
    words
        .first()
        .is_some_and(|w| QUESTION_WORDS.iter().any(|qw| w.eq_ignore_ascii_case(qw)))
}

/// Returns true if `word` is a pure `snake_case` identifier (all ASCII, lowercase letters,
/// digits and underscores, contains at least one underscore, not purely numeric).
fn is_pure_snake_case(word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let has_underscore = word.contains('_');
    if !has_underscore {
        return false;
    }
    word.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !word.chars().all(|c| c.is_ascii_digit() || c == '_')
}

impl MemoryRouter for HeuristicRouter {
    /// Returns a confidence signal based on pattern match count (W2.1 fix: gradual scale).
    ///
    /// - Exactly one route pattern matches → confidence `1.0` (clear signal)
    /// - Zero patterns match → confidence `0.0` (pure default fallback)
    /// - More than one pattern matches → confidence `1.0 / matched_count` (ambiguous, decreasing)
    fn route_with_confidence(&self, query: &str) -> RoutingDecision {
        let lower = query.to_ascii_lowercase();
        let mut matched: u32 = 0;
        if has_temporal_cue(&lower) {
            matched += 1;
        }
        if RELATIONSHIP_PATTERNS.iter().any(|p| lower.contains(p)) {
            matched += 1;
        }
        let words: Vec<&str> = query.split_whitespace().collect();
        let word_count = words.len();
        let has_structural = query.contains('/') || query.contains("::");
        let question = starts_with_question(&words);
        let has_snake = words.iter().any(|w| is_pure_snake_case(w));
        if has_structural && !question {
            matched += 1;
        }
        if question || word_count >= 6 {
            matched += 1;
        }
        if word_count <= 3 && !question {
            matched += 1;
        }
        if has_snake {
            matched += 1;
        }

        #[allow(clippy::cast_precision_loss)]
        let confidence = match matched {
            0 => 0.0,
            1 => 1.0,
            n => 1.0 / n as f32,
        };

        RoutingDecision {
            route: self.route(query),
            confidence,
            reasoning: None,
        }
    }

    fn route(&self, query: &str) -> MemoryRoute {
        let lower = query.to_ascii_lowercase();

        // 1. Temporal queries take highest priority — must run before relationship check
        //    to prevent "history of changes last week" from routing to Graph instead of Episodic.
        if has_temporal_cue(&lower) {
            return MemoryRoute::Episodic;
        }

        // 2. Relationship queries go to graph retrieval (feature-gated at call site)
        let has_relationship = RELATIONSHIP_PATTERNS.iter().any(|p| lower.contains(p));
        if has_relationship {
            return MemoryRoute::Graph;
        }

        let words: Vec<&str> = query.split_whitespace().collect();
        let word_count = words.len();

        // Code-like patterns that unambiguously indicate keyword search:
        // file paths (contain '/'), Rust paths (contain '::')
        let has_structural_code_pattern = query.contains('/') || query.contains("::");

        // Pure snake_case identifiers (e.g. "memory_limit", "error_handling")
        // but only if the query does NOT start with a question word
        let has_snake_case = words.iter().any(|w| is_pure_snake_case(w));
        let question = starts_with_question(&words);

        if has_structural_code_pattern && !question {
            return MemoryRoute::Keyword;
        }

        // Long NL queries → semantic, regardless of snake_case tokens
        if question || word_count >= 6 {
            return MemoryRoute::Semantic;
        }

        // Short queries without question words → keyword
        if word_count <= 3 && !question {
            return MemoryRoute::Keyword;
        }

        // Short code-like patterns → keyword
        if has_snake_case {
            return MemoryRoute::Keyword;
        }

        // Default
        MemoryRoute::Hybrid
    }
}

/// LLM-based memory router.
///
/// Sends the query to the configured provider and parses a JSON response:
/// `{"route": "keyword|semantic|hybrid|graph|episodic", "confidence": 0.0-1.0}`.
///
/// On LLM failure, falls back to `HeuristicRouter`.
pub struct LlmRouter {
    provider: std::sync::Arc<zeph_llm::any::AnyProvider>,
    fallback_route: MemoryRoute,
}

impl LlmRouter {
    #[must_use]
    pub fn new(
        provider: std::sync::Arc<zeph_llm::any::AnyProvider>,
        fallback_route: MemoryRoute,
    ) -> Self {
        Self {
            provider,
            fallback_route,
        }
    }

    async fn classify_async(&self, query: &str) -> RoutingDecision {
        use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

        let system = "You are a memory store routing classifier. \
            Given a user query, decide which memory backend is most appropriate. \
            Respond with ONLY a JSON object: \
            {\"route\": \"<route>\", \"confidence\": <0.0-1.0>, \"reasoning\": \"<brief>\"} \
            where <route> is one of: keyword, semantic, hybrid, graph, episodic. \
            Use 'keyword' for exact/code lookups, 'semantic' for conceptual questions, \
            'hybrid' for mixed, 'graph' for relationship queries, 'episodic' for time-scoped queries.";

        // Wrap query in delimiters to prevent injection (W2.2 fix).
        let user = format!(
            "<query>{}</query>",
            query.chars().take(500).collect::<String>()
        );

        let messages = vec![
            Message {
                role: Role::System,
                content: system.to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: user,
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.provider.chat(&messages),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "LlmRouter: LLM call failed, falling back to heuristic");
                return Self::heuristic_fallback(query);
            }
            Err(_) => {
                tracing::debug!("LlmRouter: LLM timed out, falling back to heuristic");
                return Self::heuristic_fallback(query);
            }
        };

        self.parse_llm_response(&result, query)
    }

    fn parse_llm_response(&self, raw: &str, query: &str) -> RoutingDecision {
        // Extract JSON object from the response (may have surrounding text).
        let json_str = raw
            .find('{')
            .and_then(|start| raw[start..].rfind('}').map(|end| &raw[start..=start + end]))
            .unwrap_or("");

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
            let route_str = v.get("route").and_then(|r| r.as_str()).unwrap_or("hybrid");
            #[allow(clippy::cast_possible_truncation)]
            let confidence = v
                .get("confidence")
                .and_then(serde_json::Value::as_f64)
                .map_or(0.5, |c| c.clamp(0.0, 1.0) as f32);
            let reasoning = v
                .get("reasoning")
                .and_then(|r| r.as_str())
                .map(str::to_owned);

            let route = parse_route_str(route_str, self.fallback_route);

            tracing::debug!(
                query = &query[..query.len().min(60)],
                ?route,
                confidence,
                "LlmRouter: classified"
            );

            return RoutingDecision {
                route,
                confidence,
                reasoning,
            };
        }

        tracing::debug!("LlmRouter: failed to parse JSON response, falling back to heuristic");
        Self::heuristic_fallback(query)
    }

    fn heuristic_fallback(query: &str) -> RoutingDecision {
        HeuristicRouter.route_with_confidence(query)
    }
}

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

impl MemoryRouter for LlmRouter {
    fn route(&self, query: &str) -> MemoryRoute {
        // Sync path: LLM is not available without an async executor.
        // Falls back to heuristic — use route_async() for LLM-based classification.
        HeuristicRouter.route(query)
    }

    fn route_with_confidence(&self, query: &str) -> RoutingDecision {
        // LlmRouter is designed for use in async contexts via classify_async.
        // When called synchronously (e.g. in tests), fall back to heuristic.
        HeuristicRouter.route_with_confidence(query)
    }
}

/// Async extension for LLM-capable routers.
pub trait AsyncMemoryRouter: MemoryRouter {
    fn route_async<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RoutingDecision> + Send + 'a>>;
}

impl AsyncMemoryRouter for LlmRouter {
    fn route_async<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RoutingDecision> + Send + 'a>> {
        Box::pin(self.classify_async(query))
    }
}

/// Hybrid router: heuristic-first, escalates to LLM when confidence is low.
///
/// The `HybridRouter` runs `HeuristicRouter` first. If the heuristic confidence
/// is below `confidence_threshold`, it escalates to the LLM router.
/// LLM failures always fall back to the heuristic result.
pub struct HybridRouter {
    llm: LlmRouter,
    confidence_threshold: f32,
}

impl HybridRouter {
    #[must_use]
    pub fn new(
        provider: std::sync::Arc<zeph_llm::any::AnyProvider>,
        fallback_route: MemoryRoute,
        confidence_threshold: f32,
    ) -> Self {
        Self {
            llm: LlmRouter::new(provider, fallback_route),
            confidence_threshold,
        }
    }

    pub async fn classify_async(&self, query: &str) -> RoutingDecision {
        let heuristic = HeuristicRouter.route_with_confidence(query);
        if heuristic.confidence >= self.confidence_threshold {
            tracing::debug!(
                query = &query[..query.len().min(60)],
                confidence = heuristic.confidence,
                route = ?heuristic.route,
                "HybridRouter: heuristic sufficient, skipping LLM"
            );
            return heuristic;
        }

        tracing::debug!(
            query = &query[..query.len().min(60)],
            confidence = heuristic.confidence,
            threshold = self.confidence_threshold,
            "HybridRouter: low confidence, escalating to LLM"
        );

        let llm_result = self.llm.classify_async(query).await;

        // LLM failure path: classify_async returns a heuristic fallback on error.
        // Always log the final decision.
        tracing::debug!(
            route = ?llm_result.route,
            confidence = llm_result.confidence,
            "HybridRouter: final route after LLM escalation"
        );
        llm_result
    }
}

impl MemoryRouter for HybridRouter {
    fn route(&self, query: &str) -> MemoryRoute {
        HeuristicRouter.route(query)
    }

    fn route_with_confidence(&self, query: &str) -> RoutingDecision {
        // Synchronous path: can't call async LLM, use heuristic only.
        HeuristicRouter.route_with_confidence(query)
    }
}

impl AsyncMemoryRouter for HeuristicRouter {
    fn route_async<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RoutingDecision> + Send + 'a>> {
        Box::pin(std::future::ready(self.route_with_confidence(query)))
    }
}

impl AsyncMemoryRouter for HybridRouter {
    fn route_async<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RoutingDecision> + Send + 'a>> {
        Box::pin(self.classify_async(query))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn route(q: &str) -> MemoryRoute {
        HeuristicRouter.route(q)
    }

    fn fixed_now() -> DateTime<Utc> {
        // 2026-03-14 12:00:00 UTC — fixed reference point for all temporal tests
        Utc.with_ymd_and_hms(2026, 3, 14, 12, 0, 0).unwrap()
    }

    #[test]
    fn rust_path_routes_keyword() {
        assert_eq!(route("zeph_memory::recall"), MemoryRoute::Keyword);
    }

    #[test]
    fn file_path_routes_keyword() {
        assert_eq!(
            route("crates/zeph-core/src/agent/mod.rs"),
            MemoryRoute::Keyword
        );
    }

    #[test]
    fn pure_snake_case_routes_keyword() {
        assert_eq!(route("memory_limit"), MemoryRoute::Keyword);
        assert_eq!(route("error_handling"), MemoryRoute::Keyword);
    }

    #[test]
    fn question_with_snake_case_routes_semantic() {
        // "what is the memory_limit setting" — question word overrides snake_case heuristic
        assert_eq!(
            route("what is the memory_limit setting"),
            MemoryRoute::Semantic
        );
        assert_eq!(route("how does error_handling work"), MemoryRoute::Semantic);
    }

    #[test]
    fn short_query_routes_keyword() {
        assert_eq!(route("context compaction"), MemoryRoute::Keyword);
        assert_eq!(route("qdrant"), MemoryRoute::Keyword);
    }

    #[test]
    fn question_routes_semantic() {
        assert_eq!(
            route("what is the purpose of semantic memory"),
            MemoryRoute::Semantic
        );
        assert_eq!(route("how does the agent loop work"), MemoryRoute::Semantic);
        assert_eq!(route("why does compaction fail"), MemoryRoute::Semantic);
        assert_eq!(route("explain context compression"), MemoryRoute::Semantic);
    }

    #[test]
    fn long_natural_query_routes_semantic() {
        assert_eq!(
            route("the agent keeps running out of context during long conversations"),
            MemoryRoute::Semantic
        );
    }

    #[test]
    fn medium_non_question_routes_hybrid() {
        // 4-5 words, no question word, no code pattern
        assert_eq!(route("context window token budget"), MemoryRoute::Hybrid);
    }

    #[test]
    fn empty_query_routes_keyword() {
        // 0 words, no question → keyword (short path)
        assert_eq!(route(""), MemoryRoute::Keyword);
    }

    #[test]
    fn question_word_only_routes_semantic() {
        // single question word → word_count = 1, but starts_with_question = true
        // short query with question: the question check happens first in semantic branch
        // Actually with word_count=1 and question=true: short path `<= 3 && !question` is false,
        // then `question || word_count >= 6` is true → Semantic
        assert_eq!(route("what"), MemoryRoute::Semantic);
    }

    #[test]
    fn camel_case_does_not_route_keyword_without_pattern() {
        // CamelCase words without :: or / — 4-word query without question word → Hybrid
        // (4 words: no question, no snake_case, no structural code pattern → Hybrid)
        assert_eq!(
            route("SemanticMemory configuration and options"),
            MemoryRoute::Hybrid
        );
    }

    #[test]
    fn relationship_query_routes_graph() {
        assert_eq!(
            route("what is user's opinion on neovim"),
            MemoryRoute::Graph
        );
        assert_eq!(
            route("show the relationship between Alice and Bob"),
            MemoryRoute::Graph
        );
    }

    #[test]
    fn relationship_query_related_to_routes_graph() {
        assert_eq!(
            route("how is Rust related to this project"),
            MemoryRoute::Graph
        );
        assert_eq!(
            route("how does this relates to the config"),
            MemoryRoute::Graph
        );
    }

    #[test]
    fn relationship_know_about_routes_graph() {
        assert_eq!(route("what do I know about neovim"), MemoryRoute::Graph);
    }

    #[test]
    fn translate_does_not_route_graph() {
        // "translate" contains "relate" substring but is not in RELATIONSHIP_PATTERNS
        // (we removed bare "relate", keeping only "related to" and "relates to")
        assert_ne!(route("translate this code to Python"), MemoryRoute::Graph);
    }

    #[test]
    fn non_relationship_stays_semantic() {
        assert_eq!(
            route("find similar code patterns in the codebase"),
            MemoryRoute::Semantic
        );
    }

    #[test]
    fn short_keyword_unchanged() {
        assert_eq!(route("qdrant"), MemoryRoute::Keyword);
    }

    // Regression tests for #1661: long NL queries with snake_case must go to Semantic
    #[test]
    fn long_nl_with_snake_case_routes_semantic() {
        assert_eq!(
            route("Use memory_search to find information about Rust ownership"),
            MemoryRoute::Semantic
        );
    }

    #[test]
    fn short_snake_case_only_routes_keyword() {
        assert_eq!(route("memory_search"), MemoryRoute::Keyword);
    }

    #[test]
    fn question_with_snake_case_short_routes_semantic() {
        assert_eq!(
            route("What does memory_search return?"),
            MemoryRoute::Semantic
        );
    }

    // ── Temporal routing tests ────────────────────────────────────────────────

    #[test]
    fn temporal_yesterday_routes_episodic() {
        assert_eq!(
            route("what did we discuss yesterday"),
            MemoryRoute::Episodic
        );
    }

    #[test]
    fn temporal_last_week_routes_episodic() {
        assert_eq!(
            route("remember what happened last week"),
            MemoryRoute::Episodic
        );
    }

    #[test]
    fn temporal_when_did_routes_episodic() {
        assert_eq!(
            route("when did we last talk about Qdrant"),
            MemoryRoute::Episodic
        );
    }

    #[test]
    fn temporal_last_time_routes_episodic() {
        assert_eq!(
            route("last time we discussed the scheduler"),
            MemoryRoute::Episodic
        );
    }

    #[test]
    fn temporal_today_routes_episodic() {
        assert_eq!(
            route("what did I mention today about testing"),
            MemoryRoute::Episodic
        );
    }

    #[test]
    fn temporal_this_morning_routes_episodic() {
        assert_eq!(route("what did we say this morning"), MemoryRoute::Episodic);
    }

    #[test]
    fn temporal_last_month_routes_episodic() {
        assert_eq!(
            route("find the config change from last month"),
            MemoryRoute::Episodic
        );
    }

    #[test]
    fn temporal_history_collision_routes_episodic() {
        // CRIT-01: "history of" is a relationship pattern, but temporal wins when both match.
        // Temporal check is first — "last week" causes Episodic, not Graph.
        assert_eq!(route("history of changes last week"), MemoryRoute::Episodic);
    }

    #[test]
    fn temporal_ago_word_boundary_routes_episodic() {
        assert_eq!(route("we fixed this a day ago"), MemoryRoute::Episodic);
    }

    #[test]
    fn ago_in_chicago_no_false_positive() {
        // MED-01: "Chicago" contains "ago" but must NOT route to Episodic.
        // word-boundary check prevents this false positive.
        assert_ne!(
            route("meeting in Chicago about the project"),
            MemoryRoute::Episodic
        );
    }

    #[test]
    fn non_temporal_unchanged() {
        assert_eq!(route("how does the agent loop work"), MemoryRoute::Semantic);
    }

    #[test]
    fn code_query_unchanged() {
        assert_eq!(route("zeph_memory::recall"), MemoryRoute::Keyword);
    }

    // ── resolve_temporal_range tests ─────────────────────────────────────────

    #[test]
    fn resolve_yesterday_range() {
        let now = fixed_now(); // 2026-03-14 12:00:00 UTC
        let range = resolve_temporal_range("what did we discuss yesterday", now).unwrap();
        assert_eq!(range.after.as_deref(), Some("2026-03-13 00:00:00"));
        assert_eq!(range.before.as_deref(), Some("2026-03-13 23:59:59"));
    }

    #[test]
    fn resolve_last_week_range() {
        let now = fixed_now(); // 2026-03-14 12:00:00 UTC
        let range = resolve_temporal_range("remember last week's discussion", now).unwrap();
        // 7 days before 2026-03-14 = 2026-03-07
        assert!(range.after.as_deref().unwrap().starts_with("2026-03-07"));
        assert!(range.before.is_none());
    }

    #[test]
    fn resolve_last_month_range() {
        let now = fixed_now();
        let range = resolve_temporal_range("find the bug from last month", now).unwrap();
        // 30 days before 2026-03-14 = 2026-02-12
        assert!(range.after.as_deref().unwrap().starts_with("2026-02-12"));
        assert!(range.before.is_none());
    }

    #[test]
    fn resolve_today_range() {
        let now = fixed_now();
        let range = resolve_temporal_range("what did we do today", now).unwrap();
        assert_eq!(range.after.as_deref(), Some("2026-03-14 00:00:00"));
        assert!(range.before.is_none());
    }

    #[test]
    fn resolve_this_morning_range() {
        let now = fixed_now();
        let range = resolve_temporal_range("what did we say this morning", now).unwrap();
        assert_eq!(range.after.as_deref(), Some("2026-03-14 00:00:00"));
        assert_eq!(range.before.as_deref(), Some("2026-03-14 12:00:00"));
    }

    #[test]
    fn resolve_last_night_range() {
        let now = fixed_now();
        let range = resolve_temporal_range("last night's conversation", now).unwrap();
        assert_eq!(range.after.as_deref(), Some("2026-03-13 18:00:00"));
        assert_eq!(range.before.as_deref(), Some("2026-03-14 06:00:00"));
    }

    #[test]
    fn resolve_tonight_range() {
        let now = fixed_now();
        let range = resolve_temporal_range("remind me tonight what we agreed on", now).unwrap();
        assert_eq!(range.after.as_deref(), Some("2026-03-14 18:00:00"));
        assert!(range.before.is_none());
    }

    #[test]
    fn resolve_no_temporal_returns_none() {
        let now = fixed_now();
        assert!(resolve_temporal_range("what is the purpose of semantic memory", now).is_none());
    }

    #[test]
    fn resolve_generic_temporal_returns_none() {
        // "when did", "remember when", "last time", "how long ago" — no specific range
        let now = fixed_now();
        assert!(resolve_temporal_range("when did we discuss this feature", now).is_none());
        assert!(resolve_temporal_range("remember when we fixed that bug", now).is_none());
    }

    // ── strip_temporal_keywords tests ────────────────────────────────────────

    #[test]
    fn strip_yesterday_from_query() {
        let cleaned = strip_temporal_keywords("what did we discuss yesterday about Rust");
        assert_eq!(cleaned, "what did we discuss about Rust");
    }

    #[test]
    fn strip_last_week_from_query() {
        let cleaned = strip_temporal_keywords("find the config change from last week");
        assert_eq!(cleaned, "find the config change from");
    }

    #[test]
    fn strip_does_not_alter_non_temporal() {
        let q = "what is the purpose of semantic memory";
        assert_eq!(strip_temporal_keywords(q), q);
    }

    #[test]
    fn strip_ago_word_boundary() {
        let cleaned = strip_temporal_keywords("we fixed this a day ago in the scheduler");
        // "ago" removed, rest preserved
        assert!(!cleaned.contains("ago"));
        assert!(cleaned.contains("scheduler"));
    }

    #[test]
    fn strip_does_not_touch_chicago() {
        let q = "meeting in Chicago about the project";
        assert_eq!(strip_temporal_keywords(q), q);
    }

    #[test]
    fn strip_empty_string_returns_empty() {
        assert_eq!(strip_temporal_keywords(""), "");
    }

    #[test]
    fn strip_only_temporal_keyword_returns_empty() {
        // When the entire query is a temporal keyword, stripping leaves an empty string.
        // recall_routed falls back to the original query in this case.
        assert_eq!(strip_temporal_keywords("yesterday"), "");
    }

    #[test]
    fn strip_repeated_temporal_keyword_removes_all_occurrences() {
        // IMPL-02: all occurrences must be removed, not just the first.
        let cleaned = strip_temporal_keywords("yesterday I mentioned yesterday's bug");
        assert!(
            !cleaned.contains("yesterday"),
            "both occurrences must be removed: got '{cleaned}'"
        );
        assert!(cleaned.contains("mentioned"));
    }

    // ── route_with_confidence tests ───────────────────────────────────────────

    #[test]
    fn confidence_multiple_matches_is_less_than_one() {
        // Structural code pattern + snake_case + short query fire 3 signals →
        // confidence = 1.0 / 3 < 1.0
        let d = HeuristicRouter.route_with_confidence("zeph_memory::recall");
        assert!(
            d.confidence < 1.0,
            "ambiguous query should have confidence < 1.0, got {}",
            d.confidence
        );
        assert_eq!(d.route, MemoryRoute::Keyword);
    }

    #[test]
    fn confidence_long_question_with_snake_fires_multiple_signals() {
        // Long question with snake_case fires multiple signals → confidence < 1.0
        let d = HeuristicRouter
            .route_with_confidence("what is the purpose of memory_limit in the config system");
        assert!(
            d.confidence < 1.0,
            "ambiguous query must have confidence < 1.0, got {}",
            d.confidence
        );
    }

    #[test]
    fn confidence_empty_query_is_nonzero() {
        // Empty string: word_count=0 → short path fires (<=3 && !question) → matched=1 → confidence=1.0
        let d = HeuristicRouter.route_with_confidence("");
        assert!(
            d.confidence > 0.0,
            "empty query must match short-path signal"
        );
    }

    #[test]
    fn routing_decision_route_matches_route_fn() {
        // route_with_confidence().route must agree with route()
        let queries = [
            "qdrant",
            "what is the agent loop",
            "context window token budget",
            "what did we discuss yesterday",
        ];
        for q in queries {
            let decision = HeuristicRouter.route_with_confidence(q);
            assert_eq!(
                decision.route,
                HeuristicRouter.route(q),
                "mismatch for query: {q}"
            );
        }
    }
}
