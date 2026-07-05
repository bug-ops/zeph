// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared char-level Levenshtein edit distance and normalized similarity.
//!
//! Used by [`implicit_conflict`](crate::graph::implicit_conflict) (STALE/CUPMem predicate
//! conflict detection) and [`belief_revision`](crate::graph::belief_revision) (Kumiho
//! relation-domain check) to compare relation/predicate strings.
//!
//! Only [`levenshtein_distance`] is shared with `belief_revision`: its `relation_similarity`
//! normalizes by **byte** length (not char length like [`normalized_similarity`] below) to
//! preserve its original pre-existing behavior. Do not consolidate the two normalizations —
//! they diverge for non-ASCII input and each caller's threshold was tuned against its own
//! normalization.

/// Char-level Levenshtein edit distance between two strings.
///
/// # Examples
///
/// ```
/// use zeph_memory::graph::string_similarity::levenshtein_distance;
///
/// assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
/// assert_eq!(levenshtein_distance("", "abc"), 3);
/// ```
#[must_use]
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = usize::from(a_chars[i - 1] != b_chars[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Normalized Levenshtein similarity between two strings, in `[0.0, 1.0]`.
///
/// `1.0` means identical; the distance is normalized by the longer string's char count.
/// Returns `1.0` if both strings are empty, `0.0` if only one is empty.
///
/// # Examples
///
/// ```
/// use zeph_memory::graph::string_similarity::normalized_similarity;
///
/// assert!((normalized_similarity("uses", "uses") - 1.0).abs() < f64::EPSILON);
/// assert!(normalized_similarity("uses", "xyz_unrelated_value") < 0.5);
/// ```
#[must_use]
pub fn normalized_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let len_a = a.chars().count();
    let len_b = b.chars().count();
    if len_a == 0 && len_b == 0 {
        return 1.0;
    }
    if len_a == 0 || len_b == 0 {
        return 0.0;
    }
    let dist = levenshtein_distance(a, b);
    let max_len = len_a.max(len_b);
    #[allow(clippy::cast_precision_loss)]
    let result = 1.0 - (dist as f64 / max_len as f64);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_distance_empty_strings() {
        assert_eq!(levenshtein_distance("", ""), 0);
    }

    #[test]
    fn levenshtein_distance_one_empty() {
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("", "xyz"), 3);
    }

    #[test]
    fn levenshtein_distance_single_substitution() {
        assert_eq!(levenshtein_distance("works_at", "work_at"), 1);
    }

    #[test]
    fn normalized_similarity_identical() {
        assert!((normalized_similarity("uses", "uses") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn normalized_similarity_empty_both() {
        assert!((normalized_similarity("", "") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn normalized_similarity_empty_one() {
        assert!((normalized_similarity("", "abc") - 0.0).abs() < f64::EPSILON);
        assert!((normalized_similarity("abc", "") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn normalized_similarity_completely_different() {
        let sim = normalized_similarity("uses", "xyz_unrelated_value");
        assert!(sim < 0.5, "expected low similarity, got {sim}");
    }

    #[test]
    fn normalized_similarity_partial_overlap() {
        // "works_at" vs "work_at" — 1 char diff out of 8 → ~0.875
        let s = normalized_similarity("works_at", "work_at");
        assert!(s > 0.5, "expected > 0.5, got {s}");
    }
}
