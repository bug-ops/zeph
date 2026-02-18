pub mod error;
pub mod loader;
pub mod pipeline;
pub mod splitter;
pub mod types;

pub use error::DocumentError;
pub use loader::TextLoader;
pub use pipeline::IngestionPipeline;
pub use splitter::{SplitterConfig, TextSplitter};
pub use types::{Chunk, Document, DocumentMetadata};

#[cfg(feature = "pdf")]
pub use loader::PdfLoader;

/// Default maximum file size: 50 MiB.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;

pub trait DocumentLoader: Send + Sync {
    fn load(
        &self,
        path: &std::path::Path,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Document>, DocumentError>> + Send + '_>,
    >;

    fn supported_extensions(&self) -> &[&str];
}
