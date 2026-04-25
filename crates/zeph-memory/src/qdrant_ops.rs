// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Low-level Qdrant operations shared across crates.
//!
//! [`QdrantOps`] is the single point of contact with the `qdrant-client` crate.
//! All higher-level stores ([`crate::embedding_store::EmbeddingStore`],
//! [`crate::embedding_registry::EmbeddingRegistry`]) route through this type.

use std::collections::HashMap;

use crate::vector_store::BoxFuture;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::vector_output::Vector as VectorVariant;
use qdrant_client::qdrant::{
    CreateCollectionBuilder, DeletePointsBuilder, Distance, Filter, GetPointsBuilder, PointId,
    PointStruct, PointsIdsList, ScoredPoint, ScrollPointsBuilder, SearchPointsBuilder,
    UpsertPointsBuilder, VectorParamsBuilder, value::Kind,
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
    // TODO(#2389): add optional `api_key: Option<String>` parameter and wire it via
    // `.with_api_key()` on the builder once the Qdrant config struct exposes the field.
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
    /// If the collection already exists but has a different vector dimension than `vector_size`,
    /// the collection is deleted and recreated. All existing data in the collection is lost.
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
            let existing_size = self.get_collection_vector_size(collection).await?;
            if existing_size == Some(vector_size) {
                return Ok(());
            }
            tracing::warn!(
                collection,
                existing = ?existing_size,
                required = vector_size,
                "vector dimension mismatch — recreating collection (existing data will be lost)"
            );
            self.client
                .delete_collection(collection)
                .await
                .map_err(Box::new)?;
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

    /// Returns the configured vector size of an existing collection, or `None` if it cannot be
    /// determined (e.g. named-vector collections, or `collection_info` fails gracefully).
    ///
    /// # Errors
    ///
    /// Returns an error only on hard Qdrant communication failures.
    async fn get_collection_vector_size(&self, collection: &str) -> QdrantResult<Option<u64>> {
        let info = self
            .client
            .collection_info(collection)
            .await
            .map_err(Box::new)?;
        let size = info
            .result
            .and_then(|r| r.config)
            .and_then(|cfg| cfg.params)
            .and_then(|params| params.vectors_config)
            .and_then(|vc| vc.config)
            .and_then(|cfg| match cfg {
                qdrant_client::qdrant::vectors_config::Config::Params(vp) => Some(vp.size),
                // Named-vector collections are not supported here; treat as unknown.
                qdrant_client::qdrant::vectors_config::Config::ParamsMap(_) => None,
            });
        Ok(size)
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
            .upsert_points(UpsertPointsBuilder::new(collection, points).wait(true))
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
            .delete_points(
                DeletePointsBuilder::new(collection)
                    .points(PointsIdsList { ids })
                    .wait(true),
            )
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

    /// Create a collection with scalar INT8 quantization if it does not exist,
    /// then create keyword indexes for the given fields.
    ///
    /// If the collection already exists but has a different vector dimension than `vector_size`,
    /// the collection is deleted and recreated. All existing data in the collection is lost.
    ///
    /// # Errors
    ///
    /// Returns an error if any Qdrant operation fails.
    pub async fn ensure_collection_with_quantization(
        &self,
        collection: &str,
        vector_size: u64,
        keyword_fields: &[&str],
    ) -> Result<(), crate::VectorStoreError> {
        use qdrant_client::qdrant::{
            CreateFieldIndexCollectionBuilder, FieldType, ScalarQuantizationBuilder,
        };
        if self
            .client
            .collection_exists(collection)
            .await
            .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))?
        {
            let existing_size = self
                .get_collection_vector_size(collection)
                .await
                .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))?;
            if existing_size == Some(vector_size) {
                return Ok(());
            }
            tracing::warn!(
                collection,
                existing = ?existing_size,
                required = vector_size,
                "vector dimension mismatch — recreating collection (existing data will be lost)"
            );
            self.client
                .delete_collection(collection)
                .await
                .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))?;
        }
        self.client
            .create_collection(
                CreateCollectionBuilder::new(collection)
                    .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine))
                    .quantization_config(ScalarQuantizationBuilder::default()),
            )
            .await
            .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))?;

        for field in keyword_fields {
            self.client
                .create_field_index(CreateFieldIndexCollectionBuilder::new(
                    collection,
                    *field,
                    FieldType::Keyword,
                ))
                .await
                .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))?;
        }
        Ok(())
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

