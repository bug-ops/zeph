//! Low-level Qdrant operations shared across crates.

use std::collections::HashMap;

use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    CreateCollectionBuilder, DeletePointsBuilder, Distance, Filter, PointId, PointStruct,
    PointsIdsList, ScoredPoint, ScrollPointsBuilder, SearchPointsBuilder, UpsertPointsBuilder,
    VectorParamsBuilder, value::Kind,
};

type QdrantResult<T> = Result<T, Box<qdrant_client::QdrantError>>;

/// Thin wrapper over [`Qdrant`] client encapsulating common collection operations.
#[derive(Clone)]
pub struct QdrantOps {
    client: Qdrant,
}

impl std::fmt::Debug for QdrantOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QdrantOps").finish_non_exhaustive()
    }
}

impl QdrantOps {
    /// Create a new `QdrantOps` connected to the given URL.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant client cannot be created.
    pub fn new(url: &str) -> QdrantResult<Self> {
        let client = Qdrant::from_url(url).build().map_err(Box::new)?;
        Ok(Self { client })
    }

    /// Access the underlying Qdrant client for advanced operations.
    #[must_use]
    pub fn client(&self) -> &Qdrant {
        &self.client
    }

    /// Ensure a collection exists with cosine distance vectors.
    ///
    /// Idempotent: no-op if the collection already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant cannot be reached or collection creation fails.
    pub async fn ensure_collection(&self, collection: &str, vector_size: u64) -> QdrantResult<()> {
        if self
            .client
            .collection_exists(collection)
            .await
            .map_err(Box::new)?
        {
            return Ok(());
        }
        self.client
            .create_collection(
                CreateCollectionBuilder::new(collection)
                    .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine)),
            )
            .await
            .map_err(Box::new)?;
        Ok(())
    }

    /// Check whether a collection exists.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant cannot be reached.
    pub async fn collection_exists(&self, collection: &str) -> QdrantResult<bool> {
        self.client
            .collection_exists(collection)
            .await
            .map_err(Box::new)
    }

    /// Delete a collection.
    ///
    /// # Errors
    ///
    /// Returns an error if the collection cannot be deleted.
    pub async fn delete_collection(&self, collection: &str) -> QdrantResult<()> {
        self.client
            .delete_collection(collection)
            .await
            .map_err(Box::new)?;
        Ok(())
    }

    /// Upsert points into a collection.
    ///
    /// # Errors
    ///
    /// Returns an error if the upsert fails.
    pub async fn upsert(&self, collection: &str, points: Vec<PointStruct>) -> QdrantResult<()> {
        self.client
            .upsert_points(UpsertPointsBuilder::new(collection, points))
            .await
            .map_err(Box::new)?;
        Ok(())
    }

    /// Search for similar vectors, returning scored points with payloads.
    ///
    /// # Errors
    ///
    /// Returns an error if the search fails.
    pub async fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: Option<Filter>,
    ) -> QdrantResult<Vec<ScoredPoint>> {
        let mut builder = SearchPointsBuilder::new(collection, vector, limit).with_payload(true);
        if let Some(f) = filter {
            builder = builder.filter(f);
        }
        let results = self.client.search_points(builder).await.map_err(Box::new)?;
        Ok(results.result)
    }

    /// Delete points by their IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if the deletion fails.
    pub async fn delete_by_ids(&self, collection: &str, ids: Vec<PointId>) -> QdrantResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.client
            .delete_points(DeletePointsBuilder::new(collection).points(PointsIdsList { ids }))
            .await
            .map_err(Box::new)?;
        Ok(())
    }

    /// Scroll all points in a collection, extracting string payload fields.
    ///
    /// Returns a map of `key_field` value -> { `field_name` -> `field_value` }.
    ///
    /// # Errors
    ///
    /// Returns an error if the scroll operation fails.
    pub async fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> QdrantResult<HashMap<String, HashMap<String, String>>> {
        let mut result = HashMap::new();
        let mut offset: Option<PointId> = None;

        loop {
            let mut builder = ScrollPointsBuilder::new(collection)
                .with_payload(true)
                .with_vectors(false)
                .limit(100);

            if let Some(ref off) = offset {
                builder = builder.offset(off.clone());
            }

            let response = self.client.scroll(builder).await.map_err(Box::new)?;

            for point in &response.result {
                let Some(key_val) = point.payload.get(key_field) else {
                    continue;
                };
                let Some(Kind::StringValue(key)) = &key_val.kind else {
                    continue;
                };

                let mut fields = HashMap::new();
                for (k, val) in &point.payload {
                    if let Some(Kind::StringValue(s)) = &val.kind {
                        fields.insert(k.clone(), s.clone());
                    }
                }
                result.insert(key.clone(), fields);
            }

            match response.next_page_offset {
                Some(next) => offset = Some(next),
                None => break,
            }
        }

        Ok(result)
    }

    /// Convert a JSON value to a Qdrant payload map.
    ///
    /// # Errors
    ///
    /// Returns a JSON error if deserialization fails.
    pub fn json_to_payload(
        value: serde_json::Value,
    ) -> Result<HashMap<String, qdrant_client::qdrant::Value>, serde_json::Error> {
        serde_json::from_value(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_valid_url() {
        let ops = QdrantOps::new("http://localhost:6334");
        assert!(ops.is_ok());
    }

    #[test]
    fn new_invalid_url() {
        let ops = QdrantOps::new("not a valid url");
        assert!(ops.is_err());
    }

    #[test]
    fn debug_format() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        let dbg = format!("{ops:?}");
        assert!(dbg.contains("QdrantOps"));
    }

    #[test]
    fn json_to_payload_valid() {
        let value = serde_json::json!({"key": "value", "num": 42});
        let result = QdrantOps::json_to_payload(value);
        assert!(result.is_ok());
    }

    #[test]
    fn json_to_payload_empty() {
        let result = QdrantOps::json_to_payload(serde_json::json!({}));
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}
