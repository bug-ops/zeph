// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Escaping of prompt delimiter tags inside untrusted content.
//!
//! Prompt sections wrapped in pseudo-XML tags (`<tool-output>`, `<shared-state>`, ...) are
//! only safe while content cannot forge the closing tag. [`escape_delimiter_tags`] neutralizes
//! a tag even when it is disguised with whitespace, zero-width or control characters,
//! fullwidth or small-form brackets, or compatibility ligatures that NFKC expands
//! (`</shared-ﬆate>`).
//!
//! # Matching model
//!
//! Only characters that can open a tag (`<` and its lookalikes) start a match, so text without
//! such a character stays on a zero-allocation scan. After a bracket candidate, the
//! following characters are folded: whitespace and everything stripped by
//! [`crate::sanitize::strip_control_chars_preserve_whitespace`] is skipped, non-ASCII
//! characters are expanded to their full NFKC form, ASCII is lower-cased, and dash/slash
//! lookalikes are mapped to `-` and `/`. A tag matches as a prefix of the folded stream and
//! only the bracket is replaced with `&lt;`.
//!
//! Cyrillic and Greek homoglyphs of Latin letters are a known residual: they survive NFKC
//! unchanged and are not folded.

use std::borrow::Cow;

use unicode_normalization::UnicodeNormalization;

use crate::sanitize::is_stripped_char;

/// A pseudo-XML delimiter tag that untrusted content must not be able to forge.
///
/// The set is closed: every tag the prompt builders emit has exactly one variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DelimiterTag {
    /// `<tool-output>` spotlighting wrapper.
    ToolOutput,
    /// `<external-data>` spotlighting wrapper.
    ExternalData,
    /// `<shared-state>` prompt block built from the cross-thread store.
    SharedState,
    /// `<agent-memory>` wrapper around subagent `MEMORY.md` content.
    AgentMemory,
    /// `<runtime-note>` per-iteration agent state note.
    RuntimeNote,
}

impl DelimiterTag {
    /// Every tag.
    pub const ALL: [Self; 5] = [
        Self::ToolOutput,
        Self::ExternalData,
        Self::SharedState,
        Self::AgentMemory,
        Self::RuntimeNote,
    ];

    /// Tags forged by untrusted tool, web, memory and store content.
    ///
    /// Excludes [`Self::RuntimeNote`]: that tag is guarded by a per-session nonce, so trusted
    /// tool output that legitimately contains the literal tag text stays byte-exact.
    pub const UNTRUSTED_CONTENT: [Self; 3] =
        [Self::ToolOutput, Self::ExternalData, Self::SharedState];

    /// Lower-case tag name without angle brackets.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ToolOutput => "tool-output",
            Self::ExternalData => "external-data",
            Self::SharedState => "shared-state",
            Self::AgentMemory => "agent-memory",
            Self::RuntimeNote => "runtime-note",
        }
    }
}

/// Longest [`DelimiterTag::name`], derived so a new tag can never outgrow the matcher window.
const MAX_TAG_NAME_LEN: usize = {
    let mut max = 0;
    let mut i = 0;
    while i < DelimiterTag::ALL.len() {
        let len = DelimiterTag::ALL[i].name().len();
        if len > max {
            max = len;
        }
        i += 1;
    }
    max
};
/// One byte for an optional `/` plus the longest tag name.
const FOLD_LIMIT: usize = 1 + MAX_TAG_NAME_LEN;

/// Characters that can open a tag: ASCII `<`, its complete NFKC preimage (U+FE64, U+FF1C),
/// and visual lookalikes that NFKC leaves alone.
fn is_bracket(c: char) -> bool {
    matches!(
        c,
        '<' | '\u{FE64}'
            | '\u{FF1C}'
            | '\u{2329}'
            | '\u{3008}'
            | '\u{27E8}'
            | '\u{2039}'
            | '\u{02C2}'
            | '\u{1438}'
            | '\u{276C}'
            | '\u{276E}'
            | '\u{300A}'
            | '\u{226A}'
            | '\u{226E}'
    )
}

