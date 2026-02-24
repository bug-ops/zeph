// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct DocumentMetadata {
    pub source: String,
    pub content_type: String,
    pub extra: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub content: String,
    pub metadata: DocumentMetadata,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub content: String,
    pub metadata: DocumentMetadata,
    pub chunk_index: usize,
}
