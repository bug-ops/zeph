// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Request, answer and outcome types of the model-callable `ask_user` tool.
//!
//! Every channel renders [`ASK_USER_HEADER`] ahead of the model's question and resolves a
//! question to an [`AskUserOutcome`] through [`Channel::ask_user`](super::Channel::ask_user).
//! The model-authored text and options are validated into [`QuestionText`] and
//! [`ChoiceOptions`], so a channel never renders unbounded or malformed input.

use std::fmt::Write as _;

/// Fixed warning rendered by every channel before the model's question. The model cannot alter it.
pub const ASK_USER_HEADER: &str =
    "Question from the assistant \u{2014} never share passwords, API keys or other secrets";

/// Notice shown when a slash command arrives while a question is open and cancels it.
pub const ASK_USER_COMMAND_NOTICE: &str = "Question cancelled \u{2014} resend your command";

/// Hint shown when a reply does not fit the question and the user is asked again.
pub const ASK_USER_RETRY_HINT: &str = "That reply does not match the options.";

/// Key of the single answer field of a form-based question (TUI dialog, ACP elicitation).
pub const ASK_USER_ANSWER_FIELD: &str = "answer";

/// Why a question was constructed with invalid input; the model can correct and retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AskUserInputError {
    /// The question is empty after control characters are stripped.
    #[error("question must not be empty")]
    EmptyQuestion,
    /// The question exceeds the length cap.
    #[error("question is too long")]
    QuestionTooLong,
    /// The number of options is outside the allowed range.
    #[error("expected 2 to 8 options, got {0}")]
    OptionCount(usize),
    /// An option label is empty, too long, duplicated or reserved.
    #[error("invalid option label: {0}")]
    InvalidOption(String),
    /// `multi_select` was set without `options`.
    #[error("multi_select needs options")]
    MultiSelectWithoutOptions,
    /// An option label is only digits, which a numeric reply would read as a position.
    #[error("option label {0:?} is only digits; add units, for example \"2 GB\"")]
    NumericLabel(String),
}

/// Why a user reply could not be mapped onto the question.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReplyParseError {
    /// An index is not a number or is outside `1..=options`.
    #[error("invalid option index: {0}")]
    BadIndex(String),
    /// The reply matches no option and free-text answers are not allowed.
    #[error("reply matches no option")]
    NoMatch,
}

/// Maximum characters of a question after sanitizing.
const MAX_QUESTION_CHARS: usize = 500;
/// Maximum characters of one option label.
const MAX_OPTION_CHARS: usize = 200;
/// Allowed number of options of a choice question.
const OPTION_COUNT: std::ops::RangeInclusive<usize> = 2..=8;

/// Maximum lines of a question, so blank lines cannot push the fixed header off screen.
const MAX_QUESTION_LINES: usize = 6;

/// Format, tag and bidirectional-control characters and blank glyphs that render as nothing
/// or reorder text, so they could disguise an option or a forged line.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{2800}'
            | '\u{3164}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// One line: control and invisible characters dropped, newlines and tabs turned into spaces.
