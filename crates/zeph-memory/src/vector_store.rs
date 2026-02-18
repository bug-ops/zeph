use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

#[derive(Debug, thiserror::Error)]
pub enum VectorStoreError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("collection error: {0}")]
    Collection(String),
    #[error("upsert error: {0}")]
    Upsert(String),
    #[error("search error: {0}")]
    Search(String),
    #[error("delete error: {0}")]
    Delete(String),
    #[error("scroll error: {0}")]
    Scroll(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

#[derive(Debug, Clone)]
pub struct VectorPoint {
    pub id: String,
    pub vector: Vec<f32>,
    pub payload: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct VectorFilter {
    pub must: Vec<FieldCondition>,
    pub must_not: Vec<FieldCondition>,
}

#[derive(Debug, Clone)]
pub struct FieldCondition {
    pub field: String,
    pub value: FieldValue,
}

#[derive(Debug, Clone)]
pub enum FieldValue {
    Integer(i64),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct ScoredVectorPoint {
    pub id: String,
    pub score: f32,
    pub payload: HashMap<String, serde_json::Value>,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type ScrollResult = HashMap<String, HashMap<String, String>>;

pub trait VectorStore: Send + Sync {
    fn ensure_collection(
        &self,
        collection: &str,
        vector_size: u64,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    fn collection_exists(&self, collection: &str) -> BoxFuture<'_, Result<bool, VectorStoreError>>;

    fn delete_collection(&self, collection: &str) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    fn upsert(
        &self,
        collection: &str,
        points: Vec<VectorPoint>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: Option<VectorFilter>,
    ) -> BoxFuture<'_, Result<Vec<ScoredVectorPoint>, VectorStoreError>>;

    fn delete_by_ids(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollResult, VectorStoreError>>;
}
