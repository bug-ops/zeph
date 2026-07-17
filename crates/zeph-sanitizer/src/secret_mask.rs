// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! PAAC typed-placeholder secret masking registry.
//!
//! Prevents vault-resolved secrets from appearing in LLM payloads by substituting them
//! with opaque, session-scoped placeholder tokens before context assembly.
//! Placeholders are reversed only at the tool-execution boundary.
//!
//! # Security model
//!
//! - Placeholder format: `<SECRET:category:NONCE_HEX:INDEX>` — the per-session nonce makes
//!   placeholders unguessable by an adversary, preventing placeholder-injection attacks.
//! - Secrets shorter than [`MIN_SECRET_LEN`] bytes are not masked (avoids false matches on
//!   common short strings).
//! - Replacement iterates secrets sorted by length descending (longest first) to prevent
//!   substring collision when one secret is a prefix of another.
//!
//! # Examples
//!
//! ```rust
//! use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};
//!
//! let registry = SecretMaskRegistry::new();
//! registry.register("ZEPH_OPENAI_API_KEY", "sk-supersecretvalue12345678", SecretCategory::ApiKey);
//!
//! let masked = registry.mask("Using key sk-supersecretvalue12345678 here");
//! assert!(!masked.contains("sk-supersecretvalue12345678"));
//! assert!(masked.contains("<SECRET:api_key:"));
//!
//! let unmasked = registry.unmask(&masked);
//! assert!(unmasked.contains("sk-supersecretvalue12345678"));
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;
use rand::TryRng;
use rand::rngs::SysRng;

/// Minimum byte length a secret value must have to be registered for masking.
///
/// Shorter values are not masked because they are more likely to appear as substrings
/// of legitimate content, causing false substitutions.
pub const MIN_SECRET_LEN: usize = 8;

/// Category of a masked secret, encoded in the placeholder token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecretCategory {
    /// API key (vault key name contains `api_key` or `apikey`).
    ApiKey,
    /// Bearer or session token.
    Token,
    /// Password or passphrase.
    Password,
    /// TLS certificate or private key.
    Certificate,
    /// Webhook URL or secret.
    Webhook,
    /// Anything that does not fit a more specific category.
    Generic,
}

impl SecretCategory {
    /// Infer category from a vault key name.
    ///
    /// Matching is case-insensitive and performed on the lowercased key name.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::secret_mask::SecretCategory;
    ///
    /// assert_eq!(SecretCategory::from_key_name("ZEPH_OPENAI_API_KEY"), SecretCategory::ApiKey);
    /// assert_eq!(SecretCategory::from_key_name("TELEGRAM_TOKEN"), SecretCategory::Token);
    /// assert_eq!(SecretCategory::from_key_name("DB_PASSWORD"), SecretCategory::Password);
    /// assert_eq!(SecretCategory::from_key_name("SOME_CERT"), SecretCategory::Certificate);
    /// assert_eq!(SecretCategory::from_key_name("SLACK_WEBHOOK"), SecretCategory::Webhook);
    /// assert_eq!(SecretCategory::from_key_name("SOMETHING_ELSE"), SecretCategory::Generic);
    /// ```
    #[must_use]
    pub fn from_key_name(key: &str) -> Self {
        let lower = key.to_ascii_lowercase();
        if lower.contains("api_key") || lower.contains("apikey") {
            Self::ApiKey
        } else if lower.contains("token") {
            Self::Token
        } else if lower.contains("password") || lower.contains("passwd") {
            Self::Password
        } else if lower.contains("cert") {
            Self::Certificate
        } else if lower.contains("webhook") {
            Self::Webhook
        } else {
            Self::Generic
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Token => "token",
            Self::Password => "password",
            Self::Certificate => "certificate",
            Self::Webhook => "webhook",
            Self::Generic => "generic",
        }
    }
}

