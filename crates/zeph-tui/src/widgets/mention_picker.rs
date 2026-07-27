// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Inline, non-modal `@` mention picker (spec 084, issues #6647/#6648).
//!
//! Replaces the old modal file picker (`file_picker.rs`, pre-#6647) with a popup that
//! never steals keystrokes from the input buffer: every character still lands in
//! `SessionSlot::input`, and this widget only reflects/filters what is already there.
//! The query itself is never duplicated into `MentionPickerState` — it is always derived
//! from the buffer by the reducer (`crate::app::reducer::mention_picker_query`), which is
//! what makes cursor movement, paste, and backspace "just work" without a second
//! keystroke-mirroring state machine (the bug class `SlashAutocomplete*PushChar/PopChar`
//! lives with).

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};

use nucleo_matcher::{Matcher, Utf32Str};

use zeph_core::channel::SkillCatalogItem;
use zeph_core::metrics::AgentDefSummary;

use crate::app::App;
use crate::theme::Theme;

/// Hard cap on rendered/selectable results, mirroring the old file picker's limit.
const MAX_RESULTS: usize = 10;

/// Category tab. `Left`/`Right` cycle through these while the popup is open (FR-004);
/// they never move the input cursor while the picker is open (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MentionTab {
    All,
    Files,
    Skills,
    Agents,
}

impl MentionTab {
    #[must_use]
    pub(crate) fn next(self) -> Self {
        match self {
            Self::All => Self::Files,
            Self::Files => Self::Skills,
            Self::Skills => Self::Agents,
            Self::Agents => Self::All,
        }
    }

    #[must_use]
    pub(crate) fn prev(self) -> Self {
        match self {
            Self::All => Self::Agents,
            Self::Files => Self::All,
            Self::Skills => Self::Files,
            Self::Agents => Self::Skills,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Files => "Files",
            Self::Skills => "Skills",
            Self::Agents => "Agents",
        }
    }
}

/// Discriminates a [`MentionEntry`]'s source category. Drives accept format
/// (FR-015/016/017) and the All-tab row prefix (FR-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MentionKind {
    File,
    Skill,
    Agent,
}

/// One rendered/selectable row in the popup.
pub(crate) struct MentionEntry {
    pub(crate) kind: MentionKind,
    pub(crate) display: String,
    pub(crate) description: Option<String>,
    /// Char indices into `display` for match highlighting (FR-013). Sorted and
    /// deduplicated at construction time (`Pattern::indices` appends per-atom,
    /// unsorted and un-deduped — see `nucleo_matcher::pattern::Pattern::indices` docs).
    pub(crate) indices: Vec<u32>,
}

/// The three data sources backing the picker, kept deliberately heterogeneous
/// (no unified item type) since `files`/`skills` are `Option` (loading vs. loaded-empty,
/// FR-011/FR-019) while `agents` reads straight from the always-populated
/// `MetricsSnapshot::agent_definitions` (D1 — no new plumbing needed for agents).
#[derive(Clone, Default)]
pub(crate) struct MentionCatalog {
    pub(crate) files: Option<Arc<Vec<String>>>,
    pub(crate) skills: Option<Arc<[SkillCatalogItem]>>,
    pub(crate) agents: Arc<[AgentDefSummary]>,
}

/// Popup state for the inline `@` mention picker.
///
/// `at_char_index` is the char index of the triggering `@` in the current session's
/// input buffer. The query (text between `@` and the cursor) is deliberately **not**
/// stored here — see the module doc comment.
pub(crate) struct MentionPickerState {
    pub(crate) at_char_index: usize,
    pub(crate) active_tab: MentionTab,
    pub(crate) selected: usize,
    pub(crate) filtered: Vec<MentionEntry>,
    pub(crate) catalog: MentionCatalog,
    matcher: Matcher,
}