impl crate::vector_store::VectorStore for QdrantOps {
    fn ensure_collection(
        &self,
        collection: &str,
        vector_size: u64,
    ) -> BoxFuture<'_, Result<(), crate::VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            self.ensure_collection(&collection, vector_size)
                .await
                .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))
        })
    }

    fn collection_exists(
        &self,
        collection: &str,
    ) -> BoxFuture<'_, Result<bool, crate::VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            self.collection_exists(&collection)
                .await
                .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))
        })
    }

    fn delete_collection(
        &self,
        collection: &str,
    ) -> BoxFuture<'_, Result<(), crate::VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            self.delete_collection(&collection)
                .await
                .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))
        })
    }

    fn upsert(
        &self,
        collection: &str,
        points: Vec<crate::VectorPoint>,
    ) -> BoxFuture<'_, Result<(), crate::VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let qdrant_points: Vec<PointStruct> = points
                .into_iter()
                .map(|p| {
                    let payload: HashMap<String, qdrant_client::qdrant::Value> =
                        serde_json::from_value(serde_json::Value::Object(
                            p.payload.into_iter().collect(),
                        ))
                        .unwrap_or_default();
                    PointStruct::new(p.id, p.vector, payload)
                })
                .collect();
            self.upsert(&collection, qdrant_points)
                .await
                .map_err(|e| crate::VectorStoreError::Upsert(e.to_string()))
        })
    }

    fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: Option<crate::VectorFilter>,
    ) -> BoxFuture<'_, Result<Vec<crate::ScoredVectorPoint>, crate::VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let qdrant_filter = filter.map(vector_filter_to_qdrant);
            let results = self
                .search(&collection, vector, limit, qdrant_filter)
                .await
                .map_err(|e| crate::VectorStoreError::Search(e.to_string()))?;
            Ok(results.into_iter().map(scored_point_to_vector).collect())
        })
    }

    fn delete_by_ids(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<(), crate::VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let point_ids: Vec<PointId> = ids.into_iter().map(PointId::from).collect();
            self.delete_by_ids(&collection, point_ids)
                .await
                .map_err(|e| crate::VectorStoreError::Delete(e.to_string()))
        })
    }

    fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<HashMap<String, HashMap<String, String>>, crate::VectorStoreError>>
    {
        let collection = collection.to_owned();
        let key_field = key_field.to_owned();
        Box::pin(async move {
            self.scroll_all(&collection, &key_field)
                .await
                .map_err(|e| crate::VectorStoreError::Scroll(e.to_string()))
        })
    }

    fn health_check(&self) -> BoxFuture<'_, Result<bool, crate::VectorStoreError>> {
        Box::pin(async move {
            self.client
                .health_check()
                .await
                .map(|_| true)
                .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))
        })
    }

    fn create_keyword_indexes(
        &self,
        collection: &str,
        fields: &[&str],
    ) -> BoxFuture<'_, Result<(), crate::VectorStoreError>> {
        use qdrant_client::qdrant::{CreateFieldIndexCollectionBuilder, FieldType};
        let collection = collection.to_owned();
        let fields: Vec<String> = fields.iter().map(|f| (*f).to_owned()).collect();
        Box::pin(async move {
            for field in &fields {
                self.client
                    .create_field_index(CreateFieldIndexCollectionBuilder::new(
                        &collection,
                        field.as_str(),
                        FieldType::Keyword,
                    ))
                    .await
                    .map_err(|e| crate::VectorStoreError::Collection(e.to_string()))?;
            }
            Ok(())
        })
    }

    fn get_points(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<Vec<crate::VectorPoint>, crate::VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let point_ids: Vec<PointId> = ids.into_iter().map(PointId::from).collect();
            let response = self
                .client
                .get_points(
                    GetPointsBuilder::new(&collection, point_ids)
                        .with_vectors(true)
                        .with_payload(true),
                )
                .await
                .map_err(|e| crate::VectorStoreError::Search(e.to_string()))?;

            let mut result = Vec::with_capacity(response.result.len());
            for point in response.result {
                let Some(id_str) = point_id_to_string(point.id) else {
                    continue;
                };
                // Use VectorsOutput::get_vector() to extract the default dense vector.
                let vector = match point.vectors.and_then(|v| v.get_vector()) {
                    Some(VectorVariant::Dense(dv)) => dv.data,
                    _ => continue,
                };
                let payload: HashMap<String, serde_json::Value> = point
                    .payload
                    .into_iter()
                    .filter_map(|(k, v)| {
                        let json = qdrant_value_to_json(v.kind?)?;
                        Some((k, json))
                    })
                    .collect();
                result.push(crate::VectorPoint {
                    id: id_str,
                    vector,
                    payload,
                });
            }
            Ok(result)
        })
    }
}

