// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the document ingestion pipeline and RAG context injection (#1028).

/// Ingest a plain text file, query the agent, and verify the chunk appears in agent context.
///
/// Requires a running Qdrant instance and a configured embedding provider.
#[ignore = "requires running Qdrant and embedding provider"]
#[tokio::test]
async fn ingested_document_chunk_appears_in_agent_context() {
    // TODO: call handle_ingest() with a temp .txt file, then run one agent turn,
    //       assert the ingested chunk text is present in the context messages.
    todo!("implement document ingest → RAG context injection integration test")
}
