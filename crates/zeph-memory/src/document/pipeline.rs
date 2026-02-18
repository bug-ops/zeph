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
