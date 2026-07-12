// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structural outbound-message masking at the provider boundary (#5437).
//!
//! [`MaskedProvider`] wraps any [`crate::any::AnyProvider`] so every outbound `chat*` call
//! masks message text via an injected [`OutboundMasker`] before the request leaves the
//! process. Wiring it once, at the point an `AnyProvider` is constructed
//! (`zeph_core::provider_factory::build_provider_from_entry`), covers every current and future
//! `chat*` **call site** that dispatches through an already-wrapped provider handle — no
//! per-call-site enumeration is required there.
//!
//! This does *not* by itself make an unmasked provider **assignment** impossible: `self.provider`
//! (and the other provider-typed `Agent` fields) can still be reassigned later — e.g. a runtime
//! provider switch — through a path that constructs a fresh, unwrapped `AnyProvider` and skips
//! wrapping. Two rounds of this fix each missed one such reassignment site (the ACP
//! `set_session_config_option` provider override was the last one found). Closing that class of
//! gap for good requires a second, independent guard at the *assignment* boundary — see
//! `zeph_core::agent::Agent::set_provider`, the single method every `self.provider` reassignment
//! after construction must go through, which re-wraps on every swap and `debug_assert`s the
//! invariant so a future bypass fails loudly in tests instead of silently shipping unmasked.
//!
//! `zeph-llm` cannot depend on `zeph-sanitizer` (which owns the concrete secret registry and
//! itself depends on `zeph-llm`), so the masking capability is expressed here as a minimal,
//! sanitizer-agnostic trait ([`OutboundMasker`]) that a higher-level crate implements as a thin
//! adapter over its concrete registry.

use std::fmt;
use std::sync::Arc;

use crate::LlmError;
use crate::any::AnyProvider;
use crate::provider::{
    ChatExtras, ChatResponse, ChatStream, LlmProvider, Message, MessagePart, ToolDefinition,
};
use crate::provider_dyn::LlmProviderDyn;

/// Capability for masking outbound message text before it reaches a provider's wire format.
///
/// Implemented by an adapter in a higher-level crate (`zeph-core`, which owns the concrete
/// secret registry) and injected into an [`AnyProvider`] via [`AnyProvider::masked`].
pub trait OutboundMasker: fmt::Debug + Send + Sync {
    /// Return a masked copy of `text` when it contains anything that should be masked, or
    /// `None` when `text` is unchanged. Implementors should back this with a cheap,
    /// allocation-free "does anything match" pre-check so the common case (no secret present)
    /// costs nothing beyond that check.
    fn mask(&self, text: &str) -> Option<String>;
}

/// Wraps an [`AnyProvider`] so every outbound `chat*`/`chat_with_tools*` call masks message
/// text via an [`OutboundMasker`] before delegating to the inner provider.
///
/// # Examples
///
/// ```rust
/// use std::sync::Arc;
/// use zeph_llm::any::AnyProvider;
/// use zeph_llm::masking::OutboundMasker;
/// use zeph_llm::ollama::OllamaProvider;
///
/// #[derive(Debug)]
/// struct UppercaseMasker;
/// impl OutboundMasker for UppercaseMasker {
///     fn mask(&self, text: &str) -> Option<String> {
///         if text.contains("secret") { Some(text.replace("secret", "***")) } else { None }
///     }
/// }
///
/// let inner = AnyProvider::Ollama(OllamaProvider::new("http://localhost:11434", "m".into(), "e".into()));
/// let masked = inner.masked(Arc::new(UppercaseMasker));
/// assert_eq!(masked.name(), "ollama"); // delegation still works transparently
/// # use zeph_llm::provider::LlmProvider;
/// ```
#[derive(Clone)]
pub struct MaskedProvider {
    pub(crate) inner: Box<AnyProvider>,
    pub(crate) masker: Arc<dyn OutboundMasker>,
    /// Shared across every clone of this wrapper (same `Arc`) so the count reflects total
    /// masking activity for the logical session provider, not just one clone's calls.
    applied_count: Arc<std::sync::atomic::AtomicU64>,
}

