// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-memory BM25 inverted index over skill descriptions with Reciprocal Rank Fusion.
//!
//! BM25 (Okapi BM25) is a bag-of-words ranking function that outperforms TF-IDF for
//! short descriptions by normalizing term frequency against document length.
//!
//! This module provides two components:
//!
//! 1. **[`Bm25Index`]** — an inverted index built from skill descriptions that can be
//!    queried for lexical matches.
//! 2. **[`rrf_fuse`]** — Reciprocal Rank Fusion to combine BM25 and embedding results
//!    into a single ranked list.
//!
//! # Tokenization
//!
//! Text is lower-cased and split on all non-alphanumeric characters. Tokens shorter than
//! 3 characters are discarded to reduce noise from articles, prepositions, and abbreviations.
//!
//! # Examples
//!
//! ```rust
//! use zeph_skills::bm25::Bm25Index;
//!
//! let index = Bm25Index::build(&[
//!     "run git commands and manage repositories",
//!     "manage docker containers and compose files",
//! ]);
//!
//! let results = index.search("git commit", 5);
//! assert!(!results.is_empty());
//! assert_eq!(results[0].0, 0); // git doc ranks first
//! ```

use std::collections::{HashMap, HashSet};

use crate::matcher::ScoredMatch;

/// In-memory BM25 index over skill descriptions.
///
/// Built once at skill-load time with `k1 = 1.2` and `b = 0.75` (standard BM25 defaults).
/// The index is read-only after construction; rebuild via [`Bm25Index::build`] after a reload.
#[derive(Debug)]
pub struct Bm25Index {
    inverted: HashMap<String, Vec<(usize, f32)>>,
    doc_lengths: Vec<f32>,
    avg_doc_length: f32,
    doc_count: usize,
    k1: f32,
    b: f32,
}

impl Bm25Index {
    /// Build a BM25 index from a slice of document strings.
    ///
    /// Documents are indexed positionally — `descriptions[i]` corresponds to skill index `i`
    /// in the caller's skill slice, which must remain stable across the index lifetime.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_skills::bm25::Bm25Index;
    ///
    /// let index = Bm25Index::build(&["fetch weather data", "run docker containers"]);
    /// assert!(!index.search("weather", 3).is_empty());
    /// ```
    #[must_use]
    pub fn build(descriptions: &[&str]) -> Self {
        let k1 = 1.2_f32;
        let b = 0.75_f32;
        let doc_count = descriptions.len();
        let mut inverted: HashMap<String, Vec<(usize, f32)>> = HashMap::new();
        let mut doc_lengths = Vec::with_capacity(doc_count);

        for (i, desc) in descriptions.iter().enumerate() {
            let tokens = tokenize(desc);
            #[allow(clippy::cast_precision_loss)]
            let len = tokens.len() as f32;
            doc_lengths.push(len);

            let mut term_counts: HashMap<&str, u32> = HashMap::new();
            for token in &tokens {
                *term_counts.entry(token.as_str()).or_default() += 1;
            }
            for (term, count) in term_counts {
                #[allow(clippy::cast_precision_loss)]
                inverted
                    .entry(term.to_owned())
                    .or_default()
                    .push((i, count as f32));
            }
        }

        #[allow(clippy::cast_precision_loss)]
        let avg_doc_length = if doc_count == 0 {
            0.0
        } else {
            doc_lengths.iter().sum::<f32>() / doc_count as f32
        };

        Self {
            inverted,
            doc_lengths,
            avg_doc_length,
            doc_count,
            k1,
            b,
        }
    }

    /// Score all documents against the query, returning up to `limit` `(index, score)` pairs.
    ///
    /// Documents with a score of zero (no query terms match) are excluded. Results are
    /// sorted by score descending.
    ///
    /// Returns an empty `Vec` when the index is empty or the query contains no known terms.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_skills::bm25::Bm25Index;
    ///
    /// let index = Bm25Index::build(&["git version control", "docker container orchestration"]);
    /// let results = index.search("git", 5);
    /// assert_eq!(results.len(), 1);
    /// assert_eq!(results[0].0, 0);
    /// ```
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<(usize, f32)> {
        if self.doc_count == 0 || self.avg_doc_length == 0.0 {
            return vec![];
        }
        let query_tokens = tokenize(query);
        let unique_terms: HashSet<&str> = query_tokens.iter().map(String::as_str).collect();
        let mut scores = vec![0.0_f32; self.doc_count];

        #[allow(clippy::cast_precision_loss)]
        for term in unique_terms {
            let Some(postings) = self.inverted.get(term) else {
                continue;
            };
            let df = postings.len() as f32;
            let idf = ((self.doc_count as f32 - df + 0.5) / (df + 0.5) + 1.0).ln();

            for &(doc_idx, tf) in postings {
                let dl = self.doc_lengths[doc_idx];
                let norm_tf = (tf * (self.k1 + 1.0))
                    / (tf + self.k1 * (1.0 - self.b + self.b * dl / self.avg_doc_length));
                scores[doc_idx] += idf * norm_tf;
            }
        }

        let mut results: Vec<(usize, f32)> = scores
            .into_iter()
            .enumerate()
            .filter(|(_, s)| *s > 0.0)
            .collect();
        results.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        results
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 3)
        .map(String::from)
        .collect()
}