/// Per-session registry mapping vault secret values to typed placeholder tokens.
///
/// Thread-safe via `parking_lot::RwLock`. Designed to be wrapped in `Arc` and shared
/// across the agent session. Cleared on agent restart — never persisted to disk.
///
/// # Placeholder format
///
/// `<SECRET:{category}:{nonce_hex}:{index}>` where:
/// - `category` is the lowercase [`SecretCategory`] name.
/// - `nonce_hex` is a random 16-character hex string generated once at registry creation.
/// - `index` is a per-registry monotonic counter.
///
/// The nonce makes placeholders unguessable, preventing an attacker from injecting a
/// crafted placeholder into tool output to trigger secret exfiltration via `unmask`.
#[derive(Default)]
pub struct SecretMaskRegistry {
    /// `secret_value` → placeholder
    forward: RwLock<HashMap<String, String>>,
    /// `placeholder` → `secret_value`  (reverse mapping for unmask)
    reverse: RwLock<HashMap<String, String>>,
    /// Pre-sorted pairs `(secret, placeholder)` by secret length descending, cached for `mask()`.
    sorted_pairs: RwLock<Vec<(String, String)>>,
    /// Per-session random nonce (16 hex chars, generated at construction).
    nonce: String,
    /// Monotonic counter for unique placeholder indexes (lock-free).
    counter: AtomicUsize,
}

impl std::fmt::Debug for SecretMaskRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretMaskRegistry")
            .field("nonce", &self.nonce)
            .field("entries", &self.forward.read().len())
            .finish_non_exhaustive()
    }
}

impl SecretMaskRegistry {
    /// Create a new registry with a freshly generated session nonce.
    ///
    /// # Panics
    ///
    /// Panics if the OS entropy source is unavailable to seed the nonce — this would
    /// indicate a broken host environment, not a recoverable application error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::secret_mask::SecretMaskRegistry;
    ///
    /// let r1 = SecretMaskRegistry::new();
    /// let r2 = SecretMaskRegistry::new();
    /// // Each registry has a unique nonce — placeholders from different sessions never collide.
    /// ```
    #[must_use]
    pub fn new() -> Self {
        // Explicit OS-backed CSPRNG, rather than `rand::random`'s default thread-local
        // generator, so the nonce's unguessability requirement is self-evident here.
        let nonce: u64 = SysRng
            .try_next_u64()
            .expect("OS entropy source must be available to generate the secret-mask nonce");
        let nonce = format!("{nonce:016x}");
        Self {
            forward: RwLock::new(HashMap::new()),
            reverse: RwLock::new(HashMap::new()),
            sorted_pairs: RwLock::new(Vec::new()),
            nonce,
            counter: AtomicUsize::new(0),
        }
    }

    /// Register a vault-resolved secret for masking.
    ///
    /// Secrets shorter than [`MIN_SECRET_LEN`] bytes are silently ignored.
    /// If the same secret value is registered again (possibly under a different key
    /// name), the existing placeholder is reused — no duplicate entries are created.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};
    ///
    /// let registry = SecretMaskRegistry::new();
    /// registry.register("MY_KEY", "secret-value-here", SecretCategory::ApiKey);
    /// let masked = registry.mask("value is secret-value-here");
    /// assert!(!masked.contains("secret-value-here"));
    /// ```
    pub fn register(&self, _key_name: &str, secret_value: &str, category: SecretCategory) {
        if secret_value.len() < MIN_SECRET_LEN {
            return;
        }
        // Hold the write lock for the entire check-then-insert sequence to prevent a
        // TOCTOU race where two concurrent callers both pass the contains_key check and
        // produce duplicate placeholders for the same secret.
        let mut forward = self.forward.write();
        if forward.contains_key(secret_value) {
            return;
        }
        let index = self.counter.fetch_add(1, Ordering::Relaxed);
        let placeholder = format!("<SECRET:{}:{}:{}>", category.as_str(), self.nonce, index);
        // Acquire sorted_pairs BEFORE inserting into forward so mask() never observes
        // a state where forward contains the secret but sorted_pairs does not.
        let mut pairs = self.sorted_pairs.write();
        forward.insert(secret_value.to_owned(), placeholder.clone());
        self.reverse
            .write()
            .insert(placeholder.clone(), secret_value.to_owned());
        pairs.push((secret_value.to_owned(), placeholder));
        pairs.sort_unstable_by_key(|(s, _)| std::cmp::Reverse(s.len()));
    }

