// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validated newtypes for plugin domain values.

use std::fmt;

use crate::PluginError;
use crate::manager::validate_plugin_name;

/// A validated plugin name.
///
/// A `PluginName` is guaranteed to satisfy the plugin naming rules:
/// `[a-z][a-z0-9-]*`, at most 64 characters, no path separators or dots.
///
/// Construct via [`TryFrom<String>`] or [`TryFrom<&str>`]; both delegate to the
/// same `validate_plugin_name` predicate used throughout the plugin manager.
///
/// # Examples
///
/// ```rust
/// use zeph_plugins::PluginName;
///
/// let name: PluginName = "my-plugin".try_into().unwrap();
/// assert_eq!(name.as_str(), "my-plugin");
/// assert_eq!(name.to_string(), "my-plugin");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct PluginName(String);

impl<'de> serde::Deserialize<'de> for PluginName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PluginName::try_from(s).map_err(serde::de::Error::custom)
    }
}

impl PluginName {
    /// Returns the plugin name as a `&str`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_plugins::PluginName;
    ///
    /// let name: PluginName = "cool-plugin".try_into().unwrap();
    /// assert_eq!(name.as_str(), "cool-plugin");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PluginName {
    type Error = PluginError;

    /// Validate and wrap a plugin name.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::InvalidName`] when the string does not satisfy the
    /// naming rules: `[a-z][a-z0-9-]*`, at most 64 characters, no path separators
    /// or dots.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_plugins::PluginName;
    ///
    /// let ok: PluginName = PluginName::try_from("valid-name".to_owned()).unwrap();
    /// assert_eq!(ok.as_str(), "valid-name");
    ///
    /// // Uppercase letters are rejected.
    /// let err = PluginName::try_from("Invalid_Name".to_owned());
    /// assert!(err.is_err());
    ///
    /// // Empty string is rejected.
    /// let err = PluginName::try_from(String::new());
    /// assert!(err.is_err());
    ///
    /// // Names longer than 64 characters are rejected.
    /// let err = PluginName::try_from("a".repeat(65));
    /// assert!(err.is_err());
    /// ```
    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_plugin_name(&value)?;
        Ok(Self(value))
    }
}

impl TryFrom<&str> for PluginName {
    type Error = PluginError;

    /// Validate and wrap a plugin name from a string slice.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::InvalidName`] when the string does not satisfy the
    /// naming rules: `[a-z][a-z0-9-]*`, at most 64 characters, no path separators
    /// or dots.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_plugins::PluginName;
    ///
    /// let ok: PluginName = PluginName::try_from("another-plugin").unwrap();
    /// assert_eq!(ok.as_str(), "another-plugin");
    ///
    /// let err = PluginName::try_from("BAD");
    /// assert!(err.is_err());
    /// ```
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        validate_plugin_name(value)?;
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Display for PluginName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PluginName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for PluginName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for PluginName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for PluginName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}
