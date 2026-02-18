use testcontainers::ContainerAsync;
use testcontainers::GenericImage;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use zeph_memory::embedding_store::{EmbeddingStore, MessageKind};
use zeph_memory::sqlite::SqliteStore;

const QDRANT_GRPC_PORT: ContainerPort = ContainerPort::Tcp(6334);

fn qdrant_image() -> GenericImage {
    GenericImage::new("qdrant/qdrant", "v1.16.0")
        .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
        .with_exposed_port(QDRANT_GRPC_PORT)
}

async fn setup_with_qdrant() -> (SqliteStore, EmbeddingStore, ContainerAsync<GenericImage>) {
    let container = qdrant_image().start().await.unwrap();
    let grpc_port = container.get_host_port_ipv4(6334).await.unwrap();
    let url = format!("http://127.0.0.1:{grpc_port}");

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let store = EmbeddingStore::new(&url, pool).unwrap();

    (sqlite, store, container)
}

#[tokio::test]
async fn ensure_collection_is_idempotent() {
    let (_sqlite, qdrant, _container) = setup_with_qdrant().await;

    qdrant.ensure_collection(768).await.unwrap();
    qdrant.ensure_collection(768).await.unwrap();
}

#[tokio::test]
async fn store_and_search_vector() {
    let (sqlite, qdrant, _container) = setup_with_qdrant().await;

    let cid = sqlite.create_conversation().await.unwrap();
    let msg_id = sqlite
        .save_message(cid, "user", "hello world")
        .await
        .unwrap();

    qdrant.ensure_collection(4).await.unwrap();

    let vector = vec![0.1, 0.2, 0.3, 0.4];
    let point_id = qdrant
        .store(
            msg_id,
            cid,
            "user",
            vector.clone(),
            MessageKind::Regular,
            "qwen3-embedding",
        )
        .await
        .unwrap();

    assert!(!point_id.is_empty());
    assert!(qdrant.has_embedding(msg_id).await.unwrap());

    let results = qdrant.search(&vector, 10, None).await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].message_id, msg_id);
}

#[tokio::test]
async fn search_with_conversation_filter() {
    let (sqlite, qdrant, _container) = setup_with_qdrant().await;

    let cid1 = sqlite.create_conversation().await.unwrap();
    let cid2 = sqlite.create_conversation().await.unwrap();

    let msg1 = sqlite.save_message(cid1, "user", "first").await.unwrap();
    let msg2 = sqlite.save_message(cid2, "user", "second").await.unwrap();

    qdrant.ensure_collection(4).await.unwrap();

    let v1 = vec![0.1, 0.2, 0.3, 0.4];
    let v2 = vec![0.1, 0.2, 0.3, 0.5];

    qdrant
        .store(
            msg1,
            cid1,
            "user",
            v1,
            MessageKind::Regular,
            "qwen3-embedding",
        )
        .await
        .unwrap();
    qdrant
        .store(
            msg2,
            cid2,
            "user",
            v2,
            MessageKind::Regular,
            "qwen3-embedding",
        )
        .await
        .unwrap();

    let query = vec![0.1, 0.2, 0.3, 0.4];
    let filter = zeph_memory::embedding_store::SearchFilter {
        conversation_id: Some(cid1),
        role: None,
    };

    let results = qdrant.search(&query, 10, Some(filter)).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].conversation_id, cid1);
}
