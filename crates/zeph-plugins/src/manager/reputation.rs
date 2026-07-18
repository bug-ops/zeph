// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Install-time plugin/skill name-similarity ("typosquat") advisory check (spec-043, #5864).
//!
//! Compares an incoming plugin's declared name and skill names against a corpus of known
//! names (bundled skills, managed skills, other installed plugins' skills) using a
//! Levenshtein-based similarity ratio. The default [`LocalTyposquatCheck`] implementation is
//! local-only and makes zero network calls; the [`ReputationSource`] trait boundary exists so
//! an opt-in external source could be added later without changing either install call site
//! (`PluginManager::add` / `apply_staged_update`, see FR-004).

/// Where a name in the known-names corpus came from. Carried on [`ReputationWarning`] so
/// callers can format an accurate message (e.g. "closely resembles bundled skill ...").
// TODO(critic): a future external ReputationSource (FR-004/US-003) has no variant here to
// express its own match provenance and would have to misuse `Plugin(name)` — add a dedicated
// variant (e.g. `External(String)`) when/if an external source lands.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MatchedSource {
    /// A compile-time bundled skill (`zeph_skills::bundled::bundled_skill_names()`).
    Bundled,
    /// A user-managed skill (`managed_skills_dir`).
    Managed,
    /// A skill declared by another already-installed plugin (plugin name carried here).
    Plugin(String),
}

/// A single near-match found by a [`ReputationSource`].
///
/// Transient install-time result — not persisted (spec-043 §5).
#[derive(Debug, Clone)]
pub struct ReputationWarning {
    /// The plugin or skill name being installed/updated.
    pub checked_name: String,
    /// The known name it closely resembles.
    pub matched_name: String,
    /// Similarity ratio in `[0, 1]`; `1.0` means identical.
    pub similarity: f32,
    /// Where `matched_name` came from.
    pub matched_source: MatchedSource,
}

impl std::fmt::Display for ReputationWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source_desc = match &self.matched_source {
            MatchedSource::Bundled => "bundled skill".to_owned(),
            MatchedSource::Managed => "managed skill".to_owned(),
            MatchedSource::Plugin(p) => format!("skill from installed plugin {p:?}"),
        };
        write!(
            f,
            "name {:?} closely resembles {source_desc} {:?} (similarity {:.2}); if unintended \
             this could be a typosquat",
            self.checked_name, self.matched_name, self.similarity
        )
    }
}

/// Pluggable install-time reputation/typosquat check (FR-004).
///
/// Implementations are advisory-only: they report near-matches, they never decide whether an
/// install is blocked (that is a caller-side `enforcement` policy — see
/// [`super::PluginManager::with_reputation_enforcement`]). The built-in and only shipped
/// implementation is [`LocalTyposquatCheck`]; a future opt-in external source can implement
/// this trait without either install call site changing.
pub trait ReputationSource: Send + Sync {
    /// Compare `name` against every `(name, source)` pair in `known_names` and return a
    /// warning for each near-match. Returns an empty `Vec` when nothing looks suspicious.
    fn check(&self, name: &str, known_names: &[(String, MatchedSource)]) -> Vec<ReputationWarning>;
}

/// Enforcement posture applied by [`super::PluginManager`] when [`ReputationSource::check`]
/// returns at least one warning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReputationEnforcement {
    /// Surface the warning(s); the install/update proceeds (FR-006, SC-004 default posture).
    #[default]
    Warn,
    /// Refuse the install/update before any file is written or swapped (opt-in, FR-006).
    Block,
}

/// Local, zero-network Levenshtein-similarity typosquat check — the only built-in
/// [`ReputationSource`] implementation (OQ1: local heuristic ships first; an external
/// community registry is deferred behind the trait boundary).
///
/// # Homoglyph/Unicode confusables are out of scope
///
/// This check compares names by edit distance, not visual similarity — Cyrillic-`а` vs
/// Latin-`a` style confusables are not detected. For the **plugin-name** axis this is already
/// structurally precluded: `validate_plugin_name` constrains plugin names to `[a-z][a-z0-9-]*`
/// (ASCII-only). Skill names read from `SKILL.md` frontmatter are less constrained; Unicode
/// confusable detection there is a documented future refinement (spec-043 M3), same bucket as
/// upgrading plain Levenshtein to Damerau-Levenshtein for transposition-heavy squats.
///
/// # Examples
///
/// ```rust
/// use zeph_plugins::{LocalTyposquatCheck, MatchedSource, ReputationSource};
///
/// let check = LocalTyposquatCheck::default(); // threshold 0.65, min_name_len 3
/// let known = vec![("git-pr".to_owned(), MatchedSource::Bundled)];
///
/// // "github-pr" closely resembles the bundled "git-pr" skill (similarity ~0.667).
/// let warnings = check.check("github-pr", &known);
/// assert_eq!(warnings.len(), 1);
/// assert_eq!(warnings[0].matched_name, "git-pr");
///
/// // An unrelated name never warns.
/// assert!(check.check("weather", &known).is_empty());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LocalTyposquatCheck {
    similarity_threshold: f32,
    min_name_len: usize,
}

