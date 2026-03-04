// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Classification of which memory backend(s) to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRoute {
    /// Full-text search only (`SQLite` FTS5). Fast, good for keyword/exact queries.
    Keyword,
    /// Vector search only (Qdrant). Good for semantic/conceptual queries.
    Semantic,
    /// Both backends, results merged by reciprocal rank fusion.
    Hybrid,
}

/// Decides which memory backend(s) to query for a given input.
pub trait MemoryRouter: Send + Sync {
    /// Route a query to the appropriate backend(s).
    fn route(&self, query: &str) -> MemoryRoute;
}

/// Heuristic-based memory router.
///
/// Decision logic:
/// - If query contains code-like patterns (paths, `::`, pure `snake_case` identifiers)
///   AND does NOT start with a question word → Keyword
/// - If query is a natural language question or long → Semantic
/// - Default → Hybrid
pub struct HeuristicRouter;

const QUESTION_WORDS: &[&str] = &[
    "what", "how", "why", "when", "where", "who", "which", "explain", "describe",
];

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
    fn route(&self, query: &str) -> MemoryRoute {
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

        if has_snake_case && !question {
            return MemoryRoute::Keyword;
        }

        // Short queries without question words → keyword
        if word_count <= 3 && !question {
            return MemoryRoute::Keyword;
        }

        // Natural language questions or long queries → semantic
        if question || word_count >= 6 {
            return MemoryRoute::Semantic;
        }

        // Default
        MemoryRoute::Hybrid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(q: &str) -> MemoryRoute {
        HeuristicRouter.route(q)
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
}
