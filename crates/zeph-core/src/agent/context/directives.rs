// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Turn directives and frozen prompt rendering of the `append` layout (spec 088).
//!
//! Everything that used to be rewritten into the system prompt every turn travels as a
//! `Directives` [`TurnContextBlock`] on the turn input. Two kinds of section exist:
//!
//! - content-addressed items (skill bodies, MCP tool prompts) are sent once: their key is the
//!   hash of the content, and a key already present in history is skipped;
//! - state sections (environment, catalog updates, goal, ...) are compared with the **last value
//!   sent for that source**, so a value that changes A to B and back to A is announced again.
//!
//! A trust revocation (skill demoted, blocked or removed, instruction text removed) is a
//! boundary: the stale directives are dropped from history and the frozen prompt is re-rendered.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use tracing::Instrument as _;
use zeph_agent_context::service::present_turn_context_keys;
use zeph_common::SkillTrustLevel;
use zeph_context::input::ItemKeys;
use zeph_llm::provider::{
    Message, MessagePart, RetrievalKey, TurnContextBlock, TurnContextKind, TurnContextTrust,
    context_floor,
};

use crate::agent::Agent;
use crate::channel::Channel;

/// Sources whose section is state compared with the last value sent.
const STATE_SOURCES: [&str; 7] = [
    "active_skills",
    "environment",
    "catalog_update",
    "instructions_update",
    "mcp_instructions_update",
    "preferences",
    "goal",
];

fn slot_key(source: &str) -> RetrievalKey {
    ItemKeys::key("state_slot", source)
}

fn cleared_key(source: &str) -> RetrievalKey {
    ItemKeys::key(source, "\0cleared")
}

/// Last value key sent for every state source still present in history.
///
/// Each state section stamps `[slot, value]` adjacently on its block, so the order of the keys
/// across the directive blocks tells which value was sent last.
pub(crate) fn last_state_values(messages: &[Message]) -> HashMap<RetrievalKey, RetrievalKey> {
    let slots: HashSet<RetrievalKey> = STATE_SOURCES.iter().map(|s| slot_key(s)).collect();
    let mut last = HashMap::new();
    for msg in messages.iter().skip(context_floor(messages)) {
        for part in &msg.parts {
            let MessagePart::TurnContext(block) = part else {
                continue;
            };
            if block.kind != TurnContextKind::Directives {
                continue;
            }
            let mut keys = block.keys.iter();
            while let Some(key) = keys.next() {
                if slots.contains(key)
                    && let Some(value) = keys.next()
                {
                    last.insert(*key, *value);
                }
            }
        }
    }
    last
}

/// What to do with a state section that was never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateBaseline {
    /// Send it: nothing in the frozen prompt covers it.
    Always,
    /// Skip it: the frozen prompt already holds this exact value.
    SkipIfNeverSent,
}

/// Collects the sections of one `Directives` block.
pub(crate) struct DirectiveBuilder<'a> {
    present: &'a HashSet<RetrievalKey>,
    last_state: &'a HashMap<RetrievalKey, RetrievalKey>,
    sections: Vec<String>,
    keys: Vec<RetrievalKey>,
    trust: TurnContextTrust,
}

impl<'a> DirectiveBuilder<'a> {
    /// Start a block that skips keys in `present` and compares state with `last_state`.
    pub(crate) fn new(
        present: &'a HashSet<RetrievalKey>,
        last_state: &'a HashMap<RetrievalKey, RetrievalKey>,
    ) -> Self {
        Self {
            present,
            last_state,
            sections: Vec::new(),
            keys: Vec::new(),
            trust: TurnContextTrust::Trusted,
        }
    }

    /// Add a section that is sent on every turn.
    pub(crate) fn always(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() {
            self.sections.push(text);
        }
    }

    /// Add a section sent only when `key` is not already present in history.
    pub(crate) fn once(
        &mut self,
        key: RetrievalKey,
        text: impl Into<String>,
        trust: TurnContextTrust,
    ) {
        let text = text.into();
        if text.is_empty() || self.present.contains(&key) || self.keys.contains(&key) {
            return;
        }
        self.sections.push(text);
        self.keys.push(key);
        self.trust = self.trust.max(trust);
    }

    /// Add a keyed section whose key is derived from `source` and `text`.
    pub(crate) fn once_text(
        &mut self,
        source: &'static str,
        text: impl Into<String>,
        trust: TurnContextTrust,
    ) {
        let text = text.into();
        let key = ItemKeys::key(source, &text);
        self.once(key, text, trust);
    }

    /// Add a state section for `source`, sent when it differs from the last value sent.
    ///
    /// Empty `text` means the state is gone: a `<source_cleared/>` marker is sent once if a value
    /// was sent before.
    pub(crate) fn state(
        &mut self,
        source: &'static str,
        text: impl Into<String>,
        trust: TurnContextTrust,
        baseline: StateBaseline,
    ) {
        let text = text.into();
        let slot = slot_key(source);
        let last = self.last_state.get(&slot).copied();
        let (value, section) = if text.is_empty() {
            match last {
                Some(l) if l != cleared_key(source) => {
                    (cleared_key(source), format!("<{source}_cleared/>"))
                }
                _ => return,
            }
        } else {
            let value = ItemKeys::key(source, &text);
            if last == Some(value) || (last.is_none() && baseline == StateBaseline::SkipIfNeverSent)
            {
                return;
            }
            (value, text)
        };
        if self.keys.contains(&value) {
            return;
        }
        self.sections.push(section);
        self.keys.extend([slot, value]);
        self.trust = self.trust.max(trust);
    }

