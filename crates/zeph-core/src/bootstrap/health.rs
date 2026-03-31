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
        _ => {}
    }
}

pub async fn warmup_provider(provider: &AnyProvider) {
    if let AnyProvider::Ollama(ollama) = provider {
        let start = std::time::Instant::now();
        match ollama.warmup().await {
            Ok(()) => {
                tracing::info!("ollama model ready ({:.1}s)", start.elapsed().as_secs_f64());
            }
            Err(e) => tracing::warn!("ollama warmup failed: {e:#}"),
        }
    }
}
