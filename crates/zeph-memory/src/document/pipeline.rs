use qdrant_client::qdrant::PointStruct;
use serde_json::json;
use uuid::Uuid;

use super::{Document, DocumentError, DocumentLoader, TextSplitter};
use crate::QdrantOps;

pub struct IngestionPipeline {
    splitter: TextSplitter,
    qdrant: QdrantOps,
    collection: String,
    embed_fn: Box<dyn Fn(&str) -> zeph_llm::provider::EmbedFuture + Send + Sync>,
}

impl IngestionPipeline {
    pub fn new(
        splitter: TextSplitter,
        qdrant: QdrantOps,
        collection: impl Into<String>,
        embed_fn: Box<dyn Fn(&str) -> zeph_llm::provider::EmbedFuture + Send + Sync>,
    ) -> Self {
        Self {
            splitter,
            qdrant,
            collection: collection.into(),
            embed_fn,
        }
    }

    /// Ingest a document: split -> embed -> store in Qdrant. Returns chunk count.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant storage fails.
    pub async fn ingest(&self, document: Document) -> Result<usize, DocumentError> {
        let chunks = self.splitter.split(&document);
        if chunks.is_empty() {
            return Ok(0);
        }

        let mut points = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let vector = (self.embed_fn)(&chunk.content).await?;
            let payload = QdrantOps::json_to_payload(json!({
                "source": chunk.metadata.source,
                "content_type": chunk.metadata.content_type,
                "chunk_index": chunk.chunk_index,
                "content": chunk.content,
            }))
            .map_err(|e| DocumentError::Storage(crate::error::MemoryError::Json(e)))?;

            points.push(PointStruct::new(
                Uuid::new_v4().to_string(),
                vector,
                payload,
            ));
        }

        let count = points.len();
        self.qdrant
            .upsert(&self.collection, points)
            .await
            .map_err(|e| DocumentError::Storage(crate::error::MemoryError::Qdrant(e)))?;

        Ok(count)
    }

    /// # Errors
    ///
    /// Returns an error if loading, embedding, or storage fails.
    pub async fn load_and_ingest(
        &self,
        loader: &(dyn DocumentLoader + '_),
        path: &std::path::Path,
    ) -> Result<usize, DocumentError> {
        let documents = loader.load(path).await?;
        let mut total = 0;
        for doc in documents {
            total += self.ingest(doc).await?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::splitter::SplitterConfig;
    use crate::document::types::DocumentMetadata;
    use std::collections::HashMap;

    fn make_document(content: &str) -> Document {
        Document {
            content: content.to_string(),
            metadata: DocumentMetadata {
                source: "test".to_string(),
                content_type: "text/plain".to_string(),
                extra: HashMap::new(),
            },
        }
    }

    fn noop_embed() -> Box<dyn Fn(&str) -> zeph_llm::provider::EmbedFuture + Send + Sync> {
        Box::new(|_text: &str| Box::pin(async move { Ok(vec![0.0f32; 4]) }))
    }

    fn error_embed() -> Box<dyn Fn(&str) -> zeph_llm::provider::EmbedFuture + Send + Sync> {
        Box::new(|_text: &str| {
            Box::pin(
                async move { Err(zeph_llm::error::LlmError::Other("mock embed error".into())) },
            )
        })
    }

    #[tokio::test]
    async fn ingest_empty_document_returns_zero() {
        // Empty document should short-circuit before calling Qdrant.
        // We use an invalid Qdrant URL; the early-return path won't reach it.
        let qdrant = crate::QdrantOps::new("http://127.0.0.1:1").unwrap();
        let splitter = TextSplitter::new(SplitterConfig::default());
        let pipeline = IngestionPipeline::new(splitter, qdrant, "col", noop_embed());

        let doc = make_document("");
        let count = pipeline.ingest(doc).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn ingest_document_embedding_error_propagates() {
        // Embedding failure should return DocumentError without reaching Qdrant.
        let qdrant = crate::QdrantOps::new("http://127.0.0.1:1").unwrap();
        let splitter = TextSplitter::new(SplitterConfig::default());
        let pipeline = IngestionPipeline::new(splitter, qdrant, "col", error_embed());

        let doc = make_document("hello world, this is test content for embedding");
        let result = pipeline.ingest(doc).await;
        assert!(result.is_err(), "expected error from embedding failure");
    }
}
