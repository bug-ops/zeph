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

static AWS_STS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(AWS_STS_ACCESS_KEY_ID_PATTERN).expect("aws sts shape regex is valid")
});

static TOKEN_SHAPE_REGEXES: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [GITHUB_APP_TOKEN_PATTERN, TELEGRAM_BOT_TOKEN_PATTERN]
        .map(|pattern| Regex::new(pattern).expect("token shape regex is valid"))
});

/// Minimum length for colon-less URL userinfo to be treated as an opaque credential token.
const URL_OPAQUE_USERINFO_MIN_LEN: usize = 20;

// Userinfo is everything in the authority up to the LAST '@' (greedy), so passwords containing
// '@' are consumed whole. '?' and '#' stay allowed because passwords may contain them; '"' is
// excluded so a match cannot span JSON string boundaries.
static URL_CREDS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?P<scheme>[a-z][a-z0-9+\-.]*://)(?P<userinfo>[^/\s"]*)@"#)
        .expect("url credential redaction regex is valid")
});

const URL_TEXT_DELIMITERS: [char; 4] = ['?', '#', ',', '\''];

/// `host:port?name=...`: digits, then a query whose first parameter has a name.
fn is_port_then_query(pass: &str) -> bool {
    let Some((port, query)) = pass.split_once('?') else {
        return false;
    };
    let Some((name, _)) = query.split_once('=') else {
        return false;
    };
    !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// `user:pass` (user may be empty, password must not be) is a credential unless the user part
/// contains `?`/`#` or the rest looks like `host:port?name=`; colon-less userinfo only when it is
/// long and free of text delimiters. Passwords starting with digits followed by `#`/`,` stay
/// credentials, so `host:8080#frag@x` is deliberately over-redacted.
fn is_url_credential(userinfo: &str) -> bool {
    match userinfo.split_once(':') {
        Some((user, pass)) => {
            !pass.is_empty() && !user.contains(['?', '#']) && !is_port_then_query(pass)
        }
        None => {
            userinfo.len() >= URL_OPAQUE_USERINFO_MIN_LEN && !userinfo.contains(URL_TEXT_DELIMITERS)
        }
    }
}

/// Redact credential-bearing userinfo; keep `text` borrowed when no URL qualifies.
#[expect(
    clippy::string_slice,
    reason = "index derived from a boundary API on the same string"
)]
fn redact_url_credentials(text: &str) -> Cow<'_, str> {
    let mut out: Option<String> = None;
    let mut last = 0;
    for caps in URL_CREDS_REGEX.captures_iter(text) {
        let (Some(whole), Some(scheme), Some(userinfo)) =
            (caps.get(0), caps.name("scheme"), caps.name("userinfo"))
        else {
            continue;
        };
        if !is_url_credential(userinfo.as_str()) {
            continue;
        }
        let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
        buf.push_str(&text[last..whole.start()]);
        buf.push_str(scheme.as_str());
        buf.push_str("[REDACTED]@");
        last = whole.end();
    }
    match out {
        Some(mut buf) => {
            buf.push_str(&text[last..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(text),
    }
}

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
    // The match consumes its delimiters, so a key sharing a delimiter with the previous one is
    // skipped in the first pass; the second pass redacts it.
    let current = (0..2).fold(current, |acc, _| {
        chain(acc, |t| AWS_STS_REGEX.replace_all(t, "${1}[REDACTED]${2}"))
    });
    let current = chain(current, |t| BEARER_REGEX.replace_all(t, "${1}[REDACTED]"));
    let current = chain(current, |t| JWT_REGEX.replace_all(t, "[REDACTED_JWT]"));
    chain(current, |t| {
        AWS_SECRET_REGEX.replace_all(t, "${1}[REDACTED]${2}")
    })
}

/// Strip URL-embedded credentials, then apply [`scrub_secret_shapes`].
///
/// `https://user:pass@host` becomes `https://[REDACTED]@host`. Userinfo runs to the last `@` of the
/// authority (so `u:p@ss@host` is redacted whole); colon-less userinfo is redacted only when it is
/// at least 20 characters, leaving `https://user@host` intact. A password containing a raw `"`
/// is not redacted (the quote bounds the match so JSON text is not swallowed). Returns
/// `Cow::Borrowed` when nothing changed.
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
    let current = redact_url_credentials(text);
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
    use std::assert_matches;

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
            assert_matches!(redact_prefixed_secrets(text), Cow::Borrowed(_), "{text}");
        }
    }

    #[test]
    fn clean_text_is_not_allocated() {
        assert_matches!(
            redact_prefixed_secrets("task-runner-for-k8s-clusters"),
            Cow::Borrowed(_)
        );
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
    fn sts_key_id_glued_to_underscore_is_redacted() {
        assert_eq!(
            scrub_secret_shapes("AWS_KEY_ASIAIOSFODNN7EXAMPLE"),
            "AWS_KEY_[REDACTED]"
        );
        assert_eq!(
            scrub_secret_shapes("ASIAIOSFODNN7EXAMPLE_x"),
            "[REDACTED]_x"
        );
        assert_eq!(
            scrub_secret_shapes("id=ASIAIOSFODNN7EXAMPLE end"),
            "id=[REDACTED] end"
        );
        assert_eq!(
            scrub_secret_shapes("ASIAIOSFODNN7EXAMPLE,ASIAABCDEFGHIEXAMPLE,ASIAIOSFODNN7EXAMPLE"),
            "[REDACTED],[REDACTED],[REDACTED]"
        );
    }

    #[test]
    fn keys_after_ansi_or_quoted_printable_marker_are_redacted() {
        let sts = "ASIAIOSFODNN7EXAMPLE";
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let akia = "AKIAIOSFODNN7EXAMPLE";
        let ghp = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        assert_eq!(
            scrub_secret_shapes(&format!("\x1b[32m{sts}\x1b[0m")),
            "\x1b[32m[REDACTED]\x1b[0m"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("key=3D{sts} end")),
            "key=3D[REDACTED] end"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("aws_secret_access_key=\x1b[33m{secret}\x1b[0m")),
            "aws_secret_access_key=\x1b[33m[REDACTED]\x1b[0m"
        );
        for token in [akia, ghp, "sk-short"] {
            assert_eq!(
                scrub_secret_shapes(&format!("\x1b[31m{token}\x1b[0m")),
                "\x1b[31m[REDACTED]\x1b[0m",
                "ansi-prefixed {token}"
            );
            assert_eq!(
                scrub_secret_shapes(&format!("tok=3D{token}")),
                "tok=3D[REDACTED]",
                "qp-prefixed {token}"
            );
        }
    }

    #[test]
    fn csi_colon_sgr_and_lowercase_qp_boundaries_are_redacted() {
        let sts = "ASIAIOSFODNN7EXAMPLE";
        let ghp = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(
            scrub_secret_shapes(&format!("\x1b[01;31m\x1b[K{ghp}\x1b[m")),
            "\x1b[01;31m\x1b[K[REDACTED]\x1b[m"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("\x1b[38:5:1m{ghp}")),
            "\x1b[38:5:1m[REDACTED]"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("\x1b[?25l{ghp}")),
            "\x1b[?25l[REDACTED]"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("key=3d{ghp}")),
            "key=3d[REDACTED]"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("\x1b[K{sts}")),
            "\x1b[K[REDACTED]"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("x=3d{sts} end")),
            "x=3d[REDACTED] end"
        );
        assert_eq!(
            scrub_secret_shapes(&format!("secret_access_key=\x1b[K{secret}")),
            "secret_access_key=\x1b[K[REDACTED]"
        );
    }

    #[test]
    fn token_split_by_csi_runs_is_redacted_whole() {
        assert_eq!(
            scrub_secret_shapes("sk-\x1b[m\x1b[Kproj0123456789abcdef"),
            "[REDACTED]"
        );
        assert_eq!(
            scrub_secret_shapes("a sk-abc\x1b[1mdef\x1b[0m done"),
            "a [REDACTED]\x1b[0m done"
        );
        assert_eq!(
            scrub_secret_shapes("sk-abc\x1b[0m tail"),
            "[REDACTED]\x1b[0m tail"
        );
    }

    #[test]
    fn only_qp_equals_three_d_is_a_boundary() {
        for text in [
            "PKG=dask-ml",
            "--type=cask-fonts",
            "x=3Fdisk-usage",
            "a=ABtask-list",
        ] {
            let result = scrub_secret_shapes(text);
            assert!(matches!(result, Cow::Borrowed(_)), "{text} -> {result}");
        }
        assert_eq!(scrub_secret_shapes("k=3Dsk-short"), "k=3D[REDACTED]");
    }

    #[test]
    fn csi_with_intermediate_and_private_bytes_is_a_boundary() {
        let ghp = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        for csi in ["\x1b[2 q", "\x1b[>4;1m", "\x1b[<u", "\x1b[?1;2$p"] {
            assert_eq!(
                scrub_secret_shapes(&format!("{csi}{ghp}")),
                format!("{csi}[REDACTED]"),
                "{csi:?}"
            );
        }
        assert_eq!(scrub_secret_shapes("\x1b[2 qsk-abc"), "\x1b[2 q[REDACTED]");
    }

    #[test]
    fn aws_secret_key_tolerates_csi_around_the_separator() {
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(
            scrub_secret_shapes(&format!(
                "aws_secret_access_key\x1b[01;31m\x1b[K=\x1b[0m\x1b[33m{secret}\x1b[m"
            )),
            "aws_secret_access_key\x1b[01;31m\x1b[K=\x1b[0m\x1b[33m[REDACTED]\x1b[m"
        );
    }

    #[test]
    fn non_sgr_csi_does_not_trigger_on_plain_words() {
        for text in [
            "\x1b[Kdisk-usage ok",
            "\x1b[38:5:1mtask-list",
            "x=3dtask-list",
        ] {
            let result = scrub_secret_shapes(text);
            assert_matches!(result, Cow::Borrowed(_), "{text} -> {result}");
        }
    }

    #[test]
    fn ansi_boundary_leaves_plain_text_intact() {
        let result = scrub_secret_shapes("\x1b[32mdisk-usage ok\x1b[0m");
        assert_matches!(result, Cow::Borrowed(_), "{result}");
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
            assert_matches!(result, Cow::Borrowed(_), "{text} -> {result}");
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
    fn url_password_containing_at_is_fully_redacted() {
        assert_eq!(
            scrub_content("https://u:p@ss@host/x"),
            "https://[REDACTED]@host/x"
        );
        assert_eq!(
            scrub_secrets("see https://u:p@ss@host and more"),
            "see https://[REDACTED]@host and more"
        );
    }

    #[test]
    fn url_password_with_query_or_fragment_chars_is_redacted() {
        assert_eq!(
            scrub_secrets("postgres://admin:Pa#ss1@db/x"),
            "postgres://[REDACTED]@db/x"
        );
        assert_eq!(
            scrub_secrets("https://u:p?ss@host/x"),
            "https://[REDACTED]@host/x"
        );
        assert_eq!(
            scrub_secrets("redis://:pw@host:6379"),
            "redis://[REDACTED]@host:6379"
        );
    }

    #[test]
    fn url_passwords_with_special_characters_are_redacted() {
        for pass in [
            "p[ss", "p{ss}", "p|ss", "p^ss", "p\\ss", "p<ss>", "p`ss", "p]ss", "p>ss",
        ] {
            let text = format!("postgres://u:{pass}@db/x");
            assert_eq!(scrub_secrets(&text), "postgres://[REDACTED]@db/x", "{pass}");
        }
    }

    #[test]
    fn digit_prefixed_passwords_and_apostrophe_users_are_redacted() {
        for (text, expected) in [
            (
                "postgres://admin:2024#Summer!@db/x",
                "postgres://[REDACTED]@db/x",
            ),
            ("https://u:123,abc@h", "https://[REDACTED]@h"),
            ("https://o'brien:hunter2@h", "https://[REDACTED]@h"),
        ] {
            assert_eq!(scrub_secrets(text), expected, "{text}");
        }
    }

    #[test]
    fn host_port_followed_by_query_is_not_a_credential() {
        for text in [
            "https://host:443?email=a@b.com",
            "http://localhost:3000?email=me@x.com",
        ] {
            assert_matches!(
                scrub_secrets(text),
                Cow::Borrowed(_),
                "{text} -> {}",
                scrub_secrets(text)
            );
        }
    }

    #[test]
    fn compact_json_with_url_and_email_is_not_mangled() {
        let text = r#"{"u":"https://example.com","e":"alice@x.com"}"#;
        assert_matches!(scrub_secrets(text), Cow::Borrowed(_));
        let authors = r#"{"homepage":"https://example.com","authors":["a@x.org","b@y.org"]}"#;
        assert_matches!(scrub_secrets(authors), Cow::Borrowed(_));
        assert_eq!(
            scrub_secrets("redis://:hunter2@host"),
            "redis://[REDACTED]@host"
        );
        let long = r#"{"u":"https://example.com","e":"alice.longname.abcdefgh@x.com"}"#;
        assert_matches!(scrub_secrets(long), Cow::Borrowed(_));
    }

    #[test]
    fn url_colonless_opaque_userinfo_is_redacted() {
        let token = "abcdefghijklmnopqrstuvwxyz0123";
        let text = format!("https://{token}@host/x");
        assert_eq!(scrub_content(&text), "https://[REDACTED]@host/x");
        assert_eq!(
            scrub_secrets(&format!("https://{}@host", "a".repeat(20))),
            "https://[REDACTED]@host"
        );
    }

    #[test]
    fn url_non_credential_userinfo_stays_intact() {
        for text in [
            "https://user@host/x",
            "https://aaaaaaaaaaaaaaaaaaa@host",
            "ssh://git@github.com/org/repo",
            "git@github.com:org/repo.git",
            "mail bob@example.com about https://example.com/a@b",
            "https://host:8080/path@x",
            "https://host/a:b@c",
            "https://example.com?email=someone.with.a.long.name@example.com",
        ] {
            assert_matches!(
                scrub_secrets(text),
                Cow::Borrowed(_),
                "{text} -> {}",
                scrub_secrets(text)
            );
        }
    }

    #[test]
    fn multiple_urls_each_redacted_independently() {
        let result = scrub_secrets("a https://u:p@h1/x b https://v:q@h2/y c https://safe@h3");
        assert_eq!(
            result,
            "a https://[REDACTED]@h1/x b https://[REDACTED]@h2/y c https://safe@h3"
        );
    }

    #[test]
    fn clean_text_is_borrowed_by_every_layer() {
        let text = "nothing sensitive here";
        assert_matches!(scrub_secret_shapes(text), Cow::Borrowed(_));
        assert_matches!(scrub_secrets(text), Cow::Borrowed(_));
        assert_matches!(sanitize_paths(text), Cow::Borrowed(_));
        assert_matches!(scrub_content(text), Cow::Borrowed(_));
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
