// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

use serde::Deserialize;
use zeroize::Zeroizing;

/// Wrapper for sensitive strings with redacted Debug/Display.
///
/// The inner value is wrapped in [`Zeroizing`] which overwrites the memory on drop.
/// `Clone` is intentionally not derived — secrets must be explicitly duplicated via
/// `Secret::new(existing.expose().to_owned())`.
///
/// # Clone is not implemented
///
/// ```compile_fail
/// use zeph_common::secret::Secret;
/// let s = Secret::new("x");
/// let _ = s.clone(); // must not compile — Secret intentionally does not implement Clone
/// ```
#[derive(Deserialize)]
#[serde(transparent)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Create a new secret from a string-like value.
    ///
    /// The inner string is wrapped in [`Zeroizing`], which overwrites the memory when the
    /// secret is dropped. This constructor is marked `#[must_use]` to encourage explicit
    /// handling of the returned secret value rather than accidental discarding.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::secret::Secret;
    ///
    /// let secret = Secret::new("my_api_key");
    /// assert_eq!(secret.expose(), "my_api_key");
    /// // Memory is zeroized when secret is dropped
    /// ```
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(Zeroizing::new(s.into()))
    }

    /// Expose the inner secret string as a borrowed reference.
    ///
    /// Use this method to access the secret for API calls or comparisons. The reference
    /// is bounded by the secret's lifetime, so the underlying string cannot be dropped
    /// while the reference is in use. Note that the string itself is not zeroized on
    /// reference — zeroization occurs only when the containing [`Secret`] is dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::secret::Secret;
    ///
    /// let secret = Secret::new("password123");
    /// let exposed = secret.expose();
    /// println!("Length: {}", exposed.len());
    /// ```
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Error type for vault operations.
///
/// Returned by `VaultProvider::get_secret` on failure.
///
/// The `Backend(String)` variant is the escape hatch for third-party vault implementations:
/// format the underlying error into the `String` when no more specific variant applies.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("secret not found: {0}")]
    NotFound(String),
    /// Generic backend failure. Third-party vault implementors should use this variant
    /// to surface errors that do not fit `NotFound` or `Io`.
    #[error("vault backend error: {0}")]
    Backend(String),
    #[error("vault I/O error: {0}")]
    Io(#[from] std::io::Error),
}
