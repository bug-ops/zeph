// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Outcome of the provider's thinking-block prefix check for one request.

use serde::Deserialize;

/// Whether the thinking blocks sent in a request survived the provider's prefix check.
///
/// Claude models with preserved thinking bind each thinking block to the conversation prefix it
/// was produced under. With `drop_block`, a mismatching block and everything after it is
/// silently dropped and reported back; callers use that to strip the same thinking from their
/// own history so later requests stay append-only.
///
/// # Examples
///
/// ```
/// use zeph_llm::ThinkingReplay;
///
/// assert_eq!(ThinkingReplay::default(), ThinkingReplay::Intact);
/// let dropped = ThinkingReplay::Dropped { first_message_index: 3 };
/// assert!(matches!(dropped, ThinkingReplay::Dropped { first_message_index: 3 }));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingReplay {
    /// Every thinking block was accepted, or the model does not run the check.
    #[default]
    Intact,
    /// Thinking was dropped starting at this index of the request's `messages` array.
    ///
    /// The index is in the wire `messages[]` space (system messages hoisted out, hidden
    /// messages filtered, adjacent same-role messages coalesced), which is never greater than
    /// the corresponding history index. Stripping thinking from this index onward therefore
    /// covers at least the dropped blocks.
    Dropped {
        /// First wire-message index whose thinking was dropped.
        first_message_index: usize,
    },
}

/// Why the provider dropped a thinking block.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DropReason {
    /// The block was signed over a different conversation prefix: the caller's history changed.
    PrefixBindingMismatch,
    /// The block was produced by another model; the provider omits what the current model
    /// cannot read and the history stays valid for the model that wrote it.
    ModelBindingMismatch,
    /// The block belongs to another organization; same treatment as a model mismatch.
    OrganizationBindingMismatch,
    /// A reason this client does not know. Treated like a prefix mismatch, the safe side.
    #[default]
    #[serde(other)]
    Unknown,
}

impl DropReason {
    /// Whether the caller's history must be stripped to match the provider's view.
    fn invalidates_history(self) -> bool {
        matches!(self, Self::PrefixBindingMismatch | Self::Unknown)
    }
}

/// One entry of a Claude response's `input_transformations` array.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum InputTransformation {
    /// A thinking block failed the prefix check and was removed before the model saw it.
    ThinkingDropped {
        #[serde(default)]
        path: String,
        #[serde(default)]
        reason: DropReason,
    },
    /// A thinking block failed the check but enforcement was off, so it was kept (and billed).
    ThinkingMismatchAllowed,
    /// A transformation type this client does not know; ignored.
    #[serde(other)]
    Unknown,
}

impl ThinkingReplay {
    /// Fold `input_transformations` into the lowest dropped message index.
    ///
    /// A dropped entry whose path cannot be parsed counts as index 0, which strips a superset.
    pub(crate) fn from_transformations(transformations: &[InputTransformation]) -> Self {
        transformations
            .iter()
            .filter_map(|t| match t {
                InputTransformation::ThinkingDropped { path, reason }
                    if reason.invalidates_history() =>
                {
                    Some(message_index_from_path(path).unwrap_or(0))
                }
                InputTransformation::ThinkingDropped { .. }
                | InputTransformation::ThinkingMismatchAllowed
                | InputTransformation::Unknown => None,
            })
            .min()
            .map_or(Self::Intact, |first_message_index| Self::Dropped {
                first_message_index,
            })
    }
}

/// Parse the message index out of a path such as `messages.3.content.0`.
fn message_index_from_path(path: &str) -> Option<usize> {
    path.strip_prefix("messages.")?
        .split('.')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<InputTransformation> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn empty_is_intact() {
        assert_eq!(
            ThinkingReplay::from_transformations(&[]),
            ThinkingReplay::Intact
        );
    }

    #[test]
    fn lowest_dropped_index_wins() {
        let t = parse(
            r#"[
                {"type":"thinking_dropped","path":"messages.7.content.0","reason":"prefix_binding_mismatch"},
                {"type":"thinking_dropped","path":"messages.3.content.0","reason":"prefix_binding_mismatch"}
            ]"#,
        );
        assert_eq!(
            ThinkingReplay::from_transformations(&t),
            ThinkingReplay::Dropped {
                first_message_index: 3
            }
        );
    }

    #[test]
    fn allowed_and_unknown_entries_are_ignored() {
        let t = parse(
            r#"[
                {"type":"thinking_mismatch_allowed","path":"messages.1.content.0","reason":"prefix_binding_mismatch"},
                {"type":"something_new","path":"messages.2"}
            ]"#,
        );
        assert_eq!(
            ThinkingReplay::from_transformations(&t),
            ThinkingReplay::Intact
        );
    }

    #[test]
    fn only_prefix_mismatch_and_unknown_reasons_invalidate_history() {
        for (reason, expected) in [
            ("prefix_binding_mismatch", true),
            ("something_new", true),
            ("model_binding_mismatch", false),
            ("organization_binding_mismatch", false),
        ] {
            let t = parse(&format!(
                r#"[{{"type":"thinking_dropped","path":"messages.2.content.0","reason":"{reason}"}}]"#
            ));
            let replay = ThinkingReplay::from_transformations(&t);
            assert_eq!(
                matches!(replay, ThinkingReplay::Dropped { .. }),
                expected,
                "{reason}"
            );
        }
    }

    #[test]
    fn missing_reason_counts_as_history_invalidating() {
        let t = parse(r#"[{"type":"thinking_dropped","path":"messages.1.content.0"}]"#);
        assert!(matches!(
            ThinkingReplay::from_transformations(&t),
            ThinkingReplay::Dropped { .. }
        ));
    }

    #[test]
    fn unparseable_path_strips_everything() {
        let t = parse(r#"[{"type":"thinking_dropped","path":"weird"}]"#);
        assert_eq!(
            ThinkingReplay::from_transformations(&t),
            ThinkingReplay::Dropped {
                first_message_index: 0
            }
        );
    }
}