fn sanitize_inline(raw: &str) -> String {
    raw.chars()
        .filter(|c| !is_invisible_format(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A few lines: control and invisible characters dropped, blank runs collapsed, line count
/// capped.
fn sanitize_block(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !is_invisible_format(*c) && (*c == '\n' || !c.is_control()))
        .collect();
    let mut lines: Vec<&str> = Vec::new();
    for line in cleaned.lines().map(str::trim_end) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines
        .into_iter()
        .take(MAX_QUESTION_LINES)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn is_cancel_keyword(text: &str) -> bool {
    let t = text.trim();
    t.eq_ignore_ascii_case("cancel") || t.eq_ignore_ascii_case("/cancel")
}

/// Validated, control-character-free, length-capped question text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionText(String);

impl QuestionText {
    fn new(raw: impl Into<String>) -> Result<Self, AskUserInputError> {
        let text = sanitize_block(&raw.into());
        if text.is_empty() {
            return Err(AskUserInputError::EmptyQuestion);
        }
        if text.chars().count() > MAX_QUESTION_CHARS {
            return Err(AskUserInputError::QuestionTooLong);
        }
        Ok(Self(text))
    }
}

/// Between 2 and 8 distinct, non-empty option labels, none of which is a cancel keyword.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOptions(Vec<String>);

impl ChoiceOptions {
    /// Validate `labels` into a set of options.
    ///
    /// # Errors
    ///
    /// Returns [`AskUserInputError`] when there are fewer than 2 or more than 8 labels, when a
    /// label is empty, only digits, longer than 200 characters, duplicated, or equal to
    /// `cancel` / `/cancel`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_core::channel::ChoiceOptions;
    ///
    /// let options = ChoiceOptions::new(vec!["red".into(), "green".into()]).unwrap();
    /// assert_eq!(options.labels(), ["red", "green"]);
    /// assert!(ChoiceOptions::new(vec!["only one".into()]).is_err());
    /// ```
    pub fn new(labels: Vec<String>) -> Result<Self, AskUserInputError> {
        let count = labels.len();
        if !OPTION_COUNT.contains(&count) {
            return Err(AskUserInputError::OptionCount(count));
        }
        let mut clean: Vec<String> = Vec::with_capacity(count);
        for raw in labels {
            let label = sanitize_inline(&raw);
            if !label.is_empty() && label.chars().all(|c| c.is_ascii_digit()) {
                return Err(AskUserInputError::NumericLabel(label));
            }
            if label.is_empty()
                || label.chars().count() > MAX_OPTION_CHARS
                || is_cancel_keyword(&label)
                || clean.iter().any(|l| l.eq_ignore_ascii_case(&label))
            {
                return Err(AskUserInputError::InvalidOption(label));
            }
            clean.push(label);
        }
        Ok(Self(clean))
    }

    fn position(&self, reply: &str) -> Option<usize> {
        self.0.iter().position(|l| l.eq_ignore_ascii_case(reply))
    }

    /// The validated labels in the order they were given.
    #[must_use]
    pub fn labels(&self) -> &[String] {
        &self.0
    }
}

/// Whether a choice question also accepts a free-text answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtherAnswer {
    /// A reply that matches no option is accepted as free text.
    Allowed,
    /// Only the listed options are accepted.
    Forbidden,
}

/// Shape of a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskUserKind {
    /// Open question answered with free text.
    FreeText,
    /// Exactly one of `options`.
    SingleChoice {
        /// Selectable options.
        options: ChoiceOptions,
        /// Whether a free-text answer is also accepted.
        other: OtherAnswer,
    },
    /// One or more of `options`.
    MultiChoice {
        /// Selectable options.
        options: ChoiceOptions,
        /// Whether a free-text answer is also accepted.
        other: OtherAnswer,
    },
}

/// A question the model asks the user mid-turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserRequest {
    question: QuestionText,
    kind: AskUserKind,
}

impl AskUserRequest {
    /// Open question answered with free text.
    ///
    /// # Errors
    ///
    /// Returns [`AskUserInputError`] when `question` is empty or too long.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_core::channel::{AskUserKind, AskUserRequest};
    ///
    /// let request = AskUserRequest::free_text("Which directory?").unwrap();
    /// assert_eq!(*request.kind(), AskUserKind::FreeText);
    /// assert!(AskUserRequest::free_text("").is_err());
    /// ```
    pub fn free_text(question: impl Into<String>) -> Result<Self, AskUserInputError> {
        Ok(Self {
            question: QuestionText::new(question)?,
            kind: AskUserKind::FreeText,
        })
    }