impl MentionPickerState {
    #[must_use]
    pub(crate) fn new(at_char_index: usize, catalog: MentionCatalog) -> Self {
        let mut state = Self {
            at_char_index,
            active_tab: MentionTab::All,
            selected: 0,
            filtered: Vec::new(),
            catalog,
            // `prefer_prefix: false` — arbitrary substring search over file/skill/
            // agent names, not "user is typing the entire match" (see `crate::fuzzy`).
            matcher: crate::fuzzy::matcher(false),
        };
        state.refilter("");
        state
    }

    /// Re-derives `filtered` for the active tab from the given query. Called by the
    /// reducer after every buffer/cursor mutation and after every tab change — never
    /// mutates `query` itself since none is stored (see module doc comment).
    pub(crate) fn refilter(&mut self, query: &str) {
        self.selected = 0;
        // Borrow `catalog` (not `self`) so the immutable candidate borrows below don't
        // conflict with the `&mut self.matcher` borrow needed by the scored path.
        let candidates = candidates_for_tab(&self.catalog, self.active_tab);
        self.filtered = if query.is_empty() {
            Self::round_robin(candidates, self.active_tab)
        } else {
            Self::scored(candidates, query, &mut self.matcher)
        };
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let len_i = i32::try_from(len).unwrap_or(i32::MAX);
        let cur_i = i32::try_from(self.selected).unwrap_or(0);
        let new_i = (cur_i + delta).rem_euclid(len_i);
        self.selected = usize::try_from(new_i).unwrap_or(0);
    }

    /// Empty-query path: round-robins across non-empty categories on the `All` tab so
    /// files (up to 50 000 candidates) never crowd out skills/agents before either gets a
    /// slot. Single-category tabs just take the first `MAX_RESULTS`, which for the Files
    /// category means the first `MAX_RESULTS` of `catalog.files` — recency-ordered
    /// (uncommitted changes, then mtime descending) by `crate::file_picker::FileIndex::build`,
    /// not alphabetical (#6651). Skills/Agents keep their catalog (alphabetical) order.
    fn round_robin(candidates: Vec<Candidate<'_>>, tab: MentionTab) -> Vec<MentionEntry> {
        if tab != MentionTab::All {
            return candidates
                .into_iter()
                .take(MAX_RESULTS)
                .map(Candidate::into_entry)
                .collect();
        }
        let mut groups: [Vec<Candidate<'_>>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for c in candidates {
            let slot = match c.kind {
                MentionKind::File => 0,
                MentionKind::Skill => 1,
                MentionKind::Agent => 2,
            };
            groups[slot].push(c);
        }
        let max_len = groups.iter().map(Vec::len).max().unwrap_or(0);
        let mut out = Vec::new();
        'outer: for round in 0..max_len {
            for group in &mut groups {
                if round < group.len() {
                    // Swap-free ownership grab: replace with a sentinel-free take via index.
                    let c = std::mem::replace(
                        &mut group[round],
                        Candidate {
                            kind: MentionKind::File,
                            name: "",
                            description: None,
                        },
                    );
                    out.push(Candidate::into_entry(c));
                    if out.len() >= MAX_RESULTS {
                        break 'outer;
                    }
                }
            }
        }
        out
    }

    /// Typed-query path: phase 1 scores every candidate (no allocation beyond the score
    /// itself), truncates to `MAX_RESULTS`, then phase 2 materializes `display`/
    /// `description`/`indices` only for the survivors (`Pattern::indices` allocates, so it
    /// must never run across the full candidate set).
    fn scored(
        candidates: Vec<Candidate<'_>>,
        query: &str,
        matcher: &mut Matcher,
    ) -> Vec<MentionEntry> {
        let pattern = crate::fuzzy::pattern(query);
        let mut scored: Vec<(u32, Candidate<'_>)> = candidates
            .into_iter()
            .filter_map(|c| {
                let mut buf = Vec::new();
                let haystack = Utf32Str::new(c.name, &mut buf);
                pattern.score(haystack, matcher).map(|score| (score, c))
            })
            .collect();
        // Stable sort (not `sort_unstable_by_key`): `candidates` arrives in
        // `FileIndex`'s recency order (#6651), so on a score tie this
        // preserves "most recently touched first" instead of scrambling it
        // (critic finding S4).
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.truncate(MAX_RESULTS);

        scored
            .into_iter()
            .map(|(_, c)| {
                let mut buf = Vec::new();
                let haystack = Utf32Str::new(c.name, &mut buf);
                let mut indices = Vec::new();
                pattern.indices(haystack, matcher, &mut indices);
                indices.sort_unstable();
                indices.dedup();
                MentionEntry {
                    kind: c.kind,
                    display: c.name.to_owned(),
                    description: c.description.map(str::to_owned),
                    indices,
                }
            })
            .collect()
    }
}

