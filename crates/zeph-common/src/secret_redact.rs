// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one ordered secret and path scrub pipeline shared by every redaction consumer.
//!
//! [`crate::secrets`] holds the raw prefixes and patterns; this module compiles them once and
//! applies them in the one correct order. Layers, each building on the previous:
//!
//! - [`scrub_secret_shapes`]: PEM blocks, unterminated PEM headers, secret prefixes, token
//!   shapes (GitHub App, AWS STS, Telegram), `Authorization: Bearer`, JWT, marker-anchored AWS
//!   secret keys.
//! - [`scrub_secrets`]: URL-embedded credentials, then [`scrub_secret_shapes`].
//! - [`scrub_content`]: [`scrub_secrets`], then [`sanitize_paths`].
//!
//! Keeping the pipeline here means a new pass is wired in once for every consumer
//! (`zeph-sanitizer`, `zeph-core`, `zeph-memory`).

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::secrets::{
    AWS_SECRET_KEY_PATTERN, AWS_STS_ACCESS_KEY_ID_PATTERN, BEARER_TOKEN_PATTERN,
    GITHUB_APP_TOKEN_PATTERN, GITHUB_FINE_GRAINED_PAT_PATTERN, GluedTier, JWT_PATTERN,
    PATH_PREFIXES, PEM_PRIVATE_KEY_PATTERN, PEM_PRIVATE_KEY_UNTERMINATED_PATTERN, SECRET_PREFIXES,
    SECRET_TOKEN_PATTERN, TELEGRAM_BOT_TOKEN_PATTERN, redact_glued_matches,
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

static BEARER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(BEARER_TOKEN_PATTERN).expect("bearer shape regex is valid"));

static JWT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(JWT_PATTERN).expect("jwt shape regex is valid"));

static PEM_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PEM_PRIVATE_KEY_PATTERN).expect("pem shape regex is valid"));

static PEM_UNTERMINATED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(PEM_PRIVATE_KEY_UNTERMINATED_PATTERN).expect("pem unterminated shape regex is valid")
});

static AWS_SECRET_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(AWS_SECRET_KEY_PATTERN).expect("aws secret shape regex is valid"));

static TOKEN_SHAPE_REGEXES: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        GITHUB_APP_TOKEN_PATTERN,
        AWS_STS_ACCESS_KEY_ID_PATTERN,
        TELEGRAM_BOT_TOKEN_PATTERN,
    ]
    .map(|pattern| Regex::new(pattern).expect("token shape regex is valid"))
});

// Matches basic-auth credentials embedded in URLs: https://user:pass@host
static URL_CREDS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?P<scheme>[a-z][a-z0-9+\-.]*://)(?P<creds>[^@/\s]+:[^@/\s]+@)")
        .expect("url credential redaction regex is valid")
});

static PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    let alt = PATH_PREFIXES.join("|");
    Regex::new(&format!(r#"(?:{alt})[^\s"'`,;{{}}\[\]]*"#)).expect("path redaction regex is valid")
});

/// Keep `current` when `replaced` found nothing to change, so unchanged text stays borrowed.
fn chain<'a>(
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
        current = chain(current, |t| {
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
        chain(Cow::Borrowed(text), |t| {
            GITHUB_PAT_REGEX.replace_all(t, "[REDACTED]")
        })
    } else {
        Cow::Borrowed(text)
    };
    if !SECRET_PREFIXES.iter().any(|p| current.contains(*p)) {
        return current;
    }
    let current = chain(current, |t| SECRET_REGEX.replace_all(t, "${1}[REDACTED]"));
    chain(current, redact_glued)
}