    /// Question answered with exactly one of `options`.
    ///
    /// # Errors
    ///
    /// Returns [`AskUserInputError`] when `question` is empty or too long.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_core::channel::{AskUserRequest, ChoiceOptions, OtherAnswer};
    ///
    /// let options = ChoiceOptions::new(vec!["red".into(), "green".into()]).unwrap();
    /// let request =
    ///     AskUserRequest::single_choice("Which color?", options, OtherAnswer::Forbidden).unwrap();
    /// assert_eq!(request.question(), "Which color?");
    /// ```
    pub fn single_choice(
        question: impl Into<String>,
        options: ChoiceOptions,
        other: OtherAnswer,
    ) -> Result<Self, AskUserInputError> {
        Ok(Self {
            question: QuestionText::new(question)?,
            kind: AskUserKind::SingleChoice { options, other },
        })
    }

    /// Question answered with one or more of `options`.
    ///
    /// # Errors
    ///
    /// Returns [`AskUserInputError`] when `question` is empty or too long.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_core::channel::{AskUserAnswer, AskUserRequest, ChoiceOptions, OtherAnswer};
    ///
    /// let options = ChoiceOptions::new(vec!["a".into(), "b".into(), "c".into()]).unwrap();
    /// let request = AskUserRequest::multi_choice("Which?", options, OtherAnswer::Forbidden).unwrap();
    /// assert_eq!(
    ///     request.parse_reply("1, 3").unwrap(),
    ///     AskUserAnswer::Selected { indices: vec![0, 2] }
    /// );
    /// ```
    pub fn multi_choice(
        question: impl Into<String>,
        options: ChoiceOptions,
        other: OtherAnswer,
    ) -> Result<Self, AskUserInputError> {
        Ok(Self {
            question: QuestionText::new(question)?,
            kind: AskUserKind::MultiChoice { options, other },
        })
    }

    /// The validated question text.
    #[must_use]
    pub fn question(&self) -> &str {
        &self.question.0
    }

    /// The shape of the question.
    #[must_use]
    pub fn kind(&self) -> &AskUserKind {
        &self.kind
    }

    /// Text a form-style dialog shows: the fixed header, the question, and the numbered
    /// options of a choice that is answered in a text field (see
    /// [`closed_choice_values`](Self::closed_choice_values)).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_core::channel::{ASK_USER_HEADER, AskUserRequest};
    ///
    /// let request = AskUserRequest::free_text("Which branch?").unwrap();
    /// assert!(request.dialog_message().starts_with(ASK_USER_HEADER));
    /// ```
    #[must_use]
    pub fn dialog_message(&self) -> String {
        let mut out = format!("{ASK_USER_HEADER}\n\n{}", self.question.0);
        if self.closed_choice_values().is_none()
            && let AskUserKind::SingleChoice { options, .. }
            | AskUserKind::MultiChoice { options, .. } = &self.kind
        {
            for (i, label) in options.labels().iter().enumerate() {
                let _ = write!(out, "\n{}. {label}", i + 1);
            }
        }
        out
    }

