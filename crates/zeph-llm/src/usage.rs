// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Tracks token usage and cache hit statistics.
///
/// Note: `last_cache` is only populated by Claude and `OpenAI` providers.
/// Ollama and Gemini only use `last_usage`.
#[derive(Debug, Default)]
pub(crate) struct UsageTracker {
    last_usage: std::sync::Mutex<Option<(u64, u64)>>,
    last_cache: std::sync::Mutex<Option<(u64, u64)>>,
}

impl UsageTracker {
    pub(crate) fn record_usage(&self, input: u64, output: u64) {
        if let Ok(mut g) = self.last_usage.lock() {
            *g = Some((input, output));
        }
    }

    pub(crate) fn record_cache(&self, creation: u64, read: u64) {
        if let Ok(mut g) = self.last_cache.lock() {
            *g = Some((creation, read));
        }
    }

    pub(crate) fn last_usage(&self) -> Option<(u64, u64)> {
        self.last_usage.lock().ok().and_then(|g| *g)
    }

    pub(crate) fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.last_cache.lock().ok().and_then(|g| *g)
    }
}

impl Clone for UsageTracker {
    fn clone(&self) -> Self {
        Self::default()
    }
}