    /// Replace all registered secret values in `text` with their placeholder tokens.
    ///
    /// Replacement is performed longest-secret-first to prevent substring collision
    /// (e.g. if secret A is a prefix of secret B, B is replaced before A).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};
    ///
    /// let registry = SecretMaskRegistry::new();
    /// registry.register("SHORT", "abcdefghij", SecretCategory::Token);
    /// registry.register("LONG", "abcdefghijklmno", SecretCategory::Token);
    ///
    /// let text = "prefix abcdefghijklmno suffix";
    /// let masked = registry.mask(text);
    /// // Longer secret replaced first — no leftover 'abcdefghij' fragment.
    /// assert!(!masked.contains("abcdefghij"));
    /// ```
    #[must_use]
    pub fn mask(&self, text: &str) -> String {
        let pairs = self.sorted_pairs.read();
        if pairs.is_empty() {
            return text.to_owned();
        }
        let mut result = text.to_owned();
        for (secret, placeholder) in pairs.iter() {
            result = result.replace(secret.as_str(), placeholder.as_str());
        }
        result
    }

    /// Return `true` if any registered secret appears verbatim in `text`.
    ///
    /// A cheap, allocation-free pre-check (no `String` construction, unlike [`Self::mask`]).
    /// Callers on a hot path should use this to decide whether the more expensive clone-and-mask
    /// pass over a batch of text is needed at all, instead of unconditionally cloning and masking
    /// every candidate — most turns contain no registered secret.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};
    ///
    /// let registry = SecretMaskRegistry::new();
    /// registry.register("KEY", "supersecretvalue1", SecretCategory::ApiKey);
    ///
    /// assert!(registry.would_mask("value is supersecretvalue1"));
    /// assert!(!registry.would_mask("nothing sensitive here"));
    /// ```
    #[must_use]
    pub fn would_mask(&self, text: &str) -> bool {
        let pairs = self.sorted_pairs.read();
        pairs.iter().any(|(secret, _)| text.contains(secret))
    }

    /// Replace all placeholder tokens in `text` with their original secret values.
    ///
    /// Used only at the tool-execution boundary (shell commands, HTTP headers).
    /// This method is infallible — on any lookup miss the placeholder is left as-is
    /// (never panics, never produces partial output).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};
    ///
    /// let registry = SecretMaskRegistry::new();
    /// registry.register("MY_KEY", "mysecret12345678", SecretCategory::ApiKey);
    ///
    /// let text = "value is mysecret12345678";
    /// let masked = registry.mask(text);
    /// let restored = registry.unmask(&masked);
    /// assert_eq!(restored, text);
    /// ```
    #[must_use]
    pub fn unmask(&self, text: &str) -> String {
        let reverse = self.reverse.read();
        if reverse.is_empty() {
            return text.to_owned();
        }
        let mut result = text.to_owned();
        for (placeholder, secret) in reverse.iter() {
            result = result.replace(placeholder.as_str(), secret.as_str());
        }
        result
    }

    /// Return the number of registered secrets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.read().len()
    }

    /// Return `true` when no secrets are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.read().is_empty()
    }

    /// Return the per-session nonce embedded in all placeholders from this registry.
    ///
    /// Exposed for testing nonce uniqueness across registry instances.
    #[must_use]
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
}