fn vector_filter_to_qdrant(filter: crate::VectorFilter) -> Filter {
    let must: Vec<_> = filter
        .must
        .into_iter()
        .map(field_condition_to_qdrant)
        .collect();
    let must_not: Vec<_> = filter
        .must_not
        .into_iter()
        .map(field_condition_to_qdrant)
        .collect();

    let mut f = Filter::default();
    if !must.is_empty() {
        f.must = must;
    }
    if !must_not.is_empty() {
        f.must_not = must_not;
    }
    f
}

fn field_condition_to_qdrant(cond: crate::FieldCondition) -> qdrant_client::qdrant::Condition {
    match cond.value {
        crate::FieldValue::Integer(v) => qdrant_client::qdrant::Condition::matches(cond.field, v),
        crate::FieldValue::Text(v) => qdrant_client::qdrant::Condition::matches(cond.field, v),
    }
}

/// Convert a Qdrant [`qdrant_client::qdrant::PointId`] to its string representation.
///
/// Returns `None` when the id variant is unrecognised.
fn point_id_to_string(pid: Option<qdrant_client::qdrant::PointId>) -> Option<String> {
    match pid?.point_id_options? {
        qdrant_client::qdrant::point_id::PointIdOptions::Uuid(u) => Some(u),
        qdrant_client::qdrant::point_id::PointIdOptions::Num(n) => Some(n.to_string()),
    }
}

/// Convert a Qdrant [`Kind`] to a `serde_json::Value`.
///
/// Returns `None` for unsupported kinds (structs, lists, nulls).
fn qdrant_value_to_json(kind: Kind) -> Option<serde_json::Value> {
    match kind {
        Kind::StringValue(s) => Some(serde_json::Value::String(s)),
        Kind::IntegerValue(i) => Some(serde_json::Value::Number(i.into())),
        Kind::DoubleValue(d) => serde_json::Number::from_f64(d).map(serde_json::Value::Number),
        Kind::BoolValue(b) => Some(serde_json::Value::Bool(b)),
        _ => None,
    }
}

