//! Contextualized embedding text generation.
//!
//! Embedding raw code alone gives poor retrieval quality. Prepending
//! file path, scope chain, language tag, and trimmed imports dramatically
//! improves results for conceptual queries like "where is auth handled?"

use crate::chunker::CodeChunk;

/// Maximum number of import lines included in the embedding text.
const MAX_IMPORT_LINES: usize = 5;

/// Generate text optimized for embedding (not for display).
///
/// Prepends file path, scope chain, imports, and language tag
/// to the raw code.
#[must_use]
pub fn contextualize_for_embedding(chunk: &CodeChunk) -> String {
    let mut text = String::with_capacity(chunk.code.len() + 256);

    text.push_str("# ");
    text.push_str(&chunk.file_path);
    text.push('\n');

    if !chunk.scope_chain.is_empty() {
        text.push_str("# Scope: ");
        text.push_str(&chunk.scope_chain);
        text.push('\n');
    }

    text.push_str("# Language: ");
    text.push_str(chunk.language.id());
    text.push('\n');

    if !chunk.imports.is_empty() {
        let trimmed: String = chunk
            .imports
            .lines()
            .take(MAX_IMPORT_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        text.push_str(&trimmed);
        text.push('\n');
    }

    text.push_str(&chunk.code);
    text
}

/// Generate a short header for display in retrieved results.
#[must_use]
pub fn chunk_display_header(chunk: &CodeChunk) -> String {
    let name = chunk.entity_name.as_deref().unwrap_or(&chunk.node_type);
    format!(
        "{} :: {} (lines {}-{})",
        chunk.file_path, name, chunk.line_range.0, chunk.line_range.1
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages::Lang;

    fn sample_chunk() -> CodeChunk {
        CodeChunk {
            code: "fn hello() { 42 }".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: Lang::Rust,
            node_type: "function_item".to_string(),
            entity_name: Some("hello".to_string()),
            line_range: (1, 3),
            scope_chain: "MyModule > MyStruct".to_string(),
            imports: "use std::io;\nuse std::path::Path;".to_string(),
            content_hash: "abc123".to_string(),
        }
    }

    #[test]
    fn contextualize_includes_file_path() {
        let text = contextualize_for_embedding(&sample_chunk());
        assert!(text.contains("# src/lib.rs"));
    }

    #[test]
    fn contextualize_includes_scope_chain() {
        let text = contextualize_for_embedding(&sample_chunk());
        assert!(text.contains("# Scope: MyModule > MyStruct"));
    }

    #[test]
    fn contextualize_trims_imports_to_max() {
        let mut chunk = sample_chunk();
        chunk.imports = (0..10)
            .map(|i| format!("use dep_{i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = contextualize_for_embedding(&chunk);
        let import_lines: Vec<_> = text.lines().filter(|l| l.starts_with("use ")).collect();
        assert_eq!(import_lines.len(), MAX_IMPORT_LINES);
    }

    #[test]
    fn contextualize_includes_language() {
        let text = contextualize_for_embedding(&sample_chunk());
        assert!(text.contains("# Language: rust"));
    }

    #[test]
    fn contextualize_includes_code() {
        let text = contextualize_for_embedding(&sample_chunk());
        assert!(text.contains("fn hello() { 42 }"));
    }

    #[test]
    fn contextualize_empty_scope_omitted() {
        let mut chunk = sample_chunk();
        chunk.scope_chain = String::new();
        let text = contextualize_for_embedding(&chunk);
        assert!(!text.contains("Scope:"));
    }

    #[test]
    fn contextualize_empty_imports_omitted() {
        let mut chunk = sample_chunk();
        chunk.imports = String::new();
        let text = contextualize_for_embedding(&chunk);
        assert!(!text.contains("use "));
    }

    #[test]
    fn display_header_with_entity_name() {
        let header = chunk_display_header(&sample_chunk());
        assert_eq!(header, "src/lib.rs :: hello (lines 1-3)");
    }

    #[test]
    fn display_header_falls_back_to_node_type() {
        let mut chunk = sample_chunk();
        chunk.entity_name = None;
        let header = chunk_display_header(&chunk);
        assert_eq!(header, "src/lib.rs :: function_item (lines 1-3)");
    }
}