    /// Finish the block; `None` when no section was added.
    pub(crate) fn finish(self) -> Option<TurnContextBlock> {
        if self.sections.is_empty() {
            return None;
        }
        Some(TurnContextBlock {
            kind: TurnContextKind::Directives,
            trust: self.trust,
            text: format!(
                "<turn_directives>\n{}\n</turn_directives>",
                self.sections.join("\n\n")
            ),
            keys: self.keys,
        })
    }
}

/// A skill listed in the frozen catalog with the trust level it had then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogEntry {
    pub(crate) name: String,
    pub(crate) trust: SkillTrustLevel,
}

/// Hashes of the inputs of the frozen system prompt.
///
/// A change between turns is not re-rendered into the prompt: it arrives as a delta directive,
/// unless it revokes something (see [`is_revocation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionPromptKey {
    pub(crate) catalog: u64,
    /// Skills listed in the catalog with their trust severity, sorted by name.
    pub(crate) catalog_entries: Vec<CatalogEntry>,
    pub(crate) instruction_blocks: Vec<u64>,
    pub(crate) mcp_instructions: u64,
    pub(crate) environment: u64,
    pub(crate) repo_map_tokens: usize,
    pub(crate) group_structured: bool,
}

fn hash_of(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Inputs of the last render, kept so a boundary can re-render within the same turn.
#[derive(Debug, Clone)]
pub(crate) struct PromptInputs {
    pub(crate) key: SessionPromptKey,
    pub(crate) catalog_prompt: String,
    pub(crate) mcp_instructions: String,
}

/// Whether the new inputs revoke something the frozen prompt or the history already holds.
///
/// A skill that left the catalog (blocked or removed) or got a higher trust severity, a skill
/// body sent at a lower severity than it has now, and instruction text that disappeared all
/// count. Additions and upgrades are deltas, not revocations.
pub(crate) fn revocation_reason(
    previous: Option<&SessionPromptKey>,
    current: &SessionPromptKey,
    skill_sent: &HashMap<String, SkillTrustLevel>,
) -> Option<String> {
    let demoted = |name: &str, sent: SkillTrustLevel| {
        current
            .catalog_entries
            .iter()
            .find(|e| e.name == name)
            .is_none_or(|e| e.trust.severity() > sent.severity())
    };
    let from_catalog = previous.and_then(|p| {
        p.catalog_entries
            .iter()
            .find(|e| demoted(&e.name, e.trust))
            .map(|e| format!("skill {} left the catalog or was demoted", e.name))
    });
    let from_instructions = previous
        .is_some_and(|p| {
            p.instruction_blocks
                .iter()
                .any(|h| !current.instruction_blocks.contains(h))
        })
        .then(|| "instruction text was removed".to_owned());
    let from_body = skill_sent
        .iter()
        .find(|(name, trust)| demoted(name, **trust))
        .map(|(name, _)| format!("sent body of skill {name} was demoted"));
    from_catalog.or(from_instructions).or(from_body)
}

fn trust_of(level: SkillTrustLevel) -> TurnContextTrust {
    match level {
        SkillTrustLevel::Trusted => TurnContextTrust::Trusted,
        SkillTrustLevel::Verified => TurnContextTrust::LocalUntrusted,
        // Quarantined, Blocked and any future level fail closed.
        _ => TurnContextTrust::ExternalUntrusted,
    }
}

/// One active skill's body section.
#[derive(Clone, Debug)]
pub(super) struct SkillSection {
    pub(super) key: RetrievalKey,
    pub(super) name: String,
    pub(super) trust: SkillTrustLevel,
    pub(super) text: String,
}

/// Skill-derived `append` inputs computed by `rebuild_system_prompt`.
#[derive(Default)]
pub(super) struct SkillInputs {
    /// Body section of every active skill (empty in compact and fallback mode).
    pub(super) sections: Vec<SkillSection>,
    /// Skills listed in the catalog with their trust severity.
    pub(super) catalog_entries: Vec<CatalogEntry>,
}

/// Per-turn inputs of the `append` prompt assembly, computed by `rebuild_system_prompt`.
#[derive(Clone, Debug)]
pub(super) struct AppendTurnInputs {
    pub(super) catalog_prompt: String,
    pub(super) catalog_entries: Vec<CatalogEntry>,
    pub(super) mcp_instructions: String,
    pub(super) matched_mcp_tools: Vec<zeph_mcp::McpTool>,
    pub(super) skill_sections: Vec<SkillSection>,
    pub(super) skill_fallback_mode: bool,
    pub(super) effective_mode: crate::config::SkillPromptMode,
    /// `sidequest.turn_counter` when the directives were first built (before the turn's tick).
    pub(super) time_turn: u64,
}

impl<C: Channel> Agent<C> {
    /// Inputs of the frozen prompt hashed for change detection.
    fn session_prompt_key(&self, input: &AppendTurnInputs) -> SessionPromptKey {
        let env = &self.services.session.env_context;
        let mut instruction_blocks: Vec<u64> = self
            .runtime
            .instructions
            .blocks
            .iter()
            .map(|b| hash_of(&(b.source.display().to_string(), &b.content)))
            .collect();
        instruction_blocks.sort_unstable();
        let mut catalog_entries = input.catalog_entries.clone();
        catalog_entries.sort_by(|a, b| a.name.cmp(&b.name));
        SessionPromptKey {
            catalog: hash_of(&input.catalog_prompt.as_str()),
            catalog_entries,
            instruction_blocks,
            mcp_instructions: hash_of(&input.mcp_instructions),
            environment: hash_of(&(env.os.as_str(), env.model_name.as_str())),
            repo_map_tokens: self.services.index.repo_map_tokens,
            group_structured: self.services.skill.group_structured,
        }
    }

    /// Render the frozen prompt into `messages[0]` and remember its inputs.
    async fn render_frozen_prompt(
        &mut self,
        catalog_prompt: &str,
        mcp_instructions: &str,
        key: SessionPromptKey,
        reason: &'static str,
    ) {
        let span = tracing::info_span!("core.context.session_prompt.render", reason);
        let prompt = self
            .build_stable_prompt(catalog_prompt, mcp_instructions)
            .instrument(span)
            .await;
        self.msg.set_system_prompt(prompt);
        self.recompute_prompt_tokens();
        self.msg.layout.rendered = Some(key.clone());
        self.msg.layout.prompt_inputs = Some(PromptInputs {
            key,
            catalog_prompt: catalog_prompt.to_owned(),
            mcp_instructions: mcp_instructions.to_owned(),
        });
    }

    /// Re-render a prompt that a boundary invalidated, in the same turn.
    ///
    /// Compaction and trim run after the per-turn prompt assembly; rendering right after them
    /// puts the system change into the request that is rewritten anyway, instead of costing a
    /// second full miss on the next turn.
    pub(in crate::agent) async fn rerender_invalidated_prompt(&mut self) {
        if !self.msg.layout.is_append() || self.msg.layout.rendered.is_some() {
            return;
        }
        let Some(inputs) = self.msg.layout.prompt_inputs.clone() else {
            return;
        };
        self.render_frozen_prompt(
            &inputs.catalog_prompt,
            &inputs.mcp_instructions,
            inputs.key,
            "boundary_same_turn",
        )
        .await;
    }

    /// Drop every `Directives` part from history and force a re-render.
    ///
    /// Used on a trust revocation: the old directives may carry a skill body or catalog entry
    /// that must not stay visible, even though dropping them costs one cache miss.
    #[tracing::instrument(name = "core.context.directive_purge", skip_all, level = "info")]
    fn purge_directives(&mut self, reason: &str) {
        tracing::info!(
            reason,
            "trust revocation: dropping directives and re-rendering"
        );
        for msg in &mut self.msg.messages {
            msg.parts.retain(|p| {
                !matches!(p, MessagePart::TurnContext(b) if b.kind == TurnContextKind::Directives)
            });
        }
        self.msg.layout.rendered = None;
        self.msg.layout.skill_sent.clear();
        self.recompute_prompt_tokens();
    }

    /// Per-turn prompt work of the `append` layout.
    ///
    /// Renders the system prompt only at a boundary (conversation start, `/clear`, hard
    /// compaction, focus, trim, trust revocation); every other turn leaves `messages[0]`
    /// untouched and queues the turn `Directives` block that `build_user_message` attaches.
    #[tracing::instrument(name = "core.context.assemble_append_turn", skip_all, level = "debug")]
    pub(super) async fn assemble_append_turn(&mut self, input: AppendTurnInputs) {
        let key = self.session_prompt_key(&input);
        let mut previous = self.msg.layout.rendered.clone();
        if let Some(reason) =
            revocation_reason(previous.as_ref(), &key, &self.msg.layout.skill_sent)
        {
            self.purge_directives(&reason);
            previous = None;
        }
        if previous.is_none() {
            self.render_frozen_prompt(
                &input.catalog_prompt,
                &input.mcp_instructions,
                key.clone(),
                "boundary",
            )
            .await;
        } else if let Some(inputs) = self.msg.layout.prompt_inputs.as_mut() {
            // A later same-turn re-render must use today's inputs and key.
            inputs.key = key.clone();
            input.catalog_prompt.clone_into(&mut inputs.catalog_prompt);
            inputs.mcp_instructions.clone_from(&input.mcp_instructions);
        }

        let render = previous.map(|p| (p, key));
        self.build_directives(&input, render.as_ref().map(|(p, c)| (p, c)))
            .await;
        self.msg.layout.turn_inputs = Some((input, render));
    }

    /// Rebuild the turn directives when a same-turn boundary (compaction, trim, eviction)
    /// removed directive-carrying messages after they were built.
    ///
    /// Without this a skill body, environment or goal whose message was dropped would be missing
    /// for the whole boundary turn and only re-sent on the next one.
    pub(in crate::agent) async fn rebuild_directives_if_history_changed(&mut self) {
        if !self.msg.layout.is_append() {
            return;
        }
        let present = present_turn_context_keys(&self.msg.messages, TurnContextKind::Directives);
        if self.msg.layout.directive_basis.as_ref() == Some(&present) {
            return;
        }
        let Some((input, render)) = self.msg.layout.turn_inputs.take() else {
            return;
        };
        tracing::debug!("directive basis changed by a boundary; rebuilding turn directives");
        self.build_directives(&input, render.as_ref().map(|(p, c)| (p, c)))
            .await;
        self.msg.layout.turn_inputs = Some((input, render));
    }

    /// Build the `Directives` block against the current history.
    ///
    /// `render` carries the previous and current prompt keys when the prompt was not re-rendered
    /// this turn, so inputs that changed since the render become delta sections.
    async fn build_directives(
        &mut self,
        input: &AppendTurnInputs,
        render: Option<(&SessionPromptKey, &SessionPromptKey)>,
    ) {
        let present = present_turn_context_keys(&self.msg.messages, TurnContextKind::Directives);
        let last_state = last_state_values(&self.msg.messages);
        self.msg.layout.directive_basis = Some(present.clone());
        let mut b = DirectiveBuilder::new(&present, &last_state);

        b.state(
            "active_skills",
            self.active_skills_text(input.skill_fallback_mode),
            TurnContextTrust::Trusted,
            StateBaseline::Always,
        );
        if let Some((prev, current)) = render {
            self.add_prompt_deltas(&mut b, prev, current, input);
        }
        for section in &input.skill_sections {
            b.once(section.key, section.text.clone(), trust_of(section.trust));
        }
        for tool in &input.matched_mcp_tools {
            b.once_text(
                "mcp_tool",
                zeph_mcp::format_mcp_tools_prompt(std::slice::from_ref(tool)),
                TurnContextTrust::ExternalUntrusted,
            );
        }
        self.add_environment_directive(&mut b);
        self.add_notice_directives(&mut b, input.effective_mode, input.skill_fallback_mode)
            .await;
        self.add_volatile_directives(&mut b, input.time_turn);

        let block = b.finish();
        if let Some(block) = &block {
            for section in &input.skill_sections {
                if !section.name.is_empty() && block.keys.contains(&section.key) {
                    self.msg
                        .layout
                        .skill_sent
                        .insert(section.name.clone(), section.trust);
                }
            }
        }
        self.msg.pending_preamble.directives = block;
    }

    /// The `<active_skills>` line, or an empty string when there is nothing to announce.
    ///
    /// Names are sorted in a local copy so a pure reorder by the matcher is not re-sent;
    /// `active_skill_names` itself keeps its match order (corrections attribute to its first
    /// entry). In fallback mode the list is the whole catalog, so nothing is announced.
    fn active_skills_text(&self, skill_fallback_mode: bool) -> String {
        let names = &self.services.skill.active_skill_names;
        if skill_fallback_mode || names.is_empty() {
            return String::new();
        }
        let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        format!("<active_skills>{}</active_skills>", sorted.join(", "))
    }

    /// Delta sections for session-scoped prompt inputs that differ from the last render.
    fn add_prompt_deltas(
        &self,
        b: &mut DirectiveBuilder<'_>,
        previous: &SessionPromptKey,
        current: &SessionPromptKey,
        input: &AppendTurnInputs,
    ) {
        let baseline = |same: bool| {
            if same {
                StateBaseline::SkipIfNeverSent
            } else {
                StateBaseline::Always
            }
        };
        b.state(
            "catalog_update",
            format!(
                "<skills_catalog_update>\n{}\n</skills_catalog_update>",
                input.catalog_prompt
            ),
            TurnContextTrust::Trusted,
            baseline(previous.catalog == current.catalog),
        );
        let instructions: Vec<String> = self
            .runtime
            .instructions
            .blocks
            .iter()
            .map(|blk| {
                format!(
                    "<!-- instructions: {} -->\n{}",
                    blk.source.file_name().unwrap_or_default().to_string_lossy(),
                    blk.content
                )
            })
            .collect();
        b.state(
            "instructions_update",
            format!(
                "<instructions_update>\n{}\n</instructions_update>",
                instructions.join("\n\n")
            ),
            TurnContextTrust::Trusted,
            baseline(previous.instruction_blocks == current.instruction_blocks),
        );
        let mcp = input.mcp_instructions.trim();
        b.state(
            "mcp_instructions_update",
            if mcp.is_empty() {
                String::new()
            } else {
                format!("<mcp_server_instructions_update>{mcp}</mcp_server_instructions_update>")
            },
            TurnContextTrust::ExternalUntrusted,
            baseline(previous.mcp_instructions == current.mcp_instructions),
        );
    }

    /// Working directory and git branch, sent whenever they differ from the last value sent.
    fn add_environment_directive(&self, b: &mut DirectiveBuilder<'_>) {
        use std::fmt::Write as _;

        let mut text = format!(
            "<environment_update>\nworking_directory: {}\n",
            self.working_dir_path().display()
        );
        if let Some(branch) = &self.services.session.env_context.git_branch {
            let _ = writeln!(text, "git_branch: {branch}");
        }
        text.push_str("</environment_update>");
        b.state(
            "environment",
            text,
            TurnContextTrust::Trusted,
            StateBaseline::Always,
        );
    }

    /// Preferences, goal, guest context and the caveman and `memory_save` notices.
    async fn add_notice_directives(
        &mut self,
        b: &mut DirectiveBuilder<'_>,
        effective_mode: crate::config::SkillPromptMode,
        skill_fallback_mode: bool,
    ) {
        let mut prefs = String::new();
        self.inject_learned_preferences(&mut prefs).await;
        b.state(
            "preferences",
            prefs.trim().to_owned(),
            TurnContextTrust::Trusted,
            StateBaseline::Always,
        );

        let mut goal = String::new();
        self.inject_active_goal(&mut goal).await;
        b.state(
            "goal",
            goal.trim().to_owned(),
            TurnContextTrust::Trusted,
            StateBaseline::Always,
        );

        if self.services.session.is_guest_context {
            b.always(
                "[CONTEXT: This message comes from a Telegram guest query. \
                 Provide a concise, self-contained response \u{2014} \
                 the recipient may not have prior conversation context.]",
            );
        }
        if super::assembly::should_inject_caveman_directive(
            self.services.session.caveman_active,
            &self.services.skill.active_skill_names,
            effective_mode,
            skill_fallback_mode,
        ) {
            b.once_text(
                "caveman",
                super::assembly::CAVEMAN_DIRECTIVE.trim(),
                TurnContextTrust::Trusted,
            );
        }
        if self
            .services
            .tool_state
            .completed_tool_ids
            .contains("memory_save")
        {
            b.once_text(
                "memory_save_hint",
                "Facts provided by the user in this session have been saved with memory_save \u{2014} use memory_search to recall them, not search_code.",
                TurnContextTrust::Trusted,
            );
        }
    }

    /// Budget hint and time reminder: values that change every turn are never keyed.
    fn add_volatile_directives(&self, b: &mut DirectiveBuilder<'_>, time_turn: u64) {
        if self.runtime.config.budget_hint_enabled
            && let Some(xml) = self.budget_hint_snapshot(0).format_xml()
        {
            b.always(xml);
        }
        if self.runtime.config.time_reminder_enabled {
            let interval = self.runtime.config.time_reminder_interval_requests;
            if interval > 0 && time_turn.is_multiple_of(u64::from(interval)) {
                let now = self.runtime.config.clock.now();
                b.always(format!(
                    "<current_time>{}</current_time>",
                    zeph_common::timestamp::rfc3339_from(now)
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use zeph_config::ContextLayout;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    use super::*;
    use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};
    use crate::instructions::InstructionBlock;

    fn keys(items: &[u64]) -> HashSet<RetrievalKey> {
        items.iter().copied().map(RetrievalKey).collect()
    }

    #[test]
    fn builder_skips_present_keys_and_dedups_within_a_turn() {
        let present = keys(&[1]);
        let last = HashMap::new();
        let mut b = DirectiveBuilder::new(&present, &last);
        b.always("names");
        b.once(RetrievalKey(1), "already sent", TurnContextTrust::Trusted);
        b.once(RetrievalKey(2), "new body", TurnContextTrust::Trusted);
        b.once(RetrievalKey(2), "new body again", TurnContextTrust::Trusted);
        let block = b.finish().unwrap();
        assert_eq!(block.kind, TurnContextKind::Directives);
        assert_eq!(block.keys, vec![RetrievalKey(2)]);
        assert!(block.text.contains("names"));
        assert!(block.text.contains("new body"));
        assert!(!block.text.contains("already sent"));
        assert!(!block.text.contains("again"));
    }

    #[test]
    fn builder_is_empty_when_everything_is_present() {
        let (present, last) = (HashSet::new(), HashMap::new());
        assert!(DirectiveBuilder::new(&present, &last).finish().is_none());
    }

    #[test]
    fn builder_takes_the_least_trusted_section() {
        let (present, last) = (HashSet::new(), HashMap::new());
        let mut b = DirectiveBuilder::new(&present, &last);
        b.once_text("a", "trusted", TurnContextTrust::Trusted);
        b.once_text("b", "mcp", TurnContextTrust::ExternalUntrusted);
        assert_eq!(
            b.finish().unwrap().trust,
            TurnContextTrust::ExternalUntrusted
        );
    }

    fn block_with(sections: &[(&'static str, &str)]) -> Message {
        let (present, last) = (HashSet::new(), HashMap::new());
        let mut b = DirectiveBuilder::new(&present, &last);
        for (source, text) in sections {
            b.state(
                source,
                *text,
                TurnContextTrust::Trusted,
                StateBaseline::Always,
            );
        }
        Message::from_parts(
            zeph_llm::provider::Role::User,
            vec![
                MessagePart::TurnContext(b.finish().unwrap()),
                MessagePart::Text { text: "q".into() },
            ],
        )
    }

    #[test]
    fn state_is_resent_when_it_returns_to_an_earlier_value() {
        let history = vec![
            Message::from_legacy(zeph_llm::provider::Role::System, "sys"),
            block_with(&[("environment", "A")]),
            block_with(&[("environment", "B")]),
        ];
        let (present, last) = (HashSet::new(), last_state_values(&history));
        let mut b = DirectiveBuilder::new(&present, &last);
        b.state(
            "environment",
            "A",
            TurnContextTrust::Trusted,
            StateBaseline::Always,
        );
        let block = b.finish().expect("A after B must be announced again");
        assert!(block.text.contains('A'));

        let mut same = DirectiveBuilder::new(&present, &last);
        same.state(
            "environment",
            "B",
            TurnContextTrust::Trusted,
            StateBaseline::Always,
        );
        assert!(same.finish().is_none(), "the current value is not repeated");
    }

    #[test]
    fn cleared_state_is_announced_once() {
        let history = vec![
            Message::from_legacy(zeph_llm::provider::Role::System, "sys"),
            block_with(&[("goal", "ship it")]),
        ];
        let (present, last) = (HashSet::new(), last_state_values(&history));
        let mut b = DirectiveBuilder::new(&present, &last);
        b.state("goal", "", TurnContextTrust::Trusted, StateBaseline::Always);
        let cleared = b.finish().unwrap();
        assert!(cleared.text.contains("<goal_cleared/>"));

        let mut history = history;
        history.push(Message::from_parts(
            zeph_llm::provider::Role::User,
            vec![MessagePart::TurnContext(cleared)],
        ));
        let last = last_state_values(&history);
        let mut again = DirectiveBuilder::new(&present, &last);
        again.state("goal", "", TurnContextTrust::Trusted, StateBaseline::Always);
        assert!(again.finish().is_none());
    }

    #[test]
    fn baseline_skips_a_state_the_frozen_prompt_already_holds() {
        let (present, last) = (HashSet::new(), HashMap::new());
        let mut b = DirectiveBuilder::new(&present, &last);
        b.state(
            "catalog_update",
            "A",
            TurnContextTrust::Trusted,
            StateBaseline::SkipIfNeverSent,
        );
        assert!(b.finish().is_none());
    }

    fn level(severity: u8) -> SkillTrustLevel {
        match severity {
            0 => SkillTrustLevel::Trusted,
            1 => SkillTrustLevel::Verified,
            2 => SkillTrustLevel::Quarantined,
            _ => SkillTrustLevel::Blocked,
        }
    }

    fn entry(name: &str, severity: u8) -> CatalogEntry {
        CatalogEntry {
            name: name.to_owned(),
            trust: level(severity),
        }
    }

    fn key_with(entries: &[(&str, u8)], blocks: &[u64]) -> SessionPromptKey {
        SessionPromptKey {
            catalog: 0,
            catalog_entries: entries.iter().map(|(n, s)| entry(n, *s)).collect(),
            instruction_blocks: blocks.to_vec(),
            mcp_instructions: 0,
            environment: 0,
            repo_map_tokens: 0,
            group_structured: false,
        }
    }

    #[test]
    fn revocation_covers_removal_demotion_and_dropped_instructions_only() {
        let base = key_with(&[("s", 0)], &[1, 2]);
        let none = HashMap::new();
        let reason = |prev: &SessionPromptKey, cur: &SessionPromptKey| {
            revocation_reason(Some(prev), cur, &none)
        };
        assert!(reason(&base, &base).is_none());
        assert!(
            reason(&base, &key_with(&[("s", 0), ("t", 3)], &[1, 2, 9])).is_none(),
            "additions are deltas"
        );
        assert!(
            reason(&key_with(&[("s", 2)], &[]), &key_with(&[("s", 0)], &[])).is_none(),
            "an upgrade is not a revocation"
        );
        assert!(reason(&base, &key_with(&[], &[1, 2])).is_some(), "removed");
        assert!(
            reason(&base, &key_with(&[("s", 2)], &[1, 2])).is_some(),
            "demoted"
        );
        assert!(
            reason(&base, &key_with(&[("s", 0)], &[1])).is_some(),
            "text dropped"
        );
        let sent = HashMap::from([("late".to_owned(), level(0))]);
        let why = revocation_reason(None, &key_with(&[("late", 1)], &[]), &sent);
        assert!(why.is_some_and(|w| w.contains("late")), "names the skill");
    }

    fn append_agent() -> Agent<MockChannel> {
        let mut agent = Agent::new(
            AnyProvider::Mock(MockProvider::with_responses(vec![])),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.services.memory.compaction.context_layout = ContextLayout::Append;
        agent.ensure_layout_pinned();
        agent.services.session.env_context.git_branch = None;
        agent
    }

    fn inputs(
        catalog: &str,
        entries: &[(&str, u8)],
        sections: Vec<SkillSection>,
    ) -> AppendTurnInputs {
        AppendTurnInputs {
            catalog_prompt: catalog.to_owned(),
            catalog_entries: entries.iter().map(|(n, s)| entry(n, *s)).collect(),
            mcp_instructions: String::new(),
            matched_mcp_tools: vec![],
            skill_sections: sections,
            skill_fallback_mode: false,
            effective_mode: crate::config::SkillPromptMode::Full,
            time_turn: 0,
        }
    }

    fn skill_section(name: &str, severity: u8, body: &str) -> SkillSection {
        SkillSection {
            key: ItemKeys::key("skill", &format!("{name}\0{severity}\0{body}")),
            name: name.to_owned(),
            trust: level(severity),
            text: format!("<available_skills>{name}: {body}</available_skills>"),
        }
    }

    /// Assemble one turn and push its turn input, like `process_user_message` does.
    async fn run_turn(agent: &mut Agent<MockChannel>, input: AppendTurnInputs) -> String {
        agent.assemble_append_turn(input).await;
        let preamble = std::mem::take(&mut agent.msg.pending_preamble);
        let text = preamble
            .directives
            .as_ref()
            .map(|b| b.text.clone())
            .unwrap_or_default();
        let turn = agent.build_user_message("q", vec![], preamble);
        agent.msg.messages.push(turn);
        text
    }

    fn directive_parts(agent: &Agent<MockChannel>) -> usize {
        agent
            .msg
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| {
                matches!(p, MessagePart::TurnContext(b) if b.kind == TurnContextKind::Directives)
            })
            .count()
    }

    #[tokio::test]
    async fn working_directory_a_b_a_is_announced_each_time_it_changes() {
        let mut agent = append_agent();
        agent.services.session.env_context.working_dir = "/work/a".into();
        let t1 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(t1.contains("working_directory: /work/a"));

        agent.services.session.env_context.working_dir = "/work/b".into();
        let t2 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(t2.contains("working_directory: /work/b"));

        agent.services.session.env_context.working_dir = "/work/a".into();
        let t3 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(
            t3.contains("working_directory: /work/a"),
            "back to A must be sent: {t3}"
        );

        let t4 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(
            !t4.contains("working_directory"),
            "unchanged state is not repeated"
        );
    }

    #[tokio::test]
    async fn catalog_a_b_a_is_announced_each_time_it_changes() {
        let mut agent = append_agent();
        run_turn(&mut agent, inputs("catalog A", &[], vec![])).await;
        let t2 = run_turn(&mut agent, inputs("catalog B", &[], vec![])).await;
        assert!(t2.contains("catalog B"));
        let t3 = run_turn(&mut agent, inputs("catalog A", &[], vec![])).await;
        assert!(
            t3.contains("catalog A"),
            "revert to the rendered catalog must be sent: {t3}"
        );
        let t4 = run_turn(&mut agent, inputs("catalog A", &[], vec![])).await;
        assert!(!t4.contains("skills_catalog_update"));
    }

    #[tokio::test]
    async fn skill_body_is_sent_once() {
        let mut agent = append_agent();
        let t1 = run_turn(
            &mut agent,
            inputs("c", &[("s", 0)], vec![skill_section("s", 0, "do x")]),
        )
        .await;
        assert!(t1.contains("do x"));
        let t2 = run_turn(
            &mut agent,
            inputs("c", &[("s", 0)], vec![skill_section("s", 0, "do x")]),
        )
        .await;
        assert!(!t2.contains("do x"));
    }

    async fn revocation_case(after_entries: &[(&str, u8)]) -> (Agent<MockChannel>, String) {
        let mut agent = append_agent();
        run_turn(
            &mut agent,
            inputs("c", &[("s", 0)], vec![skill_section("s", 0, "secret body")]),
        )
        .await;
        assert_eq!(directive_parts(&agent), 1);
        agent.msg.messages[0].content = "stale frozen prompt".to_owned();

        let text = run_turn(&mut agent, inputs("c", after_entries, vec![])).await;
        (agent, text)
    }

    #[tokio::test]
    async fn blocked_skill_drops_old_directives_and_re_renders() {
        let (agent, text) = Box::pin(revocation_case(&[])).await;
        assert_ne!(agent.msg.messages[0].content, "stale frozen prompt");
        assert_eq!(
            directive_parts(&agent),
            1,
            "only the new turn's directives remain"
        );
        assert!(!text.contains("secret body"));
        assert!(agent.msg.layout.skill_sent.is_empty());
    }

    #[tokio::test]
    async fn quarantined_skill_drops_the_trusted_body_from_history() {
        let (agent, _) = Box::pin(revocation_case(&[("s", 2)])).await;
        assert!(
            !agent.msg.messages.iter().flat_map(|m| &m.parts).any(
                |p| matches!(p, MessagePart::TurnContext(b) if b.text.contains("secret body"))
            ),
            "the body sent while the skill was trusted must be gone"
        );
        assert_ne!(agent.msg.messages[0].content, "stale frozen prompt");
    }

    #[tokio::test]
    async fn hash_mismatch_demotion_of_a_sent_skill_is_a_boundary() {
        let mut agent = append_agent();
        run_turn(
            &mut agent,
            inputs(
                "c",
                &[("s", 1)],
                vec![skill_section("s", 1, "verified body")],
            ),
        )
        .await;
        agent.msg.messages[0].content = "stale".to_owned();
        run_turn(&mut agent, inputs("c", &[("s", 2)], vec![])).await;
        assert_ne!(agent.msg.messages[0].content, "stale");
        assert_eq!(directive_parts(&agent), 1);
    }

    #[tokio::test]
    async fn removed_instruction_text_is_a_boundary() {
        let mut agent = append_agent();
        agent.runtime.instructions.blocks = vec![InstructionBlock {
            source: "/p/AGENTS.md".into(),
            content: "never do Y".into(),
        }];
        run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(agent.msg.messages[0].content.contains("never do Y"));

        agent.runtime.instructions.blocks.clear();
        run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(!agent.msg.messages[0].content.contains("never do Y"));
        assert_eq!(directive_parts(&agent), 1);
    }

    #[tokio::test]
    async fn an_invalidated_prompt_is_re_rendered_in_the_same_turn() {
        let mut agent = append_agent();
        run_turn(&mut agent, inputs("c", &[], vec![])).await;
        agent.msg.messages[0].content = "tampered".to_owned();
        agent.msg.layout.invalidate_prompt();
        agent.rerender_invalidated_prompt().await;
        assert_ne!(agent.msg.messages[0].content, "tampered");
        assert!(agent.msg.layout.rendered.is_some());

        agent.msg.messages[0].content = "kept".to_owned();
        agent.rerender_invalidated_prompt().await;
        assert_eq!(
            agent.msg.messages[0].content, "kept",
            "a valid prompt is left alone"
        );
    }

    #[tokio::test]
    async fn compact_mode_sends_only_the_names_line() {
        let mut agent = append_agent();
        agent.services.skill.active_skill_names = vec!["alpha".into(), "beta".into()];
        let mut input = inputs("catalog", &[], vec![]);
        input.effective_mode = crate::config::SkillPromptMode::Compact;
        let text = run_turn(&mut agent, input).await;
        assert!(text.contains("<active_skills>alpha, beta</active_skills>"));
        assert!(!text.contains("<available_skills>"));
    }

    #[tokio::test]
    async fn active_skills_line_is_sent_only_when_it_changes() {
        let mut agent = append_agent();
        agent.services.skill.active_skill_names = vec!["beta".into(), "alpha".into()];
        let t1 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(t1.contains("<active_skills>alpha, beta</active_skills>"));

        agent.services.skill.active_skill_names = vec!["alpha".into(), "beta".into()];
        let t2 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(
            !t2.contains("active_skills"),
            "a pure reorder is not a change: {t2}"
        );

        agent.services.skill.active_skill_names = vec!["gamma".into()];
        let t3 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(t3.contains("<active_skills>gamma</active_skills>"));

        agent.services.skill.active_skill_names = vec!["alpha".into(), "beta".into()];
        let t4 = run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert!(
            t4.contains("<active_skills>alpha, beta</active_skills>"),
            "A-B-A is announced"
        );
    }

    #[tokio::test]
    async fn active_skills_keeps_match_order_in_agent_state() {
        let mut agent = append_agent();
        agent.services.skill.active_skill_names = vec!["zeta".into(), "alpha".into()];
        run_turn(&mut agent, inputs("c", &[], vec![])).await;
        assert_eq!(
            agent
                .services
                .skill
                .active_skill_names
                .first()
                .map(String::as_str),
            Some("zeta")
        );
    }

    #[tokio::test]
    async fn active_skills_is_not_sent_in_fallback_mode() {
        let mut agent = append_agent();
        agent.services.skill.active_skill_names = vec!["alpha".into(), "beta".into()];
        let mut input = inputs("c", &[], vec![]);
        input.skill_fallback_mode = true;
        let text = run_turn(&mut agent, input).await;
        assert!(!text.contains("active_skills"), "{text}");
    }

    #[tokio::test]
    async fn active_skills_entering_fallback_mode_clears_the_sent_list_once() {
        let mut agent = append_agent();
        agent.services.skill.active_skill_names = vec!["alpha".into()];
        run_turn(&mut agent, inputs("c", &[], vec![])).await;

        let mut input = inputs("c", &[], vec![]);
        input.skill_fallback_mode = true;
        let t2 = run_turn(&mut agent, input.clone()).await;
        assert!(t2.contains("<active_skills_cleared/>"));
        let t3 = run_turn(&mut agent, input).await;
        assert!(!t3.contains("active_skills"));
    }

    #[tokio::test]
    async fn time_reminder_uses_the_turn_counter_of_the_first_build() {
        let mut agent = append_agent();
        agent.runtime.config.time_reminder_enabled = true;
        agent.runtime.config.time_reminder_interval_requests = 3;
        let mut input = inputs("c", &[], vec![]);
        input.time_turn = 0;
        agent.assemble_append_turn(input).await;
        assert!(
            agent
                .msg
                .pending_preamble
                .directives
                .as_ref()
                .is_some_and(|b| b.text.contains("<current_time>"))
        );

        // The tick moved the counter; a boundary rebuild must not shift the reminder.
        agent.services.sidequest.turn_counter = 1;
        agent.msg.pending_preamble.directives = None;
        agent.msg.layout.directive_basis = Some(HashSet::from([RetrievalKey(1)]));
        agent.rebuild_directives_if_history_changed().await;
        assert!(
            agent
                .msg
                .pending_preamble
                .directives
                .as_ref()
                .is_some_and(|b| b.text.contains("<current_time>")),
            "rebuilt directives keep the first build's turn"
        );
    }

    #[tokio::test]
    async fn catalog_delta_survives_a_same_turn_rebuild_without_a_render() {
        let mut agent = append_agent();
        run_turn(&mut agent, inputs("catalog A", &[], vec![])).await;

        agent
            .assemble_append_turn(inputs("catalog B", &[], vec![]))
            .await;
        assert!(
            agent
                .msg
                .pending_preamble
                .directives
                .as_ref()
                .is_some_and(|b| b.text.contains("catalog B"))
        );

        // SideQuest eviction removes the previous turn input; no re-render happens.
        agent.msg.messages.truncate(1);
        agent.rebuild_directives_if_history_changed().await;
        assert!(
            agent
                .msg
                .pending_preamble
                .directives
                .as_ref()
                .is_some_and(|b| b.text.contains("catalog B")),
            "the catalog update must not be lost by the rebuild"
        );
    }
}
