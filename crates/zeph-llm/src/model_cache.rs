// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Disk-backed cache for remote model listings with 24-hour TTL.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::LlmError;

const TTL_SECS: u64 = 86_400; // 24 hours

/// Metadata about a single model returned by a provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteModelInfo {
    /// Provider-unique model identifier.
    pub id: String,
    /// Human-readable label (e.g. `"llama3.2:3b Q4_K_M"`).
    pub display_name: String,
    /// Context window in tokens, if advertised.
    pub context_window: Option<usize>,
    /// Unix timestamp of model creation, if available.
    pub created_at: Option<i64>,
}

/// On-disk cache envelope.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEnvelope {
    /// Unix timestamp when this cache was written.
    fetched_at: u64,
    models: Vec<RemoteModelInfo>,
}

/// Filesystem cache for a single provider's model list.
pub struct ModelCache {
    path: PathBuf,
}

impl ModelCache {
    /// Build a cache handle for `slug` (e.g. `"ollama"`, `"claude"`).
    ///
    /// The slug is sanitized to `[a-zA-Z0-9_]` to prevent path traversal.
    /// Cache file lives at `{cache_dir}/zeph/models/{slug}.json`.
    #[must_use]
    pub fn for_slug(slug: &str) -> Self {
        let safe: String = slug
            .chars()
            .map(|c| if c == '-' { '_' } else { c })
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let safe = if safe.is_empty() {
            "unknown".to_string()
        } else {
            safe
        };
        let path = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from(".cache"))
            .join("zeph")
            .join("models")
            .join(format!("{safe}.json"));
        Self { path }
    }

    /// Load cached models. Returns `None` if the file does not exist or is unreadable.
    ///
    /// # Errors
    ///
    /// Returns an error only on JSON parse failure (corrupt file).
    pub fn load(&self) -> Result<Option<Vec<RemoteModelInfo>>, LlmError> {
        let Ok(data) = std::fs::read(&self.path) else {
            return Ok(None);
        };
        let envelope: CacheEnvelope =
            serde_json::from_slice(&data).map_err(|e| LlmError::Other(e.to_string()))?;
        Ok(Some(envelope.models))
    }

    /// Returns `true` if the cache file is missing or older than 24 hours.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        let Ok(data) = std::fs::read(&self.path) else {
            return true;
        };
        let Ok(envelope) = serde_json::from_slice::<CacheEnvelope>(&data) else {
            return true;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        now.saturating_sub(envelope.fetched_at) > TTL_SECS
    }

    /// Atomically write models to disk. Writes `.tmp` then renames.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the file cannot be written.
    pub fn save(&self, models: &[RemoteModelInfo]) -> Result<(), LlmError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LlmError::Other(format!("cache dir: {e}")))?;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        let envelope = CacheEnvelope {
            fetched_at: now,
            models: models.to_vec(),
        };
        let json =
            serde_json::to_vec_pretty(&envelope).map_err(|e| LlmError::Other(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| LlmError::Other(format!("cache write: {e}")))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| LlmError::Other(format!("cache rename: {e}")))?;
        Ok(())
    }

    /// Remove the cache file (for `/model refresh`).
    pub fn invalidate(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_cache() -> ModelCache {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join("zeph-test-model-cache")
            .join(format!("{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ModelCache {
            path: dir.join("test.json"),
        }
    }

    #[test]
    fn missing_file_is_stale() {
        let c = tmp_cache();
        assert!(c.is_stale());
    }

    #[test]
    fn fresh_cache_is_not_stale() {
        let c = tmp_cache();
        let models = vec![RemoteModelInfo {
            id: "m1".into(),
            display_name: "Model 1".into(),
            context_window: Some(4096),
            created_at: None,
        }];
        c.save(&models).unwrap();
        assert!(!c.is_stale());
    }

    #[test]
    fn json_round_trip() {
        let c = tmp_cache();
        let models = vec![
            RemoteModelInfo {
                id: "a".into(),
                display_name: "Alpha".into(),
                context_window: Some(8192),
                created_at: Some(1_700_000_000),
            },
            RemoteModelInfo {
                id: "b".into(),
                display_name: "Beta".into(),
                context_window: None,
                created_at: None,
            },
        ];
        c.save(&models).unwrap();
        let loaded = c.load().unwrap().unwrap();
        assert_eq!(loaded, models);
    }

    #[test]
    fn stale_detection_on_old_timestamp() {
        let c = tmp_cache();
        // Write envelope with timestamp 2 days ago.
        let old_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(2 * 86_400 + 1);
        let envelope = super::CacheEnvelope {
            fetched_at: old_ts,
            models: vec![],
        };
        let json = serde_json::to_vec_pretty(&envelope).unwrap();
        std::fs::write(&c.path, &json).unwrap();
        assert!(c.is_stale());
    }

    #[test]
    fn invalidate_removes_file() {
        let c = tmp_cache();
        let models = vec![];
        c.save(&models).unwrap();
        assert!(c.path.exists());
        c.invalidate();
        assert!(!c.path.exists());
    }
}