/// Replace secret-shaped substrings with redaction markers.
///
/// Covers PEM private-key blocks, known API-key prefixes, GitHub App, AWS STS and Telegram bot
/// token shapes, `Authorization: Bearer` headers, standalone JWTs and marker-anchored AWS secret
/// access keys. Unlike a registry of known secret values it needs no prior registration, so it
/// also catches secrets a model fabricates or echoes. Returns `Cow::Borrowed` when nothing
/// matched (zero-allocation fast path).
///
/// The two PEM passes run first, before the prefix pass: the `-----BEGIN` prefix entry would
/// otherwise consume just the header token and keep either PEM pattern from matching the header
/// at all, leaving the multi-line key body unredacted (#6592). The unterminated-header pass runs
/// after the closed-block pass so already-closed blocks are consumed first.
///
/// # Examples
///
/// ```
/// use zeph_common::secret_redact::scrub_secret_shapes;
///
/// let result = scrub_secret_shapes("here is a key: sk-test-abc123def456");
/// assert!(!result.contains("sk-test-abc123def456"));
/// assert!(result.contains("[REDACTED]"));
/// ```
#[must_use]
pub fn scrub_secret_shapes(text: &str) -> Cow<'_, str> {
    let current = PEM_REGEX.replace_all(text, "[REDACTED_PEM_KEY]");
    let current = chain(current, |t| {
        PEM_UNTERMINATED_REGEX.replace_all(t, "[REDACTED_PEM_KEY]")
    });
    let mut current = chain(current, redact_prefixed_secrets);
    for re in TOKEN_SHAPE_REGEXES.iter() {
        current = chain(current, |t| re.replace_all(t, "[REDACTED]"));
    }
    let current = chain(current, |t| BEARER_REGEX.replace_all(t, "${1}[REDACTED]"));
    let current = chain(current, |t| JWT_REGEX.replace_all(t, "[REDACTED_JWT]"));
    chain(current, |t| {
        AWS_SECRET_REGEX.replace_all(t, "${1}[REDACTED]${2}")
    })
}

/// Strip URL-embedded credentials, then apply [`scrub_secret_shapes`].
///
/// `https://user:pass@host` becomes `https://[REDACTED]@host`. Returns `Cow::Borrowed` when
/// nothing changed.
///
/// # Examples
///
/// ```
/// use zeph_common::secret_redact::scrub_secrets;
///
/// assert_eq!(
///     scrub_secrets("postgres://u:p@db/x"),
///     "postgres://[REDACTED]@db/x"
/// );
/// ```
#[must_use]
pub fn scrub_secrets(text: &str) -> Cow<'_, str> {
    let current = URL_CREDS_REGEX.replace_all(text, "${scheme}[REDACTED]@");
    chain(current, scrub_secret_shapes)
}

/// Replace absolute filesystem paths with `[PATH]` to prevent information disclosure.
///
/// # Examples
///
/// ```
/// use zeph_common::secret_redact::sanitize_paths;
///
/// assert_eq!(sanitize_paths("open /home/ann/a.txt now"), "open [PATH] now");
/// ```
#[must_use]
pub fn sanitize_paths(text: &str) -> Cow<'_, str> {
    if !PATH_PREFIXES.iter().any(|p| text.contains(*p)) {
        return Cow::Borrowed(text);
    }
    PATH_REGEX.replace_all(text, "[PATH]")
}

