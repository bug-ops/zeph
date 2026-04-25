// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// The on-disk format used by a dataset's data files.
///
/// The format determines how a [`crate::DatasetLoader`] reads the file:
/// `Jsonl` loaders iterate line-by-line, while `Json` loaders parse the
/// entire file as a single JSON value.
///
/// # Examples
///
/// ```
/// use zeph_bench::DatasetFormat;
///
/// assert_ne!(DatasetFormat::Jsonl, DatasetFormat::Json);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatasetFormat {
    /// New-line–delimited JSON: one JSON object per line.
    Jsonl,
    /// A single JSON document (object or array) spanning the entire file.
    Json,
}

/// Static metadata describing a benchmark dataset.
///
/// Instances are stored in [`DatasetRegistry`] and can be retrieved by name.
/// The `url` field points to the canonical source so users know where to
/// download the data.
///
/// # Examples
///
/// ```
/// use zeph_bench::{DatasetMeta, DatasetFormat};
///
/// let meta = DatasetMeta {
///     name: "example",
///     description: "An example dataset",
///     url: "https://example.com/dataset",
///     format: DatasetFormat::Jsonl,
/// };
/// assert_eq!(meta.name, "example");
/// ```
#[derive(Debug, Clone)]
pub struct DatasetMeta {
    /// Short identifier used in CLI arguments (e.g. `"gaia"`).
    pub name: &'static str,
    /// One-line human-readable description.
    pub description: &'static str,
    /// Canonical download URL (`HuggingFace`, GitHub, etc.).
    pub url: &'static str,
    /// File format expected by the corresponding [`crate::DatasetLoader`].
    pub format: DatasetFormat,
}

/// Registry of all datasets that `zeph-bench` knows about.
///
/// The registry is pre-populated with six built-in datasets on construction and
/// provides case-insensitive lookup by name. It is the authoritative source for
/// the `bench list` CLI subcommand.
///
/// # Built-in Datasets
///
/// | Name | Format | Source |
/// |------|--------|--------|
/// | `longmemeval` | JSONL | `HuggingFace` xiaowu0162/longmemeval |
/// | `locomo` | JSON | `HuggingFace` lmlab/locomo |
/// | `frames` | JSONL | `HuggingFace` google/frames-benchmark |
/// | `tau2-bench-retail` | JSON | GitHub sierra-research/tau2-bench |
/// | `tau2-bench-airline` | JSON | GitHub sierra-research/tau2-bench |
/// | `gaia` | JSONL | `HuggingFace` gaia-benchmark/GAIA |
///
/// # Examples
///
/// ```
/// use zeph_bench::DatasetRegistry;
///
/// let registry = DatasetRegistry::new();
///
/// // List all datasets.
/// assert_eq!(registry.list().len(), 6);
///
/// // Lookup is case-insensitive.
/// assert!(registry.get("GAIA").is_some());
/// assert!(registry.get("unknown").is_none());
/// ```
pub struct DatasetRegistry {
    datasets: Vec<DatasetMeta>,
}

impl DatasetRegistry {
    /// Create a registry pre-populated with all built-in datasets.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::DatasetRegistry;
    ///
    /// let registry = DatasetRegistry::new();
    /// assert!(!registry.list().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            datasets: vec![
                DatasetMeta {
                    name: "longmemeval",
                    description: "LongMemEval: long-term memory evaluation benchmark",
                    url: "https://huggingface.co/datasets/xiaowu0162/longmemeval",
                    format: DatasetFormat::Jsonl,
                },
                DatasetMeta {
                    name: "locomo",
                    description: "LOCOMO: long-context conversational memory benchmark",
                    url: "https://huggingface.co/datasets/lmlab/locomo",
                    format: DatasetFormat::Json,
                },
                DatasetMeta {
                    name: "frames",
                    description: "FRAMES: factual reasoning and multi-step evaluation",
                    url: "https://huggingface.co/datasets/google/frames-benchmark",
                    format: DatasetFormat::Jsonl,
                },
                DatasetMeta {
                    name: "tau2-bench-retail",
                    description: "tau2-bench retail domain: customer service tool-use evaluation",
                    url: "https://github.com/sierra-research/tau2-bench",
                    format: DatasetFormat::Json,
                },
                DatasetMeta {
                    name: "tau2-bench-airline",
                    description: "tau2-bench airline domain: flight reservation tool-use evaluation",
                    url: "https://github.com/sierra-research/tau2-bench",
                    format: DatasetFormat::Json,
                },
                DatasetMeta {
                    name: "gaia",
                    description: "GAIA: general AI assistants benchmark",
                    url: "https://huggingface.co/datasets/gaia-benchmark/GAIA",
                    format: DatasetFormat::Jsonl,
                },
            ],
        }
    }

    /// Return a slice of all registered datasets.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::DatasetRegistry;
    ///
    /// let registry = DatasetRegistry::new();
    /// for meta in registry.list() {
    ///     println!("{}: {}", meta.name, meta.url);
    /// }
    /// ```
    #[must_use]
    pub fn list(&self) -> &[DatasetMeta] {
        &self.datasets
    }

    /// Look up a dataset by name using case-insensitive ASCII comparison.
    ///
    /// Returns `None` when no dataset with the given name is registered.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::DatasetRegistry;
    ///
    /// let registry = DatasetRegistry::new();
    /// let meta = registry.get("locomo").expect("locomo is built-in");
    /// assert_eq!(meta.name, "locomo");
    ///
    /// // Case-insensitive.
    /// assert!(registry.get("LOCOMO").is_some());
    ///
    /// // Unknown dataset.
    /// assert!(registry.get("does-not-exist").is_none());
    /// ```
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DatasetMeta> {
        self.datasets
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
    }
}

impl Default for DatasetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_six_datasets() {
        let reg = DatasetRegistry::new();
        assert_eq!(reg.list().len(), 6);
    }

    #[test]
    fn registry_get_returns_correct_dataset() {
        let reg = DatasetRegistry::new();
        let ds = reg.get("gaia").unwrap();
        assert_eq!(ds.name, "gaia");
    }

    #[test]
    fn registry_get_case_insensitive() {
        let reg = DatasetRegistry::new();
        assert!(reg.get("LOCOMO").is_some());
    }

    #[test]
    fn registry_get_unknown_returns_none() {
        let reg = DatasetRegistry::new();
        assert!(reg.get("unknown-dataset").is_none());
    }
}