    /// Values for a dialog's selection list when the question is a closed single choice:
    /// each label prefixed with its position (`"1. red"`).
    ///
    /// The position travels with the value so [`parse_dialog_answer`](Self::parse_dialog_answer)
    /// reads the answer by index, never by label text, so a label such as `"2"` cannot be
    /// mistaken for position 2. `None` for every other kind of question.
    #[must_use]
    pub fn closed_choice_values(&self) -> Option<Vec<String>> {
        match &self.kind {
            AskUserKind::SingleChoice {
                options,
                other: OtherAnswer::Forbidden,
            } => Some(
                options
                    .labels()
                    .iter()
                    .enumerate()
                    .map(|(i, label)| format!("{}. {label}", i + 1))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// How to fill the text field of a dialog for a multi-choice or open choice question;
    /// `None` when the question needs no instruction.
    #[must_use]
    pub fn dialog_answer_hint(&self) -> Option<String> {
        let (multi, other) = match &self.kind {
            AskUserKind::FreeText => return None,
            AskUserKind::SingleChoice { other, .. } => (false, *other),
            AskUserKind::MultiChoice { other, .. } => (true, *other),
        };
        if self.closed_choice_values().is_some() {
            return None;
        }
        let how = if multi {
            "numbers separated by commas, or option texts"
        } else {
            "a number or the option text"
        };
        let own = if other == OtherAnswer::Allowed {
            ", or your own answer"
        } else {
            ""
        };
        Some(format!("{how}{own}"))
    }

    /// Map the value a form-style dialog returned onto this question.
    ///
    /// A closed single choice comes back as `"{n}. {label}"` (see
    /// [`closed_choice_values`](Self::closed_choice_values)); only the leading position is
    /// used. Any other value goes through [`parse_reply`](Self::parse_reply) unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ReplyParseError`] when the value cannot be mapped onto the question.
    pub fn parse_dialog_answer(&self, raw: &str) -> Result<AskUserAnswer, ReplyParseError> {
        if self.closed_choice_values().is_some()
            && let Some((position, _label)) = raw.split_once(". ")
            && !position.is_empty()
            && position.chars().all(|c| c.is_ascii_digit())
        {
            return self.parse_reply(position);
        }
        self.parse_reply(raw)
    }

    /// Map a raw user reply onto this question.
    ///
    /// Accepts 1-based indices (comma-separated for multi-choice), an exact option label
    /// (case-insensitive), or free text when [`OtherAnswer::Allowed`].
    ///
    /// # Errors
    ///
    /// Returns [`ReplyParseError`] when the reply cannot be mapped onto the question.
    pub fn parse_reply(&self, reply: &str) -> Result<AskUserAnswer, ReplyParseError> {
        match &self.kind {
            AskUserKind::FreeText => Ok(AskUserAnswer::Text(reply.to_owned())),
            AskUserKind::SingleChoice { options, other } => {
                parse_single(options, *other, reply.trim())
            }
            AskUserKind::MultiChoice { options, other } => {
                parse_multi(options, *other, reply.trim())
            }
        }
    }
}

/// A validated answer to an [`AskUserRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskUserAnswer {
    /// Free-text answer, including an "other" answer to a choice question.
    Text(String),
    /// Zero-based option indices: non-empty, strictly ascending, each below the option count.
    Selected {
        /// Selected option indices.
        indices: Vec<usize>,
    },
}

/// Why a question was declined without asking the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeclineReason {
    /// The channel has no interactive user.
    NonInteractive,
    /// The channel cannot render this kind of question.
    Unsupported,
}

/// How an `ask_user` question ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AskUserOutcome {
    /// The user answered.
    Answered(AskUserAnswer),
    /// The channel declined to ask.
    Declined(DeclineReason),
    /// The user cancelled the question.
    Cancelled,
    /// No answer arrived before the deadline.
    TimedOut,
}

impl AskUserOutcome {
    /// Shorthand for `Answered(AskUserAnswer::Text(text))`.
    #[must_use]
    pub fn answered_text(text: &str) -> Self {
        Self::Answered(AskUserAnswer::Text(text.to_owned()))
    }
}

/// Classification of a raw reply received while a question is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyKind {
    /// `cancel` or `/cancel` (trimmed, case-insensitive).
    Cancel,
    /// The reply selects a registered slash command.
    Command,
    /// Any other text, unchanged.
    Answer(String),
}

/// Classify a raw reply received while a question is open.
///
/// # Examples
///
/// ```
/// use zeph_core::channel::{ReplyKind, classify_reply};
///
/// assert_eq!(classify_reply("cancel"), ReplyKind::Cancel);
/// assert_eq!(classify_reply("/status"), ReplyKind::Command);
/// assert_eq!(classify_reply("blue"), ReplyKind::Answer("blue".into()));
/// ```
#[must_use]
pub fn classify_reply(reply: &str) -> ReplyKind {
    if is_cancel_keyword(reply) {
        return ReplyKind::Cancel;
    }
    // Same rule as `CommandRegistry::find_handler`: the exact name, or the name plus a space.
    let t = reply.trim();
    let is_command = t.starts_with('/')
        && zeph_commands::COMMANDS
            .iter()
            .any(|c| zeph_commands::matches_command_name(t, c.name));
    if is_command {
        ReplyKind::Command
    } else {
        ReplyKind::Answer(reply.to_owned())
    }
}

