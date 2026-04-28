// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;
use std::path::PathBuf;

use crate::bootstrap::AppBuilder;
use zeph_core::vault::Secret;
use zeph_llm::provider::LlmProvider;
use zeph_memory::{IngestionPipeline, QdrantOps, SplitterConfig, TextLoader, TextSplitter};

pub(crate) async fn handle_ingest(
    path: PathBuf,
    chunk_size: usize,
    chunk_overlap: usize,
    collection: String,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let app = AppBuilder::new(config_path, None, None, None).await?;
    let config = app.config();

    let qdrant = QdrantOps::new(
        &config.memory.qdrant_url,
        config.memory.qdrant_api_key.as_ref().map(Secret::expose),
    )
    .map_err(|e| anyhow::anyhow!("failed to connect to Qdrant: {e}"))?;

    let (provider, _status_tx, _status_rx) = app.build_provider().await?;
    let provider = std::sync::Arc::new(provider);
    let embed_fn = {
        let p = std::sync::Arc::clone(&provider);
        move |text: &str| -> zeph_llm::provider::EmbedFuture {
            let p = std::sync::Arc::clone(&p);
            let owned = text.to_owned();
            Box::pin(async move { p.embed(&owned).await })
        }
    };

    let splitter_cfg = SplitterConfig {
        chunk_size,
        chunk_overlap,
        ..SplitterConfig::default()
    };
    let splitter = TextSplitter::new(splitter_cfg);
    let pipeline = IngestionPipeline::new(splitter, qdrant, &collection, Box::new(embed_fn));

    let loader = TextLoader::default();
    let count = pipeline
        .load_and_ingest(&loader, &path)
        .await
        .map_err(|e| anyhow::anyhow!("ingestion failed: {e}"))?;

    println!(
        "Ingested {count} chunk(s) from {} into collection '{collection}'",
        path.display()
    );
    Ok(())
}
