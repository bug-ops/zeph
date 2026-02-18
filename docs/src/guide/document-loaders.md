# Document Loaders

Zeph supports ingesting user documents (plain text, Markdown, PDF) for retrieval-augmented generation. Documents are loaded, split into chunks, embedded, and stored in Qdrant for semantic recall.

## DocumentLoader Trait

All loaders implement `DocumentLoader`:

```rust
pub trait DocumentLoader: Send + Sync {
    fn load(&self, path: &Path) -> Pin<Box<dyn Future<Output = Result<Vec<Document>, DocumentError>> + Send + '_>>;
    fn supported_extensions(&self) -> &[&str];
}
```

Each `Document` contains `content: String` and `metadata: DocumentMetadata` (source path, content type, extra fields).

## TextLoader

Loads `.txt`, `.md`, and `.markdown` files. Always available (no feature gate).

- Reads files via `tokio::fs::read_to_string`
- Canonicalizes paths via `std::fs::canonicalize` before reading
- Rejects files exceeding `max_file_size` (default 50 MiB) with `DocumentError::FileTooLarge`
- Sets `content_type` to `text/markdown` for `.md`/`.markdown`, `text/plain` otherwise

```rust
let loader = TextLoader::default();
let docs = loader.load(Path::new("notes.md")).await?;
```

## PdfLoader

Extracts text from PDF files using `pdf-extract`. Requires the `pdf` feature:

```bash
cargo build --features pdf
```

Sync extraction is wrapped in `tokio::task::spawn_blocking`. Same `max_file_size` and path canonicalization guards as `TextLoader`.

## TextSplitter

Splits documents into chunks for embedding. Configurable via `SplitterConfig`:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `chunk_size` | 1000 | Maximum characters per chunk |
| `chunk_overlap` | 200 | Overlap between consecutive chunks |
| `sentence_aware` | true | Split on sentence boundaries (`. `, `? `, `! `, `\n\n`) |

When `sentence_aware` is false, splits on character boundaries with overlap.

```rust
let splitter = TextSplitter::new(SplitterConfig {
    chunk_size: 500,
    chunk_overlap: 100,
    sentence_aware: true,
});
let chunks = splitter.split(&document);
```

## IngestionPipeline

Orchestrates the full flow: load → split → embed → store.

```rust
let pipeline = IngestionPipeline::new(
    TextSplitter::new(SplitterConfig::default()),
    qdrant_ops,
    "my_documents",
    Box::new(provider.embed_fn()),
);

// Ingest from a loaded document
let chunk_count = pipeline.ingest(document).await?;

// Or load and ingest in one step
let chunk_count = pipeline.load_and_ingest(&TextLoader::default(), path).await?;
```

Each chunk is stored as a Qdrant point with payload fields: `source`, `content_type`, `chunk_index`, `content`.
