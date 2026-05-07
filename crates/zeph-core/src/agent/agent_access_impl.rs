// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Implementation of [`zeph_commands::traits::agent::AgentAccess`] for [`Agent<C>`].
//!
//! Each method in `AgentAccess` returns a formatted `String` result (without sending to the
//! channel directly), so that `CommandContext::sink` does not conflict with this borrow.
//! The one exception is methods for subsystems that are already channel-free (memory, graph).
//!
//! [`Agent<C>`]: super::Agent

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use zeph_commands::CommandError;
use zeph_commands::traits::agent::AgentAccess;
use zeph_memory::{GraphExtractionConfig, MessageId, extract_and_store};

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel + Send + 'static> AgentAccess for Agent<C> {
    // ----- /memory -----

    fn memory_tiers<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
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
        })
    }

    fn memory_promote<'a>(
        &'a mut self,
        ids_str: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
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
        })
    }

    // ----- /graph -----

    fn graph_stats<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };
            let Some(store) = memory.graph_store.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };

            let (entities, edges, communities, distribution) = tokio::join!(
                store.entity_count(),
                store.active_edge_count(),
                store.community_count(),
                store.edge_type_distribution()
            );
            let mut msg = format!(
                "Graph memory: {} entities, {} edges, {} communities",
                entities.unwrap_or(0),
                edges.unwrap_or(0),
                communities.unwrap_or(0)
            );
            if let Ok(dist) = distribution
                && !dist.is_empty()
            {
                let dist_str: Vec<String> = dist.iter().map(|(t, c)| format!("{t}={c}")).collect();
                write!(msg, "\nEdge types: {}", dist_str.join(", ")).unwrap_or(());
            }
            Ok(msg)
        })
    }

    fn graph_entities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };
            let Some(store) = memory.graph_store.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };

            let entities = store
                .all_entities()
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            if entities.is_empty() {
                return Ok("No entities found.".to_owned());
            }

            let total = entities.len();
            let display: Vec<String> = entities
                .iter()
                .take(50)
                .map(|e| {
                    format!(
                        "  {:<40}  {:<15}  {}",
                        e.name,
                        e.entity_type.as_str(),
                        e.last_seen_at.split('T').next().unwrap_or(&e.last_seen_at)
                    )
                })
                .collect();
            let mut msg = format!(
                "Entities ({total} total):\n  {:<40}  {:<15}  {}\n{}",
                "NAME",
                "TYPE",
                "LAST SEEN",
                display.join("\n")
            );
            if total > 50 {
                write!(msg, "\n  ...and {} more", total - 50).unwrap_or(());
            }
            Ok(msg)
        })
    }

    fn graph_facts<'a>(
        &'a mut self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };
            let Some(store) = memory.graph_store.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };

            let matches = store
                .find_entity_by_name(name)
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            if matches.is_empty() {
                return Ok(format!("No entity found matching '{name}'."));
            }

            let entity = &matches[0];
            let edges = store
                .edges_for_entity(entity.id)
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            if edges.is_empty() {
                return Ok(format!("Entity '{}' has no known facts.", entity.name));
            }

            let mut entity_names: std::collections::HashMap<i64, String> =
                std::collections::HashMap::new();
            entity_names.insert(entity.id, entity.name.clone());
            for edge in &edges {
                let other_id = if edge.source_entity_id == entity.id {
                    edge.target_entity_id
                } else {
                    edge.source_entity_id
                };
                entity_names.entry(other_id).or_default();
            }
            for (&id, name_val) in &mut entity_names {
                if name_val.is_empty() {
                    if let Ok(Some(other)) = store.find_entity_by_id(id).await {
                        *name_val = other.name;
                    } else {
                        *name_val = format!("#{id}");
                    }
                }
            }

            let lines: Vec<String> = edges
                .iter()
                .map(|e| {
                    let src = entity_names
                        .get(&e.source_entity_id)
                        .cloned()
                        .unwrap_or_else(|| format!("#{}", e.source_entity_id));
                    let tgt = entity_names
                        .get(&e.target_entity_id)
                        .cloned()
                        .unwrap_or_else(|| format!("#{}", e.target_entity_id));
                    format!(
                        "  {} --[{}/{}]--> {}: {} (confidence: {:.2})",
                        src, e.relation, e.edge_type, tgt, e.fact, e.confidence
                    )
                })
                .collect();
            Ok(format!(
                "Facts for '{}':\n{}",
                entity.name,
                lines.join("\n")
            ))
        })
    }

    fn graph_history<'a>(
        &'a mut self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };
            let Some(store) = memory.graph_store.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };

            let matches = store
                .find_entity_by_name(name)
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            if matches.is_empty() {
                return Ok(format!("No entity found matching '{name}'."));
            }

            let entity = &matches[0];
            let edges = store
                .edge_history_for_entity(entity.id, 50)
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            if edges.is_empty() {
                return Ok(format!("Entity '{}' has no edge history.", entity.name));
            }

            let mut entity_names: std::collections::HashMap<i64, String> =
                std::collections::HashMap::new();
            entity_names.insert(entity.id, entity.name.clone());
            for edge in &edges {
                for &id in &[edge.source_entity_id, edge.target_entity_id] {
                    entity_names.entry(id).or_default();
                }
            }
            for (&id, name_val) in &mut entity_names {
                if name_val.is_empty() {
                    if let Ok(Some(other)) = store.find_entity_by_id(id).await {
                        *name_val = other.name;
                    } else {
                        *name_val = format!("#{id}");
                    }
                }
            }

            let n = edges.len();
            let lines: Vec<String> = edges
                .iter()
                .map(|e| {
                    let status = if e.valid_to.is_some() {
                        let date = e
                            .valid_to
                            .as_deref()
                            .and_then(|s| s.split('T').next().or_else(|| s.split(' ').next()))
                            .unwrap_or("?");
                        format!("[expired {date}]")
                    } else {
                        "[active]".to_string()
                    };
                    let src = entity_names
                        .get(&e.source_entity_id)
                        .cloned()
                        .unwrap_or_else(|| format!("#{}", e.source_entity_id));
                    let tgt = entity_names
                        .get(&e.target_entity_id)
                        .cloned()
                        .unwrap_or_else(|| format!("#{}", e.target_entity_id));
                    format!(
                        "  {status} {} --[{}/{}]--> {}: {} (confidence: {:.2})",
                        src, e.relation, e.edge_type, tgt, e.fact, e.confidence
                    )
                })
                .collect();
            Ok(format!(
                "Edge history for '{}' ({n} edges):\n{}",
                entity.name,
                lines.join("\n")
            ))
        })
    }

    fn graph_communities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };
            let Some(store) = memory.graph_store.as_ref() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };

            let communities = store
                .all_communities()
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            if communities.is_empty() {
                return Ok("No communities detected yet. Run graph backfill first.".to_owned());
            }

            let lines: Vec<String> = communities
                .iter()
                .map(|c| format!("  [{}]: {}", c.name, c.summary))
                .collect();
            Ok(format!(
                "Communities ({}):\n{}",
                communities.len(),
                lines.join("\n")
            ))
        })
    }

    fn graph_backfill<'a>(
        &'a mut self,
        limit: Option<usize>,
        progress_cb: &'a mut (dyn FnMut(String) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(memory) = self.services.memory.persistence.memory.clone() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };
            let Some(store) = memory.graph_store.clone() else {
                return Ok("Graph memory is not enabled.".to_owned());
            };

            let total = store.unprocessed_message_count().await.unwrap_or(0);
            let cap = limit.unwrap_or(usize::MAX);

            progress_cb(format!(
                "Starting graph backfill... ({total} unprocessed messages)"
            ));

            let batch_size = 50usize;
            let mut processed = 0usize;
            let mut total_entities = 0usize;
            let mut total_edges = 0usize;

            let graph_cfg = self.services.memory.extraction.graph_config.clone();
            let provider = self.provider.clone();

            loop {
                let remaining_cap = cap.saturating_sub(processed);
                if remaining_cap == 0 {
                    break;
                }
                let batch_limit = batch_size.min(remaining_cap);
                let messages = store
                    .unprocessed_messages_for_backfill(batch_limit)
                    .await
                    .map_err(|e| CommandError::new(e.to_string()))?;
                if messages.is_empty() {
                    break;
                }

                let ids: Vec<zeph_memory::types::MessageId> =
                    messages.iter().map(|(id, _)| *id).collect();

                for (_id, content) in &messages {
                    if content.trim().is_empty() {
                        continue;
                    }
                    let extraction_cfg = GraphExtractionConfig {
                        max_entities: graph_cfg.max_entities_per_message,
                        max_edges: graph_cfg.max_edges_per_message,
                        extraction_timeout_secs: graph_cfg.extraction_timeout_secs,
                        community_refresh_interval: 0,
                        expired_edge_retention_days: graph_cfg.expired_edge_retention_days,
                        max_entities_cap: graph_cfg.max_entities,
                        community_summary_max_prompt_bytes: graph_cfg
                            .community_summary_max_prompt_bytes,
                        community_summary_concurrency: graph_cfg.community_summary_concurrency,
                        lpa_edge_chunk_size: graph_cfg.lpa_edge_chunk_size,
                        note_linking: zeph_memory::NoteLinkingConfig::default(),
                        link_weight_decay_lambda: graph_cfg.link_weight_decay_lambda,
                        link_weight_decay_interval_secs: graph_cfg.link_weight_decay_interval_secs,
                        belief_revision_enabled: graph_cfg.belief_revision.enabled,
                        belief_revision_similarity_threshold: graph_cfg
                            .belief_revision
                            .similarity_threshold,
                        conversation_id: None,
                        apex_mem_enabled: graph_cfg.apex_mem.enabled,
                    };
                    let pool = store.pool().clone();
                    match extract_and_store(
                        content.clone(),
                        vec![],
                        provider.clone(),
                        pool,
                        extraction_cfg,
                        None,
                        None,
                    )
                    .await
                    {
                        Ok(result) => {
                            total_entities += result.stats.entities_upserted;
                            total_edges += result.stats.edges_inserted;
                        }
                        Err(e) => {
                            tracing::warn!("backfill extraction error: {e:#}");
                        }
                    }
                }

                store
                    .mark_messages_graph_processed(&ids)
                    .await
                    .map_err(|e| CommandError::new(e.to_string()))?;
                processed += messages.len();

                progress_cb(format!(
                    "Backfill progress: {processed} messages processed, \
                     {total_entities} entities, {total_edges} edges"
                ));
            }

            Ok(format!(
                "Backfill complete: {total_entities} entities, {total_edges} edges \
                 extracted from {processed} messages"
            ))
        })
    }

    // ----- /guidelines -----

    fn guidelines<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
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
        })
    }

    // ----- /model, /provider -----

    fn handle_model<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            let input = if arg.is_empty() {
                "/model".to_owned()
            } else {
                format!("/model {arg}")
            };
            self.handle_model_command_as_string(&input).await
        })
    }

    fn handle_provider<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move { self.handle_provider_command_as_string(arg) })
    }

    // ----- /policy -----

    fn handle_policy<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.handle_policy_command_as_string(args)) })
    }

    // ----- /scheduler -----

    #[cfg(feature = "scheduler")]
    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = self
                .handle_scheduler_list_as_string()
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            Ok(Some(result))
        })
    }

    #[cfg(not(feature = "scheduler"))]
    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(None) })
    }

    // ----- /lsp -----

    fn lsp_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            self.handle_lsp_status_as_string()
                .await
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    // ----- /recap -----

    fn session_recap<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            match self.build_recap().await {
                Ok(text) => Ok(text),
                Err(e) => {
                    // /recap is an explicit user command — surface a fixed message so that
                    // LlmError internals (URLs with embedded credentials, response excerpts)
                    // are never forwarded to the user channel. Full detail goes to the log.
                    tracing::warn!("session recap command: {}", e.0);
                    Ok("Recap unavailable — see logs for details".to_string())
                }
            }
        })
    }

    // ----- /compact -----

    fn compact_context<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(self.compact_context_command())
    }

    // ----- /new -----

    fn reset_conversation<'a>(
        &'a mut self,
        keep_plan: bool,
        no_digest: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            match self.reset_conversation(keep_plan, no_digest).await {
                Ok((old_id, new_id)) => {
                    let old = old_id.map_or_else(|| "none".to_string(), |id| id.0.to_string());
                    let new = new_id.map_or_else(|| "none".to_string(), |id| id.0.to_string());
                    let keep_note = if keep_plan { " (plan preserved)" } else { "" };
                    Ok(format!(
                        "New conversation started. Previous: {old} → Current: {new}{keep_note}"
                    ))
                }
                Err(e) => Ok(format!("Failed to start new conversation: {e}")),
            }
        })
    }

    // ----- /cache-stats -----

    fn cache_stats(&self) -> String {
        self.tool_orchestrator.cache_stats()
    }

    // ----- /status -----

    fn session_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.handle_status_as_string()) })
    }

    // ----- /guardrail -----

    fn guardrail_status(&self) -> String {
        self.format_guardrail_status()
    }

    // ----- /focus -----

    fn focus_status(&self) -> String {
        self.format_focus_status()
    }

    // ----- /sidequest -----

    fn sidequest_status(&self) -> String {
        self.format_sidequest_status()
    }

    // ----- /image -----

    fn load_image<'a>(
        &'a mut self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.handle_image_as_string(path)) })
    }

    // ----- /mcp -----

    fn handle_mcp<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        // Extract all owned data before the async block so no &mut self reference is
        // held across an .await point, satisfying the `for<'a>` Send bound.
        let args_owned = args.to_owned();
        let parts: Vec<String> = args_owned.split_whitespace().map(str::to_owned).collect();
        let sub = parts.first().cloned().unwrap_or_default();

        match sub.as_str() {
            "list" => {
                // Read-only: clone all data before async.
                let manager = self.services.mcp.manager.clone();
                let tools_snapshot: Vec<(String, String)> = self
                    .services
                    .mcp
                    .tools
                    .iter()
                    .map(|t| (t.server_id.clone(), t.name.clone()))
                    .collect();
                Box::pin(async move {
                    use std::fmt::Write;
                    let Some(manager) = manager else {
                        return Ok("MCP is not enabled.".to_owned());
                    };
                    let server_ids = manager.list_servers().await;
                    if server_ids.is_empty() {
                        return Ok("No MCP servers connected.".to_owned());
                    }
                    let mut output = String::from("Connected MCP servers:\n");
                    let mut total = 0usize;
                    for id in &server_ids {
                        let count = tools_snapshot.iter().filter(|(sid, _)| sid == id).count();
                        total += count;
                        let _ = writeln!(output, "- {id} ({count} tools)");
                    }
                    let _ = write!(output, "Total: {total} tool(s)");
                    Ok(output)
                })
            }
            "tools" => {
                // Read-only: collect tool info before async.
                let server_id = parts.get(1).cloned();
                let owned_tools: Vec<(String, String)> = if let Some(ref sid) = server_id {
                    self.services
                        .mcp
                        .tools
                        .iter()
                        .filter(|t| &t.server_id == sid)
                        .map(|t| (t.name.clone(), t.description.clone()))
                        .collect()
                } else {
                    Vec::new()
                };
                Box::pin(async move {
                    use std::fmt::Write;
                    let Some(server_id) = server_id else {
                        return Ok("Usage: /mcp tools <server_id>".to_owned());
                    };
                    if owned_tools.is_empty() {
                        return Ok(format!("No tools found for server '{server_id}'."));
                    }
                    let mut output =
                        format!("Tools for '{server_id}' ({} total):\n", owned_tools.len());
                    for (name, desc) in &owned_tools {
                        if desc.is_empty() {
                            let _ = writeln!(output, "- {name}");
                        } else {
                            let _ = writeln!(output, "- {name} — {desc}");
                        }
                    }
                    Ok(output)
                })
            }
            // add/remove require mutating self after async I/O.
            // handle_mcp_command is structured so the only .await crossing a &mut self
            // boundary goes through a cloned Arc<McpManager> — no &self fields are held
            // across that .await.  The subsequent state-change methods (rebuild_semantic_index,
            // sync_mcp_registry) are also async fn(&mut self), but they only hold owned locals
            // across their own .await points (cloned tools Vec, cloned Arcs).
            _ => Box::pin(async move {
                self.handle_mcp_command(&args_owned)
                    .await
                    .map_err(|e| CommandError::new(e.to_string()))
            }),
        }
    }

    // ----- /skill -----

    fn handle_skill<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let args_owned = args.to_owned();
        Box::pin(async move {
            self.handle_skill_command_as_string(&args_owned)
                .await
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    // ----- /skills -----

    fn handle_skills<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let args_owned = args.to_owned();
        Box::pin(async move {
            self.handle_skills_as_string(&args_owned)
                .await
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    // ----- /feedback -----

    fn handle_feedback_command<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let args_owned = args.to_owned();
        Box::pin(async move {
            self.handle_feedback_as_string(&args_owned)
                .await
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    // ----- /plan -----

    #[cfg(feature = "scheduler")]
    fn handle_plan<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            self.dispatch_plan_command_as_string(input)
                .await
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    #[cfg(not(feature = "scheduler"))]
    fn handle_plan<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(String::new()) })
    }

    // ----- /experiment -----

    fn handle_experiment<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            self.handle_experiment_command_as_string(input)
                .await
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    // ----- /agent, @mention -----

    fn handle_agent_dispatch<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            match self.dispatch_agent_command(input).await {
                Some(Err(e)) => Err(CommandError::new(e.to_string())),
                Some(Ok(())) | None => Ok(None),
            }
        })
    }

    // ----- /plugins -----

    fn handle_plugins<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let args_owned = args.to_owned();
        // Clone the fields needed by PluginManager before entering the async block.
        // spawn_blocking requires 'static, so we cannot borrow &self inside the closure.
        let managed_dir = self.services.skill.managed_dir.clone();
        let mcp_allowed = self.services.mcp.allowed_commands.clone();
        let base_shell_allowed = self.runtime.lifecycle.startup_shell_overlay.allowed.clone();
        Box::pin(async move {
            // PluginManager performs synchronous filesystem I/O (copy, remove_dir_all,
            // read_dir). Run on a blocking thread to avoid stalling the tokio worker.
            tokio::task::spawn_blocking(move || {
                Self::run_plugin_command(&args_owned, managed_dir, mcp_allowed, base_shell_allowed)
            })
            .await
            .map_err(|e| CommandError(format!("plugin task panicked: {e}")))
        })
    }

    // ----- /acp -----

    fn handle_acp<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            self.handle_acp_as_string(args)
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    // ----- /loop -----

    fn handle_loop<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        use zeph_commands::handlers::loop_cmd::parse_loop_args;

        let args_owned = args.trim().to_owned();
        Box::pin(async move {
            if args_owned == "stop" {
                return Ok(self.stop_user_loop());
            }
            if args_owned == "status" {
                return Ok(match &self.runtime.lifecycle.user_loop {
                    Some(ls) => format!(
                        "Loop active: \"{}\" (iteration {}, interval every {}s).",
                        ls.prompt,
                        ls.iteration,
                        ls.interval.period().as_secs(),
                    ),
                    None => "No active loop.".to_owned(),
                });
            }
            let (prompt, interval_secs) = parse_loop_args(&args_owned)?;

            if prompt.starts_with('/') {
                return Err(CommandError::new(
                    "Loop prompt must not start with '/'. Slash commands cannot be used as loop prompts.",
                ));
            }

            let min_secs = self.runtime.config.loop_min_interval_secs;
            if interval_secs < min_secs {
                return Err(CommandError::new(format!(
                    "Minimum loop interval is {min_secs}s. Got {interval_secs}s."
                )));
            }
            if self.runtime.lifecycle.user_loop.is_some() {
                return Err(CommandError::new(
                    "A loop is already active. Use /loop stop first.",
                ));
            }

            self.start_user_loop(prompt.clone(), interval_secs);
            Ok(format!(
                "Loop started: \"{prompt}\" every {interval_secs}s. Use /loop stop to cancel."
            ))
        })
    }

    fn notify_test<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let notifier = self.runtime.lifecycle.notifier.clone();
        Box::pin(async move {
            let Some(notifier) = notifier else {
                return Ok(
                    "Notifications are disabled. Set `notifications.enabled = true` in config."
                        .to_owned(),
                );
            };
            match notifier.fire_test().await {
                Ok(()) => Ok("Test notification sent.".to_owned()),
                Err(e) => Err(CommandError::new(format!("notification test failed: {e}"))),
            }
        })
    }

    fn handle_trajectory(&mut self, args: &str) -> String {
        self.handle_trajectory_command_as_string(args)
    }

    fn handle_scope(&self, args: &str) -> String {
        self.handle_scope_command_as_string(args)
    }

    // ----- /goal -----

    fn handle_goal<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        // Extract all non-Send data synchronously before entering the async block.
        if self.services.goal_accounting.is_none() {
            if !self.runtime.config.goals.enabled {
                return Box::pin(async {
                    Ok("Goals are disabled. Set `[goals] enabled = true` in config.".to_owned())
                });
            }
            let pool = match self.services.memory.persistence.memory.as_ref() {
                Some(m) => std::sync::Arc::new(m.sqlite().pool().clone()),
                None => {
                    return Box::pin(async {
                        Ok("Goals require a database backend (memory not configured).".to_owned())
                    });
                }
            };
            let store = std::sync::Arc::new(crate::goal::GoalStore::new(pool));
            let accounting = std::sync::Arc::new(crate::goal::GoalAccounting::new(store));
            self.services.goal_accounting = Some(accounting);
        }

        let accounting =
            self.services.goal_accounting.clone().expect(
                "invariant: goal_accounting is always Some at this point (initialized above)",
            );
        let max_chars = self.runtime.config.goals.max_text_chars;
        let default_budget = self.runtime.config.goals.default_token_budget;

        Box::pin(async move {
            let _ = accounting.refresh().await;
            let store = accounting.get_store();

            match args {
                "" | "status" => goal_status(&accounting).await,
                "pause" => goal_pause(&accounting, &store).await,
                "resume" => goal_resume(&accounting, &store).await,
                "complete" => goal_complete(&accounting, &store).await,
                "clear" => goal_clear(&accounting, &store).await,
                "list" => goal_list(&store).await,
                _ if args.starts_with("create") => {
                    goal_create(args, &accounting, &store, max_chars, default_budget).await
                }
                _ => Ok(
                    "Unknown /goal subcommand. Try: create, pause, resume, complete, clear, status, list."
                        .to_owned(),
                ),
            }
        })
    }

    fn active_goal_snapshot(&self) -> Option<zeph_commands::GoalSnapshot> {
        let accounting = self.services.goal_accounting.as_ref()?;
        let snap = accounting.snapshot()?;
        Some(zeph_commands::GoalSnapshot {
            id: snap.id,
            text: snap.text,
            status: match snap.status {
                crate::goal::GoalStatus::Active => zeph_commands::GoalStatusView::Active,
                crate::goal::GoalStatus::Paused => zeph_commands::GoalStatusView::Paused,
                crate::goal::GoalStatus::Completed => zeph_commands::GoalStatusView::Completed,
                crate::goal::GoalStatus::Cleared => zeph_commands::GoalStatusView::Cleared,
            },
            turns_used: snap.turns_used,
            tokens_used: snap.tokens_used,
            token_budget: snap.token_budget,
        })
    }
}

