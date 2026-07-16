// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Text rendering for MCP [`ContentBlock`] results.
//!
//! rmcp 2.0 unifies tool-result content into [`ContentBlock`] (replacing the old
//! text-only `RawContent`). This module renders the full union — text, image, audio,
//! embedded resource, resource link — to a `String` so callers no longer silently drop
//! non-text content. It stays within the existing String-centric `ToolOutput` contract:
//! binary payloads (image/audio bytes, blob resources) are never decoded or inlined, only
//! described as `[kind: mime, N bytes]` placeholders. True multimodal passthrough (decoding
//! `ContentBlock::Image` into an LLM-facing `MessagePart::Image`) is a separate, deferred
//! epic — see the tracking issue referenced in the crate changelog.

use rmcp::model::{ContentBlock, ResourceContents};
use zeph_common::text::{truncate_to_bytes, truncate_to_bytes_ref};

/// Per-block render cap, in bytes.
///
/// Mirrors the order of magnitude of `zeph_sanitizer::ContentSanitizer`'s default
/// `max_content_size` (65536 bytes) divided across multiple blocks, so a single
/// oversized block cannot dominate the pre-sanitizer string.
const MAX_BLOCK_RENDER_BYTES: usize = 8192;

/// Maximum number of content blocks individually rendered.
///
/// Extra blocks are summarized as a single "N more blocks omitted" marker instead of
/// being rendered one by one — bounds the work a server can force by returning an
/// unbounded number of content blocks in a single tool result.
const MAX_RENDERED_BLOCKS: usize = 64;

/// Render a single [`ContentBlock`] to a human-readable text placeholder.
///
/// `Text` blocks render as-is (truncated to `MAX_BLOCK_RENDER_BYTES`). Binary content
/// (`Image`, `Audio`, blob `Resource`) never has its payload inlined — only a
/// `[kind: mime, N bytes]` placeholder, where `N` is the encoded payload length. Unknown
/// variants (the enum is `#[non_exhaustive]`) render as a neutral placeholder rather than
/// panicking.
///
/// # Examples
///
/// ```
/// use rmcp::model::ContentBlock;
/// use zeph_mcp::render_content_block;
///
/// let block = ContentBlock::text("hello");
/// assert_eq!(render_content_block(&block), "hello");
///
/// let image = ContentBlock::image("YmFzZTY0", "image/png");
/// assert_eq!(render_content_block(&image), "[image: image/png, 8 bytes]");
/// ```
#[must_use]
pub fn render_content_block(block: &ContentBlock) -> String {
    let rendered = match block {
        // Bounded-prefix borrow, not a full clone — a single oversized text block must not
        // cost O(input size) to render before the final truncate_to_bytes backstop below.
        ContentBlock::Text(t) => truncate_to_bytes_ref(&t.text, MAX_BLOCK_RENDER_BYTES).to_owned(),
        ContentBlock::Image(img) => {
            format!("[image: {}, {} bytes]", img.mime_type, img.data.len())
        }
        // TODO(#5366): Audio/blob/resource-link MCP passthrough deferred — Audio needs
        // Ask-First MessagePart::Audio variant (invariant #4); see specs/072.
        ContentBlock::Audio(audio) => {
            format!("[audio: {}, {} bytes]", audio.mime_type, audio.data.len())
        }
        ContentBlock::Resource(res) => render_resource_contents(&res.resource),
        ContentBlock::ResourceLink(link) => {
            format!("[resource_link: {} ({})]", link.uri, link.name)
        }
        _ => "[unsupported content block]".to_owned(),
    };
    truncate_to_bytes(&rendered, MAX_BLOCK_RENDER_BYTES)
}

/// Render the inner contents of an embedded [`ContentBlock::Resource`].
fn render_resource_contents(resource: &ResourceContents) -> String {
    match resource {
        ResourceContents::TextResourceContents { uri, text, .. } => {
            // Truncate before formatting — same rationale as the Text block branch above.
            let text = truncate_to_bytes_ref(text, MAX_BLOCK_RENDER_BYTES);
            format!("[resource: {uri}]\n{text}")
        }
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => {
            let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
            format!("[resource: {uri} {mime} {} bytes]", blob.len())
        }
        _ => "[unsupported resource content]".to_owned(),
    }
}