struct Candidate<'a> {
    kind: MentionKind,
    name: &'a str,
    description: Option<&'a str>,
}

/// Collects borrowed candidates from every category relevant to `active_tab`. Phase 1
/// of the two-phase refilter (NFR-001): no `String`/`Vec<u32>` allocation here — only
/// `&str` borrows from the underlying `Arc` catalogs. A free function (not a
/// `&self` method) so callers can borrow `catalog` and `matcher` disjointly.
fn candidates_for_tab(catalog: &MentionCatalog, active_tab: MentionTab) -> Vec<Candidate<'_>> {
    let mut out = Vec::new();
    let want_files = matches!(active_tab, MentionTab::All | MentionTab::Files);
    let want_skills = matches!(active_tab, MentionTab::All | MentionTab::Skills);
    let want_agents = matches!(active_tab, MentionTab::All | MentionTab::Agents);

    if want_files && let Some(files) = &catalog.files {
        out.extend(files.iter().map(|p| Candidate {
            kind: MentionKind::File,
            name: p.as_str(),
            description: None,
        }));
    }
    if want_skills && let Some(skills) = &catalog.skills {
        out.extend(skills.iter().map(|s| Candidate {
            kind: MentionKind::Skill,
            name: s.name.as_str(),
            description: Some(s.description.as_str()),
        }));
    }
    if want_agents {
        out.extend(catalog.agents.iter().map(|a| Candidate {
            kind: MentionKind::Agent,
            name: a.name.as_str(),
            description: Some(a.description.as_str()),
        }));
    }
    out
}

impl Candidate<'_> {
    fn into_entry(self) -> MentionEntry {
        MentionEntry {
            kind: self.kind,
            display: self.name.to_owned(),
            description: self.description.map(str::to_owned),
            indices: Vec::new(),
        }
    }
}

fn category_total(state: &MentionPickerState) -> usize {
    match state.active_tab {
        MentionTab::Files => state.catalog.files.as_ref().map_or(0, |f| f.len()),
        MentionTab::Skills => state.catalog.skills.as_ref().map_or(0, |s| s.len()),
        MentionTab::Agents => state.catalog.agents.len(),
        MentionTab::All => {
            state.catalog.files.as_ref().map_or(0, |f| f.len())
                + state.catalog.skills.as_ref().map_or(0, |s| s.len())
                + state.catalog.agents.len()
        }
    }
}

/// Placeholder text shown when `filtered` is empty (FR-011/FR-019): distinguishes
/// "still loading" (`Option::None`) from "loaded and genuinely empty" (`Some(empty)`).
fn placeholder_text(state: &MentionPickerState) -> &'static str {
    match state.active_tab {
        MentionTab::Files => {
            if state.catalog.files.is_none() {
                "indexing files…"
            } else {
                "no files found"
            }
        }
        MentionTab::Skills => {
            if state.catalog.skills.is_none() {
                "loading skills…"
            } else {
                "no skills loaded"
            }
        }
        MentionTab::Agents => "no agents loaded",
        MentionTab::All => "no results",
    }
}

