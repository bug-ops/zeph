// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use zeph_index::chunker::{ChunkerConfig, chunk_file};
use zeph_index::languages::Lang;

const LANGS: [Lang; 9] = [
    Lang::Rust,
    Lang::Python,
    Lang::JavaScript,
    Lang::TypeScript,
    Lang::Go,
    Lang::Bash,
    Lang::Toml,
    Lang::Json,
    Lang::Markdown,
];

/// Structured fuzz input. `libfuzzer-sys` decodes this via `Arbitrary::arbitrary_take_rest`,
/// which reads `lang_selector` with plain `arbitrary()` (1 byte from the front), then — since
/// `source` is the last field — decodes it with `String::arbitrary_take_rest`: all remaining
/// bytes, taken verbatim as the longest valid UTF-8 prefix. There is no length prefix anywhere
/// in this layout. Seed files must follow this exact byte layout — see `fuzz/README.md`.
#[derive(Arbitrary, Debug)]
struct Input {
    lang_selector: u8,
    source: String,
}

fuzz_target!(|input: Input| {
    let lang = LANGS[(input.lang_selector as usize) % LANGS.len()];
    let _ = chunk_file(&input.source, "fuzz_input", lang, &ChunkerConfig::default());
});
