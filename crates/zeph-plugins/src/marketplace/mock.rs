// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only [`RegistryClient`] implementation proving the trait boundary is real
//! (SC-004/NFR-003): a second, independent backend that plugs in without touching any call
//! site in `zeph skill search`/`zeph skill get`/`zeph plugin search`/`zeph plugin get`.

use std::collections::HashMap;
use std::pin::Pin;

use super::{PackageArchive, RegistryClient, RegistryEntry, RegistryError, materialize_package};

/// In-memory registry backend for tests.
///
/// Holds a fixed set of [`RegistryEntry`] results and, for each `registry_id`, the set of
/// `(path, content)` pairs that [`fetch`](RegistryClient::fetch) materializes into a
/// [`tempfile::TempDir`].
#[derive(Default)]
pub struct MockRegistryClient {
    entries: Vec<RegistryEntry>,
    packages: HashMap<String, Vec<(String, String)>>,
    /// When `Some`, every call returns this error instead of touching `entries`/`packages`.
    fail_with: Option<String>,
}

impl MockRegistryClient {
    /// Create an empty mock (search returns no results, fetch always returns `NotFound`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a search result. `search()` matches on substring containment against
    /// `entry.name`/`entry.description`, case-insensitively.
    #[must_use]
    pub fn with_entry(mut self, entry: RegistryEntry) -> Self {
        self.entries.push(entry);
        self
    }

    /// Register the files that `fetch(registry_id)` materializes.
    #[must_use]
    pub fn with_package(
        mut self,
        registry_id: impl Into<String>,
        files: Vec<(String, String)>,
    ) -> Self {
        self.packages.insert(registry_id.into(), files);
        self
    }

    /// Force every `search`/`fetch` call to fail with [`RegistryError::Backend`] carrying
    /// `message` in the body. Used to test error-propagation paths.
    #[must_use]
    pub fn failing(mut self, message: impl Into<String>) -> Self {
        self.fail_with = Some(message.into());
        self
    }
}

impl RegistryClient for MockRegistryClient {
    fn search(
        &self,
        query: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<RegistryEntry>, RegistryError>> + Send + '_>> {
        let query = query.to_lowercase();
        Box::pin(async move {
            if let Some(msg) = &self.fail_with {
                return Err(RegistryError::Backend {
                    status: 500,
                    body: msg.clone(),
                });
            }
            Ok(self
                .entries
                .iter()
                .filter(|e| {
                    e.name.to_lowercase().contains(&query)
                        || e.description.to_lowercase().contains(&query)
                })
                .cloned()
                .collect())
        })
    }

    fn fetch(
        &self,
        registry_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<PackageArchive, RegistryError>> + Send + '_>> {
        let registry_id = registry_id.to_owned();
        Box::pin(async move {
            if let Some(msg) = &self.fail_with {
                return Err(RegistryError::Backend {
                    status: 500,
                    body: msg.clone(),
                });
            }
            let files = self
                .packages
                .get(&registry_id)
                .ok_or_else(|| RegistryError::NotFound(registry_id.clone()))?;

            let tmp = tempfile::tempdir()?;
            let (has_plugin_manifest, install_dir) = materialize_package(tmp.path(), files)?;

            Ok(PackageArchive {
                registry_id,
                has_plugin_manifest,
                extracted_dir: tmp,
                install_dir,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(id: &str, name: &str) -> RegistryEntry {
        RegistryEntry {
            registry_id: id.to_owned(),
            name: name.to_owned(),
            description: "a test skill".to_owned(),
            tags: vec![],
            author: None,
            security_audit_status: None,
        }
    }

    #[tokio::test]
    async fn search_filters_by_name_case_insensitively() {
        let mock = MockRegistryClient::new().with_entry(sample_entry("a/b", "PDF Tools"));
        let results = mock.search("pdf").await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(mock.search("nomatch").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_returns_not_found_for_unregistered_id() {
        let mock = MockRegistryClient::new();
        let err = mock.fetch("missing").await.unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(id) if id == "missing"));
    }

    #[tokio::test]
    async fn fetch_materializes_registered_package() {
        let mock = MockRegistryClient::new().with_package(
            "a/b",
            vec![(
                "SKILL.md".to_owned(),
                "---\nname: b\ndescription: a test skill\n---\nbody".to_owned(),
            )],
        );
        let archive = mock.fetch("a/b").await.unwrap();
        // install_dir is named after the skill (`b`), not extracted_dir's random tmp name —
        // see PackageArchive::install_dir docs.
        assert!(archive.install_dir.join("SKILL.md").is_file());
        assert!(!archive.has_plugin_manifest);
    }

    #[tokio::test]
    async fn failing_mock_returns_configured_error() {
        let mock = MockRegistryClient::new().failing("boom");
        let err = mock.search("x").await.unwrap_err();
        assert!(matches!(err, RegistryError::Backend { .. }));
        let err = mock.fetch("x").await.unwrap_err();
        assert!(matches!(err, RegistryError::Backend { .. }));
    }
}