/// Reciprocal Rank Fusion: combine two ranked result lists into one.
///
/// Uses constant `k=60` from the original RRF paper (Cormack et al., 2009).
/// Results are sorted by fused score, highest first, and truncated to `limit`.
#[must_use]
pub fn rrf_fuse(
    embedding_results: &[ScoredMatch],
    bm25_results: &[(usize, f32)],
    limit: usize,
) -> Vec<ScoredMatch> {
    const K: f32 = 60.0;
    let mut scores: HashMap<usize, f32> = HashMap::new();

    #[allow(clippy::cast_precision_loss)]
    for (rank, m) in embedding_results.iter().enumerate() {
        *scores.entry(m.index).or_default() += 1.0 / (K + rank as f32 + 1.0);
    }
    #[allow(clippy::cast_precision_loss)]
    for (rank, (idx, _)) in bm25_results.iter().enumerate() {
        *scores.entry(*idx).or_default() += 1.0 / (K + rank as f32 + 1.0);
    }

    let mut fused: Vec<ScoredMatch> = scores
        .into_iter()
        .map(|(index, score)| ScoredMatch { index, score })
        .collect();
    fused.sort_unstable_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    fused.truncate(limit);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_empty() {
        let idx = Bm25Index::build(&[]);
        assert!(idx.search("anything", 5).is_empty());
    }

    #[test]
    fn search_no_match() {
        let idx = Bm25Index::build(&["run git commands", "manage docker containers"]);
        let results = idx.search("zzzyyyxxx", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn search_exact_term_ranks_highest() {
        let idx = Bm25Index::build(&[
            "manage docker containers",
            "git commit push pull",
            "web search browsing",
        ]);
        let results = idx.search("git", 3);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 1, "git doc should rank first");
    }

    #[test]
    fn search_multi_word_query() {
        let idx = Bm25Index::build(&["run shell commands bash", "git version control commit"]);
        let results = idx.search("git commit", 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn tokenize_filters_short_tokens() {
        let tokens = super::tokenize("a ab abc");
        assert!(!tokens.contains(&"a".to_string()));
        assert!(!tokens.contains(&"ab".to_string()));
        assert!(tokens.contains(&"abc".to_string()));
    }

    #[test]
    fn tokenize_splits_on_non_alphanumeric() {
        let tokens = super::tokenize("foo-bar baz_qux");
        assert!(tokens.contains(&"foo".to_string()));
        assert!(tokens.contains(&"bar".to_string()));
        assert!(tokens.contains(&"baz".to_string()));
        assert!(tokens.contains(&"qux".to_string()));
    }

    #[test]
    fn rrf_fuse_merges_lists() {
        let emb = vec![
            ScoredMatch {
                index: 0,
                score: 0.9,
            },
            ScoredMatch {
                index: 1,
                score: 0.5,
            },
        ];
        let bm25 = vec![(1, 2.0_f32), (2, 1.5_f32)];
        let fused = rrf_fuse(&emb, &bm25, 5);
        // index 1 appears in both lists — should score highest
        assert_eq!(fused[0].index, 1);
    }

    #[test]
    fn rrf_fuse_disjoint_lists() {
        let emb = vec![ScoredMatch {
            index: 0,
            score: 0.9,
        }];
        let bm25 = vec![(1, 1.0_f32)];
        let fused = rrf_fuse(&emb, &bm25, 5);
        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn rrf_fuse_empty_embedding() {
        let bm25 = vec![(0, 1.0_f32)];
        let fused = rrf_fuse(&[], &bm25, 5);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].index, 0);
    }

    #[test]
    fn rrf_fuse_empty_bm25() {
        let emb = vec![ScoredMatch {
            index: 0,
            score: 0.9,
        }];
        let fused = rrf_fuse(&emb, &[], 5);
        assert_eq!(fused.len(), 1);
    }

    #[test]
    fn rrf_fuse_respects_limit() {
        let emb = vec![
            ScoredMatch {
                index: 0,
                score: 0.9,
            },
            ScoredMatch {
                index: 1,
                score: 0.8,
            },
            ScoredMatch {
                index: 2,
                score: 0.7,
            },
        ];
        let fused = rrf_fuse(&emb, &[], 2);
        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn search_limit_zero_returns_empty() {
        let idx = Bm25Index::build(&["run git commands"]);
        let results = idx.search("git", 0);
        assert!(results.is_empty());
    }

    #[test]
    fn search_single_doc_hit() {
        let idx = Bm25Index::build(&["run git commands"]);
        let results = idx.search("git", 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0);
        assert!(results[0].1 > 0.0);
    }

    #[test]
    fn rrf_fuse_both_empty_returns_empty() {
        let fused = rrf_fuse(&[], &[], 5);
        assert!(fused.is_empty());
    }

    #[test]
    fn tokenize_all_short_returns_empty() {
        let tokens = super::tokenize("a bb cc");
        assert!(
            tokens.is_empty(),
            "all tokens shorter than 3 chars should be filtered"
        );
    }

    #[test]
    fn rrf_fuse_rank_matters_first_beats_second() {
        // Both lists have same two items; item at rank 0 scores higher than rank 1
        let emb = vec![
            ScoredMatch {
                index: 0,
                score: 0.9,
            },
            ScoredMatch {
                index: 1,
                score: 0.8,
            },
        ];
        let bm25 = vec![(0, 2.0_f32), (1, 1.0_f32)];
        let fused = rrf_fuse(&emb, &bm25, 5);
        // index 0 is rank-1 in both lists → higher RRF score than index 1
        assert_eq!(fused[0].index, 0);
        assert!(fused[0].score > fused[1].score);
    }

    #[test]
    fn search_scores_positive_for_matching_term() {
        let idx = Bm25Index::build(&["database query optimization", "network protocol handling"]);
        let results = idx.search("database", 5);
        assert_eq!(results.len(), 1);
        assert!(results[0].1 > 0.0, "BM25 score should be positive");
    }
}
