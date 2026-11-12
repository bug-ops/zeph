// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Strongly-typed identifiers and shared tool types across `zeph-*` crates.
//!
//! This module defines `ToolName`, `ProviderName`, `SkillName`, `SessionId`, and
//! `ToolDefinition` — types shared by multiple crates without creating cross-crate
//! dependencies.
//!
//! `ToolName`, `ProviderName`, `SkillName`, and `SessionId` use `#[serde(transparent)]`
//! for zero-cost serialization compatibility: the JSON wire format is unchanged relative
//! to plain `String` fields.

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Generates an `Arc<str>`-backed newtype with the shared trait surface used by
/// `ToolName` and `ProviderName`: `Default`, `Display`, `AsRef<str>`,
/// `Borrow<str>`, `From<&str>`, `From<String>`, `FromStr`, and both directions of
/// `PartialEq` against `str`/`&str`/`String`, plus `new`/`as_str` constructors.
///
/// `Borrow<str>` and `derive(Hash)` are kept consistent so instances can be used as
/// `HashMap` keys and looked up by `&str` without allocating.
macro_rules! arc_str_newtype {
    (
        $(#[$struct_doc:meta])*
        struct $name:ident;
        new_doc: $(#[$new_doc:meta])*
        as_str_doc: $(#[$as_str_doc:meta])*
        default_doc: $(#[$default_doc:meta])*
    ) => {
        $(#[$struct_doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Arc<str>);

        impl $name {
            $(#[$new_doc])*
            #[must_use]
            pub fn new(s: impl Into<Arc<str>>) -> Self {
                Self(s.into())
            }

            $(#[$as_str_doc])*
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            $(#[$default_doc])*
            fn default() -> Self {
                Self(Arc::from(""))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(Arc::from(s))
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(Arc::from(s.as_str()))
            }
        }

        impl FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self::from(s))
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0.as_ref() == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0.as_ref() == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.0.as_ref() == other.as_str()
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.0.as_ref()
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                self.as_str() == other.0.as_ref()
            }
        }
    };
}

arc_str_newtype!(
    /// Strongly-typed tool name label.
    ///
    /// `ToolName` identifies a tool by its canonical name (e.g., `"shell"`, `"web_scrape"`).
    /// It is produced by the LLM in JSON tool-use responses and matched against the registered
    /// tool registry at dispatch time.
    ///
    /// # Label semantics (not a validated reference)
    ///
    /// `ToolName` is an unvalidated label from untrusted input (LLM JSON). It does **not**
    /// guarantee that a tool with this name is registered. Validation happens downstream at
    /// tool dispatch, not at construction.
    ///
    /// # Inner type: `Arc<str>`
    ///
    /// The inner type is `Arc<str>`, not `String`. Tool names are cloned into multiple contexts
    /// (event channels, tracing spans, tool output structs) during a single tool execution.
    /// `Arc<str>` makes all clones O(1) vs O(n) for `String`. Use `.clone()` to duplicate
    /// a `ToolName` — it is cheap.
    ///
    /// # No `Deref<Target=str>`
    ///
    /// `ToolName` does **not** implement `Deref<Target=str>`. This prevents the `.to_owned()`
    /// footgun where muscle memory returns `String` instead of `ToolName`. Use `.as_str()` for
    /// explicit string conversion and `.clone()` to duplicate the `ToolName`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ToolName;
    ///
    /// let name = ToolName::new("shell");
    /// assert_eq!(name.as_str(), "shell");
    /// assert_eq!(name, "shell");
    ///
    /// // Clone is O(1) — Arc reference count increment only.
    /// let name2 = name.clone();
    /// assert_eq!(name, name2);
    /// ```
    struct ToolName;
    new_doc:
    /// Construct a `ToolName` from any value convertible to `Arc<str>`.
    ///
    /// This is the primary constructor. The name is accepted without validation — it is a
    /// label from the LLM wire or tool registry, not a proof of registration.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ToolName;
    ///
    /// let name = ToolName::new("shell");
    /// assert_eq!(name.as_str(), "shell");
    /// ```
    as_str_doc:
    /// Return the inner string slice.
    ///
    /// Prefer this over `Deref` (which is intentionally not implemented) when an `&str`
    /// reference is needed.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ToolName;
    ///
    /// let name = ToolName::new("web_scrape");
    /// assert_eq!(name.as_str(), "web_scrape");
    /// ```
    default_doc:
    /// Returns an empty `ToolName`.
    ///
    /// This implementation exists solely for `#[serde(default)]` on optional fields.
    /// Do not construct a `ToolName` with an empty string in application code.
);

arc_str_newtype!(
    /// Opaque tool-call identifier assigned by an LLM provider to a tool invocation.
    ///
    /// The id links a `ToolUse` request to its `ToolResult` in message history and on the
    /// provider wire. It is an opaque label, never parsed. Serialization is transparent, so
    /// stored history and provider payloads are byte-identical to a plain string.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ToolCallId;
    ///
    /// let id = ToolCallId::new("toolu_01");
    /// assert_eq!(id.as_str(), "toolu_01");
    /// assert_eq!(serde_json::to_string(&id).unwrap(), "\"toolu_01\"");
    /// ```
    struct ToolCallId;
    new_doc:
    /// Construct a `ToolCallId` from any value convertible to `Arc<str>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ToolCallId;
    ///
    /// assert_eq!(ToolCallId::new("call_1"), "call_1");
    /// ```
    as_str_doc:
    /// Return the inner string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ToolCallId;
    ///
    /// assert_eq!(ToolCallId::new("call_1").as_str(), "call_1");
    /// ```
    default_doc:
    /// Returns an empty id. Provided for the shared newtype trait surface; no field relies on
    /// it, and an empty id is never valid on the wire.
);

/// Error returned by [`ToolExecutionId::new`] when the supplied id is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("tool execution id must not be empty")]
pub struct EmptyToolExecutionId;

/// Agent-assigned correlation id for one tool execution.
///
/// Unlike [`ToolCallId`] (assigned by the LLM provider), this id is generated once by the
/// agent per tool call and carried through start, output and diff events so that channels
/// (TUI, ACP) can correlate them. It is never empty: the only constructors are
/// [`ToolExecutionId::generate`] and the validating [`ToolExecutionId::new`], and there is
/// no `Default` or infallible `From<&str>`. Serialization is transparent.
///
/// # Examples
///
/// ```
/// use zeph_common::ToolExecutionId;
///
/// let id = ToolExecutionId::generate();
/// assert!(!id.as_str().is_empty());
/// assert!(ToolExecutionId::new("").is_err());
/// assert_eq!(ToolExecutionId::new("exec_1").unwrap().as_str(), "exec_1");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ToolExecutionId(Arc<str>);

impl ToolExecutionId {
    /// Generate a fresh random id (UUID v4).
    #[must_use]
    pub fn generate() -> Self {
        Self(Arc::from(uuid::Uuid::new_v4().to_string()))
    }

    /// Wrap an externally supplied id (ACP/TUI boundaries, tests).
    ///
    /// # Errors
    ///
    /// Returns [`EmptyToolExecutionId`] when the id is empty.
    pub fn new(id: impl Into<Arc<str>>) -> Result<Self, EmptyToolExecutionId> {
        let id = id.into();
        if id.is_empty() {
            Err(EmptyToolExecutionId)
        } else {
            Ok(Self(id))
        }
    }

    /// Return the inner string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ToolExecutionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ── ProviderName ─────────────────────────────────────────────────────────────

arc_str_newtype!(
    /// Strongly-typed LLM provider name.
    ///
    /// `ProviderName` identifies a configured provider by its name field (e.g., `"fast"`,
    /// `"quality"`, `"ollama-local"`). Names come from `[[llm.providers]] name = "…"` in the
    /// TOML config; subsystems reference providers by this name via `*_provider` fields.
    ///
    /// # Inner type: `Arc<str>`
    ///
    /// The inner type is `Arc<str>`. Provider names are cloned widely across subsystem config
    /// structs, metric labels, and log spans. `Arc<str>` makes all clones O(1).
    ///
    /// # No `Deref<Target=str>`
    ///
    /// `ProviderName` does **not** implement `Deref<Target=str>`. Use `.as_str()` for explicit
    /// string conversion and `.clone()` to duplicate.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ProviderName;
    ///
    /// let name = ProviderName::new("fast");
    /// assert_eq!(name.as_str(), "fast");
    /// assert_eq!(name, "fast");
    ///
    /// // Clone is O(1) — Arc reference count increment only.
    /// let name2 = name.clone();
    /// assert_eq!(name, name2);
    /// ```
    struct ProviderName;
    new_doc:
    /// Construct a `ProviderName` from any value convertible to `Arc<str>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ProviderName;
    ///
    /// let name = ProviderName::new("quality");
    /// assert_eq!(name.as_str(), "quality");
    /// ```
    as_str_doc:
    /// Return the inner string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ProviderName;
    ///
    /// let name = ProviderName::new("ollama-local");
    /// assert_eq!(name.as_str(), "ollama-local");
    /// ```
    default_doc:
    /// Returns an empty `ProviderName`.
    ///
    /// Exists solely for `#[serde(default)]` on optional fields. Do not use in
    /// application code — an empty name will fail provider lookup.
);

impl ProviderName {
    /// Return `true` when this is the empty sentinel (use the primary provider).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ProviderName;
    ///
    /// assert!(ProviderName::default().is_empty());
    /// assert!(!ProviderName::new("fast").is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Return `Some(&str)` when non-empty, `None` for the empty sentinel.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::ProviderName;
    ///
    /// assert_eq!(ProviderName::default().as_non_empty(), None);
    /// assert_eq!(ProviderName::new("fast").as_non_empty(), Some("fast"));
    /// ```
    #[must_use]
    pub fn as_non_empty(&self) -> Option<&str> {
        if self.0.is_empty() {
            None
        } else {
            Some(&self.0)
        }
    }
}

// ── kebab-case identifiers ───────────────────────────────────────────────────

/// Rule violated by a string rejected as a kebab-case identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KebabRule {
    Length,
    Charset,
    EdgeHyphen,
    DoubleHyphen,
}

/// Check that `s` is 1..=`max_len` bytes of `[a-z0-9-]` with no leading, trailing or
/// doubled hyphen. Shared by [`SkillName`] and [`SkillCategory`].
fn check_kebab_id(s: &str, max_len: usize) -> Result<(), KebabRule> {
    if s.is_empty() || s.len() > max_len {
        return Err(KebabRule::Length);
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(KebabRule::Charset);
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err(KebabRule::EdgeHyphen);
    }
    if s.contains("--") {
        return Err(KebabRule::DoubleHyphen);
    }
    Ok(())
}

// ── SkillName ────────────────────────────────────────────────────────────────

/// Reason a string was rejected as a [`SkillName`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillNameError {
    /// Length is not within 1..=64 bytes.
    #[error("skill name must be 1-64 characters, got {0}")]
    Length(usize),
    /// Contains a character outside `[a-z0-9-]`.
    #[error("skill name must contain only lowercase letters, digits, and hyphens: {0:?}")]
    Charset(String),
    /// Starts or ends with a hyphen.
    #[error("skill name must not start or end with hyphen: {0:?}")]
    EdgeHyphen(String),
    /// Contains two consecutive hyphens.
    #[error("skill name must not contain consecutive hyphens: {0:?}")]
    DoubleHyphen(String),
}

/// Validated skill name identifier.
///
/// A `SkillName` is 1-64 bytes of `[a-z0-9-]`, with no leading, trailing or doubled
/// hyphen. Names come from `SKILL.md` frontmatter `name:` fields and are used at match
/// time, invocation routing, and telemetry. Invalid names are unrepresentable: the only
/// constructors are validating ([`SkillName::parse`], `TryFrom`, `FromStr`, serde).
///
/// The inner type is `Arc<str>` so clones are O(1). There is no `Deref<Target=str>`,
/// `Default` or infallible `From`; use [`SkillName::as_str`] for the string view.
///
/// # Examples
///
/// ```
/// use zeph_common::SkillName;
///
/// let name = SkillName::parse("rust-agents").unwrap();
/// assert_eq!(name.as_str(), "rust-agents");
/// assert_eq!(name, "rust-agents");
/// assert!(SkillName::parse("Bad Name").is_err());
/// assert!(SkillName::parse("").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillName(Arc<str>);

impl SkillName {
    /// Maximum length of a skill name in bytes.
    pub const MAX_LEN: usize = 64;

    /// Validate `s` and build a `SkillName`.
    ///
    /// # Errors
    ///
    /// Returns [`SkillNameError`] when `s` is empty, longer than 64 bytes, contains
    /// characters outside `[a-z0-9-]`, or has a leading, trailing or doubled hyphen.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::{SkillName, SkillNameError};
    ///
    /// assert!(SkillName::parse("readme-generator").is_ok());
    /// assert_eq!(SkillName::parse("a--b"), Err(SkillNameError::DoubleHyphen("a--b".into())));
    /// ```
    pub fn parse(s: &str) -> Result<Self, SkillNameError> {
        check_kebab_id(s, SkillName::MAX_LEN).map_err(|rule| match rule {
            KebabRule::Length => SkillNameError::Length(s.len()),
            KebabRule::Charset => SkillNameError::Charset(s.to_owned()),
            KebabRule::EdgeHyphen => SkillNameError::EdgeHyphen(s.to_owned()),
            KebabRule::DoubleHyphen => SkillNameError::DoubleHyphen(s.to_owned()),
        })?;
        Ok(Self(Arc::from(s)))
    }

    /// Return the inner string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::SkillName;
    ///
    /// assert_eq!(SkillName::parse("rust-agents").unwrap().as_str(), "rust-agents");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SkillName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SkillName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for SkillName {
    type Error = SkillNameError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl TryFrom<String> for SkillName {
    type Error = SkillNameError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<SkillName> for String {
    fn from(n: SkillName) -> Self {
        n.0.as_ref().to_owned()
    }
}

impl FromStr for SkillName {
    type Err = SkillNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl PartialEq<str> for SkillName {
    fn eq(&self, other: &str) -> bool {
        self.0.as_ref() == other
    }
}

impl PartialEq<&str> for SkillName {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_ref() == *other
    }
}

impl PartialEq<String> for SkillName {
    fn eq(&self, other: &String) -> bool {
        self.0.as_ref() == other.as_str()
    }
}

impl PartialEq<SkillName> for str {
    fn eq(&self, other: &SkillName) -> bool {
        self == other.0.as_ref()
    }
}

impl PartialEq<SkillName> for String {
    fn eq(&self, other: &SkillName) -> bool {
        self.as_str() == other.0.as_ref()
    }
}

// ── SkillCategory ────────────────────────────────────────────────────────────

/// Reason a string was rejected as a [`SkillCategory`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillCategoryError {
    /// Length is not within 1..=32 bytes.
    #[error("category must be 1-32 characters, got {0}")]
    Length(usize),
    /// Contains a character outside `[a-z0-9-]`.
    #[error("category must contain only lowercase letters, digits, and hyphens: {0:?}")]
    Charset(String),
    /// Starts or ends with a hyphen.
    #[error("category must not start or end with hyphen: {0:?}")]
    EdgeHyphen(String),
    /// Contains two consecutive hyphens.
    #[error("category must not contain consecutive hyphens: {0:?}")]
    DoubleHyphen(String),
}

/// Validated skill category, the optional `category:` frontmatter value.
///
/// 1-32 bytes of `[a-z0-9-]` with no leading, trailing or doubled hyphen; the same
/// rules as [`SkillName`] with a shorter length limit. Categories group skills for
/// hierarchical matching.
///
/// # Examples
///
/// ```
/// use zeph_common::SkillCategory;
///
/// let c = SkillCategory::parse("data-v2").unwrap();
/// assert_eq!(c.as_str(), "data-v2");
/// assert!(SkillCategory::parse("Web").is_err());
/// assert!(SkillCategory::parse(&"a".repeat(33)).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillCategory(Arc<str>);

impl SkillCategory {
    /// Maximum length of a category in bytes.
    pub const MAX_LEN: usize = 32;

    /// Validate `s` and build a `SkillCategory`.
    ///
    /// # Errors
    ///
    /// Returns [`SkillCategoryError`] when `s` is empty, longer than 32 bytes, contains
    /// characters outside `[a-z0-9-]`, or has a leading, trailing or doubled hyphen.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::{SkillCategory, SkillCategoryError};
    ///
    /// assert!(SkillCategory::parse("web").is_ok());
    /// assert_eq!(
    ///     SkillCategory::parse("-web"),
    ///     Err(SkillCategoryError::EdgeHyphen("-web".into()))
    /// );
    /// ```
    pub fn parse(s: &str) -> Result<Self, SkillCategoryError> {
        check_kebab_id(s, Self::MAX_LEN).map_err(|rule| match rule {
            KebabRule::Length => SkillCategoryError::Length(s.len()),
            KebabRule::Charset => SkillCategoryError::Charset(s.to_owned()),
            KebabRule::EdgeHyphen => SkillCategoryError::EdgeHyphen(s.to_owned()),
            KebabRule::DoubleHyphen => SkillCategoryError::DoubleHyphen(s.to_owned()),
        })?;
        Ok(Self(Arc::from(s)))
    }

    /// Return the inner string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::SkillCategory;
    ///
    /// assert_eq!(SkillCategory::parse("web").unwrap().as_str(), "web");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SkillCategory {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for SkillCategory {
    type Error = SkillCategoryError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl TryFrom<String> for SkillCategory {
    type Error = SkillCategoryError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<SkillCategory> for String {
    fn from(c: SkillCategory) -> Self {
        c.0.as_ref().to_owned()
    }
}

impl FromStr for SkillCategory {
    type Err = SkillCategoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl PartialEq<str> for SkillCategory {
    fn eq(&self, other: &str) -> bool {
        self.0.as_ref() == other
    }
}

impl PartialEq<&str> for SkillCategory {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_ref() == *other
    }
}

// ── SessionId ────────────────────────────────────────────────────────────────

/// Identifies a single agent session (one binary invocation or one ACP connection).
///
/// Uses `String` internally to support both UUID-based IDs (production) and
/// arbitrary string IDs (tests, experiments). UUID validation is enforced only at
/// [`SessionId::generate`] time; [`SessionId::new`] accepts any non-empty string for
/// flexibility in test fixtures.
///
/// Session IDs are joined onto a filesystem path by `zeph_session::session_dir`, so a
/// string containing `/`, `\`, `..`, or a NUL byte could in principle escape the
/// intended session directory. Every call site in this codebase constructs `SessionId`
/// from either [`SessionId::generate`]'s own UUID output or a value already gated by a
/// registry/`SessionStore` lookup, so this is not exploitable today — but callers that
/// accept a session id from an untrusted boundary (e.g. an HTTP path parameter) should
/// use [`SessionId::try_new`] instead, which rejects those characters up front.
///
/// # Serialization
///
/// `SessionId` uses `#[serde(transparent)]` — it serializes as a plain JSON string
/// identical to the raw `String` fields it replaces. No wire format change, no DB
/// schema migration required.
///
/// # ACP Note
///
/// `acp::SessionId` from the external `agent_client_protocol` crate is distinct.
/// This type is for **our own** session tracking only.
///
/// # Examples
///
/// ```
/// use zeph_common::SessionId;
///
/// // Production: generate a fresh UUID session
/// let id = SessionId::generate();
/// assert!(!id.as_str().is_empty());
///
/// // Tests: use a readable fixture string
/// let test_id = SessionId::new("test-session");
/// assert_eq!(test_id.as_str(), "test-session");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

/// Rejection reasons for [`SessionId::try_new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SessionIdError {
    /// The candidate string was empty.
    #[error("session id must not be empty")]
    Empty,
    /// The candidate string contains a path separator (`/` or `\`), a `..` traversal
    /// segment, or a NUL byte — any of which could escape the intended directory when
    /// joined onto a filesystem path by `zeph_session::session_dir`.
    #[error("session id must not contain path separators, '..', or NUL bytes")]
    UnsafeCharacters,
}

impl SessionId {
    /// Create a `SessionId` from any non-empty string.
    ///
    /// Accepts UUID strings (production), readable names (tests), or any other
    /// non-empty value. In debug builds, an empty string triggers a `debug_assert!`
    /// to catch accidental construction early.
    ///
    /// This constructor is for **trusted** call sites only — internal generation,
    /// test fixtures, and values already validated by a prior registry/store lookup.
    /// For a string coming from an untrusted boundary, use [`SessionId::try_new`].
    ///
    /// # Panics
    ///
    /// Panics in **debug builds only** if `s` is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::SessionId;
    ///
    /// let id = SessionId::new("test-session");
    /// assert_eq!(id.as_str(), "test-session");
    /// ```
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        debug_assert!(!s.is_empty(), "SessionId must not be empty");
        Self(s)
    }

    /// Create a `SessionId` from a string, rejecting values unsafe for filesystem use.
    ///
    /// Rejects an empty string, and any string containing `/`, `\`, a `..` segment, or
    /// a NUL byte — the characters that could let a `SessionId` escape its intended
    /// directory once joined by `zeph_session::session_dir`. Use this constructor
    /// whenever the candidate string originates from an untrusted boundary (e.g. an
    /// HTTP path parameter or another externally-supplied identifier).
    ///
    /// # Errors
    ///
    /// Returns [`SessionIdError::Empty`] if `s` is empty, or
    /// [`SessionIdError::UnsafeCharacters`] if `s` contains `/`, `\`, `..`, or a NUL byte.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::{SessionId, SessionIdError};
    ///
    /// let id = SessionId::try_new("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
    /// assert_eq!(id.as_str(), "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
    ///
    /// assert_eq!(SessionId::try_new("../etc/passwd"), Err(SessionIdError::UnsafeCharacters));
    /// assert_eq!(SessionId::try_new(""), Err(SessionIdError::Empty));
    /// ```
    pub fn try_new(s: impl Into<String>) -> Result<Self, SessionIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(SessionIdError::Empty);
        }
        if s.contains('/') || s.contains('\\') || s.contains("..") || s.contains('\0') {
            return Err(SessionIdError::UnsafeCharacters);
        }
        Ok(Self(s))
    }

    /// Generate a new session ID backed by a random UUID v4.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::SessionId;
    ///
    /// let id = SessionId::generate();
    /// assert!(!id.as_str().is_empty());
    /// // UUIDs are 36 chars: "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
    /// assert_eq!(id.as_str().len(), 36);
    /// ```
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Return the inner string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::SessionId;
    ///
    /// let id = SessionId::new("s1");
    /// assert_eq!(id.as_str(), "s1");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    /// Generate a new UUID-backed session ID.
    fn default() -> Self {
        Self::generate()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for SessionId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<uuid::Uuid> for SessionId {
    fn from(u: uuid::Uuid) -> Self {
        Self(u.to_string())
    }
}

impl FromStr for SessionId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SessionId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for SessionId {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<SessionId> for str {
    fn eq(&self, other: &SessionId) -> bool {
        self == other.0
    }
}

impl PartialEq<SessionId> for String {
    fn eq(&self, other: &SessionId) -> bool {
        *self == other.0
    }
}

// ── ToolDefinition ───────────────────────────────────────────────────────────

/// Minimal tool definition passed to LLM providers.
///
/// Decoupled from `zeph-tools::ToolDef` to avoid cross-crate dependencies.
/// Providers translate this into their native tool/function format before sending to the API.
///
/// # Examples
///
/// ```
/// use zeph_common::types::ToolDefinition;
/// use zeph_common::ToolName;
///
/// let tool = ToolDefinition {
///     name: ToolName::new("get_weather"),
///     description: "Return current weather for a city.".into(),
///     parameters: serde_json::json!({
///         "type": "object",
///         "properties": {
///             "city": { "type": "string" }
///         },
///         "required": ["city"]
///     }),
///     output_schema: None,
/// };
/// assert_eq!(tool.name, "get_weather");
/// ```
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    /// Tool name — must match the name used in the response `ToolUseRequest`.
    pub name: ToolName,
    /// Human-readable description guiding the model on when to call this tool.
    pub description: String,
    /// JSON Schema object describing parameters.
    pub parameters: serde_json::Value,
    /// Raw output schema advertised by the MCP server, if present.
    ///
    /// When `mcp.forward_output_schema = true`, LLM provider assemblers append a compact JSON
    /// hint to the tool description rather than adding a new top-level field (unsupported by
    /// the Anthropic and `OpenAI` APIs).
    ///
    /// DO NOT convert to `schemars::Schema` — lossy; see #2931 critique P0-1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

/// Unique identifier for a subgoal within a session.
///
/// Carried on `MessageMetadata::subgoal_id` so subgoal membership travels with the message
/// itself and survives drains, reinserts, and deferred-summary insertions without any
/// index bookkeeping.
///
/// # Examples
///
/// ```
/// use zeph_common::SubgoalId;
///
/// let id = SubgoalId(3);
/// assert_eq!(id, SubgoalId(3));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubgoalId(pub u32);

/// Unique identifier of a long-horizon goal (UUID v4).
///
/// Built only by [`GoalId::new_v4`] (goal creation) or [`GoalId::parse`] (database row and
/// user input boundaries), so a goal id cannot be confused with a session id, a conversation
/// id, or plain goal text. Serializes as its hyphenated UUID string.
///
/// # Examples
///
/// ```
/// use zeph_common::GoalId;
///
/// let id = GoalId::new_v4();
/// assert_eq!(GoalId::parse(&id.to_string()).unwrap(), id);
/// assert_eq!(id.short().len(), 8);
/// assert!(GoalId::parse("not-a-uuid").is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GoalId(uuid::Uuid);

/// Error returned by [`GoalId::parse`] for a string that is not a valid UUID.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid goal id: {0}")]
pub struct GoalIdError(#[from] uuid::Error);

impl GoalId {
    /// Generate a fresh random goal id.
    #[must_use]
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// Parse a goal id from its UUID text form.
    ///
    /// # Errors
    ///
    /// Returns [`GoalIdError`] when `s` is not a valid UUID.
    pub fn parse(s: &str) -> Result<Self, GoalIdError> {
        Ok(Self(uuid::Uuid::parse_str(s)?))
    }

    /// First 8 hex digits of the id, for compact display.
    #[must_use]
    pub fn short(&self) -> String {
        format!("{:08x}", self.0.as_u128() >> 96)
    }
}

impl fmt::Display for GoalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

impl FromStr for GoalId {
    type Err = GoalIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for GoalId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for GoalId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Reason why the agent turn ended early.
///
/// Emitted by the agent loop when a non-default terminal condition is detected.
/// Consumers (e.g. the ACP layer) map this to the protocol-level `StopReason`.
///
/// # Examples
///
/// ```
/// use zeph_common::StopHint;
///
/// let hint = StopHint::MaxTokens;
/// assert!(matches!(hint, StopHint::MaxTokens));
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopHint {
    /// The LLM response was cut off by the token limit.
    MaxTokens,
    /// The turn loop exhausted `max_turns` without a final text response.
    MaxTurnRequests,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_id_short_is_first_eight_hex_digits() {
        let id = GoalId::parse("a1b2c3d4-0000-4000-8000-000000000000").unwrap();
        assert_eq!(id.short(), "a1b2c3d4");
        let leading_zero = GoalId::parse("0000abcd-0000-4000-8000-000000000000").unwrap();
        assert_eq!(leading_zero.short(), "0000abcd");
    }

    #[test]
    fn goal_id_serde_round_trips_as_string() {
        let id = GoalId::new_v4();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<GoalId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<GoalId>("\"nope\"").is_err());
    }

    #[test]
    fn goal_id_parse_rejects_non_uuid() {
        assert!("".parse::<GoalId>().is_err());
        assert!("a\u{e9}\u{e9}\u{e9}".parse::<GoalId>().is_err());
    }

    #[test]
    fn tool_name_construction_and_equality() {
        let name = ToolName::new("shell");
        assert_eq!(name.as_str(), "shell");
        assert_eq!(name, "shell");
        assert_eq!(name, "shell".to_owned());
        // Reverse direction: PartialEq<ToolName> for str/String
        assert_eq!(*"shell", name);
        assert_eq!("shell".to_owned(), name);
    }

    #[test]
    fn tool_name_default_is_empty() {
        let name = ToolName::default();
        assert_eq!(name.as_str(), "");
    }

    #[test]
    fn tool_name_clone_is_cheap() {
        let name = ToolName::new("web_scrape");
        let name2 = name.clone();
        assert_eq!(name, name2);
        // Both Arc<str> point to same allocation
        assert!(Arc::ptr_eq(&name.0, &name2.0));
    }

    #[test]
    fn tool_name_from_impls() {
        let from_str: ToolName = ToolName::from("bash");
        let from_string: ToolName = ToolName::from("bash".to_owned());
        let parsed: ToolName = "bash".parse().unwrap();
        assert_eq!(from_str, from_string);
        assert_eq!(from_str, parsed);
    }

    #[test]
    fn tool_name_as_hashmap_key() {
        use std::collections::HashMap;
        let mut map: HashMap<ToolName, u32> = HashMap::new();
        map.insert(ToolName::new("shell"), 1);
        // Borrow<str> enables lookup by &str
        assert_eq!(map.get("shell"), Some(&1));
    }

    #[test]
    fn tool_name_display() {
        let name = ToolName::new("my_tool");
        assert_eq!(format!("{name}"), "my_tool");
    }

    #[test]
    fn tool_name_serde_transparent() {
        let name = ToolName::new("shell");
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#""shell""#);
        let back: ToolName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, name);
    }

    #[test]
    fn session_id_new_roundtrip() {
        let id = SessionId::new("test-session");
        assert_eq!(id.as_str(), "test-session");
        assert_eq!(id.to_string(), "test-session");
    }

    #[test]
    fn session_id_generate_is_uuid() {
        let id = SessionId::generate();
        assert_eq!(id.as_str().len(), 36);
        assert!(uuid::Uuid::parse_str(id.as_str()).is_ok());
    }

    #[test]
    fn session_id_default_is_generated() {
        let id = SessionId::default();
        assert_ne!(id.as_str(), "");
        assert_eq!(id.as_str().len(), 36);
    }

    #[test]
    fn session_id_from_uuid() {
        let u = uuid::Uuid::new_v4();
        let id = SessionId::from(u);
        assert_eq!(id.as_str(), u.to_string());
    }

    #[test]
    fn session_id_deref_slicing() {
        let id = SessionId::new("abcdefgh");
        // Deref<Target=str> enables string slicing
        assert_eq!(&id[..4], "abcd");
    }

    #[test]
    fn session_id_serde_transparent() {
        let id = SessionId::new("sess-abc");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""sess-abc""#);
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn session_id_from_str_parses() {
        let id: SessionId = "my-session".parse().unwrap();
        assert_eq!(id.as_str(), "my-session");
    }

    #[test]
    fn session_id_try_new_accepts_valid_uuid() {
        let id = SessionId::try_new(uuid::Uuid::new_v4().to_string()).unwrap();
        assert_eq!(id.as_str().len(), 36);
    }

    #[test]
    fn session_id_try_new_accepts_plain_string() {
        let id = SessionId::try_new("sess-abc123").unwrap();
        assert_eq!(id.as_str(), "sess-abc123");
    }

    #[test]
    fn session_id_try_new_rejects_empty() {
        assert_eq!(SessionId::try_new(""), Err(SessionIdError::Empty));
    }

    #[test]
    fn session_id_try_new_rejects_path_traversal() {
        assert_eq!(
            SessionId::try_new("../../etc/passwd"),
            Err(SessionIdError::UnsafeCharacters)
        );
        assert_eq!(
            SessionId::try_new("foo/../bar"),
            Err(SessionIdError::UnsafeCharacters)
        );
    }

    #[test]
    fn session_id_try_new_rejects_forward_slash() {
        assert_eq!(
            SessionId::try_new("foo/bar"),
            Err(SessionIdError::UnsafeCharacters)
        );
    }

    #[test]
    fn session_id_try_new_rejects_backslash() {
        assert_eq!(
            SessionId::try_new("foo\\bar"),
            Err(SessionIdError::UnsafeCharacters)
        );
    }

    #[test]
    fn session_id_try_new_rejects_nul_byte() {
        assert_eq!(
            SessionId::try_new("foo\0bar"),
            Err(SessionIdError::UnsafeCharacters)
        );
    }

    #[test]
    fn provider_name_construction_and_equality() {
        let name = ProviderName::new("fast");
        assert_eq!(name.as_str(), "fast");
        assert_eq!(name, "fast");
        assert_eq!(name, "fast".to_owned());
        // Reverse direction: PartialEq<ProviderName> for str/String
        assert_eq!(*"fast", name);
        assert_eq!("fast".to_owned(), name);
    }

    #[test]
    fn provider_name_clone_is_cheap() {
        let name = ProviderName::new("quality");
        let name2 = name.clone();
        assert_eq!(name, name2);
        assert!(Arc::ptr_eq(&name.0, &name2.0));
    }

    #[test]
    fn provider_name_from_impls() {
        let from_str: ProviderName = ProviderName::from("fast");
        let from_string: ProviderName = ProviderName::from("fast".to_owned());
        let parsed: ProviderName = "fast".parse().unwrap();
        assert_eq!(from_str, from_string);
        assert_eq!(from_str, parsed);
    }

    #[test]
    fn provider_name_as_hashmap_key() {
        use std::collections::HashMap;
        let mut map: HashMap<ProviderName, u32> = HashMap::new();
        map.insert(ProviderName::new("fast"), 1);
        assert_eq!(map.get("fast"), Some(&1));
    }

    #[test]
    fn provider_name_display() {
        let name = ProviderName::new("ollama-local");
        assert_eq!(format!("{name}"), "ollama-local");
    }

    #[test]
    fn provider_name_serde_transparent() {
        let name = ProviderName::new("quality");
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#""quality""#);
        let back: ProviderName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, name);
    }

    #[test]
    fn skill_name_construction_and_equality() {
        let name = SkillName::parse("rust-agents").unwrap();
        assert_eq!(name.as_str(), "rust-agents");
        assert_eq!(name, "rust-agents");
        assert_eq!(name, "rust-agents".to_owned());
        assert_eq!(*"rust-agents", name);
        assert_eq!("rust-agents".to_owned(), name);
    }

    #[test]
    fn skill_name_rejects_invalid() {
        assert_eq!(SkillName::parse(""), Err(SkillNameError::Length(0)));
        assert!(matches!(
            SkillName::parse(&"a".repeat(65)),
            Err(SkillNameError::Length(65))
        ));
        assert!(SkillName::parse(&"a".repeat(64)).is_ok());
        for bad in ["Alpha", "a_b", "a b", "é", "a/b", "../x"] {
            assert!(
                matches!(SkillName::parse(bad), Err(SkillNameError::Charset(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            SkillName::parse("-a"),
            Err(SkillNameError::EdgeHyphen(_))
        ));
        assert!(matches!(
            SkillName::parse("a-"),
            Err(SkillNameError::EdgeHyphen(_))
        ));
        assert!(matches!(
            SkillName::parse("a--b"),
            Err(SkillNameError::DoubleHyphen(_))
        ));
    }

    #[test]
    fn skill_name_and_category_errors_escape_control_characters() {
        let hostile = "a\u{1b}[31mb";
        let name_err = SkillName::parse(hostile).unwrap_err().to_string();
        let category_err = SkillCategory::parse(hostile).unwrap_err().to_string();
        for msg in [name_err, category_err] {
            assert!(!msg.contains('\u{1b}'), "{msg:?}");
        }
        for bad in ["-a\u{1b}", "a--\u{1b}"] {
            assert!(
                !SkillName::parse(bad)
                    .unwrap_err()
                    .to_string()
                    .contains('\u{1b}')
            );
            assert!(
                !SkillCategory::parse(bad)
                    .unwrap_err()
                    .to_string()
                    .contains('\u{1b}')
            );
        }
    }

    #[test]
    fn skill_name_clone_is_cheap() {
        let name = SkillName::parse("readme-generator").unwrap();
        let name2 = name.clone();
        assert_eq!(name, name2);
        assert!(Arc::ptr_eq(&name.0, &name2.0));
    }

    #[test]
    fn skill_name_try_from_and_parse() {
        let a = SkillName::try_from("rust-agents").unwrap();
        let b = SkillName::try_from("rust-agents".to_owned()).unwrap();
        let c: SkillName = "rust-agents".parse().unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert!(SkillName::try_from("Bad").is_err());
        assert!("Bad".parse::<SkillName>().is_err());
    }

    #[test]
    fn skill_name_as_hashmap_key() {
        use std::collections::HashMap;
        let mut map: HashMap<SkillName, u32> = HashMap::new();
        map.insert(SkillName::parse("rust-agents").unwrap(), 1);
        assert_eq!(map.get("rust-agents"), Some(&1));
    }

    #[test]
    fn skill_name_display() {
        let name = SkillName::parse("readme-generator").unwrap();
        assert_eq!(format!("{name}"), "readme-generator");
    }

    #[test]
    fn skill_name_serde_roundtrip_and_rejects_invalid() {
        let name = SkillName::parse("rust-agents").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, r#""rust-agents""#);
        let back: SkillName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, name);
        assert!(serde_json::from_str::<SkillName>(r#""Not Valid""#).is_err());
        assert!(serde_json::from_str::<SkillName>(r#""""#).is_err());
    }

    #[test]
    fn tool_execution_id_serializes_as_plain_string() {
        let id = ToolExecutionId::new("exec-1").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"exec-1\"");
    }

    #[test]
    fn tool_execution_id_rejects_empty_and_generates_unique() {
        assert_eq!(ToolExecutionId::new(""), Err(EmptyToolExecutionId));
        assert_ne!(ToolExecutionId::generate(), ToolExecutionId::generate());
    }
}
