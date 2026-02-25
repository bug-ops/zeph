// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;

pub async fn health_check(provider: &AnyProvider) {
    match provider {
        AnyProvider::Ollama(ollama) => match ollama.health_check().await {
            Ok(()) => tracing::info!("ollama health check passed"),
            Err(e) => tracing::warn!("ollama health check failed: {e:#}"),
        },
        #[cfg(feature = "candle")]
        AnyProvider::Candle(candle) => {
            tracing::info!("candle provider loaded, device: {}", candle.device_name());
        }
        AnyProvider::Orchestrator(orch) => {
            for (name, p) in orch.providers() {
                tracing::info!(
                    "orchestrator sub-provider '{name}': {}",
                    zeph_llm::provider::LlmProvider::name(p)
                );
            }
        }
        _ => {}
    }
}

pub async fn warmup_provider(provider: &AnyProvider) {
    match provider {
        AnyProvider::Ollama(ollama) => {
            let start = std::time::Instant::now();
            match ollama.warmup().await {
                Ok(()) => {
                    tracing::info!("ollama model ready ({:.1}s)", start.elapsed().as_secs_f64());
                }
                Err(e) => tracing::warn!("ollama warmup failed: {e:#}"),
            }
        }
        AnyProvider::Orchestrator(orch) => {
            for (name, p) in orch.providers() {
                if let zeph_llm::orchestrator::SubProvider::Ollama(ollama) = p {
                    let start = std::time::Instant::now();
                    match ollama.warmup().await {
                        Ok(()) => tracing::info!(
                            "ollama '{name}' ready ({:.1}s)",
                            start.elapsed().as_secs_f64()
                        ),
                        Err(e) => tracing::warn!("ollama '{name}' warmup failed: {e:#}"),
                    }
                }
            }
        }
        _ => {}
    }
}
