// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`zeph_commands::MemoryAccess`] implementation for [`Agent<C>`]: memory tier stats and
//! promotion, the cross-thread key-value store, and compression guidelines.
//!
//! [`Agent<C>`]: super::Agent

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use tracing::Instrument as _;
use zeph_commands::{CommandError, MemoryAccess};
use zeph_memory::MessageId;

use super::Agent;
use crate::channel::Channel;

impl<C: Channel + Send + 'static> MemoryAccess for Agent<C> {
    // ----- /memory -----

    fn memory_tiers<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                let Some(memory) = self.services.memory.persistence.memory.clone() else {
                    return Ok("Memory not configured.".to_owned());
                };
                match memory.sqlite().count_messages_by_tier().await {
                    Ok((episodic, semantic)) => {
                        let mut out = String::new();
                        let _ = writeln!(out, "Memory tiers:");
                        let _ = writeln!(out, "  Working:  (current context window — virtual)");
                        let _ = writeln!(out, "  Episodic: {episodic} messages");
                        let _ = writeln!(out, "  Semantic: {semantic} facts");
                        Ok(out.trim_end().to_owned())
                    }
                    Err(e) => Ok(format!("Failed to query tier stats: {e}")),
                }
            }
            .instrument(tracing::info_span!("core.agent_access.memory_tiers")),
        )
    }

    fn memory_promote<'a>(
        &'a mut self,
        ids_str: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                let Some(memory) = self.services.memory.persistence.memory.clone() else {
                    return Ok("Memory not configured.".to_owned());
                };
                let ids: Vec<MessageId> = ids_str
                    .split_whitespace()
                    .filter_map(|s| s.parse::<i64>().ok().map(MessageId))
                    .collect();
                if ids.is_empty() {
                    return Ok(
                        "Usage: /memory promote <id> [id...]\nExample: /memory promote 42 43 44"
                            .to_owned(),
                    );
                }
                match memory.sqlite().manual_promote(&ids).await {
                    Ok(count) => Ok(format!("Promoted {count} message(s) to semantic tier.")),
                    Err(e) => Ok(format!("Promotion failed: {e}")),
                }
            }
            .instrument(tracing::info_span!("core.agent_access.memory_promote")),
        )
    }

    // ----- /store -----

    fn store_command<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                const USAGE: &str = "Usage: /store {get <ns> <key> | put <ns> <key> <value...> \
                                      | list <ns_prefix> [limit] | delete <ns> <key>}";

                let store_config = self.services.memory.persistence.store_config.clone();
                if !store_config.enabled {
                    return Ok(
                        "Cross-thread store is disabled ([memory.store].enabled = false)."
                            .to_owned(),
                    );
                }
                let Some(memory) = self.services.memory.persistence.memory.clone() else {
                    return Ok("Memory not configured.".to_owned());
                };

                let owner_key = self.services.session.owner_key.as_str();
                let mut parts = args.split_whitespace();
                let Some(sub) = parts.next() else {
                    return Ok(USAGE.to_owned());
                };

                let result = match sub {
                    "get" => {
                        let (Some(ns), Some(key)) = (parts.next(), parts.next()) else {
                            return Ok("Usage: /store get <namespace> <key>".to_owned());
                        };
                        match memory.sqlite().store_get(owner_key, ns, key).await {
                            Ok(Some(item)) => item.value,
                            Ok(None) => format!("No value found for {ns}/{key}."),
                            Err(e) => return Err(CommandError::new(e.to_string())),
                        }
                    }
                    "put" => {
                        let (Some(ns), Some(key)) = (parts.next(), parts.next()) else {
                            return Ok("Usage: /store put <namespace> <key> <value...>".to_owned());
                        };
                        let value = parts.collect::<Vec<_>>().join(" ");
                        if value.is_empty() {
                            return Ok("Usage: /store put <namespace> <key> <value...>".to_owned());
                        }
                        match memory
                            .sqlite()
                            .store_put(
                                owner_key,
                                ns,
                                key,
                                &value,
                                store_config.max_value_bytes,
                                None,
                            )
                            .await
                        {
                            Ok(item) => format!("Stored {ns}/{key} (version {}).", item.version),
                            Err(e) => return Err(CommandError::new(e.to_string())),
                        }
                    }
                    "list" => {
                        let prefix = parts.next().unwrap_or("");
                        let limit = parts
                            .next()
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        match memory.sqlite().store_list(owner_key, prefix, limit).await {
                            Ok(items) if items.is_empty() => "No rows found.".to_owned(),
                            Ok(items) => items
                                .iter()
                                .map(|i| {
                                    format!(
                                        "{}/{} = {} (v{})",
                                        i.namespace, i.key, i.value, i.version
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                            Err(e) => return Err(CommandError::new(e.to_string())),
                        }
                    }
                    "delete" => {
                        let (Some(ns), Some(key)) = (parts.next(), parts.next()) else {
                            return Ok("Usage: /store delete <namespace> <key>".to_owned());
                        };
                        match memory.sqlite().store_delete(owner_key, ns, key).await {
                            Ok(true) => format!("Deleted {ns}/{key}."),
                            Ok(false) => format!("No value found for {ns}/{key}."),
                            Err(e) => return Err(CommandError::new(e.to_string())),
                        }
                    }
                    _ => USAGE.to_owned(),
                };
                Ok(result)
            }
            .instrument(tracing::info_span!("core.agent_access.store_command")),
        )
    }

    // ----- /guidelines -----

    fn guidelines<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                const MAX_DISPLAY_CHARS: usize = 4096;

                let Some(memory) = &self.services.memory.persistence.memory else {
                    return Ok("No memory backend initialised.".to_owned());
                };

                let cid = self.services.memory.persistence.conversation_id;
                let sqlite = memory.sqlite();

                let (version, text) = sqlite
                    .load_compression_guidelines(cid)
                    .await
                    .map_err(|e: zeph_memory::MemoryError| CommandError::new(e.to_string()))?;

                if version == 0 || text.is_empty() {
                    return Ok("No compression guidelines generated yet.".to_owned());
                }

                let (_, created_at) = sqlite
                    .load_compression_guidelines_meta(cid)
                    .await
                    .unwrap_or((0, String::new()));

                let (body, truncated) = if text.len() > MAX_DISPLAY_CHARS {
                    let end = text.floor_char_boundary(MAX_DISPLAY_CHARS);
                    (&text[..end], true)
                } else {
                    (text.as_str(), false)
                };

                let mut output =
                    format!("Compression Guidelines (v{version}, updated {created_at}):\n\n{body}");
                if truncated {
                    output.push_str("\n\n[truncated]");
                }
                Ok(output)
            }
            .instrument(tracing::info_span!("core.agent_access.guidelines")),
        )
    }
}
