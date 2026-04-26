// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::LlmProvider;

use super::Agent;

impl<C: crate::channel::Channel> Agent<C> {
    /// Switch the active provider to one serving `model_id`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the model is not found.
    pub(crate) fn set_model(&mut self, model_id: &str) -> Result<(), String> {
        if model_id.is_empty() {
            return Err("model id must not be empty".to_string());
        }
        if model_id.len() > 256 {
            return Err("model id exceeds maximum length of 256 characters".to_string());
        }
        if !model_id
            .chars()
            .all(|c| c.is_ascii() && !c.is_ascii_control())
        {
            return Err("model id must contain only printable ASCII characters".to_string());
        }
        self.runtime.config.model_name = model_id.to_string();
        tracing::info!(model = model_id, "set_model called");
        Ok(())
    }

    /// Refresh the remote model cache, then return a result message.
    pub(crate) async fn model_refresh_as_string(&mut self) -> String {
        if let Some(cache_dir) = dirs::cache_dir() {
            let models_dir = cache_dir.join("zeph").join("models");
            if let Ok(entries) = std::fs::read_dir(&models_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        match self.provider.list_models_remote().await {
            Ok(models) => format!("Fetched {} models.", models.len()),
            Err(e) => format!("Error fetching models: {e}"),
        }
    }

    /// List available models, returning a formatted string.
    pub(crate) async fn model_list_as_string(&mut self) -> String {
        let cache = zeph_llm::model_cache::ModelCache::for_slug(self.provider.name());
        let cached = if cache.is_stale() {
            None
        } else {
            cache.load().unwrap_or(None)
        };
        let models = if let Some(m) = cached {
            m
        } else {
            match self.provider.list_models_remote().await {
                Ok(m) => m,
                Err(e) => return format!("Error fetching models: {e}"),
            }
        };
        if models.is_empty() {
            return "No models available.".to_owned();
        }
        let mut lines = vec!["Available models:".to_string()];
        for (i, m) in models.iter().enumerate() {
            lines.push(format!("  {}. {} ({})", i + 1, m.display_name, m.id));
        }
        lines.join("\n")
    }

    /// Switch to a different model, returning a result message.
    pub(crate) async fn model_switch_as_string(&mut self, model_id: &str) -> String {
        let cache = zeph_llm::model_cache::ModelCache::for_slug(self.provider.name());
        let known_models: Option<Vec<zeph_llm::model_cache::RemoteModelInfo>> = if cache.is_stale()
        {
            match self.provider.list_models_remote().await {
                Ok(m) if !m.is_empty() => Some(m),
                _ => None,
            }
        } else {
            cache.load().unwrap_or(None)
        };
        let list_unavailable = known_models.is_none();
        if let Some(models) = known_models {
            if !models.iter().any(|m| m.id == model_id) {
                let mut lines = vec![format!("Unknown model '{model_id}'. Available models:")];
                for m in &models {
                    lines.push(format!("  • {} ({})", m.display_name, m.id));
                }
                return lines.join("\n");
            }
        } else {
            // Model list unavailable — proceed with a warning.
            tracing::warn!("model list unavailable, switching to '{model_id}' without validation");
        }
        match self.set_model(model_id) {
            Ok(()) => {
                let switch_msg = format!("Switched to model: {model_id}");
                if list_unavailable {
                    format!(
                        "Model list unavailable, switching anyway — verify your model name is correct.\n{switch_msg}"
                    )
                } else {
                    switch_msg
                }
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Handle `/model`, `/model <id>`, and `/model refresh` commands, returning a string result.
    pub(crate) async fn handle_model_command_as_string(&mut self, trimmed: &str) -> String {
        let arg = trimmed.strip_prefix("/model").map_or("", str::trim);
        if arg == "refresh" {
            self.model_refresh_as_string().await
        } else if arg.is_empty() {
            self.model_list_as_string().await
        } else {
            self.model_switch_as_string(arg).await
        }
    }
}