impl LocalTyposquatCheck {
    /// Build a check with an explicit `[0,1]` similarity threshold and minimum comparable name
    /// length (shorter names are skipped as noise — see spec-043 M1 for why `3`, not `4`, is
    /// the shipped default: it keeps the bundled `git` skill covered).
    #[must_use]
    pub fn new(similarity_threshold: f32, min_name_len: usize) -> Self {
        Self {
            similarity_threshold,
            min_name_len,
        }
    }
}

impl Default for LocalTyposquatCheck {
    /// `similarity_threshold = 0.65`, `min_name_len = 3` — matches
    /// `zeph_config::plugins::ReputationConfig::default()`.
    fn default() -> Self {
        Self::new(0.65, 3)
    }
}

impl ReputationSource for LocalTyposquatCheck {
    fn check(&self, name: &str, known_names: &[(String, MatchedSource)]) -> Vec<ReputationWarning> {
        let mut warnings = Vec::new();
        for (known, source) in known_names {
            // Raw byte-identical names are a legitimate re-install/update, not a typosquat —
            // checked BEFORE the similarity passes so the hyphen-strip pass below stays
            // meaningful for separator-removal squats (spec-043 S2: a similarity==1.0 guard
            // would cancel that pass entirely).
            if name == known {
                continue;
            }
            let shorter_len = name.chars().count().min(known.chars().count());
            if shorter_len < self.min_name_len {
                continue;
            }
            let similarity = combined_similarity(name, known);
            if similarity >= self.similarity_threshold {
                warnings.push(ReputationWarning {
                    checked_name: name.to_owned(),
                    matched_name: known.clone(),
                    similarity,
                    matched_source: source.clone(),
                });
            }
        }
        warnings
    }
}

/// `max` of the raw-name similarity and the hyphen-stripped-name similarity.
///
/// The hyphen-strip pass catches separator-removal squats (`gitpr` vs `git-pr`, which are
/// byte-identical once hyphens are stripped and therefore score `1.0` on that pass alone).
fn combined_similarity(a: &str, b: &str) -> f32 {
    let raw = normalized_similarity(a, b);
    let stripped = normalized_similarity(&strip_hyphens(a), &strip_hyphens(b));
    raw.max(stripped)
}

fn strip_hyphens(s: &str) -> String {
    s.chars().filter(|&c| c != '-').collect()
}

/// Levenshtein edit distance normalized to a `[0, 1]` similarity ratio; `1.0` means identical.
fn normalized_similarity(a: &str, b: &str) -> f32 {
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss)]
    // name lengths are tiny (<= 64 chars, see MAX_DEPENDENCIES-adjacent bound)
    let ratio = 1.0 - levenshtein(a, b) as f32 / max_len as f32;
    ratio
}