fn render_tab_bar(active: MentionTab, frame: &mut Frame, area: Rect, theme: &Theme) {
    let tabs = [
        MentionTab::All,
        MentionTab::Files,
        MentionTab::Skills,
        MentionTab::Agents,
    ];
    let mut spans = Vec::with_capacity(tabs.len() * 2);
    for (i, tab) in tabs.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" | "));
        }
        let style = if tab == active {
            theme.highlight
        } else {
            theme.panel_title
        };
        spans.push(Span::styled(tab.label(), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Splits `entry.display` into highlighted/plain `Span`s at the (sorted, deduped)
/// nucleo match indices, then appends the dimmed, width-truncated description.
fn render_row(entry: &MentionEntry, active_tab: MentionTab, theme: &Theme) -> ListItem<'static> {
    let prefix = match (active_tab, entry.kind) {
        (MentionTab::All, MentionKind::File) => "[F] ",
        (MentionTab::All, MentionKind::Skill) => "[S] ",
        (MentionTab::All, MentionKind::Agent) => "[A] ",
        _ => "",
    };
    let mut spans = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::raw(prefix));
    }
    let mut buf = String::new();
    let mut highlighted = false;
    for (i, c) in entry.display.chars().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let is_hl = entry.indices.binary_search(&(i as u32)).is_ok();
        if is_hl != highlighted && !buf.is_empty() {
            let style = if highlighted {
                theme.highlight
            } else {
                Style::default()
            };
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        highlighted = is_hl;
        buf.push(c);
    }
    if !buf.is_empty() {
        let style = if highlighted {
            theme.highlight
        } else {
            Style::default()
        };
        spans.push(Span::styled(buf, style));
    }
    if let Some(desc) = &entry.description {
        let truncated = crate::layout::truncate_to_width(desc, 30);
        spans.push(Span::styled(
            format!("  — {truncated}"),
            theme.system_message,
        ));
    }
    ListItem::new(Line::from(spans))
}