fn scored_point_to_vector(point: ScoredPoint) -> crate::ScoredVectorPoint {
    let payload: HashMap<String, serde_json::Value> = point
        .payload
        .into_iter()
        .filter_map(|(k, v)| Some((k, qdrant_value_to_json(v.kind?)?)))
        .collect();

    let id = point_id_to_string(point.id).unwrap_or_default();

    crate::ScoredVectorPoint {
        id,
        score: point.score,
        payload,
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

    #[test]
    fn delete_by_ids_empty_is_ok_sync() {
        // Constructing QdrantOps with a valid URL succeeds even without a live server.
        // delete_by_ids with empty list short-circuits before any network call.
        // We validate the early-return logic via the async test below.
        let ops = QdrantOps::new("http://localhost:6334");
        assert!(ops.is_ok());
    }

    /// Requires a live Qdrant instance at localhost:6334.
    #[tokio::test]
    #[ignore = "requires a live Qdrant instance at localhost:6334"]
    async fn ensure_collection_with_quantization_idempotent() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        let collection = "test_quant_idempotent";

        // Clean up from any prior run
        let _ = ops.delete_collection(collection).await;

        // First call — creates collection
        ops.ensure_collection_with_quantization(collection, 128, &["language", "file_path"])
            .await
            .unwrap();

        assert!(ops.collection_exists(collection).await.unwrap());

        // Second call — idempotent, must not error
        ops.ensure_collection_with_quantization(collection, 128, &["language", "file_path"])
            .await
            .unwrap();

        // Cleanup
        ops.delete_collection(collection).await.unwrap();
    }

    /// Requires a live Qdrant instance at localhost:6334.
    #[tokio::test]
    #[ignore = "requires a live Qdrant instance at localhost:6334"]
    async fn delete_by_ids_empty_no_network_call() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        // Empty ID list must short-circuit and return Ok without hitting Qdrant.
        let result = ops.delete_by_ids("nonexistent_collection", vec![]).await;
        assert!(result.is_ok());
    }

    /// Requires a live Qdrant instance at localhost:6334.
    #[tokio::test]
    #[ignore = "requires a live Qdrant instance at localhost:6334"]
    async fn ensure_collection_idempotent_same_size() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        let collection = "test_ensure_idempotent";

        let _ = ops.delete_collection(collection).await;

        ops.ensure_collection(collection, 128).await.unwrap();
        assert!(ops.collection_exists(collection).await.unwrap());

        // Second call with same size must be a no-op.
        ops.ensure_collection(collection, 128).await.unwrap();
        assert!(ops.collection_exists(collection).await.unwrap());

        ops.delete_collection(collection).await.unwrap();
    }

    /// Requires a live Qdrant instance at localhost:6334.
    ///
    /// Verifies that `ensure_collection` detects a vector dimension mismatch and
    /// recreates the collection instead of silently reusing the wrong-dimension one.
    #[tokio::test]
    #[ignore = "requires a live Qdrant instance at localhost:6334"]
    async fn ensure_collection_recreates_on_dimension_mismatch() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        let collection = "test_dim_mismatch";

        let _ = ops.delete_collection(collection).await;

        // Create with 128 dims.
        ops.ensure_collection(collection, 128).await.unwrap();
        assert_eq!(
            ops.get_collection_vector_size(collection).await.unwrap(),
            Some(128)
        );

        // Call again with a different size — must recreate.
        ops.ensure_collection(collection, 256).await.unwrap();
        assert_eq!(
            ops.get_collection_vector_size(collection).await.unwrap(),
            Some(256),
            "collection must have been recreated with the new dimension"
        );

        ops.delete_collection(collection).await.unwrap();
    }

    /// Requires a live Qdrant instance at localhost:6334.
    ///
    /// Verifies that `ensure_collection_with_quantization` also detects dimension mismatch.
    #[tokio::test]
    #[ignore = "requires a live Qdrant instance at localhost:6334"]
    async fn ensure_collection_with_quantization_recreates_on_dimension_mismatch() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        let collection = "test_quant_dim_mismatch";

        let _ = ops.delete_collection(collection).await;

        ops.ensure_collection_with_quantization(collection, 128, &["language"])
            .await
            .unwrap();
        assert_eq!(
            ops.get_collection_vector_size(collection).await.unwrap(),
            Some(128)
        );

        // Call again with a different size — must recreate.
        ops.ensure_collection_with_quantization(collection, 384, &["language"])
            .await
            .unwrap();
        assert_eq!(
            ops.get_collection_vector_size(collection).await.unwrap(),
            Some(384),
            "collection must have been recreated with the new dimension"
        );

        ops.delete_collection(collection).await.unwrap();
    }
}