fn canonical(c: char) -> char {
    match c {
        '\u{2215}' | '\u{2044}' | '\u{FF0F}' | '\u{29F8}' | '\u{2571}' => '/',
        '\u{2010}'..='\u{2015}'
        | '\u{2043}'
        | '\u{2212}'
        | '\u{2500}'
        | '\u{2E3A}'
        | '\u{2E3B}'
        | '\u{30FC}'
        | '\u{FE63}'
        | '\u{FF0D}' => '-',
        _ => c,
    }
}

/// Skip set: a superset of what the stripper removes, plus invisible variation selectors and
/// combining marks that render as part of the neighbouring glyph.
fn is_skipped(c: char) -> bool {
    c.is_whitespace()
        || is_stripped_char(c)
        || matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{180E}' | '\u{E0100}'..='\u{E01EF}')
        || unicode_normalization::char::is_combining_mark(c)
}

/// Fixed-size folded prefix; non-ASCII output can never match a tag name and is kept as a
/// placeholder byte so no allocation is needed per candidate.
struct Folded {
    buf: [u8; FOLD_LIMIT],
    len: usize,
}

impl Folded {
    const NON_ASCII: u8 = 0xFF;

    fn new() -> Self {
        Self {
            buf: [0; FOLD_LIMIT],
            len: 0,
        }
    }

    fn push(&mut self, c: char) {
        if self.len < FOLD_LIMIT {
            self.buf[self.len] = u8::try_from(c)
                .ok()
                .filter(u8::is_ascii)
                .unwrap_or(Self::NON_ASCII);
            self.len += 1;
        }
    }

    fn is_full(&self) -> bool {
        self.len == FOLD_LIMIT
    }

    fn body(&self) -> &[u8] {
        let bytes = &self.buf[..self.len];
        bytes.strip_prefix(b"/").unwrap_or(bytes)
    }

    fn may_open(&self, tags: &[DelimiterTag]) -> bool {
        let body = self.body();
        tags.iter().any(|tag| {
            tag.name().as_bytes().starts_with(body) || body.starts_with(tag.name().as_bytes())
        })
    }

    fn opens(&self, tags: &[DelimiterTag]) -> bool {
        let body = self.body();
        tags.iter()
            .any(|tag| body.starts_with(tag.name().as_bytes()))
    }
}

fn push_folded(c: char, out: &mut Folded) {
    if c.is_ascii() {
        if !is_skipped(c) {
            out.push(c.to_ascii_lowercase());
        }
        return;
    }
    if is_skipped(c) {
        return;
    }
    for expanded in std::iter::once(c).nfkc() {
        if is_skipped(expanded) {
            continue;
        }
        for lower in canonical(expanded).to_lowercase() {
            out.push(canonical(lower));
        }
    }
}

fn opens_tag(after_bracket: &str, tags: &[DelimiterTag]) -> bool {
    let mut folded = Folded::new();
    for c in after_bracket.chars() {
        push_folded(c, &mut folded);
        if folded.is_full() {
            break;
        }
        if !folded.may_open(tags) {
            return false;
        }
    }
    folded.opens(tags)
}