/// Apply [`scrub_secrets`] and then [`sanitize_paths`] in a single pass over the text.
///
/// Use this before persisting or displaying untrusted text. Returns `Cow::Borrowed` when no
/// changes are needed (zero-allocation fast path).
///
/// # Examples
///
/// ```
/// use zeph_common::secret_redact::scrub_content;
///
/// assert_eq!(scrub_content("sk-abc123 in /tmp/x"), "[REDACTED] in [PATH]");
/// ```
#[must_use]
pub fn scrub_content(text: &str) -> Cow<'_, str> {
    chain(scrub_secrets(text), sanitize_paths)
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

    const AWS_SECRET: &str = "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    #[test]
    fn new_token_shapes_are_redacted() {
        let gh = "A".repeat(36);
        let gh_refresh = "B".repeat(76);
        for text in [
            format!("GITHUB_TOKEN=ghs_{gh}"),
            format!("token ghu_{gh}"),
            format!("refresh ghr_{gh_refresh}"),
            format!("x1ghs_{gh}"),
            "AWS_ACCESS_KEY_ID=ASIAIOSFODNN7EXAMPLE".to_owned(),
            "slack xapp-1-A0123456789-1234567890123-abcdef".to_owned(),
            "stripe rk_live_51H8abcDEFghiJKLmnoPQRstu".to_owned(),
            "stripe rk_test_51H8abcDEFghiJKLmnoPQRstu".to_owned(),
            "bot 1234567890:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw0".to_owned(),
        ] {
            let result = scrub_secret_shapes(&text);
            assert!(result.contains("[REDACTED]"), "{text} -> {result}");
        }
        let bot = scrub_secret_shapes("bot 1234567890:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw0");
        assert_eq!(bot, "bot [REDACTED]");
    }

    #[test]
    fn identifiers_resembling_new_prefixes_stay_intact() {
        for text in [
            "ASIAN_countries",
            "the ghs_config option",
            "ghu_settings and ghr_cache",
            "pid 12345:AA is not a token",
        ] {
            let result = scrub_secret_shapes(text);
            assert!(matches!(result, Cow::Borrowed(_)), "{text} -> {result}");
        }
    }

    #[test]
    fn scrub_content_covers_aws_url_credentials_and_paths() {
        let text = format!("{AWS_SECRET} at https://user:pass@host/x from /Users/dev/file");
        let result = scrub_content(&text);
        assert!(!result.contains("wJalr"), "{result}");
        assert!(!result.contains("user:pass"), "{result}");
        assert!(!result.contains("/Users/dev"), "{result}");
    }

    #[test]
    fn scrub_secrets_leaves_paths_but_strips_url_credentials() {
        let result = scrub_secrets("postgres://u:p@h/db in /home/a/b");
        assert_eq!(result, "postgres://[REDACTED]@h/db in /home/a/b");
    }

    #[test]
    fn clean_text_is_borrowed_by_every_layer() {
        let text = "nothing sensitive here";
        assert!(matches!(scrub_secret_shapes(text), Cow::Borrowed(_)));
        assert!(matches!(scrub_secrets(text), Cow::Borrowed(_)));
        assert!(matches!(sanitize_paths(text), Cow::Borrowed(_)));
        assert!(matches!(scrub_content(text), Cow::Borrowed(_)));
    }

    #[test]
    fn telegram_token_in_bot_api_url_is_redacted() {
        let text =
            "https://api.telegram.org/bot1234567890:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw0/getMe";
        let result = scrub_secret_shapes(text);
        assert_eq!(result, "https://api.telegram.org/bot[REDACTED]/getMe");
    }

    #[test]
    fn token_shape_length_boundaries() {
        let redacted = |text: &str| scrub_secret_shapes(text).contains("[REDACTED]");
        for prefix in ["ghs_", "ghu_", "ghr_"] {
            assert!(
                !redacted(&format!("{prefix}{}", "a".repeat(35))),
                "{prefix} n-1"
            );
            assert!(
                redacted(&format!("{prefix}{}", "a".repeat(36))),
                "{prefix} n"
            );
            let text = format!("{prefix}{}", "a".repeat(37));
            let longer = scrub_secret_shapes(&text);
            assert_eq!(longer, "[REDACTED]", "{prefix} n+1 is consumed whole");
        }
        assert!(!redacted("ASIA0123456789ABCDE"), "ASIA n-1 (15 chars)");
        assert!(redacted("ASIA0123456789ABCDEF"), "ASIA n (16 chars)");
        assert!(
            !redacted("ASIA0123456789ABCDEFG"),
            "ASIA n+1 is not a key id"
        );

        let bot = |tail: usize| format!("1234567890:AA{}", "a".repeat(tail));
        assert!(!redacted(&bot(32)), "telegram n-1");
        assert!(redacted(&bot(33)), "telegram n");
        let text = bot(34);
        assert_eq!(scrub_secret_shapes(&text), "[REDACTED]", "telegram n+1");
        assert!(
            !redacted("1234567:AAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "id too short"
        );
    }
}
