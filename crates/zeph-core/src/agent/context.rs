use super::{
    Agent, CODE_CONTEXT_PREFIX, CROSS_SESSION_PREFIX, Channel, ContextBudget, EnvironmentContext,
    LlmProvider, Message, MessagePart, RECALL_PREFIX, Role, SUMMARY_PREFIX, Skill, ToolExecutor,
    build_system_prompt, estimate_tokens, format_skills_catalog, format_skills_prompt,
};

impl<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(super) fn should_compact(&self) -> bool {
        let Some(ref budget) = self.context_budget else {
            return false;
        };
        let total_tokens: usize = self
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
        let threshold = (budget.max_tokens() as f32 * self.compaction_threshold) as usize;
        let should = total_tokens > threshold;
        tracing::debug!(
            total_tokens,
            threshold,
            message_count = self.messages.len(),
            should_compact = should,
            "context budget check"
        );
        should
    }

    pub(super) async fn compact_context(&mut self) -> anyhow::Result<()> {
        let preserve_tail = self.compaction_preserve_tail;

        if self.messages.len() <= preserve_tail + 1 {
            return Ok(());
        }

        let compact_end = self.messages.len() - preserve_tail;
        let to_compact = &self.messages[1..compact_end];
        if to_compact.is_empty() {
            return Ok(());
        }

        let history_text: String = to_compact
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                };
                format!("[{role}]: {}", m.content)
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let compaction_prompt = format!(
            "Summarize this conversation excerpt into a structured continuation note. \
             Include:\n\
             1. Task overview\n\
             2. Current state\n\
             3. Key discoveries (file paths, errors, decisions)\n\
             4. Next steps\n\
             5. Critical context (variable names, config values)\n\
             \n\
             Keep it concise but preserve all actionable details.\n\
             \n\
             Conversation:\n{history_text}"
        );

        let summary = self
            .provider
            .chat(&[Message {
                role: Role::User,
                content: compaction_prompt,
                parts: vec![],
            }])
            .await?;

        let compacted_count = to_compact.len();
        self.messages.drain(1..compact_end);
        self.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!(
                    "[conversation summary — {compacted_count} messages compacted]\n{summary}"
                ),
                parts: vec![],
            },
        );

        tracing::info!(
            compacted_count,
            summary_tokens = estimate_tokens(&summary),
            "compacted context"
        );

        self.update_metrics(|m| {
            m.context_compactions += 1;
        });

        if let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id)
            && let Err(e) = memory.store_session_summary(cid, &summary).await
        {
            tracing::warn!("failed to store session summary: {e:#}");
        }

        Ok(())
    }

    /// Prune tool output bodies outside the protection zone, oldest first.
    /// Returns the number of tokens freed.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn prune_tool_outputs(&mut self, min_to_free: usize) -> usize {
        let protect = self.prune_protect_tokens;
        let mut tail_tokens = 0usize;
        let mut protection_boundary = self.messages.len();
        if protect > 0 {
            for (i, msg) in self.messages.iter().enumerate().rev() {
                tail_tokens += estimate_tokens(&msg.content);
                if tail_tokens >= protect {
                    protection_boundary = i;
                    break;
                }
                if i == 0 {
                    protection_boundary = 0;
                }
            }
        }

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        for msg in &mut self.messages[..protection_boundary] {
            if freed >= min_to_free {
                break;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let &mut MessagePart::ToolOutput {
                    ref mut body,
                    ref mut compacted_at,
                    ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += estimate_tokens(body);
                    *compacted_at = Some(now);
                    *body = String::new();
                    modified = true;
                }
            }
            if modified {
                msg.rebuild_content();
            }
        }

        if freed > 0 {
            self.update_metrics(|m| m.tool_output_prunes += 1);
            tracing::info!(freed, protection_boundary, "pruned tool outputs");
        }
        freed
    }

    /// Two-tier compaction: Tier 1 prunes tool outputs, Tier 2 falls back to full LLM compaction.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(super) async fn maybe_compact(&mut self) -> anyhow::Result<()> {
        if !self.should_compact() {
            return Ok(());
        }

        let budget = self
            .context_budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let total_tokens: usize = self
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
        let threshold = (budget as f32 * self.compaction_threshold) as usize;
        let min_to_free = total_tokens.saturating_sub(threshold);

        let freed = self.prune_tool_outputs(min_to_free);
        if freed >= min_to_free {
            tracing::info!(freed, "tier-1 pruning sufficient");
            return Ok(());
        }

        tracing::info!(
            freed,
            min_to_free,
            "tier-1 insufficient, falling back to tier-2 compaction"
        );
        let _ = self.channel.send_status("compacting context...").await;
        let result = self.compact_context().await;
        let _ = self.channel.send_status("").await;
        result
    }

    pub(super) fn remove_recall_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::Recall { .. }))
            {
                return false;
            }
            !m.content.starts_with(RECALL_PREFIX)
        });
    }

    pub(super) async fn inject_semantic_recall(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> anyhow::Result<()> {
        self.remove_recall_messages();

        let Some(memory) = &self.memory else {
            return Ok(());
        };
        if self.recall_limit == 0 || token_budget == 0 {
            return Ok(());
        }

        let recalled = memory.recall(query, self.recall_limit, None).await?;
        if recalled.is_empty() {
            return Ok(());
        }

        let mut recall_text = String::from(RECALL_PREFIX);
        let mut tokens_used = estimate_tokens(&recall_text);

        for item in &recalled {
            let role_label = match item.message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            let entry = format!("- [{}] {}\n", role_label, item.message.content);
            let entry_tokens = estimate_tokens(&entry);
            if tokens_used + entry_tokens > token_budget {
                break;
            }
            recall_text.push_str(&entry);
            tokens_used += entry_tokens;
        }

        if tokens_used > estimate_tokens(RECALL_PREFIX) && self.messages.len() > 1 {
            self.messages.insert(
                1,
                Message::from_parts(
                    Role::System,
                    vec![MessagePart::Recall { text: recall_text }],
                ),
            );
        }

        Ok(())
    }

    pub(super) fn remove_code_context_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::CodeContext { .. }))
            {
                return false;
            }
            !m.content.starts_with(CODE_CONTEXT_PREFIX)
        });
    }

    fn remove_summary_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::Summary { .. }))
            {
                return false;
            }
            !m.content.starts_with(SUMMARY_PREFIX)
        });
    }

    fn remove_cross_session_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::CrossSession { .. }))
            {
                return false;
            }
            !m.content.starts_with(CROSS_SESSION_PREFIX)
        });
    }

    async fn inject_cross_session_context(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> anyhow::Result<()> {
        self.remove_cross_session_messages();

        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return Ok(());
        };
        if token_budget == 0 {
            return Ok(());
        }

        let threshold = self.cross_session_score_threshold;
        let results: Vec<_> = memory
            .search_session_summaries(query, 5, Some(cid))
            .await?
            .into_iter()
            .filter(|r| r.score >= threshold)
            .collect();
        if results.is_empty() {
            return Ok(());
        }

        let mut text = String::from(CROSS_SESSION_PREFIX);
        let mut tokens_used = estimate_tokens(&text);

        for item in &results {
            let entry = format!("- {}\n", item.summary_text);
            let cost = estimate_tokens(&entry);
            if tokens_used + cost > token_budget {
                break;
            }
            text.push_str(&entry);
            tokens_used += cost;
        }

        if tokens_used > estimate_tokens(CROSS_SESSION_PREFIX) && self.messages.len() > 1 {
            self.messages.insert(
                1,
                Message::from_parts(Role::System, vec![MessagePart::CrossSession { text }]),
            );
            tracing::debug!(tokens_used, "injected cross-session context");
        }

        Ok(())
    }

    async fn inject_summaries(&mut self, token_budget: usize) -> anyhow::Result<()> {
        self.remove_summary_messages();

        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return Ok(());
        };
        if token_budget == 0 {
            return Ok(());
        }

        let summaries = memory.load_summaries(cid).await?;
        if summaries.is_empty() {
            return Ok(());
        }

        let mut summary_text = String::from(SUMMARY_PREFIX);
        let mut tokens_used = estimate_tokens(&summary_text);

        for summary in summaries.iter().rev() {
            let entry = format!(
                "- Messages {}-{}: {}\n",
                summary.first_message_id, summary.last_message_id, summary.content
            );
            let cost = estimate_tokens(&entry);
            if tokens_used + cost > token_budget {
                break;
            }
            summary_text.push_str(&entry);
            tokens_used += cost;
        }

        if tokens_used > estimate_tokens(SUMMARY_PREFIX) && self.messages.len() > 1 {
            self.messages.insert(
                1,
                Message::from_parts(
                    Role::System,
                    vec![MessagePart::Summary { text: summary_text }],
                ),
            );
            tracing::debug!(tokens_used, "injected summaries into context");
        }

        Ok(())
    }

    fn trim_messages_to_budget(&mut self, token_budget: usize) {
        if token_budget == 0 {
            return;
        }

        let history_start = self
            .messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(self.messages.len());

        if history_start >= self.messages.len() {
            return;
        }

        let mut total = 0usize;
        let mut keep_from = self.messages.len();

        for i in (history_start..self.messages.len()).rev() {
            let msg_tokens = estimate_tokens(&self.messages[i].content);
            if total + msg_tokens > token_budget {
                break;
            }
            total += msg_tokens;
            keep_from = i;
        }

        if keep_from > history_start {
            let removed = keep_from - history_start;
            self.messages.drain(history_start..keep_from);
            tracing::info!(
                removed,
                token_budget,
                "trimmed messages to fit context budget"
            );
        }
    }

    pub(super) async fn prepare_context(&mut self, query: &str) -> anyhow::Result<()> {
        let Some(ref budget) = self.context_budget else {
            return Ok(());
        };

        let system_prompt = self.messages.first().map_or("", |m| m.content.as_str());
        let alloc = budget.allocate(system_prompt, &self.last_skills_prompt);

        self.inject_summaries(alloc.summaries).await?;

        self.inject_cross_session_context(query, alloc.cross_session)
            .await?;

        self.inject_semantic_recall(query, alloc.semantic_recall)
            .await?;

        #[cfg(feature = "index")]
        self.inject_code_rag(query, alloc.code_context).await?;

        self.trim_messages_to_budget(alloc.recent_history);

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn rebuild_system_prompt(&mut self, query: &str) {
        let all_meta = self.registry.all_meta();
        let matched_indices: Vec<usize> = if let Some(matcher) = &self.matcher {
            let provider = self.provider.clone();
            matcher
                .match_skills(&all_meta, query, self.max_active_skills, |text| {
                    let owned = text.to_owned();
                    let p = provider.clone();
                    Box::pin(async move { p.embed(&owned).await })
                })
                .await
        } else {
            (0..all_meta.len()).collect()
        };

        self.active_skill_names = matched_indices
            .iter()
            .filter_map(|&i| all_meta.get(i).map(|m| m.name.clone()))
            .collect();

        let skill_names = self.active_skill_names.clone();
        let total = all_meta.len();
        self.update_metrics(|m| {
            m.active_skills = skill_names;
            m.total_skills = total;
        });

        if !self.active_skill_names.is_empty()
            && let Some(memory) = &self.memory
        {
            let names: Vec<&str> = self.active_skill_names.iter().map(String::as_str).collect();
            if let Err(e) = memory.sqlite().record_skill_usage(&names).await {
                tracing::warn!("failed to record skill usage: {e:#}");
            }
        }

        let all_skills: Vec<Skill> = self
            .registry
            .all_meta()
            .iter()
            .filter_map(|m| self.registry.get_skill(&m.name).ok())
            .collect();
        let active_skills: Vec<Skill> = self
            .active_skill_names
            .iter()
            .filter_map(|name| self.registry.get_skill(name).ok())
            .collect();
        let remaining_skills: Vec<Skill> = all_skills
            .iter()
            .filter(|s| !self.active_skill_names.contains(&s.name().to_string()))
            .cloned()
            .collect();

        let skills_prompt = format_skills_prompt(&active_skills, std::env::consts::OS);
        let catalog_prompt = format_skills_catalog(&remaining_skills);
        self.last_skills_prompt.clone_from(&skills_prompt);
        let env = EnvironmentContext::gather(&self.model_name);
        let tool_catalog = {
            let defs = self.tool_executor.tool_definitions();
            if defs.is_empty() {
                None
            } else {
                let reg = zeph_tools::ToolRegistry::from_definitions(defs);
                Some(reg.format_for_prompt_filtered(&self.permission_policy))
            }
        };
        #[allow(unused_mut)]
        let mut system_prompt =
            build_system_prompt(&skills_prompt, Some(&env), tool_catalog.as_deref());

        if !catalog_prompt.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&catalog_prompt);
        }

        #[cfg(feature = "mcp")]
        self.append_mcp_prompt(query, &mut system_prompt).await;

        let cwd = std::env::current_dir().unwrap_or_default();
        let project_configs = crate::project::discover_project_configs(&cwd);
        let project_context = crate::project::load_project_context(&project_configs);
        if !project_context.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&project_context);
        }

        #[cfg(feature = "index")]
        if self.code_retriever.is_some() && self.repo_map_tokens > 0 {
            let now = std::time::Instant::now();
            let map = if let Some((ref cached, generated_at)) = self.cached_repo_map
                && now.duration_since(generated_at) < self.repo_map_ttl
            {
                cached.clone()
            } else {
                let fresh = zeph_index::repo_map::generate_repo_map(&cwd, self.repo_map_tokens)
                    .unwrap_or_default();
                self.cached_repo_map = Some((fresh.clone(), now));
                fresh
            };
            if !map.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&map);
            }
        }

        tracing::debug!(
            len = system_prompt.len(),
            skills = ?self.active_skill_names,
            "system prompt rebuilt"
        );
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;

    #[test]
    fn should_compact_disabled_without_budget() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        for i in 0..20 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i} with some content to add tokens"),
                parts: vec![],
            });
        }
        assert!(!agent.should_compact());
    }

    #[test]
    fn should_compact_below_threshold() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0);
        assert!(!agent.should_compact());
    }

    #[test]
    fn should_compact_above_threshold() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 4, 0);

        for i in 0..20 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message number {i} with enough content to push over budget"),
                parts: vec![],
            });
        }
        assert!(agent.should_compact());
    }

    #[tokio::test]
    async fn compact_context_preserves_system_and_tail() {
        let provider = MockProvider::new(vec!["compacted summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0);

        let system_content = agent.messages[0].content.clone();

        for i in 0..8 {
            agent.messages.push(Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: format!("message {i}"),
                parts: vec![],
            });
        }

        agent.compact_context().await.unwrap();

        assert_eq!(agent.messages[0].role, Role::System);
        assert_eq!(agent.messages[0].content, system_content);

        assert_eq!(agent.messages[1].role, Role::System);
        assert!(agent.messages[1].content.contains("[conversation summary"));

        let tail = &agent.messages[2..];
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].content, "message 6");
        assert_eq!(tail[1].content, "message 7");
    }

    #[tokio::test]
    async fn compact_context_too_few_messages() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 4, 0);

        agent.messages.push(Message {
            role: Role::User,
            content: "msg1".to_string(),
            parts: vec![],
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "msg2".to_string(),
            parts: vec![],
        });

        let len_before = agent.messages.len();
        agent.compact_context().await.unwrap();
        assert_eq!(agent.messages.len(), len_before);
    }

    #[test]
    fn with_context_budget_zero_disables() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(0, 0.20, 0.75, 4, 0);
        assert!(agent.context_budget.is_none());
    }

    #[test]
    fn with_context_budget_nonzero_enables() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(4096, 0.20, 0.80, 6, 0);

        assert!(agent.context_budget.is_some());
        assert_eq!(agent.context_budget.as_ref().unwrap().max_tokens(), 4096);
        assert!((agent.compaction_threshold - 0.80).abs() < f32::EPSILON);
        assert_eq!(agent.compaction_preserve_tail, 6);
    }

    #[tokio::test]
    async fn compact_context_increments_metric() {
        let provider = MockProvider::new(vec!["summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0)
            .with_metrics(tx);

        for i in 0..8 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }

        agent.compact_context().await.unwrap();
        assert_eq!(rx.borrow().context_compactions, 1);
    }

    #[tokio::test]
    async fn test_prepare_context_no_budget_is_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.prepare_context("test query").await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_recall_injection_removed_between_turns() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{RECALL_PREFIX}old recall data"),
                parts: vec![],
            },
        );
        assert_eq!(agent.messages.len(), 2);

        agent.remove_recall_messages();
        assert_eq!(agent.messages.len(), 1);
        assert!(!agent.messages[0].content.starts_with(RECALL_PREFIX));
    }

    #[tokio::test]
    async fn test_recall_without_qdrant_returns_empty() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.inject_semantic_recall("test", 1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_trim_messages_preserves_system() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }
        assert_eq!(agent.messages.len(), 11);

        agent.trim_messages_to_budget(5);

        assert_eq!(agent.messages[0].role, Role::System);
        assert!(agent.messages.len() < 11);
    }

    #[tokio::test]
    async fn test_trim_messages_keeps_recent() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("msg {i}"),
                parts: vec![],
            });
        }

        agent.trim_messages_to_budget(5);

        let last = agent.messages.last().unwrap();
        assert_eq!(last.content, "msg 9");
    }

    #[tokio::test]
    async fn test_trim_zero_budget_is_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..5 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }
        let msg_count = agent.messages.len();

        agent.trim_messages_to_budget(0);
        assert_eq!(agent.messages.len(), msg_count);
    }

    async fn create_memory_with_summaries(
        provider: MockProvider,
        summaries: &[&str],
    ) -> (SemanticMemory<MockProvider>, zeph_memory::ConversationId) {
        let memory = SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test")
            .await
            .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        for content in summaries {
            let m1 = memory
                .sqlite()
                .save_message(cid, "user", "q")
                .await
                .unwrap();
            let m2 = memory
                .sqlite()
                .save_message(cid, "assistant", "a")
                .await
                .unwrap();
            memory
                .sqlite()
                .save_summary(cid, content, m1, m2, estimate_tokens(content) as i64)
                .await
                .unwrap();
        }
        (memory, cid)
    }

    #[tokio::test]
    async fn test_inject_summaries_no_memory_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.inject_summaries(1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_zero_budget_noop() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary text"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);
        let msg_count = agent.messages.len();

        agent.inject_summaries(0).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_empty_summaries_noop() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);
        let msg_count = agent.messages.len();

        agent.inject_summaries(1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_inserts_at_position_1() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) =
            create_memory_with_summaries(provider.clone(), &["User asked about Rust ownership"])
                .await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);

        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        });

        agent.inject_summaries(1000).await.unwrap();

        assert_eq!(agent.messages[0].role, Role::System);
        assert!(agent.messages[1].content.starts_with(SUMMARY_PREFIX));
        assert_eq!(agent.messages[1].role, Role::System);
        assert!(
            agent.messages[1]
                .content
                .contains("User asked about Rust ownership")
        );
        assert_eq!(agent.messages[2].content, "hello");
    }

    #[tokio::test]
    async fn test_inject_summaries_removes_old_before_inject() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) =
            create_memory_with_summaries(provider.clone(), &["new summary data"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{SUMMARY_PREFIX}old summary data"),
                parts: vec![],
            },
        );
        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        });
        assert_eq!(agent.messages.len(), 3);

        agent.inject_summaries(1000).await.unwrap();

        let summary_msgs: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.content.starts_with(SUMMARY_PREFIX))
            .collect();
        assert_eq!(summary_msgs.len(), 1);
        assert!(summary_msgs[0].content.contains("new summary data"));
        assert!(!summary_msgs[0].content.contains("old summary data"));
    }

    #[tokio::test]
    async fn test_inject_summaries_respects_token_budget() {
        let provider = MockProvider::new(vec![]);
        // Each summary entry is "- Messages X-Y: <content>\n" (~prefix overhead + content)
        let (memory, cid) = create_memory_with_summaries(
            provider.clone(),
            &[
                "short",
                "this is a much longer summary that should consume more tokens",
            ],
        )
        .await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);

        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        });

        // Use a very small budget: only the prefix + maybe one short entry
        let prefix_cost = estimate_tokens(SUMMARY_PREFIX);
        agent.inject_summaries(prefix_cost + 10).await.unwrap();

        let summary_msg = agent
            .messages
            .iter()
            .find(|m| m.content.starts_with(SUMMARY_PREFIX));

        if let Some(msg) = summary_msg {
            let token_count = estimate_tokens(&msg.content);
            assert!(token_count <= prefix_cost + 10);
        }
    }

    #[tokio::test]
    async fn test_remove_summary_messages_preserves_other_system() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{SUMMARY_PREFIX}old summary"),
                parts: vec![],
            },
        );
        agent.messages.insert(
            2,
            Message {
                role: Role::System,
                content: format!("{RECALL_PREFIX}recall data"),
                parts: vec![],
            },
        );
        assert_eq!(agent.messages.len(), 3);

        agent.remove_summary_messages();
        assert_eq!(agent.messages.len(), 2);
        assert!(agent.messages[1].content.starts_with(RECALL_PREFIX));
    }

    #[test]
    fn test_prune_frees_tokens() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0)
            .with_metrics(tx);

        let big_body = "x".repeat(500);
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: big_body,
                compacted_at: None,
            }],
        ));

        let freed = agent.prune_tool_outputs(10);
        assert!(freed > 0);
        assert_eq!(rx.borrow().tool_output_prunes, 1);

        if let MessagePart::ToolOutput {
            body, compacted_at, ..
        } = &agent.messages[1].parts[0]
        {
            assert!(compacted_at.is_some());
            assert!(body.is_empty(), "body should be cleared after prune");
        } else {
            panic!("expected ToolOutput");
        }
    }

    #[test]
    fn test_prune_respects_protection_zone() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(10000, 0.20, 0.75, 4, 999_999);

        let big_body = "x".repeat(500);
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: big_body,
                compacted_at: None,
            }],
        ));

        let freed = agent.prune_tool_outputs(10);
        assert_eq!(freed, 0);

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.messages[1].parts[0] {
            assert!(compacted_at.is_none());
        } else {
            panic!("expected ToolOutput");
        }
    }

    #[tokio::test]
    async fn test_tier2_after_insufficient_prune() {
        let provider = MockProvider::new(vec!["summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0)
            .with_metrics(tx);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i} with enough content to push over budget threshold"),
                parts: vec![],
            });
        }

        agent.maybe_compact().await.unwrap();
        assert_eq!(rx.borrow().context_compactions, 1);
    }

    #[tokio::test]
    async fn test_inject_cross_session_no_memory_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent
            .inject_cross_session_context("test", 1000)
            .await
            .unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_cross_session_zero_budget_noop() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);
        let msg_count = agent.messages.len();

        agent.inject_cross_session_context("test", 0).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_remove_cross_session_messages() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CrossSession {
                    text: "old cross-session".into(),
                }],
            ),
        );
        assert_eq!(agent.messages.len(), 2);

        agent.remove_cross_session_messages();
        assert_eq!(agent.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_remove_cross_session_preserves_other_system() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::Summary {
                    text: "keep this summary".into(),
                }],
            ),
        );
        agent.messages.insert(
            2,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CrossSession {
                    text: "remove this".into(),
                }],
            ),
        );
        assert_eq!(agent.messages.len(), 3);

        agent.remove_cross_session_messages();
        assert_eq!(agent.messages.len(), 2);
        assert!(agent.messages[1].content.contains("keep this summary"));
    }

    #[tokio::test]
    async fn test_store_session_summary_on_compaction() {
        let provider = MockProvider::new(vec!["compacted summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50)
            .with_context_budget(10000, 0.20, 0.80, 2, 0);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }

        // compact_context should succeed (non-fatal store)
        agent.compact_context().await.unwrap();
        assert!(agent.messages[1].content.contains("compacted summary"));
    }

    #[test]
    fn test_budget_allocation_cross_session() {
        let budget = crate::context::ContextBudget::new(1000, 0.20);
        let alloc = budget.allocate("", "");

        assert!(alloc.cross_session > 0);
        assert!(alloc.summaries > 0);
        assert!(alloc.semantic_recall > 0);
        // cross_session should be smaller than summaries
        assert!(alloc.cross_session < alloc.summaries);
    }

    #[test]
    fn test_cross_session_score_threshold_filters() {
        use zeph_memory::semantic::SessionSummaryResult;

        let threshold: f32 = 0.35;

        let results = vec![
            SessionSummaryResult {
                summary_text: "high score".into(),
                score: 0.9,
                conversation_id: zeph_memory::ConversationId(1),
            },
            SessionSummaryResult {
                summary_text: "at threshold".into(),
                score: 0.35,
                conversation_id: zeph_memory::ConversationId(2),
            },
            SessionSummaryResult {
                summary_text: "below threshold".into(),
                score: 0.2,
                conversation_id: zeph_memory::ConversationId(3),
            },
            SessionSummaryResult {
                summary_text: "way below".into(),
                score: 0.0,
                conversation_id: zeph_memory::ConversationId(4),
            },
        ];

        let filtered: Vec<_> = results
            .into_iter()
            .filter(|r| r.score >= threshold)
            .collect();

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].summary_text, "high score");
        assert_eq!(filtered[1].summary_text, "at threshold");
    }

    #[test]
    fn context_budget_80_percent_threshold() {
        let budget = ContextBudget::new(1000, 0.20);
        let threshold = budget.max_tokens() * 4 / 5;
        assert_eq!(threshold, 800);
        assert!(800 >= threshold); // at threshold → should stop
        assert!(799 < threshold); // below threshold → should continue
    }
}
