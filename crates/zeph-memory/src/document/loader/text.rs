use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;

use super::super::{
    DEFAULT_MAX_FILE_SIZE, Document, DocumentError, DocumentLoader, DocumentMetadata,
};

pub struct TextLoader {
    pub max_file_size: u64,
}

impl Default for TextLoader {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

impl DocumentLoader for TextLoader {
    fn load(
        &self,
        path: &Path,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<Document>, DocumentError>> + Send + '_>>
    {
        let path = path.to_path_buf();
        let max_size = self.max_file_size;
        Box::pin(async move {
            let path = std::fs::canonicalize(&path)?;

            let meta = tokio::fs::metadata(&path).await?;
            if meta.len() > max_size {
                return Err(DocumentError::FileTooLarge(meta.len()));
            }

            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

            let content_type = match ext {
                "md" | "markdown" => "text/markdown",
                _ => "text/plain",
            };

            let content = tokio::fs::read_to_string(&path).await?;

            Ok(vec![Document {
                content,
                metadata: DocumentMetadata {
                    source: path.display().to_string(),
                    content_type: content_type.to_owned(),
                    extra: HashMap::new(),
                },
            }])
        })
    }

    fn supported_extensions(&self) -> &[&str] {
        &["txt", "md", "markdown"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello world").unwrap();

        let docs = TextLoader::default().load(&file).await.unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content, "hello world");
        assert_eq!(docs[0].metadata.content_type, "text/plain");
    }

    #[tokio::test]
    async fn load_markdown_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("readme.md");
        std::fs::write(&file, "# Title").unwrap();

        let docs = TextLoader::default().load(&file).await.unwrap();
        assert_eq!(docs[0].metadata.content_type, "text/markdown");
    }

    #[tokio::test]
    async fn load_nonexistent_file() {
        let result = TextLoader::default()
            .load(Path::new("/nonexistent/file.txt"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn load_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.txt");
        std::fs::write(&file, "").unwrap();

        let docs = TextLoader::default().load(&file).await.unwrap();
        assert_eq!(docs.len(), 1);
        assert!(docs[0].content.is_empty());
    }

    #[tokio::test]
    async fn load_markdown_extension_variant() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("doc.markdown");
        std::fs::write(&file, "content").unwrap();

        let docs = TextLoader::default().load(&file).await.unwrap();
        assert_eq!(docs[0].metadata.content_type, "text/markdown");
    }

    #[tokio::test]
    async fn unknown_extension_treated_as_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.csv");
        std::fs::write(&file, "a,b,c").unwrap();

        let docs = TextLoader::default().load(&file).await.unwrap();
        assert_eq!(docs[0].metadata.content_type, "text/plain");
    }

    #[test]
    fn supported_extensions_list() {
        let loader = TextLoader::default();
        let exts = loader.supported_extensions();
        assert!(exts.contains(&"txt"));
        assert!(exts.contains(&"md"));
        assert!(exts.contains(&"markdown"));
    }

    #[tokio::test]
    async fn metadata_source_is_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "data").unwrap();

        let docs = TextLoader::default().load(&file).await.unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        assert_eq!(docs[0].metadata.source, canonical.display().to_string());
    }

    #[tokio::test]
    async fn file_too_large_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        std::fs::write(&file, "x").unwrap();

        let loader = TextLoader { max_file_size: 0 };
        let result = loader.load(&file).await;
        assert!(matches!(result, Err(DocumentError::FileTooLarge(_))));
    }
}
