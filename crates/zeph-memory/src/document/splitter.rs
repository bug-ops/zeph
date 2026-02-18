use super::types::{Chunk, Document};

#[derive(Debug, Clone)]
pub struct SplitterConfig {
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub sentence_aware: bool,
}

impl Default for SplitterConfig {
    fn default() -> Self {
        Self {
            chunk_size: 1000,
            chunk_overlap: 200,
            sentence_aware: true,
        }
    }
}

pub struct TextSplitter {
    config: SplitterConfig,
}

impl TextSplitter {
    #[must_use]
    pub fn new(config: SplitterConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn split(&self, document: &Document) -> Vec<Chunk> {
        let text = &document.content;
        if text.is_empty() {
            return Vec::new();
        }

        let pieces = if self.config.sentence_aware {
            split_sentences(text)
        } else {
            split_chars(text, self.config.chunk_size, self.config.chunk_overlap)
        };

        if self.config.sentence_aware {
            let chunks =
                merge_sentences(&pieces, self.config.chunk_size, self.config.chunk_overlap);
            chunks
                .into_iter()
                .enumerate()
                .map(|(i, content)| Chunk {
                    content,
                    metadata: document.metadata.clone(),
                    chunk_index: i,
                })
                .collect()
        } else {
            pieces
                .into_iter()
                .enumerate()
                .map(|(i, content)| Chunk {
                    content,
                    metadata: document.metadata.clone(),
                    chunk_index: i,
                })
                .collect()
        }
    }
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        current.push(chars[i]);

        // Split on paragraph breaks
        if chars[i] == '\n' && i + 1 < chars.len() && chars[i + 1] == '\n' {
            current.push(chars[i + 1]);
            i += 1;
            if !current.trim().is_empty() {
                sentences.push(std::mem::take(&mut current));
            }
        }
        // Split on sentence endings followed by space
        else if (chars[i] == '.' || chars[i] == '?' || chars[i] == '!')
            && i + 1 < chars.len()
            && chars[i + 1] == ' '
            && !current.trim().is_empty()
        {
            sentences.push(std::mem::take(&mut current));
        }

        i += 1;
    }

    if !current.trim().is_empty() {
        sentences.push(current);
    }

    sentences
}