type GoalStore = crate::goal::GoalStore;
type GoalAccounting = crate::goal::GoalAccounting;

async fn goal_status(accounting: &GoalAccounting) -> Result<String, CommandError> {
    match accounting.get_active().await {
        Ok(Some(g)) => {
            let budget_line = g.token_budget.map_or_else(
                || format!("  tokens used: {}", g.tokens_used),
                |b| format!("  budget: {}/{b}", g.tokens_used),
            );
            Ok(format!(
                "Active goal [{}]: {}\n  status: {}\n  turns: {}\n{}",
                &g.id[..8],
                g.text,
                g.status,
                g.turns_used,
                budget_line
            ))
        }
        Ok(None) => Ok("No active goal. Use `/goal create <text>` to set one.".to_owned()),
        Err(e) => Ok(format!("Goal lookup failed: {e}")),
    }
}

async fn goal_create(
    args: &str,
    accounting: &GoalAccounting,
    store: &GoalStore,
    max_chars: usize,
    default_budget: u64,
) -> Result<String, CommandError> {
    let rest = args.strip_prefix("create").unwrap_or("").trim();
    let (text, explicit_budget) = parse_goal_create_args(rest);
    if text.is_empty() {
        return Ok("Usage: /goal create <text> [--budget N]".to_owned());
    }
    let budget = explicit_budget.or(if default_budget > 0 {
        Some(default_budget)
    } else {
        None
    });
    match store.create(text, budget, max_chars).await {
        Ok(g) => {
            let _ = accounting.refresh().await;
            Ok(format!("Goal created [{}]: {}", &g.id[..8], g.text))
        }
        Err(crate::goal::store::GoalError::TextTooLong { max }) => Ok(format!(
            "Goal text exceeds {max} characters. Please shorten it."
        )),
        Err(e) => Ok(format!("Failed to create goal: {e}")),
    }
}

