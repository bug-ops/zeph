// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session digest generation at session end (#2289).
//!
//! Generates a compact NL digest of the conversation and stores it in `SQLite`
//! for injection into the context at the start of the next session.

use std::fmt::Write as _;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use zeph_llm::provider::{Message, MessageMetadata, Role};
use zeph_memory::TokenCounter;

/// Strip prompt-injection patterns from LLM-generated digest text.
fn sanitize_digest(text: &str) -> String {
    static INJECTION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        vec![
            Regex::new(r"<[^>]{1,100}>").unwrap(),
            Regex::new(r"(?i)\[/?INST\]|\[/?SYS\]").unwrap(),
            Regex::new(r"<\|[^|]{1,30}\|>").unwrap(),
            Regex::new(r"(?im)^(system|assistant|user)\s*:\s*").unwrap(),
        ]
    });
    static INJECTION_LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)ignore\s+.{0,30}(instruction|above|previous|system)").unwrap()
    });

    let mut result = text.to_string();
    for pattern in INJECTION_PATTERNS.iter() {
        let replaced = pattern.replace_all(&result, "");
        result = replaced.into_owned();
    }
    result
        .lines()
        .filter(|line| !INJECTION_LINE.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncate `text` to at most `max_tokens` tokens using the same binary-search approach.
fn truncate_digest(text: &str, max_tokens: usize, tc: &TokenCounter) -> String {
    if tc.count_tokens(text) <= max_tokens {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect();
        if tc.count_tokens(&candidate) <= max_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let candidate: String = chars[..lo].iter().collect();
    if let Some(pos) = candidate.rfind('\n') {
        candidate[..pos].to_string()
    } else {
        candidate
    }
}

use crate::channel::Channel;

use super::Agent;

/// Generate and persist a digest for a completed conversation from a background task.
///
/// Called fire-and-forget from `reset_conversation`. All errors are logged as warnings
/// and swallowed.
pub(super) async fn generate_and_store_digest(
    provider: &zeph_llm::any::AnyProvider,
    memory: &zeph_memory::semantic::SemanticMemory,
    conversation_id: zeph_memory::ConversationId,
    messages: &[zeph_llm::provider::Message],
    digest_config: &crate::config::DigestConfig,
    tc: &zeph_memory::TokenCounter,
) {
    if messages.is_empty() {
        return;
    }

    let max_input = digest_config.max_input_messages;
    let max_tokens = digest_config.max_tokens;

    let slice = if messages.len() > max_input {
        &messages[messages.len() - max_input..]
    } else {
        messages
    };

    let mut conv_text = String::new();
    for msg in slice {
        let role = match msg.role {
            zeph_llm::provider::Role::User => "User",
            zeph_llm::provider::Role::Assistant => "Assistant",
            zeph_llm::provider::Role::System => "System",
        };
        let _ =
            std::fmt::Write::write_fmt(&mut conv_text, format_args!("{role}: {}\n\n", msg.content));
    }

    let prompt = format!(
        "You are a session summarizer. Read the following conversation excerpt and produce \
         a compact digest (under {max_tokens} tokens) of the key facts, decisions, outcomes, \
         and open questions from this session. Be specific and concise. \
         Output ONLY the digest text, no preamble.\n\n\
         Conversation:\n{conv_text}\n\
         Digest:"
    );

    let chat_messages = vec![zeph_llm::provider::Message {
        role: zeph_llm::provider::Role::User,
        content: prompt,
        parts: vec![],
        metadata: zeph_llm::provider::MessageMetadata::default(),
    }];

    let timeout = Duration::from_secs(30);
    let digest_text = tokio::select! {
        () = async { tokio::time::sleep(timeout).await } => {
            tracing::warn!("session digest (/new): LLM call timed out");
            return;
        }
        result = provider.chat_with_named_provider(&digest_config.provider, &chat_messages) => {
            match result {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!("session digest (/new): LLM call failed: {e:#}");
                    return;
                }
            }
        }
    };

    let sanitized = sanitize_digest(&digest_text);
    let final_text = truncate_digest(&sanitized, max_tokens, tc);
    let token_count = i64::try_from(tc.count_tokens(&final_text)).unwrap_or(i64::MAX);

    if let Err(e) = memory
        .sqlite()
        .save_session_digest(conversation_id, &final_text, token_count)
        .await
    {
        tracing::warn!("session digest (/new): storage failed: {e:#}");
    } else {
        tracing::info!(
            conversation_id = conversation_id.0,
            tokens = token_count,
            "session digest stored (via /new)"
        );
    }
}

impl<C: Channel> Agent<C> {
    /// Generate and persist a session digest at shutdown when digest is enabled.
    ///
    /// All errors are logged as warnings and swallowed — shutdown must never fail.
    pub(super) async fn maybe_store_session_digest(&mut self) {
        if !self.memory_state.digest_config.enabled {
            return;
        }
        let Some(memory) = self.memory_state.memory.clone() else {
            return;
        };
        let Some(conversation_id) = self.memory_state.conversation_id else {
            return;
        };

        let max_input = self.memory_state.digest_config.max_input_messages;
        let max_tokens = self.memory_state.digest_config.max_tokens;
        let provider_name = self.memory_state.digest_config.provider.clone();

        // Collect last N non-system messages.
        let non_system: Vec<_> = self
            .msg
            .messages
            .iter()
            .skip(1)
            .filter(|m| m.role != Role::System)
            .collect();
        if non_system.is_empty() {
            return;
        }
        let slice = if non_system.len() > max_input {
            &non_system[non_system.len() - max_input..]
        } else {
            &non_system[..]
        };

        let mut conv_text = String::new();
        for msg in slice {
            let role = match msg.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::System => "System",
            };
            let _ = write!(conv_text, "{role}: {}\n\n", msg.content);
        }

        let prompt = format!(
            "You are a session summarizer. Read the following conversation excerpt and produce \
             a compact digest (under {max_tokens} tokens) of the key facts, decisions, outcomes, \
             and open questions from this session. Be specific and concise. \
             Output ONLY the digest text, no preamble.\n\n\
             Conversation:\n{conv_text}\n\
             Digest:"
        );

        let chat_messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let _ = self
            .channel
            .send_status("Generating session digest...")
            .await;

        let timeout = Duration::from_secs(30);
        let digest_text = tokio::select! {
            () = async { tokio::time::sleep(timeout).await } => {
                tracing::warn!("session digest: LLM call timed out");
                let _ = self.channel.send_status("").await;
                return;
            }
            result = self.provider.chat_with_named_provider(&provider_name, &chat_messages) => {
                match result {
                    Ok(text) => text,
                    Err(e) => {
                        tracing::warn!("session digest: LLM call failed: {e:#}");
                        let _ = self.channel.send_status("").await;
                        return;
                    }
                }
            }
        };

        // Sanitize to prevent injection via LLM-generated content (strip role prefixes).
        let sanitized = sanitize_digest(&digest_text);

        // Truncate to max_tokens budget.
        let tc = &self.metrics.token_counter;
        let final_text = truncate_digest(&sanitized, max_tokens, tc);

        let token_count = i64::try_from(tc.count_tokens(&final_text)).unwrap_or(i64::MAX);

        if let Err(e) = memory
            .sqlite()
            .save_session_digest(conversation_id, &final_text, token_count)
            .await
        {
            tracing::warn!("session digest: storage failed: {e:#}");
        } else {
            tracing::info!(
                conversation_id = conversation_id.0,
                tokens = token_count,
                "session digest stored"
            );
            // Update the cached digest so it is available in the same session if re-used.
            self.memory_state.cached_session_digest = Some((
                final_text,
                usize::try_from(token_count).unwrap_or(max_tokens),
            ));
        }

        let _ = self.channel.send_status("").await;
    }

    /// Load the session digest from `SQLite` and cache it in `MemoryState`.
    ///
    /// Called once at session start so the digest is ready for context injection.
    /// All errors are logged and swallowed.
    pub(super) async fn load_and_cache_session_digest(&mut self) {
        if !self.memory_state.digest_config.enabled {
            return;
        }
        let Some(memory) = self.memory_state.memory.clone() else {
            return;
        };
        let Some(conversation_id) = self.memory_state.conversation_id else {
            return;
        };

        match memory.sqlite().load_session_digest(conversation_id).await {
            Ok(Some(digest)) => {
                let token_count =
                    usize::try_from(digest.token_count).unwrap_or(digest.digest.len() / 4);
                tracing::debug!(
                    conversation_id = conversation_id.0,
                    tokens = token_count,
                    "session digest loaded"
                );
                self.memory_state.cached_session_digest = Some((digest.digest, token_count));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("session digest: load failed: {e:#}");
            }
        }
    }
}