impl fmt::Debug for MaskedProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaskedProvider")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl MaskedProvider {
    /// Wrap `inner` with `masker`.
    #[must_use]
    pub fn new(inner: AnyProvider, masker: Arc<dyn OutboundMasker>) -> Self {
        Self {
            inner: Box::new(inner),
            masker,
            applied_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Return the wrapped provider, discarding the masking layer.
    #[must_use]
    pub fn inner(&self) -> &AnyProvider {
        &self.inner
    }

    /// Number of outbound calls (across every clone of this wrapper) that had at least one
    /// secret masked. Exposed for the `secret_mask_applied` observability metric.
    #[must_use]
    pub fn applied_count(&self) -> u64 {
        self.applied_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Build a masked copy of `messages` and record it in [`Self::applied_count`], or `None`
    /// when nothing needed masking. See [`mask_messages`] for the masking rules.
    pub(crate) fn mask_messages(&self, messages: &[Message]) -> Option<Vec<Message>> {
        let result = mask_messages(self.masker.as_ref(), messages);
        if result.is_some() {
            self.applied_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }
}

/// Build a masked copy of `messages` against `masker`, or `None` when nothing needed masking.
///
/// Masks every text-bearing [`MessagePart`] variant that can carry model-visible content
/// derived from tool output or conversation history — including `ToolOutput.body` and
/// `ToolResult.content` (a gap in the crate's own [`MessagePart::as_plain_text`] helper, which
/// only covers `Text`/`Recall`/`CodeContext`/`Summary`/`CrossSession`) — while never touching
/// `ThinkingBlock`/`RedactedThinkingBlock` (mutating these would invalidate the provider's
/// signature verification) or `ToolUse.input`/`Image` (structural, not model-visible free text).
///
/// This is the same masking pass [`MaskedProvider`] applies to every outbound `chat*` call.
/// Exposed standalone for callers outside the provider dispatch path that still need a masked
/// view of a message slice for local purposes — e.g. debug-dump serialization, which writes a
/// human-readable JSON representation of the conversation independent of whatever wire format
/// the provider's own `debug_request_json` produces.
///
/// Runs a non-cloning pre-scan over borrowed text first (calling [`OutboundMasker::mask`] on
/// each candidate field without touching `messages`) and returns `None` immediately if nothing
/// matches, so the common case — masking enabled but this particular slice has no registered
/// secret in it — never clones a single [`Message`]. Every agent-loop call site (Await
/// Discipline, `.claude/rules/rust-code.md`) runs this on every outbound dispatch, so the
/// no-match path must stay allocation-free for `messages` itself; only the (rare) match path
/// pays for `Message`/`MessagePart` clones.
#[must_use]
pub fn mask_messages(masker: &dyn OutboundMasker, messages: &[Message]) -> Option<Vec<Message>> {
    let any_candidate = messages.iter().any(|m| {
        if m.parts.is_empty() {
            masker.mask(&m.content).is_some()
        } else {
            m.parts
                .iter()
                .filter_map(part_text_ref)
                .any(|text| masker.mask(text).is_some())
        }
    });
    if !any_candidate {
        return None;
    }

    let mut any_masked = false;
    let result: Vec<Message> = messages
        .iter()
        .map(|original| {
            let mut msg = original.clone();
            if msg.parts.is_empty() {
                if let Some(masked_content) = masker.mask(&msg.content) {
                    any_masked = true;
                    msg.content = masked_content;
                }
            } else {
                let mut changed = false;
                for part in &mut msg.parts {
                    if let Some(text) = part_text_mut(part)
                        && let Some(masked_text) = masker.mask(text)
                    {
                        *text = masked_text;
                        changed = true;
                    }
                }
                if changed {
                    any_masked = true;
                    msg.rebuild_content();
                }
            }
            msg
        })
        .collect();
    any_masked.then_some(result)
}

/// Return the mutable text field of a text-bearing [`MessagePart`] variant, `None` for
/// variants that carry no maskable free text: `ToolUse.input` (structural JSON the model
/// produced, never a raw secret), `Image`, and `ThinkingBlock`/`RedactedThinkingBlock` (must
/// never be mutated — doing so invalidates the provider's signature verification).
fn part_text_mut(part: &mut MessagePart) -> Option<&mut String> {
    match part {
        MessagePart::Text { text }
        | MessagePart::Recall { text }
        | MessagePart::CodeContext { text }
        | MessagePart::Summary { text }
        | MessagePart::CrossSession { text } => Some(text),
        MessagePart::ToolOutput { body, .. } => Some(body),
        MessagePart::ToolResult { content, .. } => Some(content),
        MessagePart::Compaction { summary } => Some(summary),
        _ => None,
    }
}

/// Immutable-reference counterpart of [`part_text_mut`], used for the non-cloning pre-scan.
fn part_text_ref(part: &MessagePart) -> Option<&str> {
    match part {
        MessagePart::Text { text }
        | MessagePart::Recall { text }
        | MessagePart::CodeContext { text }
        | MessagePart::Summary { text }
        | MessagePart::CrossSession { text } => Some(text.as_str()),
        MessagePart::ToolOutput { body, .. } => Some(body.as_str()),
        MessagePart::ToolResult { content, .. } => Some(content.as_str()),
        MessagePart::Compaction { summary } => Some(summary.as_str()),
        _ => None,
    }
}

impl LlmProvider for MaskedProvider {
    fn context_window(&self) -> Option<usize> {
        LlmProvider::context_window(self.inner.as_ref())
    }

    // The recursive calls below go through `LlmProviderDyn` (concrete `BoxFuture` return),
    // not `LlmProvider` (opaque `impl Future` return) — `AnyProvider::chat` delegates to
    // `MaskedProvider::chat` for its `Masked` variant, so calling back into
    // `LlmProvider::chat` here would make the two native async-fn-in-trait impls'
    // opaque return types mutually depend on each other, which rustc cannot resolve
    // (E0391 cycle). `LlmProviderDyn`'s named `BoxFuture` type breaks the cycle.

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        let masked = self.mask_messages(messages);
        LlmProviderDyn::chat(self.inner.as_ref(), masked.as_deref().unwrap_or(messages)).await
    }

    async fn chat_with_extras(
        &self,
        messages: &[Message],
    ) -> Result<(String, ChatExtras), LlmError> {
        let masked = self.mask_messages(messages);
        LlmProviderDyn::chat_with_extras(self.inner.as_ref(), masked.as_deref().unwrap_or(messages))
            .await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let masked = self.mask_messages(messages);
        LlmProviderDyn::chat_stream(self.inner.as_ref(), masked.as_deref().unwrap_or(messages))
            .await
    }

    fn supports_streaming(&self) -> bool {
        LlmProvider::supports_streaming(self.inner.as_ref())
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        // Embeddings feed semantic search/similarity, not model-visible chat context — masking
        // would corrupt the embedding space for no confidentiality benefit (the embedding
        // vector itself doesn't reveal the plaintext to a human/log reader the way a chat
        // transcript does). Not a #5437 concern; pass through unchanged.
        LlmProviderDyn::embed(self.inner.as_ref(), text).await
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        LlmProviderDyn::embed_batch(self.inner.as_ref(), texts).await
    }

    fn supports_embeddings(&self) -> bool {
        LlmProvider::supports_embeddings(self.inner.as_ref())
    }

    fn name(&self) -> &str {
        LlmProvider::name(self.inner.as_ref())
    }

    fn model_identifier(&self) -> &str {
        LlmProvider::model_identifier(self.inner.as_ref())
    }

    fn effective_model_identifier(&self) -> &str {
        LlmProvider::effective_model_identifier(self.inner.as_ref())
    }

    fn supports_structured_output(&self) -> bool {
        LlmProvider::supports_structured_output(self.inner.as_ref())
    }

    fn supports_vision(&self) -> bool {
        LlmProvider::supports_vision(self.inner.as_ref())
    }

    fn supports_tool_use(&self) -> bool {
        LlmProvider::supports_tool_use(self.inner.as_ref())
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let masked = self.mask_messages(messages);
        LlmProviderDyn::chat_with_tools(
            self.inner.as_ref(),
            masked.as_deref().unwrap_or(messages),
            tools,
        )
        .await
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        LlmProvider::last_cache_usage(self.inner.as_ref())
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        LlmProvider::last_usage(self.inner.as_ref())
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let masked = self.mask_messages(messages);
        LlmProvider::debug_request_json(
            self.inner.as_ref(),
            masked.as_deref().unwrap_or(messages),
            tools,
            stream,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::provider::Role;

    #[derive(Debug)]
    struct FixedMasker;
    impl OutboundMasker for FixedMasker {
        fn mask(&self, text: &str) -> Option<String> {
            if text.contains("SECRET_VALUE") {
                Some(text.replace("SECRET_VALUE", "<MASKED>"))
            } else {
                None
            }
        }
    }

    fn masked(inner: AnyProvider) -> MaskedProvider {
        MaskedProvider::new(inner, Arc::new(FixedMasker))
    }

    fn mock_any(responses: Vec<String>) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(responses))
    }

    #[tokio::test]
    async fn chat_masks_flat_content() {
        let (mock, recorded) = MockProvider::with_responses(vec!["ok".into()]).with_recording();
        let mp = masked(AnyProvider::Mock(mock));
        let messages = vec![Message::from_legacy(
            Role::User,
            "value is SECRET_VALUE here",
        )];
        LlmProvider::chat(&mp, &messages).await.unwrap();
        let sent = recorded.lock().unwrap();
        assert!(!sent[0][0].content.contains("SECRET_VALUE"));
        assert!(sent[0][0].content.contains("<MASKED>"));
    }

    #[tokio::test]
    async fn chat_with_no_match_forwards_unchanged() {
        let (mock, recorded) = MockProvider::with_responses(vec!["ok".into()]).with_recording();
        let mp = masked(AnyProvider::Mock(mock));
        let messages = vec![Message::from_legacy(Role::User, "nothing sensitive")];
        LlmProvider::chat(&mp, &messages).await.unwrap();
        let sent = recorded.lock().unwrap();
        assert_eq!(sent[0][0].content, "nothing sensitive");
    }

    /// Counts `mask()` invocations, to pin the non-cloning pre-scan's cost profile: a no-match
    /// slice must call `mask()` exactly once per text field and then stop (pre-scan only, no
    /// second clone-and-mask pass); a slice with a match calls `mask()` up to twice per matching
    /// field (once in the pre-scan, once when actually building the masked copy).
    #[derive(Debug)]
    struct CountingMasker {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl CountingMasker {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl OutboundMasker for CountingMasker {
        fn mask(&self, text: &str) -> Option<String> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if text.contains("SECRET_VALUE") {
                Some(text.replace("SECRET_VALUE", "<MASKED>"))
            } else {
                None
            }
        }
    }

    #[test]
    fn mask_messages_no_match_returns_none_via_pre_scan_only() {
        let masker = CountingMasker::new();
        let messages: Vec<Message> = (0..25)
            .map(|i| Message::from_legacy(Role::User, format!("clean message #{i}")))
            .collect();

        let result = mask_messages(&masker, &messages);

        assert!(
            result.is_none(),
            "no registered secret anywhere — must return None"
        );
        assert_eq!(
            masker.call_count(),
            25,
            "no-match path must call mask() exactly once per message (pre-scan only) — a \
             higher count would mean a second clone-and-mask pass ran despite no match"
        );
    }

    #[test]
    fn mask_messages_finds_match_among_many_clean_messages() {
        let masker = CountingMasker::new();
        let mut messages: Vec<Message> = (0..10)
            .map(|i| Message::from_legacy(Role::User, format!("clean message #{i}")))
            .collect();
        messages.push(Message::from_legacy(
            Role::User,
            "value is SECRET_VALUE here",
        ));

        let result = mask_messages(&masker, &messages).expect("one message matches");

        assert_eq!(result.len(), 11);
        assert!(!result[10].content.contains("SECRET_VALUE"));
        assert!(result[10].content.contains("<MASKED>"));
        // Unrelated clean messages are still present and unchanged.
        assert_eq!(result[0].content, "clean message #0");
    }

    #[tokio::test]
    async fn chat_masks_tool_result_content() {
        let (mock, recorded) = MockProvider::with_responses(vec!["ok".into()]).with_recording();
        let mp = masked(AnyProvider::Mock(mock));
        let messages = vec![Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "id1".into(),
                content: "tool printed SECRET_VALUE".into(),
                is_error: false,
            }],
        )];
        LlmProvider::chat(&mp, &messages).await.unwrap();
        let sent = recorded.lock().unwrap();
        let MessagePart::ToolResult { content, .. } = &sent[0][0].parts[0] else {
            panic!("expected ToolResult part");
        };
        assert!(!content.contains("SECRET_VALUE"));
        // flat `content` field must be resynced too.
        assert!(!sent[0][0].content.contains("SECRET_VALUE"));
    }

    #[tokio::test]
    async fn chat_masks_tool_output_body() {
        let (mock, recorded) = MockProvider::with_responses(vec!["ok".into()]).with_recording();
        let mp = masked(AnyProvider::Mock(mock));
        let messages = vec![Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "env dump: SECRET_VALUE".into(),
                compacted_at: None,
            }],
        )];
        LlmProvider::chat(&mp, &messages).await.unwrap();
        let sent = recorded.lock().unwrap();
        let MessagePart::ToolOutput { body, .. } = &sent[0][0].parts[0] else {
            panic!("expected ToolOutput part");
        };
        assert!(!body.contains("SECRET_VALUE"));
    }

    #[tokio::test]
    async fn chat_never_touches_thinking_block() {
        let (mock, recorded) = MockProvider::with_responses(vec!["ok".into()]).with_recording();
        let mp = masked(AnyProvider::Mock(mock));
        let messages = vec![Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ThinkingBlock {
                thinking: "reasoning mentions SECRET_VALUE".into(),
                signature: "sig123".into(),
            }],
        )];
        LlmProvider::chat(&mp, &messages).await.unwrap();
        let sent = recorded.lock().unwrap();
        let MessagePart::ThinkingBlock {
            thinking,
            signature,
        } = &sent[0][0].parts[0]
        else {
            panic!("expected ThinkingBlock part");
        };
        // Untouched — mutating a signed thinking block would break signature verification.
        assert!(thinking.contains("SECRET_VALUE"));
        assert_eq!(signature, "sig123");
    }

    #[tokio::test]
    async fn delegation_methods_pass_through_to_inner() {
        let mp = masked(mock_any(vec![]));
        assert_eq!(LlmProvider::name(&mp), "mock");
        assert_eq!(
            LlmProvider::context_window(&mp),
            LlmProvider::context_window(mp.inner())
        );
    }

    #[test]
    fn debug_impl_does_not_panic() {
        let mp = masked(mock_any(vec![]));
        let s = format!("{mp:?}");
        assert!(s.contains("MaskedProvider"));
    }

    /// #6183: a masked Router must still resolve the real dispatched sub-provider's model id,
    /// not fall through to the trait default (`model_identifier()` -> inner `"router"` label).
    #[test]
    fn effective_model_identifier_resolves_through_masked_router() {
        use crate::claude::ClaudeProvider;
        use crate::router::RouterProvider;

        let claude = AnyProvider::Claude(ClaudeProvider::new(
            "k".into(),
            "claude-3-opus-think".into(),
            1024,
        ));
        let router = RouterProvider::new(vec![claude]);
        *router.state.last_active_provider.lock() = Some("claude".to_owned());

        let mp = masked(AnyProvider::Router(Box::new(router)));

        assert_eq!(
            LlmProvider::effective_model_identifier(&mp),
            "claude-3-opus-think"
        );
        assert_ne!(LlmProvider::effective_model_identifier(&mp), "router");
    }
}
