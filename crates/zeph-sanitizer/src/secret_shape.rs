// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shape-based secret detection: redacts strings that *look like* a secret (known API-key
//! prefix, `Authorization: Bearer` header, standalone JWT) regardless of whether the value
//! was ever registered with a [`crate::secret_mask::SecretMaskRegistry`].
//!
//! [`SecretMaskRegistry`][crate::secret_mask::SecretMaskRegistry] only masks literal secret
//! *values* explicitly registered at runtime (actual vault-loaded secrets) — a string a
//! subagent invents, echoes, or fabricates in its own response text is invisible to it. This
//! module closes that gap by scanning for the same secret shapes `zeph-core::redact` already
//! scrubs from debug dumps and tool-execution output (issue #6571), using the canonical
//! prefix/pattern constants in
//! [`zeph_common::secrets`]. The pipeline itself lives in [`zeph_common::secret_redact`] and is
//! re-exported here.

pub use zeph_common::secret_redact::scrub_secret_shapes;

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn redacts_generic_api_key_shape() {
        let text = "the key is sk-test-abc123def456, use it wisely";
        let result = scrub_secret_shapes(text);
        assert!(!result.contains("sk-test-abc123def456"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_bearer_token() {
        let result =
            scrub_secret_shapes("Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.payload.signature");
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("eyJhbGciOiJSUzI1NiJ9"));
        assert!(result.contains("Authorization:"));
    }

    #[test]
    fn redacts_standalone_jwt() {
        let jwt = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyMTIzIn0.SflKxwRJSMeKKF2";
        let text = format!("token value: {jwt} was found");
        let result = scrub_secret_shapes(&text);
        assert!(result.contains("[REDACTED_JWT]"));
        assert!(!result.contains("eyJhbGci"));
    }

    #[test]
    fn preserves_normal_text() {
        let text = "This is a normal response with no secrets";
        let result = scrub_secret_shapes(text);
        assert_eq!(result, text);
        assert_matches!(result, Cow::Borrowed(_));
    }

    #[test]
    fn all_secret_prefixes_detected() {
        for prefix in zeph_common::secrets::SECRET_PREFIXES {
            let text = format!("token: {prefix}abc123");
            let result = scrub_secret_shapes(&text);
            assert!(result.contains("[REDACTED]"), "failed for prefix: {prefix}");
            assert!(!result.contains(*prefix), "prefix not redacted: {prefix}");
        }
    }

    #[test]
    fn preserves_words_that_merely_contain_a_prefix() {
        for text in [
            "run disk-usage now",
            "see disk-usage-table",
            "recipe-create-task-list",
            "a shelf_x item",
        ] {
            let result = scrub_secret_shapes(text);
            assert_eq!(result, text);
        }
    }

    #[test]
    fn redacts_prefix_after_boundary_or_escape() {
        for (text, kept) in [
            ("x sk-a1", "x "),
            ("KEY_sk-a1", "KEY_"),
            ("(sk-a1)", "("),
            ("=sk-a1", "="),
            (r#"{"k":"a\nsk-proj-1"}"#, r#"{"k":"a\n"#),
            (r"line\tghp_x1", r"line\t"),
            (r"a\x0ask-a1", r"a\x0a"),
            (r"a\u000ask-a1", r"a\u000a"),
            ("p%3Dsk-proj-1", "p%3D"),
        ] {
            let result = scrub_secret_shapes(text);
            assert!(result.contains("[REDACTED]"), "not redacted: {text}");
            assert!(
                result.starts_with(kept),
                "boundary lost: {text} -> {result}"
            );
            assert!(
                !result.contains("sk-") && !result.contains("ghp_"),
                "{text} -> {result}"
            );
        }
    }

    #[test]
    fn redacts_glued_prefix_with_key_shaped_body() {
        for text in [
            "\x1b[32msk-proj-0123456789abcdef\x1b[0m",
            r"\x1b[33msk-proj-0123456789abcdef",
            r"\u001b[1;31msk-proj-0123456789abcdef",
            r"\u{1b}[0msk-proj-0123456789abcdef",
            "P=3Dsk-proj-0123456789abcdef",
            "p%253Dsk-proj-0123456789abcdef",
            r"\075sk-proj-0123456789abcdef",
            r"\U0000000Ask-proj-0123456789abcdef",
            r"\ask-proj-0123456789abcdef",
            "token1sk-proj-0123456789abcdef",
            "id9ghp_0123456789abcdefghij",
        ] {
            let result = scrub_secret_shapes(text);
            assert!(result.contains("[REDACTED]"), "not redacted: {text}");
            assert!(!result.contains("0123456789"), "{text} -> {result}");
        }
    }

    #[test]
    fn glued_rejection_does_not_swallow_a_following_real_key() {
        for text in [
            "run-task-id=3Dsk-abc123def456ghij",
            "?next=%2Ftask-list%253Fp%253Dsk-abc123def456ghij",
            r"disk-a\075sk-abc123def456ghij",
            "/opt/task-runner/out.log:P=3Dsk-abc123def456ghij",
            "disk-a.bsk-abc123def456ghij",
        ] {
            let result = scrub_secret_shapes(text);
            assert!(!result.contains("abc123def456"), "{text} -> {result}");
        }
    }

    #[test]
    fn non_secret_glued_words_keep_the_zero_alloc_path() {
        for text in [
            "see disk-usage and task-list",
            "risk-2024-report",
            "whisk-proj-notes",
        ] {
            assert_matches!(
                scrub_secret_shapes(text),
                Cow::Borrowed(_),
                "{text} must not allocate"
            );
        }
    }

    #[test]
    fn keeps_hyphenated_identifiers_and_redacts_hyphenated_keys() {
        for text in [
            "task-runner-for-k8s-clusters",
            "disk-cleanup-utility-v2",
            "risk-assessment-2024-q3",
            "/var/cache/pnpm_store_v3_linux_x64_node20",
        ] {
            assert_matches!(scrub_secret_shapes(text), Cow::Borrowed(_), "{text}");
        }
        for text in [
            "x9sk-ant-api03-aB3dEf_gH1jKlMnOpQrStUvWxYz-0123456789",
            "id9sk-proj-abc_def-0123456789abcdef_tail",
        ] {
            let result = scrub_secret_shapes(text);
            assert!(!result.contains("0123456789"), "{text} -> {result}");
        }
    }

    #[test]
    fn rejected_glued_candidates_never_hide_a_later_key() {
        for text in [
            "run-task-id=3Dsk-abc123def456ghij",
            "?next=%2Ftask-list%253Fp%253Dsk-abc123def456ghij",
            r"disk-a\075sk-abc123def456ghij",
            "/opt/task-runner/out.log:P=3Dsk-abc123def456ghij",
            "disk-a.bsk-abc123def456ghij",
            "flask-app=3Dsk-abc123def456ghij",
            "/disk-usage/x9sk-abc123def456ghij",
        ] {
            let result = scrub_secret_shapes(text);
            assert!(!result.contains("abc123def456"), "{text} -> {result}");
        }
    }

    #[test]
    fn keeps_glued_prefix_in_ordinary_words() {
        for text in [
            "disk-usage",
            "disk-usage-table",
            "recipe-create-task-list",
            "risk-2024-report",
            "whisk-proj-handover-notes",
            "abcsk-x",
            "shelf_x",
        ] {
            assert_eq!(scrub_secret_shapes(text), text);
        }
    }

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn body(&mut self, alphabet: &[u8], len: usize) -> String {
            (0..len)
                .map(|_| {
                    alphabet[usize::try_from(
                        self.next() % u64::try_from(alphabet.len()).unwrap_or(1),
                    )
                    .unwrap_or(0)] as char
                })
                .collect()
        }
    }

    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    const ALNUM_SEP: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    const BASE32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

    #[test]
    fn generated_keys_of_listed_formats_are_redacted_after_every_glue() {
        let glue = ["\\x1b[32m", "=3D", "%253D", "\\075", "\\ask", "a"];
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let formats: [(&str, &[u8], usize); 9] = [
            ("hf_", LETTERS, 34),
            ("glpat-", ALNUM_SEP, 20),
            ("dckr_pat_", ALNUM_SEP, 27),
            ("AIza", ALNUM_SEP, 35),
            ("AKIA", BASE32, 16),
            ("npm_", ALNUM, 36),
            ("ghp_", ALNUM, 36),
            ("gho_", ALNUM, 36),
            ("sk_live_", ALNUM, 24),
        ];
        for (prefix, alphabet, len) in formats {
            for i in 0..2000 {
                let body = rng.body(alphabet, len);
                let text = format!("{}{prefix}{body}", glue[i % glue.len()]);
                let result = scrub_secret_shapes(&text);
                assert!(!result.contains(&body), "{prefix}: {text} -> {result}");
            }
        }
    }

    #[test]
    fn generated_github_fine_grained_tokens_are_redacted_for_any_version_and_glue() {
        let glue = ["", "token: ", "\\x1b[32m", "=3D", "%253D", "a"];
        let mut rng = XorShift(0x0F1E_2D3C_4B5A_6978);
        for i in 0..2000 {
            let (head, tail) = (rng.body(ALNUM, 22), rng.body(ALNUM, 59));
            let text = format!("{}github_pat_{head}_{tail}", glue[i % glue.len()]);
            let result = scrub_secret_shapes(&text);
            assert!(
                !result.contains(&head) && !result.contains(&tail),
                "{text} -> {result}"
            );
        }
    }

    #[test]
    fn glued_ordinary_hyphenated_identifiers_stay_intact() {
        for text in [
            "task-runner-for-k8s-clusters",
            "ask-rotate-credentials-2024-quarterly",
            "mask-rcnn-resnet50-fpn-v2-coco-weights",
            "flask-session-management-and-cookies",
            "pnpm_store_v3_linux_x64_node20",
            "chf_report_generation_pipeline_stage",
            "xAKIAshort",
            "xglpat-short",
        ] {
            assert_eq!(scrub_secret_shapes(text), text);
        }
    }

    #[test]
    fn lowercase_snake_identifiers_with_a_distinctive_prefix_stay_intact() {
        for text in [
            "disk_test_results_summary_report",
            "run_task_test_with_retries_enabled",
            "risk_live_dashboard_refresh_interval",
            "mygithub_pat_rotation_schedule_doc",
            "aya29.something_long_identifier_name",
            "load_github_pat_from_env()",
            "the github_pat_ prefix",
            "/docs/flask-sqlalchemyintegrationguideforbeginners",
            "chf_conversionratesfromswissnationalbankfeed",
        ] {
            assert_eq!(scrub_secret_shapes(text), text);
        }
    }

    #[test]
    fn glued_distinctive_key_nested_in_a_rejected_ambiguous_run_is_redacted() {
        let mut rng = XorShift(0x1234_5678_9ABC_DEF1);
        for i in 0..2000 {
            let glpat = rng.body(ALNUM_SEP, 20);
            let dckr = rng.body(ALNUM_SEP, 27);
            for (text, body) in [
                (format!("ask-ab_cglpat-{glpat}"), glpat),
                (format!("9hf_xdckr_pat_{dckr}"), dckr),
            ] {
                let result = scrub_secret_shapes(&text);
                assert!(!result.contains(&body), "{i}: {text} -> {result}");
            }
        }
    }

    #[test]
    fn glued_key_length_boundaries() {
        let distinctive = |len: usize| format!("xglpat-aB{}", "d".repeat(len - 2));
        assert_eq!(
            scrub_secret_shapes(&distinctive(15)),
            distinctive(15),
            "15 chars"
        );
        assert!(scrub_secret_shapes(&distinctive(16)).contains("[REDACTED]"));

        let digit = |len: usize| format!("xsk-a1{}", "d".repeat(len - 2));
        assert_eq!(scrub_secret_shapes(&digit(15)), digit(15), "15 chars");
        assert!(scrub_secret_shapes(&digit(16)).contains("[REDACTED]"));

        let mixed = |len: usize| format!("xhf_A{}", "b".repeat(len - 1));
        assert_eq!(scrub_secret_shapes(&mixed(29)), mixed(29), "29 chars");
        assert!(scrub_secret_shapes(&mixed(30)).contains("[REDACTED]"));
        let lower_30 = format!("xhf_{}", "b".repeat(30));
        assert_eq!(scrub_secret_shapes(&lower_30), lower_30);
    }

    #[test]
    fn redacts_full_pem_block() {
        let text = "here is my key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG9w0B\nAQEFAASCAT8wggE7AgEAAkEA\n-----END RSA PRIVATE KEY-----\nthanks";
        let result = scrub_secret_shapes(text);
        assert!(result.contains("[REDACTED_PEM_KEY]"));
        assert!(!result.contains("MIIBVQIBADANBgkqhkiG9w0B"));
        assert!(result.contains("here is my key:"));
        assert!(result.contains("thanks"));
    }

    #[test]
    fn redacts_multiple_pem_blocks_independently() {
        let text = "-----BEGIN PRIVATE KEY-----\nfirstbody\n-----END PRIVATE KEY-----\nsome text in between\n-----BEGIN EC PRIVATE KEY-----\nsecondbody\n-----END EC PRIVATE KEY-----";
        let result = scrub_secret_shapes(text);
        assert_eq!(result.matches("[REDACTED_PEM_KEY]").count(), 2);
        assert!(!result.contains("firstbody"));
        assert!(!result.contains("secondbody"));
        assert!(result.contains("some text in between"));
    }

    #[test]
    fn no_false_positive_on_bare_begin_or_private_words() {
        let text = "Let's BEGIN the PRIVATE discussion about keys without any PEM markers";
        let result = scrub_secret_shapes(text);
        assert_eq!(result, text);
        assert_matches!(result, Cow::Borrowed(_));
    }

    #[test]
    fn redacts_pem_block_with_mismatched_header_footer_labels() {
        // Documented over-match tradeoff: `regex` has no backreferences, so a footer whose
        // label doesn't match the header's label still gets redacted (over-matching is
        // acceptable; under-matching would leak key material).
        let text = "-----BEGIN RSA PRIVATE KEY-----\nbody-material\n-----END EC PRIVATE KEY-----";
        let result = scrub_secret_shapes(text);
        assert!(result.contains("[REDACTED_PEM_KEY]"));
        assert!(!result.contains("body-material"));
    }

    #[test]
    fn redacts_footerless_pem_header_via_fallback() {
        // C2: a PEM header with no matching footer at all (truncated input, adversarially
        // omitted, or a footer chunk dropped by a bounded channel) must still be redacted —
        // not just the header token, leaving the whole body exposed.
        let text = "here is my key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG9w0B\nAQEFAASCAT8wggE7AgEAAkEA";
        let result = scrub_secret_shapes(text);
        assert!(result.contains("[REDACTED_PEM_KEY]"));
        assert!(!result.contains("MIIBVQIBADANBgkqhkiG9w0B"));
        assert!(!result.contains("-----BEGIN"));
        assert!(result.contains("here is my key:"));
    }

    #[test]
    fn adversarial_repeated_unterminated_pem_headers_are_bounded() {
        // M3 / anti-censorship guard: a subagent wrapping arbitrary content behind a forged
        // or genuinely unterminated header must not be able to make an unbounded amount of it
        // vanish — only up to PEM_BODY_CAP characters past each header are redacted; content
        // beyond that cap must remain visible rather than being silently swallowed.
        use zeph_common::secrets::PEM_BODY_CAP;

        let huge_body = "A".repeat(50_000);
        let text = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{huge_body}\n-----BEGIN EC PRIVATE KEY-----\n{huge_body}"
        );
        let result = scrub_secret_shapes(&text);
        assert_eq!(result.matches("[REDACTED_PEM_KEY]").count(), 2);
        // Each huge_body is far larger than PEM_BODY_CAP, so most of its 'A' characters must
        // still be present in the output — proving the fallback did not swallow it whole.
        let remaining_as = result.matches('A').count();
        assert!(
            remaining_as > huge_body.len() - PEM_BODY_CAP,
            "expected most of the oversized body to remain visible past the cap, got only \
             {remaining_as} 'A' chars remaining"
        );
    }

    #[test]
    fn redacts_marker_anchored_aws_secret_key() {
        let text = "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let result = scrub_secret_shapes(text);
        assert!(!result.contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
        assert!(result.contains("[REDACTED]"));
        assert!(result.contains("aws_secret_access_key="));
    }

    #[test]
    fn preserves_unanchored_high_entropy_string() {
        // Same 40-char base64-ish shape as a real AWS secret key, but with no marker nearby —
        // must not be flagged, or ordinary hashes/IDs would be false positives.
        let text = "build hash: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY was produced";
        let result = scrub_secret_shapes(text);
        assert_eq!(result, text);
    }
}