/// Replace the opening bracket of every occurrence of `tags` (opening or closing) in
/// `content` with `&lt;`, so the tag renders as inert text.
///
/// Matching is case-insensitive and tolerant of whitespace, zero-width, DEL and C1 control
/// characters inside the tag, fullwidth/small-form/angle-bracket lookalikes, dash and slash
/// lookalikes, and NFKC expansions such as U+FB06 (`ﬆ` -> `st`). The result is idempotent
/// and returns [`Cow::Borrowed`] when nothing needs escaping.
///
/// # Examples
///
/// ```
/// use zeph_common::delimiter::{DelimiterTag, escape_delimiter_tags};
///
/// let tags = DelimiterTag::UNTRUSTED_CONTENT;
/// assert_eq!(
///     escape_delimiter_tags("a </Tool-Output> b", &tags),
///     "a &lt;/Tool-Output> b"
/// );
/// assert_eq!(
///     escape_delimiter_tags("</ shared-\u{200B}\u{FB06}ate>", &tags),
///     "&lt;/ shared-\u{200B}\u{FB06}ate>"
/// );
/// assert!(matches!(
///     escape_delimiter_tags("plain <b>text</b>", &tags),
///     std::borrow::Cow::Borrowed(_)
/// ));
/// ```
#[must_use]
pub fn escape_delimiter_tags<'a>(content: &'a str, tags: &[DelimiterTag]) -> Cow<'a, str> {
    let mut escaped: Option<String> = None;
    let mut copied_to = 0;
    for (idx, bracket) in content.char_indices().filter(|&(_, c)| is_bracket(c)) {
        let after = idx + bracket.len_utf8();
        if !opens_tag(&content[after..], tags) {
            continue;
        }
        let out = escaped.get_or_insert_with(|| String::with_capacity(content.len() + 16));
        out.push_str(&content[copied_to..idx]);
        out.push_str("&lt;");
        copied_to = after;
    }
    match escaped {
        None => Cow::Borrowed(content),
        Some(mut out) => {
            out.push_str(&content[copied_to..]);
            Cow::Owned(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use regex::Regex;

    use super::*;
    use crate::sanitize::strip_control_chars_preserve_whitespace;

    fn esc(s: &str) -> String {
        escape_delimiter_tags(s, &DelimiterTag::ALL).into_owned()
    }

    fn tag_re() -> Regex {
        let names: Vec<_> = DelimiterTag::ALL.iter().map(|t| t.name()).collect();
        Regex::new(&format!("(?i)</?({})", names.join("|"))).unwrap()
    }

    #[test]
    fn every_tag_escaped_in_each_disguise() {
        for tag in DelimiterTag::ALL {
            let n = tag.name();
            let (a, b) = n.split_at(3);
            let disguises = [
                format!("</{n}>"),
                format!("<{n}>"),
                format!("</{}>", n.to_uppercase()),
                format!("< / {n} >"),
                format!("<\u{FF0F}{n}>"),
                format!("\u{FF1C}/{n}>"),
                format!("\u{FE64}/{n}>"),
                format!("\u{2329}/{n}>"),
                format!("\u{3008}/{n}>"),
                format!("\u{27E8}/{n}>"),
                format!("\u{2039}/{n}>"),
                format!("\u{02C2}/{n}>"),
                format!("</{a}\u{200B}{b}>"),
                format!("<\u{7f}/{n}>"),
                format!("</{a}\u{85}{b}>"),
                format!("</{a}\u{AD}{b}>"),
                format!("</{}>", n.replace('-', "\u{2011}")),
                format!("</{}>", n.replace('-', "\u{2212}")),
                format!("</{}>", n.replace('-', "\u{2014}")),
                format!("</{}>", n.replace('-', "\u{FF0D}")),
                format!("</{}>", n.replace('t', "\u{FF54}")),
                format!("</{}>", n.replace('s', "\u{017F}")),
                format!("</{}>", n.replace('k', "\u{212A}")),
                format!("</{}>", n.replace("st", "\u{FB06}")),
                format!("</{}>", n.replace("st", "\u{FB05}")),
                format!("</{a}\u{FE0F}{b}>"),
                format!("</{a}\u{E0100}{b}>"),
                format!("</{a}\u{180E}{b}>"),
                format!("</{a}\u{0301}{b}>"),
                format!("\u{1438}/{n}>"),
                format!("\u{276E}/{n}>"),
                format!("\u{276C}/{n}>"),
                format!("\u{300A}/{n}>"),
                format!("\u{226A}/{n}>"),
                format!("\u{226E}/{n}>"),
                format!("<\u{29F8}{n}>"),
                format!("<\u{2571}{n}>"),
                format!("</{}>", n.replace('-', "\u{2500}")),
                format!("</{}>", n.replace('-', "\u{30FC}")),
                format!("</{}>", n.replace('-', "\u{2E3A}")),
                format!("</{}>", n.replace('-', "\u{2043}")),
            ];
            for d in disguises {
                let out = esc(&d);
                assert!(out.contains("&lt;"), "{d:?} -> {out:?}");
                assert!(!tag_re().is_match(&out), "{d:?} -> {out:?}");
            }
        }
    }

    #[test]
    fn variation_selector_inside_tool_output_is_escaped() {
        assert_eq!(esc("</tool\u{FE0F}-output>"), "&lt;/tool\u{FE0F}-output>");
    }

    #[test]
    fn runtime_note_is_not_in_untrusted_set() {
        let s = "<runtime-note id=\"n\">x</runtime-note>";
        assert!(matches!(
            escape_delimiter_tags(s, &DelimiterTag::UNTRUSTED_CONTENT),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn ligature_vector_from_review() {
        assert_eq!(esc("</shared-\u{FB06}ate>"), "&lt;/shared-\u{FB06}ate>");
    }

    #[test]
    fn untouched_text_is_borrowed() {
        for s in [
            "",
            "plain",
            "a < b and c > d",
            "<b>bold</b>",
            "<tool>x</tool>",
            "日本語 <p>",
        ] {
            assert!(matches!(
                escape_delimiter_tags(s, &DelimiterTag::ALL),
                Cow::Borrowed(_)
            ));
        }
    }

    #[test]
    fn only_requested_tags_are_escaped() {
        let out = escape_delimiter_tags(
            "</tool-output></agent-memory>",
            &[DelimiterTag::AgentMemory],
        );
        assert_eq!(out, "</tool-output>&lt;/agent-memory>");
    }

    #[test]
    fn escape_is_idempotent() {
        let once = esc("x</ shared\u{200B}-state>y<\u{FF1C}tool-output>");
        assert_eq!(esc(&once), once);
    }

    #[test]
    fn nested_bracket_does_not_hide_tag() {
        assert_eq!(esc("<<tool-output>"), "<&lt;tool-output>");
    }

    #[test]
    fn bracket_table_covers_nfkc_preimage_of_less_than() {
        let missing: Vec<char> = (0..=0x10_FFFF_u32)
            .filter_map(char::from_u32)
            .filter(|&c| std::iter::once(c).nfkc().eq(['<']))
            .filter(|&c| !is_bracket(c))
            .collect();
        assert!(
            missing.is_empty(),
            "NFKC preimage of '<' missing from table: {missing:?}"
        );
    }

    #[test]
    fn dash_and_slash_nfkc_preimages_fold_to_ascii() {
        for c in (0..=0x10_FFFF_u32).filter_map(char::from_u32) {
            for ascii in ['-', '/'] {
                if std::iter::once(c).nfkc().eq([ascii]) {
                    let mut out = Folded::new();
                    push_folded(c, &mut out);
                    assert_eq!(out.buf[0], ascii as u8, "U+{:04X}", c as u32);
                }
            }
        }
    }

    fn token() -> impl Strategy<Value = String> {
        let fixed = prop::sample::select(vec![
            "<",
            "</",
            "/",
            "-",
            " ",
            "\n",
            "\u{200B}",
            "\u{7f}",
            "\u{85}",
            "\u{FEFF}",
            "\u{FE0F}",
            "\u{0301}",
            "\u{1438}",
            "\u{FF1C}",
            "\u{FE64}",
            "\u{2329}",
            "\u{FF0F}",
            "\u{2011}",
            "\u{FB06}",
            "\u{FB05}",
            "\u{212A}",
            "tool-output",
            "external-data",
            "shared-state",
            "agent-memory",
            "runtime-note",
            "shared-\u{FB06}ate",
            "TOOL",
            "x",
            ">",
            "&lt;",
        ])
        .prop_map(String::from);
        prop_oneof![fixed, any::<char>().prop_map(String::from)]
    }

    proptest! {
        #[test]
        fn escaped_and_stripped_output_has_no_tag(parts in prop::collection::vec(token(), 0..14)) {
            let input: String = parts.concat();
            let out = esc(&input);
            prop_assert!(!tag_re().is_match(&out), "{input:?} -> {out:?}");
            let stripped = strip_control_chars_preserve_whitespace(&out);
            prop_assert!(!tag_re().is_match(&stripped), "{input:?} -> {stripped:?}");
            prop_assert_eq!(esc(&out), out);
        }
    }

    #[test]
    fn matcher_window_covers_every_tag_name() {
        assert!(
            DelimiterTag::ALL
                .iter()
                .all(|t| t.name().len() <= MAX_TAG_NAME_LEN)
        );
        assert!(
            DelimiterTag::ALL
                .iter()
                .any(|t| t.name().len() == MAX_TAG_NAME_LEN)
        );
    }
}