/// Render a full MCP tool result's content blocks to a single joined string.
///
/// Each block is rendered via [`render_content_block`] (truncated, no binary payloads
/// inlined) and joined with `\n`. An empty slice renders as an empty string. Blocks beyond
/// `MAX_RENDERED_BLOCKS` are summarized as a single omitted-count marker instead of
/// being individually rendered.
///
/// Call this **before** any prompt-injection wrapping (e.g. `intent_anchor_wrap`) — every
/// field surfaced here (URIs, MIME types, resource names) is untrusted MCP server content
/// and must land inside the same trust boundary as today's text-only rendering.
///
/// # Examples
///
/// ```
/// use rmcp::model::ContentBlock;
/// use zeph_mcp::render_content_blocks;
///
/// let blocks = vec![ContentBlock::text("a"), ContentBlock::text("b")];
/// assert_eq!(render_content_blocks(&blocks), "a\nb");
/// assert_eq!(render_content_blocks(&[]), "");
/// ```
#[must_use]
pub fn render_content_blocks(content: &[ContentBlock]) -> String {
    if content.is_empty() {
        return String::new();
    }
    let total = content.len();
    let rendered_count = total.min(MAX_RENDERED_BLOCKS);
    let mut parts: Vec<String> = content[..rendered_count]
        .iter()
        .map(render_content_block)
        .collect();
    if total > MAX_RENDERED_BLOCKS {
        parts.push(format!(
            "[{} more content block(s) omitted]",
            total - MAX_RENDERED_BLOCKS
        ));
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Resource;

    #[test]
    fn renders_empty_slice_as_empty_string() {
        assert_eq!(render_content_blocks(&[]), "");
    }

    #[test]
    fn renders_text_block_as_is() {
        let block = ContentBlock::text("hello world");
        assert_eq!(render_content_block(&block), "hello world");
    }

    #[test]
    fn joins_mixed_blocks_with_newline() {
        let blocks = vec![
            ContentBlock::text("first"),
            ContentBlock::image("ZGF0YQ==", "image/png"),
        ];
        let rendered = render_content_blocks(&blocks);
        assert_eq!(rendered, "first\n[image: image/png, 8 bytes]");
    }

    #[test]
    fn renders_image_placeholder_without_payload() {
        let block = ContentBlock::image("c2VjcmV0Ynl0ZXM=", "image/jpeg");
        let rendered = render_content_block(&block);
        assert!(rendered.starts_with("[image: image/jpeg,"));
        assert!(rendered.ends_with("bytes]"));
        assert!(!rendered.contains("c2VjcmV0Ynl0ZXM="));
    }

    #[test]
    fn renders_audio_placeholder_without_payload() {
        let block = ContentBlock::audio("c2VjcmV0YXVkaW8=", "audio/wav");
        let rendered = render_content_block(&block);
        assert!(rendered.starts_with("[audio: audio/wav,"));
        assert!(!rendered.contains("c2VjcmV0YXVkaW8="));
    }

    #[test]
    fn renders_embedded_text_resource_with_uri_and_text() {
        let block = ContentBlock::embedded_text("file:///notes.txt", "important resource content");
        let rendered = render_content_block(&block);
        assert!(rendered.contains("file:///notes.txt"));
        assert!(rendered.contains("important resource content"));
    }

    #[test]
    fn renders_blob_resource_as_byte_count_placeholder() {
        let block = ContentBlock::resource(ResourceContents::blob("Ymxvb2I=", "file:///x.bin"));
        let rendered = render_content_block(&block);
        assert!(rendered.starts_with("[resource: file:///x.bin"));
        assert!(rendered.ends_with("bytes]"));
        assert!(!rendered.contains("Ymxvb2I="));
    }

    #[test]
    fn renders_resource_link_with_uri_and_name() {
        let resource = Resource::new("file:///report.pdf", "report.pdf");
        let block = ContentBlock::resource_link(resource);
        let rendered = render_content_block(&block);
        assert_eq!(rendered, "[resource_link: file:///report.pdf (report.pdf)]");
    }

    #[test]
    fn truncates_oversized_text_block() {
        let huge = "a".repeat(MAX_BLOCK_RENDER_BYTES * 2);
        let block = ContentBlock::text(huge);
        let rendered = render_content_block(&block);
        assert_eq!(rendered.len(), MAX_BLOCK_RENDER_BYTES);
    }

    #[test]
    fn text_block_at_exactly_cap_is_not_truncated() {
        let exact = "a".repeat(MAX_BLOCK_RENDER_BYTES);
        let block = ContentBlock::text(exact.clone());
        let rendered = render_content_block(&block);
        assert_eq!(rendered.len(), MAX_BLOCK_RENDER_BYTES);
        assert_eq!(rendered, exact);
    }

    #[test]
    fn text_block_one_byte_over_cap_truncates_to_exactly_cap() {
        let over = "a".repeat(MAX_BLOCK_RENDER_BYTES + 1);
        let block = ContentBlock::text(over);
        let rendered = render_content_block(&block);
        assert_eq!(rendered.len(), MAX_BLOCK_RENDER_BYTES);
    }

    #[test]
    fn caps_total_blocks_rendered() {
        let blocks: Vec<ContentBlock> = (0..(MAX_RENDERED_BLOCKS + 10))
            .map(|i| ContentBlock::text(format!("block-{i}")))
            .collect();
        let rendered = render_content_blocks(&blocks);
        assert!(rendered.contains("10 more content block(s) omitted"));
        // Only MAX_RENDERED_BLOCKS individual blocks plus the marker line are present.
        assert_eq!(rendered.lines().count(), MAX_RENDERED_BLOCKS + 1);
    }

    #[test]
    fn exactly_max_rendered_blocks_has_no_omitted_marker() {
        let blocks: Vec<ContentBlock> = (0..MAX_RENDERED_BLOCKS)
            .map(|i| ContentBlock::text(format!("block-{i}")))
            .collect();
        let rendered = render_content_blocks(&blocks);
        assert!(!rendered.contains("omitted"));
        assert_eq!(rendered.lines().count(), MAX_RENDERED_BLOCKS);
    }

    #[test]
    fn one_block_over_cap_renders_cap_blocks_plus_single_omitted_marker() {
        let blocks: Vec<ContentBlock> = (0..=MAX_RENDERED_BLOCKS)
            .map(|i| ContentBlock::text(format!("block-{i}")))
            .collect();
        let rendered = render_content_blocks(&blocks);
        assert!(rendered.contains("1 more content block(s) omitted"));
        assert_eq!(rendered.lines().count(), MAX_RENDERED_BLOCKS + 1);
    }
}