async fn goal_pause(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    match accounting.get_active().await {
        Ok(Some(g)) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Paused, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] paused.", &g.id[..8]))
                }
                Err(crate::goal::store::GoalError::StaleUpdate(_)) => {
                    let current = accounting.get_active().await.ok().flatten();
                    Ok(format!(
                        "Goal state changed concurrently. Current: {}",
                        current.map_or_else(|| "none".into(), |g| g.status.to_string())
                    ))
                }
                Err(e) => Ok(format!("Pause failed: {e}")),
            }
        }
        Ok(None) => Ok("No active goal to pause.".to_owned()),
        Err(e) => Ok(format!("Failed: {e}")),
    }
}

async fn goal_resume(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    let goals = store.list(10).await.unwrap_or_default();
    let paused = goals
        .into_iter()
        .find(|g| g.status == crate::goal::GoalStatus::Paused);
    match paused {
        Some(g) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Active, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] resumed: {}", &g.id[..8], g.text))
                }
                Err(crate::goal::store::GoalError::StaleUpdate(_)) => {
                    Ok("Goal state changed concurrently — please retry.".to_owned())
                }
                Err(e) => Ok(format!("Resume failed: {e}")),
            }
        }
        None => Ok("No paused goal to resume.".to_owned()),
    }
}

async fn goal_complete(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    match accounting.get_active().await {
        Ok(Some(g)) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Completed, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] marked complete.", &g.id[..8]))
                }
                Err(e) => Ok(format!("Complete failed: {e}")),
            }
        }
        Ok(None) => Ok("No active goal.".to_owned()),
        Err(e) => Ok(format!("Failed: {e}")),
    }
}