fn other_or(
    other: OtherAnswer,
    reply: &str,
    err: ReplyParseError,
) -> Result<AskUserAnswer, ReplyParseError> {
    match other {
        OtherAnswer::Allowed if !reply.is_empty() => Ok(AskUserAnswer::Text(reply.to_owned())),
        _ => Err(err),
    }
}

/// A 1-based position typed as pure ASCII digits; `str::parse` would also accept a leading `+`.
fn parse_position(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.parse().unwrap_or(usize::MAX))
}

fn parse_single(
    options: &ChoiceOptions,
    other: OtherAnswer,
    reply: &str,
) -> Result<AskUserAnswer, ReplyParseError> {
    if let Some(n) = parse_position(reply) {
        return match n.checked_sub(1).filter(|i| *i < options.0.len()) {
            Some(i) => Ok(AskUserAnswer::Selected { indices: vec![i] }),
            None => other_or(other, reply, ReplyParseError::BadIndex(reply.to_owned())),
        };
    }
    match options.position(reply) {
        Some(i) => Ok(AskUserAnswer::Selected { indices: vec![i] }),
        None => other_or(other, reply, ReplyParseError::NoMatch),
    }
}

fn parse_multi(
    options: &ChoiceOptions,
    other: OtherAnswer,
    reply: &str,
) -> Result<AskUserAnswer, ReplyParseError> {
    let parts: Vec<&str> = reply.split(',').map(str::trim).collect();
    if parts.iter().all(|p| parse_position(p).is_some()) {
        let mut indices = std::collections::BTreeSet::new();
        for part in &parts {
            let n = parse_position(part).unwrap_or(0);
            match n.checked_sub(1).filter(|i| *i < options.0.len()) {
                Some(i) => {
                    indices.insert(i);
                }
                None => {
                    return other_or(other, reply, ReplyParseError::BadIndex((*part).to_owned()));
                }
            }
        }
        return Ok(AskUserAnswer::Selected {
            indices: indices.into_iter().collect(),
        });
    }
    let labelled: Option<std::collections::BTreeSet<usize>> =
        parts.iter().map(|p| options.position(p)).collect();
    match labelled {
        Some(indices) => Ok(AskUserAnswer::Selected {
            indices: indices.into_iter().collect(),
        }),
        None => other_or(other, reply, ReplyParseError::NoMatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(labels: &[&str]) -> ChoiceOptions {
        ChoiceOptions::new(labels.iter().map(|s| (*s).to_owned()).collect()).unwrap()
    }

    #[test]
    fn a_signed_reply_is_a_label_not_a_position() {
        let options = opts(&["+2", "+1"]);
        assert_eq!(
            parse_single(&options, OtherAnswer::Forbidden, "+1").unwrap(),
            AskUserAnswer::Selected { indices: vec![1] }
        );
        assert_eq!(
            parse_multi(&options, OtherAnswer::Forbidden, "+2, +1").unwrap(),
            AskUserAnswer::Selected {
                indices: vec![0, 1]
            }
        );
    }

    #[test]
    fn tag_characters_and_blank_glyphs_are_stripped() {
        assert_eq!(sanitize_inline("a\u{E0041}\u{3164}\u{061C}b"), "ab");
    }

    #[test]
    fn digit_only_labels_are_rejected_with_a_correctable_error() {
        let err = ChoiceOptions::new(vec!["2".into(), "4".into()]).unwrap_err();
        assert_eq!(err, AskUserInputError::NumericLabel("2".into()));
        assert!(err.to_string().contains("add units"));
        assert!(ChoiceOptions::new(vec![" 8 ".into(), "x".into()]).is_err());
        assert!(ChoiceOptions::new(vec!["2 GB".into(), "4 GB".into()]).is_ok());
    }

    #[test]
    fn bidi_and_zero_width_characters_are_stripped() {
        let o = opts(&["re\u{200b}d", "gr\u{202e}een"]);
        assert_eq!(o.labels(), ["red", "green"]);
        let q = AskUserRequest::free_text("a\u{2066}b\u{feff}c").unwrap();
        assert_eq!(q.question(), "abc");
    }

    #[test]
    fn labels_stay_on_one_line() {
        let o = opts(&["a\nb", "c\td"]);
        assert_eq!(o.labels(), ["a b", "c d"]);
    }

    #[test]
    fn question_blank_lines_collapse_and_line_count_is_capped() {
        let q = AskUserRequest::free_text(format!("first{}last", "\n".repeat(400))).unwrap();
        assert_eq!(q.question(), "first\n\nlast");
        let many = (0..50)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let q = AskUserRequest::free_text(many).unwrap();
        assert_eq!(q.question().lines().count(), MAX_QUESTION_LINES);
    }

    #[test]
    fn multi_with_other_allowed_falls_back_to_text_like_single() {
        let req =
            AskUserRequest::multi_choice("q", opts(&["a", "b"]), OtherAnswer::Allowed).unwrap();
        assert_eq!(
            req.parse_reply("1, 9").unwrap(),
            AskUserAnswer::Text("1, 9".into())
        );
        assert_eq!(
            req.parse_reply("something else").unwrap(),
            AskUserAnswer::Text("something else".into())
        );
    }

    #[test]
    fn single_numeric_reply_is_always_a_position() {
        let req = AskUserRequest::single_choice(
            "q",
            opts(&["2 GB", "4 GB", "8 GB"]),
            OtherAnswer::Forbidden,
        )
        .unwrap();
        assert_eq!(
            req.parse_reply("1").unwrap(),
            AskUserAnswer::Selected { indices: vec![0] }
        );
    }

    #[test]
    fn dialog_answer_for_a_closed_choice_is_read_by_position() {
        let req = AskUserRequest::single_choice(
            "Size?",
            opts(&["2 GB", "4 GB", "8 GB"]),
            OtherAnswer::Forbidden,
        )
        .unwrap();
        assert_eq!(
            req.closed_choice_values().unwrap(),
            ["1. 2 GB", "2. 4 GB", "3. 8 GB"]
        );
        assert_eq!(
            req.parse_dialog_answer("1. 2 GB").unwrap(),
            AskUserAnswer::Selected { indices: vec![0] }
        );
        assert_eq!(
            req.parse_dialog_answer("3. 8 GB").unwrap(),
            AskUserAnswer::Selected { indices: vec![2] }
        );
    }

    #[test]
    fn dialog_answer_for_free_text_is_unchanged() {
        let req = AskUserRequest::free_text("Where?").unwrap();
        assert_eq!(
            req.parse_dialog_answer("1. my path").unwrap(),
            AskUserAnswer::Text("1. my path".into())
        );
    }

    #[test]
    fn dialog_message_lists_options_only_for_text_field_choices() {
        let closed =
            AskUserRequest::single_choice("Pick", opts(&["a", "b"]), OtherAnswer::Forbidden)
                .unwrap();
        assert!(!closed.dialog_message().contains("1. a"));
        assert!(closed.dialog_answer_hint().is_none());
        let open =
            AskUserRequest::multi_choice("Pick", opts(&["a", "b"]), OtherAnswer::Allowed).unwrap();
        assert!(open.dialog_message().contains("1. a") && open.dialog_message().contains("2. b"));
        assert!(
            open.dialog_answer_hint()
                .unwrap()
                .contains("your own answer")
        );
        assert!(open.dialog_message().starts_with(ASK_USER_HEADER));
    }
}
