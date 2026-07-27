// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared `nucleo_matcher` construction for the TUI's two completion
//! surfaces — `crate::command::filter_commands` (`/` slash autocomplete and
//! the `:` Command Palette) and `crate::widgets::mention_picker` (the `@`
//! picker) — unified onto one fuzzy-matching engine by issue #6650.
//!
//! Both surfaces share `CaseMatching::Smart`/`Normalization::Smart`/
//! `AtomKind::Fuzzy` via [`pattern`], but deliberately differ on
//! [`Config::prefer_prefix`] via the `prefer_prefix` parameter to [`matcher`]:
//! nucleo's own docs recommend enabling it only for "autocompletion usecases
//! where the expectation is that the user is typing the entire match" (short
//! command ids/labels), and explicitly recommend leaving it off for
//! fzf-style substring search (arbitrary paths, e.g. `@`-mentioned files) —
//! see `nucleo_matcher::config::Config::prefer_prefix`. Centralizing the
//! shared parts here keeps the two surfaces from silently drifting apart on
//! the settings that *should* stay identical, while making the one
//! intentional difference an explicit, self-documenting call-site argument
//! instead of two independently-maintained literal `Config` values.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// Builds a `Matcher` with the TUI's shared case/normalization defaults.
///
/// Pass `prefer_prefix: true` for short-string autocompletion (command ids
/// and labels, where the user is expected to type the whole thing) and
/// `false` for arbitrary substring search (file paths, skill/agent names).
#[must_use]
pub(crate) fn matcher(prefer_prefix: bool) -> Matcher {
    // `Config` is `#[non_exhaustive]`, so it can't be built with struct-update
    // syntax outside its crate — mutate the one field on top of `DEFAULT`.
    let mut config = Config::DEFAULT;
    config.prefer_prefix = prefer_prefix;
    Matcher::new(config)
}

/// Builds a fuzzy [`Pattern`] with the TUI's shared case/normalization
/// defaults (`CaseMatching::Smart`, `Normalization::Smart`, `AtomKind::Fuzzy`).
#[must_use]
pub(crate) fn pattern(query: &str) -> Pattern {
    Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    )
}