async fn goal_clear(
    accounting: &GoalAccounting,
    store: &GoalStore,
) -> Result<String, CommandError> {
    let goals = store.list(10).await.unwrap_or_default();
    let target = goals.into_iter().find(|g| {
        g.status == crate::goal::GoalStatus::Active || g.status == crate::goal::GoalStatus::Paused
    });
    match target {
        Some(g) => {
            match store
                .transition(&g.id, crate::goal::GoalStatus::Cleared, g.updated_at)
                .await
            {
                Ok(_) => {
                    let _ = accounting.refresh().await;
                    Ok(format!("Goal [{}] cleared.", &g.id[..8]))
                }
                Err(e) => Ok(format!("Clear failed: {e}")),
            }
        }
        None => Ok("No active or paused goal to clear.".to_owned()),
    }
}

async fn goal_list(store: &GoalStore) -> Result<String, CommandError> {
    let goals = store.list(20).await.unwrap_or_default();
    if goals.is_empty() {
        return Ok("No goals recorded.".to_owned());
    }
    let mut out = String::from("Goals:\n");
    for g in goals {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "  {} [{}] {} — {} turns\n",
                g.status.badge_symbol(),
                &g.id[..8],
                g.text,
                g.turns_used
            ),
        );
    }
    Ok(out.trim_end().to_owned())
}

fn parse_goal_create_args(args: &str) -> (&str, Option<u64>) {
    if let Some(pos) = args.find("--budget") {
        let text = args[..pos].trim();
        let rest = args[pos + "--budget".len()..].trim();
        let budget = rest
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok());
        (text, budget)
    } else {
        (args, None)
    }
}

/// Convert `AgentError` to `CommandError` for the trait boundary.
impl From<AgentError> for CommandError {
    fn from(e: AgentError) -> Self {
        Self(e.to_string())
    }
}
