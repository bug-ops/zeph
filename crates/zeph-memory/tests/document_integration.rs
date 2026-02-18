use std::collections::HashMap;

use testcontainers::GenericImage;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use zeph_memory::QdrantOps;
use zeph_memory::document::{
    Document, DocumentMetadata, IngestionPipeline, SplitterConfig, TextLoader, TextSplitter,
};

const QDRANT_GRPC_PORT: ContainerPort = ContainerPort::Tcp(6334);
const COLLECTION: &str = "test_documents";
const VECTOR_SIZE: u64 = 4;

fn qdrant_image() -> GenericImage {
    GenericImage::new("qdrant/qdrant", "v1.16.0")
        .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
        .with_exposed_port(QDRANT_GRPC_PORT)
}

fn fake_embed_fn() -> Box<dyn Fn(&str) -> zeph_llm::provider::EmbedFuture + Send + Sync> {
    Box::new(|text: &str| {
        let len = text.len() as f32;
        Box::pin(async move { Ok(vec![len / 1000.0, 0.1, 0.2, 0.3]) })
    })
}

fn make_doc(content: &str) -> Document {
    Document {
        content: content.to_owned(),
        metadata: DocumentMetadata {
            source: "test.txt".to_owned(),
            content_type: "text/plain".to_owned(),
            extra: HashMap::new(),
        },
    }
}

#[tokio::test]
async fn ingest_single_document() {
    let container = qdrant_image().start().await.unwrap();
    let port = container.get_host_port_ipv4(6334).await.unwrap();
    let qdrant = QdrantOps::new(&format!("http://127.0.0.1:{port}")).unwrap();
    qdrant
        .ensure_collection(COLLECTION, VECTOR_SIZE)
        .await
        .unwrap();

    let pipeline = IngestionPipeline::new(
        TextSplitter::new(SplitterConfig::default()),
        qdrant.clone(),
        COLLECTION,
        fake_embed_fn(),
    );

    let doc = make_doc("Hello world. This is a test document.");
    let count = pipeline.ingest(doc).await.unwrap();
    assert_eq!(count, 1); // small doc = single chunk

    let results = qdrant
        .search(COLLECTION, vec![0.036, 0.1, 0.2, 0.3], 10, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn ingest_empty_document_returns_zero() {
    let container = qdrant_image().start().await.unwrap();
    let port = container.get_host_port_ipv4(6334).await.unwrap();
    let qdrant = QdrantOps::new(&format!("http://127.0.0.1:{port}")).unwrap();
    qdrant
        .ensure_collection(COLLECTION, VECTOR_SIZE)
        .await
        .unwrap();

    let pipeline = IngestionPipeline::new(
        TextSplitter::new(SplitterConfig::default()),
        qdrant,
        COLLECTION,
        fake_embed_fn(),
    );

    let count = pipeline.ingest(make_doc("")).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn ingest_multi_chunk_document() {
    let container = qdrant_image().start().await.unwrap();
    let port = container.get_host_port_ipv4(6334).await.unwrap();
    let qdrant = QdrantOps::new(&format!("http://127.0.0.1:{port}")).unwrap();
    qdrant
        .ensure_collection(COLLECTION, VECTOR_SIZE)
        .await
        .unwrap();

    let pipeline = IngestionPipeline::new(
        TextSplitter::new(SplitterConfig {
            chunk_size: 20,
            chunk_overlap: 5,
            sentence_aware: true,
        }),
        qdrant.clone(),
        COLLECTION,
        fake_embed_fn(),
    );

    let doc = make_doc("First sentence. Second sentence. Third sentence. Fourth sentence.");
    let count = pipeline.ingest(doc).await.unwrap();
    assert!(count > 1, "expected multiple chunks, got {count}");

    let results = qdrant
        .search(COLLECTION, vec![0.0, 0.1, 0.2, 0.3], 100, None)
        .await
        .unwrap();
    assert_eq!(results.len(), count);
}

#[tokio::test]
async fn load_and_ingest_text_file() {
    let container = qdrant_image().start().await.unwrap();
    let port = container.get_host_port_ipv4(6334).await.unwrap();
    let qdrant = QdrantOps::new(&format!("http://127.0.0.1:{port}")).unwrap();
    qdrant
        .ensure_collection(COLLECTION, VECTOR_SIZE)
        .await
        .unwrap();

    let pipeline = IngestionPipeline::new(
        TextSplitter::new(SplitterConfig::default()),
        qdrant.clone(),
        COLLECTION,
        fake_embed_fn(),
    );

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("readme.md");
    std::fs::write(&file, "# Hello\n\nThis is a test markdown file.").unwrap();

    let loader = TextLoader::default();
    let count = pipeline.load_and_ingest(&loader, &file).await.unwrap();
    assert_eq!(count, 1);

    let results = qdrant
        .search(COLLECTION, vec![0.0, 0.1, 0.2, 0.3], 10, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn ingested_chunks_have_correct_payload() {
    let container = qdrant_image().start().await.unwrap();
    let port = container.get_host_port_ipv4(6334).await.unwrap();
    let qdrant = QdrantOps::new(&format!("http://127.0.0.1:{port}")).unwrap();
    let collection = "test_payload";
    qdrant
        .ensure_collection(collection, VECTOR_SIZE)
        .await
        .unwrap();

    let pipeline = IngestionPipeline::new(
        TextSplitter::new(SplitterConfig::default()),
        qdrant.clone(),
        collection,
        fake_embed_fn(),
    );

    let doc = make_doc("Some content for payload verification.");
    pipeline.ingest(doc).await.unwrap();

    let all = qdrant.scroll_all(collection, "source").await.unwrap();
    assert_eq!(all.len(), 1);

    let entry = all.values().next().unwrap();
    assert_eq!(entry.get("source").unwrap(), "test.txt");
    assert_eq!(entry.get("content_type").unwrap(), "text/plain");
    assert!(entry.contains_key("content"));
    // chunk_index is stored as integer, scroll_all only extracts string fields
}
