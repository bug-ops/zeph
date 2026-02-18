use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;

use super::super::{
    DEFAULT_MAX_FILE_SIZE, Document, DocumentError, DocumentLoader, DocumentMetadata,
};

pub struct PdfLoader {
    pub max_file_size: u64,
}

impl Default for PdfLoader {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

impl DocumentLoader for PdfLoader {
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

            let source = path.display().to_string();
            let path_buf = path.to_path_buf();
            let content = tokio::task::spawn_blocking(move || {
                pdf_extract::extract_text(&path_buf).map_err(|e| DocumentError::Pdf(e.to_string()))
            })
            .await
            .map_err(|e| DocumentError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))??;

            Ok(vec![Document {
                content,
                metadata: DocumentMetadata {
                    source,
                    content_type: "application/pdf".to_owned(),
                    extra: HashMap::new(),
                },
            }])
        })
    }

    fn supported_extensions(&self) -> &[&str] {
        &["pdf"]
    }
}
