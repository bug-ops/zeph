// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Strongly-typed identifiers and shared tool types across `zeph-*` crates.
//!
//! This module defines `ToolName`, `SessionId`, and `ToolDefinition` — types shared
//! by multiple crates without creating cross-crate dependencies.
//!
//! `ToolName` and `SessionId` use `#[serde(transparent)]` for zero-cost serialization
//! compatibility: the JSON wire format is unchanged relative to plain `String` fields.

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolName(Arc<str>);

impl ToolName {
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
    #[must_use]
    pub fn new(s: impl Into<Arc<str>>) -> Self {
        Self(s.into())
    }

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
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ToolName {
    /// Returns an empty `ToolName`.
    ///
    /// This implementation exists solely for `#[serde(default)]` on optional fields.
    /// Do not construct a `ToolName` with an empty string in application code.
    fn default() -> Self {
        Self(Arc::from(""))
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ToolName {
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<String> for ToolName {
    fn from(s: String) -> Self {
        Self(Arc::from(s.as_str()))
    }
}

impl FromStr for ToolName {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}

impl PartialEq<str> for ToolName {
    fn eq(&self, other: &str) -> bool {
        self.0.as_ref() == other
    }
}

impl PartialEq<&str> for ToolName {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_ref() == *other
    }
}

impl PartialEq<String> for ToolName {
    fn eq(&self, other: &String) -> bool {
        self.0.as_ref() == other.as_str()
    }
}

impl PartialEq<ToolName> for str {
    fn eq(&self, other: &ToolName) -> bool {
        self == other.0.as_ref()
    }
}

impl PartialEq<ToolName> for String {
    fn eq(&self, other: &ToolName) -> bool {
        self.as_str() == other.0.as_ref()
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

impl SessionId {
    /// Create a `SessionId` from any non-empty string.
    ///
    /// Accepts UUID strings (production), readable names (tests), or any other
    /// non-empty value. In debug builds, an empty string triggers a `debug_assert!`
    /// to catch accidental construction early.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_construction_and_equality() {
        let name = ToolName::new("shell");
        assert_eq!(name.as_str(), "shell");
        assert_eq!(name, "shell");
        assert_eq!(name, "shell".to_owned());
        assert_eq!(name, "shell"); // symmetric check via PartialEq<str>
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
        assert!(!id.as_str().is_empty());
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
}
