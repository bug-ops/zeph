// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;

use zeph_memory::{GraphExtractionConfig, extract_and_store};

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Dispatch `/graph [subcommand]` slash command.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel send fails or graph store query fails.
    pub async fn handle_graph_command(&mut self, input: &str) -> Result<(), AgentError> {
        let args = input.strip_prefix("/graph").unwrap_or("").trim();

        if args.is_empty() {
            return self.handle_graph_stats().await;
        }
        if args == "entities" || args.starts_with("entities ") {
            return self.handle_graph_entities().await;
        }
        if let Some(name) = args.strip_prefix("facts ") {
            return self.handle_graph_facts(name.trim()).await;
        }
        if args == "communities" {
            return self.handle_graph_communities().await;
        }
        if args == "backfill" || args.starts_with("backfill ") {
            let limit = parse_backfill_limit(args);
            return self.handle_graph_backfill(limit).await;
        }

        self.channel
            .send(
                "Unknown /graph subcommand. Available: /graph, /graph entities, \
                 /graph facts <name>, /graph communities, /graph backfill [--limit N]",
            )
            .await?;
        Ok(())
    }

    async fn handle_graph_stats(&mut self) -> Result<(), AgentError> {
        let Some(memory) = self.memory_state.memory.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };
        let Some(store) = memory.graph_store.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };

        let (entities, edges, communities) = tokio::join!(
            store.entity_count(),
            store.active_edge_count(),
            store.community_count()
        );
        let msg = format!(
            "Graph memory: {} entities, {} edges, {} communities",
            entities.unwrap_or(0),
            edges.unwrap_or(0),
            communities.unwrap_or(0)
        );
        self.channel.send(&msg).await?;
        Ok(())
    }

    async fn handle_graph_entities(&mut self) -> Result<(), AgentError> {
        let Some(memory) = self.memory_state.memory.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };
        let Some(store) = memory.graph_store.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };

        self.channel.send("Loading graph entities...").await?;
        let entities = store.all_entities().await?;
        if entities.is_empty() {
            self.channel.send("No entities found.").await?;
            return Ok(());
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
        self.channel.send(&msg).await?;
        Ok(())
    }

    async fn handle_graph_facts(&mut self, name: &str) -> Result<(), AgentError> {
        let Some(memory) = self.memory_state.memory.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };
        let Some(store) = memory.graph_store.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };

        let matches = store.find_entity_by_name(name).await?;
        if matches.is_empty() {
            self.channel
                .send(&format!("No entity found matching '{name}'."))
                .await?;
            return Ok(());
        }

        let entity = &matches[0];
        let edges = store.edges_for_entity(entity.id).await?;
        if edges.is_empty() {
            self.channel
                .send(&format!("Entity '{}' has no known facts.", entity.name))
                .await?;
            return Ok(());
        }

        // Build entity id → name lookup for display
        let mut entity_names: std::collections::HashMap<i64, String> =
            std::collections::HashMap::new();
        entity_names.insert(entity.id, entity.name.clone());
        for edge in &edges {
            let other_id = if edge.source_entity_id == entity.id {
                edge.target_entity_id
            } else {
                edge.source_entity_id
            };
            entity_names.entry(other_id).or_insert_with(|| {
                // We'll fill these lazily; for simplicity use a placeholder here
                // and fetch below.
                String::new()
            });
        }
        // Fetch names for any entries we inserted as empty placeholder
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
                    "  {} --[{}]--> {}: {} (confidence: {:.2})",
                    src, e.relation, tgt, e.fact, e.confidence
                )
            })
            .collect();
        let msg = format!("Facts for '{}':\n{}", entity.name, lines.join("\n"));
        self.channel.send(&msg).await?;
        Ok(())
    }

    async fn handle_graph_communities(&mut self) -> Result<(), AgentError> {
        let Some(memory) = self.memory_state.memory.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };
        let Some(store) = memory.graph_store.as_ref() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };

        self.channel.send("Loading graph communities...").await?;
        let communities = store.all_communities().await?;
        if communities.is_empty() {
            self.channel
                .send("No communities detected yet. Run graph backfill first.")
                .await?;
            return Ok(());
        }

        let lines: Vec<String> = communities
            .iter()
            .map(|c| format!("  [{}]: {}", c.name, c.summary))
            .collect();
        let msg = format!("Communities ({}):\n{}", communities.len(), lines.join("\n"));
        self.channel.send(&msg).await?;
        Ok(())
    }

    async fn handle_graph_backfill(&mut self, limit: Option<usize>) -> Result<(), AgentError> {
        let Some(memory) = self.memory_state.memory.clone() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };
        let Some(store) = memory.graph_store.clone() else {
            self.channel.send("Graph memory is not enabled.").await?;
            return Ok(());
        };

        let total = store.unprocessed_message_count().await.unwrap_or(0);
        let cap = limit.unwrap_or(usize::MAX);

        self.channel
            .send(&format!(
                "Starting graph backfill... ({total} unprocessed messages)"
            ))
            .await?;

        let batch_size = 50usize;
        let mut processed = 0usize;
        let mut total_entities = 0usize;
        let mut total_edges = 0usize;

        let graph_cfg = self.memory_state.graph_config.clone();
        let provider = self.provider.clone();

        loop {
            let remaining_cap = cap.saturating_sub(processed);
            if remaining_cap == 0 {
                break;
            }
            let batch_limit = batch_size.min(remaining_cap);
            let messages = store.unprocessed_messages_for_backfill(batch_limit).await?;
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
                };
                let pool = store.pool().clone();
                match extract_and_store(
                    content.clone(),
                    vec![],
                    provider.clone(),
                    pool,
                    extraction_cfg,
                )
                .await
                {
                    Ok(stats) => {
                        total_entities += stats.entities_upserted;
                        total_edges += stats.edges_inserted;
                    }
                    Err(e) => {
                        tracing::warn!("backfill extraction error: {e:#}");
                    }
                }
            }

            store.mark_messages_graph_processed(&ids).await?;
            processed += messages.len();

            self.channel
                .send(&format!(
                    "Backfill progress: {processed} messages processed, \
                     {total_entities} entities, {total_edges} edges"
                ))
                .await?;
        }

        self.channel
            .send(&format!(
                "Backfill complete: {total_entities} entities, {total_edges} edges \
                 extracted from {processed} messages"
            ))
            .await?;
        Ok(())
    }
}

fn parse_backfill_limit(args: &str) -> Option<usize> {
    let pos = args.find("--limit")?;
    args[pos + "--limit".len()..]
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
    use super::parse_backfill_limit;

    #[test]
    fn handle_graph_backfill_limit_parsing() {
        assert_eq!(parse_backfill_limit("backfill --limit 100"), Some(100));
        assert_eq!(parse_backfill_limit("backfill"), None);
        assert_eq!(parse_backfill_limit("backfill --limit"), None);
        assert_eq!(parse_backfill_limit("backfill --limit 0"), Some(0));
    }
}
