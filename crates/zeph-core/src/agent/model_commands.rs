// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::LlmProvider;

use super::Agent;

impl<C: crate::channel::Channel> Agent<C> {
    /// Switch the active provider to one serving `model_id`.
    ///
    /// Looks up the model in the provider's remote model list (or cache).
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
        self.runtime.model_name = model_id.to_string();
        tracing::info!(model = model_id, "set_model called");
        Ok(())
    }

    pub(super) async fn handle_model_refresh(&mut self) {
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
            Ok(models) => {
                let _ = self
                    .channel
                    .send(&format!("Fetched {} models.", models.len()))
                    .await;
            }
            Err(e) => {
                let _ = self
                    .channel
                    .send(&format!("Error fetching models: {e}"))
                    .await;
            }
        }
    }

    pub(super) async fn handle_model_list(&mut self) {
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
                Err(e) => {
                    let _ = self
                        .channel
                        .send(&format!("Error fetching models: {e}"))
                        .await;
                    return;
                }
            }
        };
        if models.is_empty() {
            let _ = self.channel.send("No models available.").await;
            return;
        }
        let mut lines = vec!["Available models:".to_string()];
        for (i, m) in models.iter().enumerate() {
            lines.push(format!("  {}. {} ({})", i + 1, m.display_name, m.id));
        }
        let _ = self.channel.send(&lines.join("\n")).await;
    }

    pub(super) async fn handle_model_switch(&mut self, model_id: &str) {
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
        if let Some(models) = known_models {
            if !models.iter().any(|m| m.id == model_id) {
                let mut lines = vec![format!("Unknown model '{model_id}'. Available models:")];
                for m in &models {
                    lines.push(format!("  • {} ({})", m.display_name, m.id));
                }
                let _ = self.channel.send(&lines.join("\n")).await;
                return;
            }
        } else {
            let _ = self
                .channel
                .send(
                    "Model list unavailable, switching anyway — verify your model name is correct.",
                )
                .await;
        }
        match self.set_model(model_id) {
            Ok(()) => {
                let _ = self
                    .channel
                    .send(&format!("Switched to model: {model_id}"))
                    .await;
            }
            Err(e) => {
                let _ = self.channel.send(&format!("Error: {e}")).await;
            }
        }
    }

    /// Handle `/model`, `/model <id>`, and `/model refresh` commands.
    pub(super) async fn handle_model_command(&mut self, trimmed: &str) {
        let arg = trimmed.strip_prefix("/model").map_or("", str::trim);
        if arg == "refresh" {
            self.handle_model_refresh().await;
        } else if arg.is_empty() {
            self.handle_model_list().await;
        } else {
            self.handle_model_switch(arg).await;
        }
    }
}
