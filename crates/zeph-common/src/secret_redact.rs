// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compiled secret-prefix redaction shared by `zeph-sanitizer` and `zeph-memory`.
//!
//! [`crate::secrets`] holds the raw prefixes and patterns; this module compiles them once and
//! applies, in the one correct order, the GitHub fine-grained token shape, the boundary-anchored
//! prefix tier ([`SECRET_TOKEN_PATTERN`]) and the glued-prefix tiers ([`GluedTier::PASSES`]).
//! Keeping the pipeline here means a new tier is wired in once for every consumer.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::secrets::{
    GITHUB_FINE_GRAINED_PAT_PATTERN, GluedTier, SECRET_PREFIXES, SECRET_TOKEN_PATTERN,
    redact_glued_matches,
};

static GITHUB_PAT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(GITHUB_FINE_GRAINED_PAT_PATTERN).expect("github pat shape regex is valid")
});

static SECRET_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(SECRET_TOKEN_PATTERN).expect("secret shape regex is valid"));

static GLUED_REGEXES: LazyLock<[(GluedTier, Regex); 2]> = LazyLock::new(|| {
    GluedTier::PASSES.map(|tier| {
        let re = Regex::new(tier.pattern()).expect("glued secret shape regex is valid");
        (tier, re)
    })
});

/// Keep `current` when `replaced` found nothing to change, so unchanged text stays borrowed.
fn apply<'a>(
    current: Cow<'a, str>,
    replaced: impl for<'b> FnOnce(&'b str) -> Cow<'b, str>,
) -> Cow<'a, str> {
    match replaced(current.as_ref()) {
        Cow::Borrowed(_) => current,
        Cow::Owned(o) => Cow::Owned(o),
    }
}

fn redact_glued(text: &str) -> Cow<'_, str> {
    let mut current = Cow::Borrowed(text);
    for (tier, re) in GLUED_REGEXES.iter() {
        current = apply(current, |t| {
            redact_glued_matches(
                t,
                *tier,
                re.captures_iter(t).filter_map(|caps| {
                    Some((
                        caps.get(0)?.range(),
                        caps.get(1)?.range(),
                        caps.get(2)?.range(),
                    ))
                }),
            )
        });
    }
    current
}

/// Redact the secret-prefix tokens in `text`.
///
/// Runs, in order: the GitHub fine-grained token shape, the boundary-anchored prefix tier
/// (replaced with `${1}[REDACTED]`), and the glued-prefix tiers (distinctive before ambiguous,
/// see [`GluedTier`]). Returns `Cow::Borrowed` when nothing matched, so clean text costs no
/// allocation.
///
/// # Examples
///
/// ```
/// use zeph_common::secret_redact::redact_prefixed_secrets;
///
/// assert_eq!(redact_prefixed_secrets("key: sk-abc123"), "key: [REDACTED]");
/// assert_eq!(redact_prefixed_secrets("a1glpat-aB3dEf_gH1jKlMnOpQr"), "a1[REDACTED]");
/// assert_eq!(redact_prefixed_secrets("disk-usage"), "disk-usage");
/// ```
#[must_use]
pub fn redact_prefixed_secrets(text: &str) -> Cow<'_, str> {
    let current = if text.contains("github_pat_") {
        apply(Cow::Borrowed(text), |t| {
            GITHUB_PAT_REGEX.replace_all(t, "[REDACTED]")
        })
    } else {
        Cow::Borrowed(text)
    };
    if !SECRET_PREFIXES.iter().any(|p| current.contains(*p)) {
        return current;
    }
    let current = apply(current, |t| SECRET_REGEX.replace_all(t, "${1}[REDACTED]"));
    apply(current, redact_glued)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB_TAIL: &str = "mNoPqRsTuVwXyZ0123456789aBcDeFgHiJkLmNoPqRsTuVwXyZ01234567";

    fn github_token(version: &str) -> String {
        format!("github_pat_{version}ABCDEFG0aBcDeFgHiJkL_{GITHUB_TAIL}")
    }

    #[test]
    fn github_fine_grained_token_is_redacted_for_any_version_and_glue() {
        for version in ["11", "12", "AB", "9z"] {
            let token = github_token(version);
            for text in [
                format!("token: {token}"),
                format!("x1{token}"),
                format!("\x1b[32m{token}"),
            ] {
                let result = redact_prefixed_secrets(&text);
                assert!(!result.contains(GITHUB_TAIL), "{text} -> {result}");
            }
        }
    }

    #[test]
    fn github_identifiers_and_prose_stay_intact() {
        for text in [
            "load_github_pat_from_env()",
            "the github_pat_ prefix",
            "mygithub_pat_rotation_schedule_doc",
            "github_pat_short_token",
        ] {
            assert!(
                matches!(redact_prefixed_secrets(text), Cow::Borrowed(_)),
                "{text}"
            );
        }
    }

    #[test]
    fn clean_text_is_not_allocated() {
        assert!(matches!(
            redact_prefixed_secrets("task-runner-for-k8s-clusters"),
            Cow::Borrowed(_)
        ));
    }
}