impl zeph_llm::masking::OutboundMasker for SecretMaskRegistry {
    /// Adapts [`SecretMaskRegistry::mask`] to the provider-boundary [`zeph_llm::masking::OutboundMasker`]
    /// capability (#5437) — this is what lets `AnyProvider::masked` wrap a live provider with
    /// this registry via `Arc<dyn OutboundMasker>`, without `zeph-llm` depending on
    /// `zeph-sanitizer` (masking is applied structurally at the provider boundary, so every
    /// `chat`/`chat_with_tools`/`chat_stream` call is covered by construction, not per-call-site
    /// enumeration).
    fn mask(&self, text: &str) -> Option<String> {
        if self.would_mask(text) {
            Some(self.mask(text))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(secrets: &[(&str, &str, SecretCategory)]) -> SecretMaskRegistry {
        let r = SecretMaskRegistry::new();
        for (key, val, cat) in secrets {
            r.register(key, val, *cat);
        }
        r
    }

    // --- category inference ---

    #[test]
    fn category_from_key_name_api_key() {
        assert_eq!(
            SecretCategory::from_key_name("ZEPH_OPENAI_API_KEY"),
            SecretCategory::ApiKey
        );
        assert_eq!(
            SecretCategory::from_key_name("APIKEY_STRIPE"),
            SecretCategory::ApiKey
        );
    }

    #[test]
    fn category_from_key_name_token() {
        assert_eq!(
            SecretCategory::from_key_name("TELEGRAM_BOT_TOKEN"),
            SecretCategory::Token
        );
    }

    #[test]
    fn category_from_key_name_password() {
        assert_eq!(
            SecretCategory::from_key_name("DB_PASSWORD"),
            SecretCategory::Password
        );
        assert_eq!(
            SecretCategory::from_key_name("REDIS_PASSWD"),
            SecretCategory::Password
        );
    }

    #[test]
    fn category_from_key_name_generic() {
        assert_eq!(
            SecretCategory::from_key_name("SOMETHING_RANDOM"),
            SecretCategory::Generic
        );
    }

    // --- min length threshold ---

    #[test]
    fn short_secret_below_min_len_not_registered() {
        let r = SecretMaskRegistry::new();
        r.register("K", "short", SecretCategory::Generic); // len=5 < 8
        assert!(
            r.is_empty(),
            "secret shorter than MIN_SECRET_LEN must be ignored"
        );
    }

    #[test]
    fn secret_exactly_at_min_len_is_registered() {
        let r = SecretMaskRegistry::new();
        r.register("K", "12345678", SecretCategory::Generic); // len=8 == MIN_SECRET_LEN
        assert_eq!(r.len(), 1);
    }

    // --- mask / unmask round-trip ---

    #[test]
    fn mask_unmask_roundtrip() {
        let r = registry_with(&[("KEY", "mysecretvalue!!", SecretCategory::ApiKey)]);
        let original = "Authorization: Bearer mysecretvalue!!";
        let masked = r.mask(original);
        assert!(
            !masked.contains("mysecretvalue!!"),
            "secret must not appear in masked output"
        );
        assert!(
            masked.contains("<SECRET:api_key:"),
            "placeholder prefix must appear"
        );
        let restored = r.unmask(&masked);
        assert_eq!(restored, original, "unmask must restore original text");
    }

    #[test]
    fn mask_text_without_secret_unchanged() {
        let r = registry_with(&[("KEY", "mysecretvalue!!", SecretCategory::ApiKey)]);
        let text = "no secrets here at all";
        assert_eq!(r.mask(text), text);
    }

    #[test]
    fn unmask_text_without_placeholder_unchanged() {
        let r = registry_with(&[("KEY", "mysecretvalue!!", SecretCategory::ApiKey)]);
        let text = "no placeholders here";
        assert_eq!(r.unmask(text), text);
    }

    // --- nonce uniqueness ---

    #[test]
    fn nonce_unique_across_registries() {
        // Create many registries — with a 64-bit random nonce the probability of any
        // collision across 1000 instances is negligible (birthday bound ~2^32).
        let nonces: Vec<String> = (0..20)
            .map(|_| SecretMaskRegistry::new().nonce().to_owned())
            .collect();
        // Every nonce must be non-empty and 16 hex chars.
        for n in &nonces {
            assert_eq!(n.len(), 16, "nonce must be 16 hex chars");
            assert!(
                n.chars().all(|c| c.is_ascii_hexdigit()),
                "nonce must be hex"
            );
        }
        // Check all 20 are pairwise distinct (extremely unlikely to collide with rand u64).
        let unique: std::collections::HashSet<&str> = nonces.iter().map(String::as_str).collect();
        assert_eq!(
            unique.len(),
            nonces.len(),
            "all registry nonces must be distinct"
        );
    }

    #[test]
    fn placeholder_contains_nonce() {
        let r = SecretMaskRegistry::new();
        r.register("KEY", "secretpassword1", SecretCategory::Password);
        let masked = r.mask("secretpassword1");
        assert!(
            masked.contains(r.nonce()),
            "placeholder must embed the session nonce"
        );
    }

    // --- sort by length (substring collision prevention) ---

    #[test]
    fn longer_secret_replaced_before_shorter_prefix() {
        let r = SecretMaskRegistry::new();
        r.register("SHORT", "abcdefgh", SecretCategory::Generic);
        r.register("LONG", "abcdefghijklmnop", SecretCategory::Generic);

        let text = "value: abcdefghijklmnop extra";
        let masked = r.mask(text);
        // After masking the long secret, the short prefix must also be gone.
        assert!(
            !masked.contains("abcdefgh"),
            "no secret fragment must remain after mask"
        );
    }

    // --- duplicate secret value ---

    #[test]
    fn duplicate_secret_value_reuses_placeholder() {
        let r = SecretMaskRegistry::new();
        r.register("KEY1", "shared-secret-abc", SecretCategory::Token);
        r.register("KEY2", "shared-secret-abc", SecretCategory::Token); // same value
        assert_eq!(
            r.len(),
            1,
            "duplicate secret value must not create a second entry"
        );
    }

    // --- empty registry ---

    #[test]
    fn empty_registry_mask_is_identity() {
        let r = SecretMaskRegistry::new();
        assert_eq!(r.mask("any text"), "any text");
        assert_eq!(r.unmask("any text"), "any text");
    }

    // --- would_mask cheap pre-check ---

    #[test]
    fn would_mask_true_when_secret_present() {
        let r = registry_with(&[("KEY", "supersecretvalue1", SecretCategory::ApiKey)]);
        assert!(r.would_mask("prefix supersecretvalue1 suffix"));
    }

    #[test]
    fn would_mask_false_when_no_secret_present() {
        let r = registry_with(&[("KEY", "supersecretvalue1", SecretCategory::ApiKey)]);
        assert!(!r.would_mask("nothing sensitive in this text"));
    }

    #[test]
    fn would_mask_false_on_empty_registry() {
        let r = SecretMaskRegistry::new();
        assert!(!r.would_mask("supersecretvalue1"));
    }

    #[test]
    fn would_mask_matches_mask_outcome() {
        // would_mask must never disagree with whether mask() actually changes the text.
        let r = registry_with(&[("KEY", "supersecretvalue1", SecretCategory::ApiKey)]);
        for text in ["supersecretvalue1 here", "nothing here", ""] {
            let predicted = r.would_mask(text);
            let actual_changed = r.mask(text) != text;
            assert_eq!(predicted, actual_changed, "mismatch for text: {text:?}");
        }
    }

    // --- TOCTOU regression (#4280): after register() returns, concurrent mask() must never
    // expose the raw secret — tests the atomic sorted_pairs+forward critical section.
    #[test]
    fn concurrent_register_and_mask_never_expose_raw_secret() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let registry = Arc::new(SecretMaskRegistry::new());
        let secret = "super-secret-value-9999";
        let text = format!("token={secret} end");

        // Pre-register so the secret is fully committed before concurrent access starts.
        registry.register("KEY", secret, SecretCategory::ApiKey);

        // Barrier: both threads start simultaneously to stress-test concurrent mask() reads
        // and the dedup re-registration path (which also touches sorted_pairs).
        let barrier = Arc::new(Barrier::new(2));
        let iterations = 2_000;

        let reg_clone = Arc::clone(&registry);
        let barrier_clone = Arc::clone(&barrier);
        // Thread 1: repeatedly re-registers the same secret (hits the dedup early-return path,
        // which still acquires forward.write() and must not corrupt sorted_pairs).
        let register_thread = thread::spawn(move || {
            barrier_clone.wait();
            for _ in 0..iterations {
                reg_clone.register("KEY", secret, SecretCategory::ApiKey);
            }
        });

        // Thread 2: calls mask() — after the pre-registration above, the secret must always
        // be masked regardless of concurrent dedup calls in thread 1.
        barrier.wait();
        for _ in 0..iterations {
            let masked = registry.mask(&text);
            assert!(
                !masked.contains(secret),
                "raw secret must not appear in masked output: {masked}"
            );
        }

        register_thread.join().expect("register thread panicked");
    }

    // --- cross-registry isolation: placeholder from r2 is opaque to r1 ---

    #[test]
    fn cross_registry_unmask_isolation() {
        let r1 = SecretMaskRegistry::new();
        r1.register("K1", "secret-alpha-xyz1", SecretCategory::ApiKey);

        let r2 = SecretMaskRegistry::new();
        r2.register("K2", "secret-beta-abc99", SecretCategory::Token);

        // Mask with r2 to get a placeholder that contains r2's nonce.
        let masked_by_r2 = r2.mask("value secret-beta-abc99 end");
        assert!(!masked_by_r2.contains("secret-beta-abc99"));

        // r1 does not know r2's secrets or placeholders — must pass through unchanged.
        let result = r1.unmask(&masked_by_r2);
        assert_eq!(
            result, masked_by_r2,
            "r1 must not unmask a placeholder it never registered (nonce isolation)"
        );

        // r2 must correctly unmask its own placeholder.
        let restored = r2.unmask(&masked_by_r2);
        assert!(
            restored.contains("secret-beta-abc99"),
            "r2 must unmask its own placeholder"
        );
    }
}