/// Plain Levenshtein edit distance (insert/delete/substitute), operating on `char`s so
/// multi-byte UTF-8 skill names are compared correctly. Two-row DP, O(`a.len() * b.len()`);
/// plugin/skill names are bounded to 64 chars (`validate_plugin_name`), so this is trivial.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (a_len, b_len) = (a.len(), b.len());
    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr: Vec<usize> = vec![0; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── levenshtein ──────────────────────────────────────────────────────────────

    #[test]
    fn levenshtein_identical_is_zero() {
        assert_eq!(levenshtein("git-pr", "git-pr"), 0);
    }

    #[test]
    fn levenshtein_single_insert_is_one() {
        assert_eq!(levenshtein("gitpr", "git-pr"), 1);
    }

    #[test]
    fn levenshtein_single_delete_is_one() {
        assert_eq!(levenshtein("git-pr", "gitpr"), 1);
    }

    #[test]
    fn levenshtein_single_substitute_is_one() {
        assert_eq!(levenshtein("git-pr", "git-pq"), 1);
    }

    #[test]
    fn levenshtein_transposition_is_two() {
        // Plain Levenshtein (not Damerau) counts an adjacent transposition as 2 edits.
        assert_eq!(levenshtein("gti-pr", "git-pr"), 2);
    }

    #[test]
    fn levenshtein_empty_pair_is_zero() {
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn levenshtein_one_empty_is_other_len() {
        assert_eq!(levenshtein("", "git-pr"), 6);
        assert_eq!(levenshtein("git-pr", ""), 6);
    }

    // ── normalized_similarity ────────────────────────────────────────────────────

    #[test]
    fn normalized_similarity_identical_is_one() {
        assert!((normalized_similarity("git-pr", "git-pr") - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalized_similarity_disjoint_is_low() {
        assert!(normalized_similarity("weather", "git-pr") < 0.3);
    }

    #[test]
    fn normalized_similarity_motivating_example() {
        // levenshtein("github-pr","git-pr") = 3 (insert "hub"), max_len = 9 -> 1 - 3/9 = 0.667
        let sim = normalized_similarity("github-pr", "git-pr");
        assert!((sim - 0.667).abs() < 0.01, "got {sim}");
    }

    // ── LocalTyposquatCheck::check ───────────────────────────────────────────────

    fn corpus(names: &[(&str, MatchedSource)]) -> Vec<(String, MatchedSource)> {
        names
            .iter()
            .map(|(n, s)| ((*n).to_owned(), s.clone()))
            .collect()
    }

    #[test]
    fn check_warns_on_motivating_example() {
        let check = LocalTyposquatCheck::default();
        let known = corpus(&[("git-pr", MatchedSource::Bundled)]);
        let warnings = check.check("github-pr", &known);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].matched_name, "git-pr");
        assert!(warnings[0].similarity >= 0.65);
    }

    #[test]
    fn check_exact_match_does_not_warn() {
        let check = LocalTyposquatCheck::default();
        let known = corpus(&[("git-pr", MatchedSource::Bundled)]);
        assert!(check.check("git-pr", &known).is_empty());
    }

    #[test]
    fn check_unrelated_name_does_not_warn() {
        let check = LocalTyposquatCheck::default();
        let known = corpus(&[("git-pr", MatchedSource::Bundled)]);
        assert!(check.check("weather", &known).is_empty());
    }

    #[test]
    fn check_skips_names_shorter_than_min_name_len() {
        let known = corpus(&[("abc", MatchedSource::Bundled)]);
        // "abc" vs "abd": distance 1, max_len 3, similarity 0.667 >= 0.65 -> warns when the
        // pair clears the min_name_len gate...
        let permissive = LocalTyposquatCheck::new(0.65, 3);
        assert_eq!(permissive.check("abd", &known).len(), 1);
        // ...but shorter_len = min(3, 3) = 3 < 4, so a stricter min_name_len skips the pair
        // entirely regardless of how similar the names are.
        let strict_len = LocalTyposquatCheck::new(0.65, 4);
        assert!(strict_len.check("abd", &known).is_empty());
    }

    #[test]
    fn check_covers_short_bundled_git_name_at_min_len_three() {
        let check = LocalTyposquatCheck::new(0.65, 3);
        let known = corpus(&[("git", MatchedSource::Bundled)]);
        // "gti" vs "git": one transposition, distance 2, max_len 3 -> sim = 1 - 2/3 = 0.333, no warn
        assert!(check.check("gti", &known).is_empty());
        // "git" vs "git" is an exact match anyway (no warn) — use a 1-edit near-miss instead:
        // "gitt" (len 4) vs "git" (len 3): distance 1, max_len 4 -> sim = 0.75 >= 0.65, warns.
        let warnings = check.check("gitt", &known);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn check_hyphen_stripped_separator_removal_squat_is_caught() {
        // S2 regression guard: "gitpr" vs "git-pr" are not raw-equal, so the raw-equality skip
        // does not apply; the hyphen-stripped pass makes them byte-identical (sim 1.0), which
        // must warn, not be silently dropped as a "legitimate exact match".
        let check = LocalTyposquatCheck::default();
        let known = corpus(&[("git-pr", MatchedSource::Bundled)]);
        let warnings = check.check("gitpr", &known);
        assert_eq!(warnings.len(), 1);
        assert!((warnings[0].similarity - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn check_empty_known_names_never_warns() {
        let check = LocalTyposquatCheck::default();
        assert!(check.check("github-pr", &[]).is_empty());
    }

    #[test]
    fn check_threshold_boundary_flips_correctly() {
        let known = corpus(&[("git-pr", MatchedSource::Bundled)]);
        // similarity("github-pr","git-pr") == 0.667 (see normalized_similarity_motivating_example)
        let below = LocalTyposquatCheck::new(0.70, 3);
        assert!(below.check("github-pr", &known).is_empty());
        let above = LocalTyposquatCheck::new(0.60, 3);
        assert_eq!(above.check("github-pr", &known).len(), 1);
    }

    #[test]
    fn matched_source_is_preserved_on_warning() {
        let check = LocalTyposquatCheck::default();
        let known = corpus(&[("git-pr", MatchedSource::Plugin("acme-tools".to_owned()))]);
        let warnings = check.check("github-pr", &known);
        assert_eq!(
            warnings[0].matched_source,
            MatchedSource::Plugin("acme-tools".to_owned())
        );
    }
}
