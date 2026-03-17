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

## CLI ingestion

Documents are ingested from the command line with the `zeph ingest` subcommand:

```bash
zeph ingest ./docs/                          # ingest directory recursively
zeph ingest README.md --chunk-size 256       # custom chunk size
zeph ingest ./knowledge --collection my_kb  # custom Qdrant collection
```

Options:

| Flag | Default | Description |
|------|---------|-------------|
| `--chunk-size <N>` | `512` | Target character count per chunk |
| `--chunk-overlap <N>` | `64` | Overlap between consecutive chunks |
| `--collection <NAME>` | `zeph_documents` | Qdrant collection to store chunks |

TUI users can trigger ingestion via the command palette: `/ingest <path>`.

## RAG context injection

When `memory.documents.rag_enabled = true`, the agent automatically queries the `zeph_documents` Qdrant collection on each turn and prepends the top-K most relevant chunks to the context window under a `## Relevant documents` heading.

```toml
[memory.documents]
rag_enabled = true
collection = "zeph_documents"
chunk_size = 512
chunk_overlap = 64
top_k = 3
```

RAG injection is a no-op when the collection is empty — no error is raised, the agent simply skips the retrieval step.

> [!TIP]
> Run `zeph ingest ./docs/` once to populate the knowledge base. Subsequent agent sessions will automatically retrieve and inject relevant chunks without any additional setup.
