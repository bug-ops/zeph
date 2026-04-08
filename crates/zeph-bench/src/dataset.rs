// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Format of a dataset's data files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatasetFormat {
    Jsonl,
    Json,
}

/// Static metadata for a benchmark dataset.
#[derive(Debug, Clone)]
pub struct DatasetMeta {
    pub name: &'static str,
    pub description: &'static str,
    pub url: &'static str,
    pub format: DatasetFormat,
}

/// Registry of all built-in benchmark datasets.
pub struct DatasetRegistry {
    datasets: Vec<DatasetMeta>,
}

impl DatasetRegistry {
    /// Create a registry pre-populated with all built-in datasets.
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
                    name: "tau-bench",
                    description: "tau-bench: tool-augmented user simulation benchmark",
                    url: "https://github.com/sierra-research/tau-bench",
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

    /// List all registered datasets.
    #[must_use]
    pub fn list(&self) -> &[DatasetMeta] {
        &self.datasets
    }

    /// Look up a dataset by name (case-insensitive).
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
    fn registry_contains_five_datasets() {
        let reg = DatasetRegistry::new();
        assert_eq!(reg.list().len(), 5);
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