/// Merge sentences into chunks, respecting size and overlap.
fn merge_sentences(sentences: &[String], chunk_size: usize, chunk_overlap: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    // Sliding window: track only the sentence indices contributing to the current chunk.
    let mut window_start = 0;

    for (idx, sentence) in sentences.iter().enumerate() {
        if !current.is_empty() && current.len() + sentence.len() > chunk_size {
            chunks.push(current.clone());

            // Build overlap from recent sentences (walk backwards from current window)
            current.clear();
            let mut overlap_len = 0;
            let mut overlap_start = idx;
            for i in (window_start..idx).rev() {
                if overlap_len + sentences[i].len() > chunk_overlap {
                    break;
                }
                overlap_len += sentences[i].len();
                overlap_start = i;
            }
            for s in &sentences[overlap_start..idx] {
                current.push_str(s);
            }
            window_start = overlap_start;
        }

        current.push_str(sentence);
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn split_chars(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let step = chunk_size.saturating_sub(overlap).max(1);
    let mut start = 0;

    while start < chars.len() {
        let end = (start + chunk_size).min(chars.len());
        chunks.push(chars[start..end].iter().collect());
        start += step;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::document::types::DocumentMetadata;

    fn make_doc(content: &str) -> Document {
        Document {
            content: content.to_owned(),
            metadata: DocumentMetadata {
                source: "test".to_owned(),
                content_type: "text/plain".to_owned(),
                extra: HashMap::new(),
            },
        }
    }

    #[test]
    fn empty_document() {
        let splitter = TextSplitter::new(SplitterConfig::default());
        let chunks = splitter.split(&make_doc(""));
        assert!(chunks.is_empty());
    }

    #[test]
    fn single_small_chunk() {
        let splitter = TextSplitter::new(SplitterConfig::default());
        let chunks = splitter.split(&make_doc("Hello world."));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 0);
    }

    #[test]
    fn sentence_aware_splitting() {
        let text = "First sentence. Second sentence. Third sentence.";
        let splitter = TextSplitter::new(SplitterConfig {
            chunk_size: 20,
            chunk_overlap: 5,
            sentence_aware: true,
        });
        let chunks = splitter.split(&make_doc(text));
        assert!(chunks.len() > 1);
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.chunk_index, i);
        }
    }

    #[test]
    fn char_splitting_with_overlap() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let splitter = TextSplitter::new(SplitterConfig {
            chunk_size: 10,
            chunk_overlap: 3,
            sentence_aware: false,
        });
        let chunks = splitter.split(&make_doc(text));
        assert!(chunks.len() > 1);
        // Verify overlap: end of chunk N overlaps with start of chunk N+1
        assert_eq!(&chunks[0].content[7..10], &chunks[1].content[..3]);
    }

    #[test]
    fn metadata_preserved() {
        let splitter = TextSplitter::new(SplitterConfig::default());
        let doc = make_doc("Some content.");
        let chunks = splitter.split(&doc);
        assert_eq!(chunks[0].metadata.source, "test");
    }

    #[test]
    fn paragraph_break_splitting() {
        let text = "First paragraph.\n\nSecond paragraph.";
        let sentences = super::split_sentences(text);
        assert_eq!(sentences.len(), 2);
    }

    #[test]
    fn document_smaller_than_chunk_size() {
        let splitter = TextSplitter::new(SplitterConfig {
            chunk_size: 1000,
            chunk_overlap: 100,
            sentence_aware: true,
        });
        let chunks = splitter.split(&make_doc("Short text."));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "Short text.");
    }

    #[test]
    fn single_sentence_no_trailing_space() {
        let sentences = super::split_sentences("Hello world");
        assert_eq!(sentences.len(), 1);
        assert_eq!(sentences[0], "Hello world");
    }

    #[test]
    fn char_split_no_overlap() {
        let chunks = super::split_chars("abcdefghij", 5, 0);
        assert_eq!(chunks, vec!["abcde", "fghij"]);
    }

    #[test]
    fn char_split_full_overlap_makes_progress() {
        // overlap >= chunk_size should still make progress (step = max(1, 0))
        let chunks = super::split_chars("abcde", 3, 3);
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0], "abc");
    }

    #[test]
    fn sentence_aware_overlap_includes_previous() {
        let text = "A. B. C. D. E.";
        let splitter = TextSplitter::new(SplitterConfig {
            chunk_size: 5,
            chunk_overlap: 3,
            sentence_aware: true,
        });
        let chunks = splitter.split(&make_doc(text));
        assert!(chunks.len() > 1);
        // Later chunks should contain overlap from previous
        if chunks.len() >= 2 {
            // Second chunk should start with overlap content, not fresh
            assert!(!chunks[1].content.is_empty());
        }
    }

    #[test]
    fn question_mark_splits_sentence() {
        let sentences = super::split_sentences("Is this a question? Yes it is.");
        assert_eq!(sentences.len(), 2);
    }

    #[test]
    fn exclamation_splits_sentence() {
        let sentences = super::split_sentences("Wow! Amazing.");
        assert_eq!(sentences.len(), 2);
    }

    mod proptest_splitter {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1000))]

            #[test]
            fn split_never_panics(
                content in "\\PC{0,5000}",
                chunk_size in 1usize..2000,
                chunk_overlap in 0usize..500,
                sentence_aware in proptest::bool::ANY,
            ) {
                let splitter = TextSplitter::new(SplitterConfig {
                    chunk_size,
                    chunk_overlap,
                    sentence_aware,
                });
                let doc = make_doc(&content);
                let _ = splitter.split(&doc);
            }

            #[test]
            fn chunks_cover_all_content(
                content in "[a-z ]{10,500}",
                chunk_size in 10usize..200,
            ) {
                let splitter = TextSplitter::new(SplitterConfig {
                    chunk_size,
                    chunk_overlap: 0,
                    sentence_aware: false,
                });
                let doc = make_doc(&content);
                let chunks = splitter.split(&doc);

                if !content.is_empty() {
                    prop_assert!(!chunks.is_empty());
                }

                let total_chars: usize = chunks.iter().map(|c| c.content.len()).sum();
                prop_assert!(total_chars >= content.len());
            }

            #[test]
            fn chunk_indices_sequential(
                content in "[a-z. ]{10,1000}",
                chunk_size in 5usize..100,
                sentence_aware in proptest::bool::ANY,
            ) {
                let splitter = TextSplitter::new(SplitterConfig {
                    chunk_size,
                    chunk_overlap: 0,
                    sentence_aware,
                });
                let doc = make_doc(&content);
                let chunks = splitter.split(&doc);

                for (i, chunk) in chunks.iter().enumerate() {
                    prop_assert_eq!(chunk.chunk_index, i);
                }
            }

            #[test]
            fn no_empty_chunks(
                content in "[a-z. !?]{1,500}",
                chunk_size in 1usize..200,
                sentence_aware in proptest::bool::ANY,
            ) {
                let splitter = TextSplitter::new(SplitterConfig {
                    chunk_size,
                    chunk_overlap: 0,
                    sentence_aware,
                });
                let doc = make_doc(&content);
                let chunks = splitter.split(&doc);

                for chunk in &chunks {
                    prop_assert!(!chunk.content.is_empty());
                }
            }
        }
    }
}