/// Renders the popup anchored to the triggering `@` character, flipping above/below
/// `input_area` based on available space. Reuses [`crate::widgets::input::caret_xy`] so
/// the popup never disagrees with where the terminal cursor is actually drawn (M3).
pub(crate) fn render(
    app: &App,
    state: &MentionPickerState,
    frame: &mut Frame,
    input_area: Rect,
    theme: &Theme,
) {
    const TAB_BAR_H: u16 = 1;
    const BORDER_H: u16 = 2;

    let (anchor_x, _anchor_y) =
        crate::widgets::input::caret_xy(app, input_area, state.at_char_index);

    let visible_rows = state.filtered.len().clamp(1, MAX_RESULTS);
    #[allow(clippy::cast_possible_truncation)]
    let list_h = visible_rows as u16;
    let height = list_h + TAB_BAR_H + BORDER_H;

    let width: u16 = 50.min(input_area.width.max(1));
    let max_x = input_area.x + input_area.width.saturating_sub(width);
    let x = anchor_x.min(max_x);

    let frame_height = frame.area().height;
    let y = if input_area.y >= height {
        input_area.y - height
    } else {
        (input_area.y + input_area.height).min(frame_height.saturating_sub(height))
    };

    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let total = category_total(state);
    let title = format!(
        " {} ({}/{total}) ",
        state.active_tab.label(),
        state.filtered.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.panel_border)
        .title(title)
        .title_style(theme.panel_title);
    frame.render_widget(block, popup);

    let inner = Rect::new(
        popup.x + 1,
        popup.y + 1,
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let tab_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let list_area = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1),
    );

    render_tab_bar(state.active_tab, frame, tab_area, theme);

    if state.filtered.is_empty() {
        let msg = Paragraph::new(placeholder_text(state)).style(theme.system_message);
        frame.render_widget(msg, list_area);
        return;
    }

    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .map(|entry| render_row(entry, state.active_tab, theme))
        .collect();
    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let list = List::new(items)
        .highlight_style(selected_style)
        .highlight_symbol("> ");
    let mut list_state = ListState::default();
    list_state.select(Some(state.selected));
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::render_to_string;

    fn catalog(files: &[&str], skills: &[(&str, &str)], agents: &[(&str, &str)]) -> MentionCatalog {
        MentionCatalog {
            files: Some(Arc::new(files.iter().map(|s| (*s).to_owned()).collect())),
            skills: Some(Arc::from(
                skills
                    .iter()
                    .map(|(name, description)| SkillCatalogItem {
                        name: (*name).to_owned(),
                        description: (*description).to_owned(),
                    })
                    .collect::<Vec<_>>(),
            )),
            agents: Arc::from(
                agents
                    .iter()
                    .map(|(name, description)| AgentDefSummary {
                        name: (*name).to_owned(),
                        description: (*description).to_owned(),
                        ..AgentDefSummary::default()
                    })
                    .collect::<Vec<_>>(),
            ),
        }
    }

    fn make_app() -> App {
        let (user_tx, _user_rx) = tokio::sync::mpsc::channel(1);
        let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel(1);
        App::new(user_tx, agent_rx)
    }

    #[test]
    fn new_opens_on_all_tab_with_empty_query() {
        let state = MentionPickerState::new(0, catalog(&["a.rs", "b.rs"], &[], &[]));
        assert_eq!(state.active_tab, MentionTab::All);
        assert!(!state.filtered.is_empty());
    }

    #[test]
    fn empty_query_round_robins_across_categories() {
        let cat = catalog(
            &["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"],
            &[("skill_one", "desc")],
            &[("agent_one", "desc")],
        );
        let mut state = MentionPickerState::new(0, cat);
        state.refilter("");
        let kinds: Vec<MentionKind> = state.filtered.iter().map(|e| e.kind).collect();
        assert!(
            kinds.contains(&MentionKind::Skill),
            "skills must not be starved by files on the All tab: {kinds:?}"
        );
        assert!(
            kinds.contains(&MentionKind::Agent),
            "agents must not be starved by files on the All tab: {kinds:?}"
        );
    }

    #[test]
    fn all_tab_omits_empty_categories() {
        let cat = catalog(&["a.rs"], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.refilter("");
        assert!(state.filtered.iter().all(|e| e.kind == MentionKind::File));
    }

    #[test]
    fn empty_query_files_tab_preserves_catalog_order() {
        // Recency ordering is computed upstream by `crate::file_picker::FileIndex::build`
        // (#6651); this widget must not re-sort — it just takes the catalog's head.
        let cat = catalog(&["z_recent.rs", "a_old.rs", "m_mid.rs"], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.active_tab = MentionTab::Files;
        state.refilter("");
        let names: Vec<&str> = state.filtered.iter().map(|e| e.display.as_str()).collect();
        assert_eq!(names, vec!["z_recent.rs", "a_old.rs", "m_mid.rs"]);
    }

    #[test]
    fn typed_query_score_tie_preserves_catalog_recency_order() {
        // Critic finding S4: on the typed-query path, candidates already arrive in
        // `catalog.files`'s recency order (#6651). A single-char query matching an
        // identically-positioned character in same-shaped names (single letter + ".rs")
        // ties in fuzzy score for all three — `scored()` uses a *stable* `sort_by_key`
        // (not `sort_unstable_by_key`) so the tie falls back to that recency order
        // instead of being scrambled.
        //
        // NOTE (testing-round finding): this test does not actually discriminate
        // `sort_by_key` from `sort_unstable_by_key` on the current toolchain — Rust's
        // unstable pdqsort-based sort special-cases a fully-tied input (as constructed
        // here) and happens to leave it in original order too, so reverting to
        // `sort_unstable_by_key` would not turn this test red. A black-box test that
        // reliably forces a *reordering* difference between stable and unstable sort
        // isn't practical without depending on unspecified sort-implementation
        // internals across toolchain versions. Kept as documentation of the intended
        // behavior (and a regression guard against unrelated logic bugs in `scored()`),
        // not as a guard on the specific stable-vs-unstable sort choice.
        let cat = catalog(&["c.rs", "a.rs", "b.rs"], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.active_tab = MentionTab::Files;
        state.refilter(".");
        let names: Vec<&str> = state.filtered.iter().map(|e| e.display.as_str()).collect();
        assert_eq!(
            names,
            vec!["c.rs", "a.rs", "b.rs"],
            "tied scores must preserve catalog order, not be reshuffled"
        );
    }

    #[test]
    fn single_tab_filters_only_that_category() {
        let cat = catalog(&["main.rs"], &[("main_skill", "desc")], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.active_tab = MentionTab::Skills;
        state.refilter("main");
        assert!(state.filtered.iter().all(|e| e.kind == MentionKind::Skill));
    }

    #[test]
    fn typed_query_filters_and_sorts_by_score() {
        let cat = catalog(&["src/main.rs", "src/lib.rs", "tests/foo.rs"], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.refilter("main");
        assert!(!state.filtered.is_empty());
        assert!(state.filtered.iter().any(|e| e.display.contains("main")));
    }

    #[test]
    fn indices_are_sorted_and_deduped() {
        let cat = catalog(&["aabbaabb.rs"], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.refilter("ab");
        for entry in &state.filtered {
            let mut sorted = entry.indices.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                entry.indices, sorted,
                "indices must already be sorted+deduped"
            );
        }
    }

    #[test]
    fn move_selection_wraps() {
        let cat = catalog(&["a.rs", "b.rs", "c.rs"], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        assert_eq!(state.selected, 0);
        state.move_selection(-1);
        assert_eq!(state.selected, state.filtered.len() - 1);
    }

    #[test]
    fn move_selection_noop_on_empty() {
        let mut state = MentionPickerState::new(0, MentionCatalog::default());
        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn files_loading_shows_placeholder() {
        let cat = MentionCatalog {
            files: None,
            skills: None,
            agents: Arc::from(Vec::<AgentDefSummary>::new()),
        };
        let mut state = MentionPickerState::new(0, cat);
        state.active_tab = MentionTab::Files;
        state.refilter("");
        assert!(state.filtered.is_empty());
        assert_eq!(placeholder_text(&state), "indexing files…");
    }

    #[test]
    fn files_loaded_empty_shows_different_placeholder() {
        let cat = catalog(&[], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.active_tab = MentionTab::Files;
        state.refilter("");
        assert_eq!(placeholder_text(&state), "no files found");
    }

    #[test]
    fn render_shows_tabs_and_counter() {
        let cat = catalog(&["src/main.rs", "src/lib.rs"], &[], &[]);
        let state = MentionPickerState::new(0, cat);
        let input_area = Rect::new(0, 15, 60, 3);
        let app = make_app();
        let output = render_to_string(60, 20, |frame, _area| {
            let theme = crate::theme::Theme::default();
            render(&app, &state, frame, input_area, &theme);
        });
        assert!(output.contains("All"));
        assert!(output.contains("Files"));
        assert!(output.contains("Skills"));
        assert!(output.contains("Agents"));
        assert!(output.contains("main.rs"));
    }

    #[test]
    fn render_multi_byte_path_highlights_without_panic() {
        let cat = catalog(&["src/данные.rs"], &[], &[]);
        let mut state = MentionPickerState::new(0, cat);
        state.active_tab = MentionTab::Files;
        state.refilter("дан");
        let input_area = Rect::new(0, 15, 60, 3);
        let app = make_app();
        let output = render_to_string(60, 20, |frame, _area| {
            let theme = crate::theme::Theme::default();
            render(&app, &state, frame, input_area, &theme);
        });
        assert!(output.contains("данные"));
    }
}
