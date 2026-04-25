// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{LlmProvider, Message, Role, SemanticMemory};
use std::path::PathBuf;
use zeph_llm::provider::MessageMetadata;

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Drop guard that sends an empty string to `status_tx` when dropped, clearing the TUI spinner.
pub(super) struct ClearStatusOnDrop(pub Option<tokio::sync::mpsc::UnboundedSender<String>>);

impl Drop for ClearStatusOnDrop {
    fn drop(&mut self) {
        if let Some(ref tx) = self.0 {
            let _ = tx.send(String::new());
        }
    }
}

pub(super) fn defer_clear_status(
    tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> ClearStatusOnDrop {
    ClearStatusOnDrop(tx)
}

pub(super) async fn write_skill_file(
    skill_paths: &[PathBuf],
    skill_name: &str,
    description: &str,
    body: &str,
) -> Result<(), super::super::error::AgentError> {
    if skill_name.contains('/') || skill_name.contains('\\') || skill_name.contains("..") {
        return Err(
            super::super::error::SkillOperationFailure::InvalidName(skill_name.to_owned()).into(),
        );
    }
    for base in skill_paths {
        let skill_dir = base.join(skill_name);
        let skill_file = skill_dir.join("SKILL.md");
        if skill_file.exists() {
            let content =
                format!("---\nname: {skill_name}\ndescription: {description}\n---\n{body}\n");
            tokio::fs::write(&skill_file, content).await?;
            return Ok(());
        }
    }
    Err(super::super::error::SkillOperationFailure::DirectoryNotFound(skill_name.to_owned()).into())
}

/// Naive parser for `SQLite` datetime strings (e.g. "2024-01-15 10:30:00") to Unix seconds.
pub(super) fn chrono_parse_sqlite(s: &str) -> Result<u64, ()> {
    // Format: "YYYY-MM-DD HH:MM:SS"
    let parts: Vec<&str> = s.split(&['-', ' ', ':'][..]).collect();
    if parts.len() < 6 {
        return Err(());
    }
    let year: u64 = parts[0].parse().map_err(|_| ())?;
    let month: u64 = parts[1].parse().map_err(|_| ())?;
    let day: u64 = parts[2].parse().map_err(|_| ())?;
    let hour: u64 = parts[3].parse().map_err(|_| ())?;
    let min: u64 = parts[4].parse().map_err(|_| ())?;
    let sec: u64 = parts[5].parse().map_err(|_| ())?;

    // Rough approximation (sufficient for cooldown comparison)
    let days_approx = (year - 1970) * 365 + (month - 1) * 30 + (day - 1);
    Ok(days_approx * 86400 + hour * 3600 + min * 60 + sec)
}

// ── ARISE background task ─────────────────────────────────────────────────────

pub(super) struct AriseTaskArgs {
    pub provider: zeph_llm::any::AnyProvider,
    pub memory: std::sync::Arc<SemanticMemory>,
    pub skill_name: String,
    pub skill_body: String,
    pub skill_desc: String,
    pub trace: String,
    pub max_auto_sections: u32,
    pub skill_paths: Vec<PathBuf>,
    pub auto_activate: bool,
    pub max_versions: u32,
    pub domain_success_gate: bool,
    pub status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

pub(super) async fn arise_trace_task(args: AriseTaskArgs) {
    let _clear_status = defer_clear_status(args.status_tx.clone());
    let prompt = zeph_skills::evolution::build_trace_improvement_prompt(
        &args.skill_name,
        &args.skill_body,
        &args.trace,
    );
    let messages = vec![
        Message {
            role: Role::System,
            content: "You are a skill improvement assistant. Output only the improved skill body."
                .into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let generated = match args.provider.chat(&messages).await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("ARISE trace improvement LLM call failed: {e:#}");
            return;
        }
    };
    let generated = generated.trim().to_string();
    if generated.is_empty()
        || generated.len() > zeph_skills::evolution::MAX_BODY_BYTES
        || !zeph_skills::evolution::validate_body_size(&args.skill_body, &generated)
        || !zeph_skills::evolution::validate_body_sections(&generated, args.max_auto_sections)
    {
        tracing::warn!(skill = %args.skill_name, "ARISE: generated body rejected (validation)");
        return;
    }
    if !arise_check_domain_gate(&args, &generated).await {
        return;
    }
    arise_store_version(args, generated).await;
}

async fn arise_check_domain_gate(args: &AriseTaskArgs, generated: &str) -> bool {
    if !args.domain_success_gate {
        return true;
    }
    let gate_prompt = zeph_skills::evolution::build_domain_gate_prompt(
        &args.skill_name,
        &args.skill_desc,
        generated,
    );
    let gate_messages = vec![Message {
        role: Role::User,
        content: gate_prompt,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    match args
        .provider
        .chat_typed_erased::<zeph_skills::evolution::DomainGateResult>(&gate_messages)
        .await
    {
        Ok(gate) if !gate.domain_relevant => {
            tracing::warn!(skill = %args.skill_name, "ARISE: domain gate rejected generated body");
            false
        }
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                "ARISE: domain gate check failed for {}: {e:#}",
                args.skill_name
            );
            true // proceed on gate failure
        }
    }
}

async fn arise_store_version(args: AriseTaskArgs, generated: String) {
    let active = match args
        .memory
        .sqlite()
        .active_skill_version(&args.skill_name)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "ARISE: read active version failed for {}: {e:#}",
                args.skill_name
            );
            return;
        }
    };
    let next_ver = match args
        .memory
        .sqlite()
        .next_skill_version(&args.skill_name)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("ARISE: next version query failed: {e:#}");
            return;
        }
    };
    let predecessor_id = active.as_ref().map(|v| v.id);
    // CRITICAL: ARISE-generated versions MUST start at quarantined trust level.
    let version_id = match args
        .memory
        .sqlite()
        .save_skill_version(
            &args.skill_name,
            next_ver,
            &generated,
            &args.skill_desc,
            "arise_trace",
            None,
            predecessor_id,
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("ARISE: save_skill_version failed: {e:#}");
            return;
        }
    };
    tracing::info!(skill = %args.skill_name, version = next_ver, "ARISE: saved trace-improved version (quarantined)");
    if args.auto_activate {
        if let Err(e) = args
            .memory
            .sqlite()
            .activate_skill_version(&args.skill_name, version_id)
            .await
        {
            tracing::warn!("ARISE: activate_skill_version failed: {e:#}");
            return;
        }
        if let Err(e) = write_skill_file(
            &args.skill_paths,
            &args.skill_name,
            &args.skill_desc,
            &generated,
        )
        .await
        {
            tracing::warn!("ARISE: write_skill_file failed: {e:#}");
        }
    }
    if let Err(e) = args
        .memory
        .sqlite()
        .prune_skill_versions(&args.skill_name, args.max_versions)
        .await
    {
        tracing::warn!("ARISE: prune_skill_versions failed: {e:#}");
    }
}

// ── STEM background task ─────────────────────────────────────────────────────

pub(super) struct StemTaskArgs {
    pub provider: zeph_llm::any::AnyProvider,
    pub memory: std::sync::Arc<SemanticMemory>,
    pub tool_sequence: String,
    pub sequence_hash: String,
    pub context_hash: String,
    pub outcome: String,
    pub conv_id: Option<zeph_memory::ConversationId>,
    pub min_occurrences: u32,
    pub min_success_rate: f64,
    pub window_days: u32,
    pub retention_days: u32,
    pub max_auto_sections: u32,
    pub skill_paths: Vec<PathBuf>,
    pub status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

pub(super) async fn stem_detection_task(args: StemTaskArgs) {
    let _clear_status = defer_clear_status(args.status_tx.clone());
    if let Err(e) = args
        .memory
        .sqlite()
        .insert_tool_usage_log(
            &args.tool_sequence,
            &args.sequence_hash,
            &args.context_hash,
            &args.outcome,
            args.conv_id,
        )
        .await
    {
        tracing::warn!("STEM: insert_tool_usage_log failed: {e:#}");
        return;
    }
    let _ = args
        .memory
        .sqlite()
        .prune_tool_usage_log(args.retention_days)
        .await;
    let patterns = match args
        .memory
        .sqlite()
        .find_recurring_patterns(args.min_occurrences, args.window_days)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("STEM: find_recurring_patterns failed: {e:#}");
            return;
        }
    };
    for (seq, hash, occ, suc) in patterns {
        let pattern = zeph_skills::stem::ToolPattern {
            tool_sequence: seq.clone(),
            sequence_hash: hash,
            occurrence_count: occ,
            success_count: suc,
        };
        if !zeph_skills::stem::should_generate_skill(
            &pattern,
            args.min_occurrences,
            args.min_success_rate,
        ) {
            continue;
        }
        stem_generate_skill(&args, &seq, &pattern, occ).await;
    }
}

async fn stem_generate_skill(
    args: &StemTaskArgs,
    seq: &str,
    pattern: &zeph_skills::stem::ToolPattern,
    occ: u32,
) {
    let prompt = zeph_skills::stem::build_pattern_to_skill_prompt(seq, &[]);
    let messages = vec![
        Message {
            role: Role::System,
            content: "You are a skill generation assistant. Output only the SKILL.md body.".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let generated = match args.provider.chat(&messages).await {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("STEM: skill generation LLM call failed: {e:#}");
            return;
        }
    };
    let generated = generated.trim().to_string();
    if generated.is_empty()
        || generated.len() > zeph_skills::evolution::MAX_BODY_BYTES
        || !zeph_skills::evolution::validate_body_sections(&generated, args.max_auto_sections)
    {
        tracing::warn!("STEM: generated body rejected for pattern '{seq}'");
        return;
    }
    let skill_name = format!("stem-{}", &pattern.sequence_hash[..8].to_lowercase());
    let description = format!("Auto-generated from tool pattern: {seq}");
    let Some(skill_dir) = args.skill_paths.first() else {
        return;
    };
    let skill_file = skill_dir.join(format!("{skill_name}.md"));
    if skill_file.exists() {
        tracing::debug!("STEM: skill file already exists for pattern '{seq}', skipping");
        return;
    }
    let content = format!(
        "---\nname: {skill_name}\ndescription: {description}\ntrust: quarantined\nsource: stem\n---\n\n{generated}"
    );
    if let Err(e) = tokio::fs::write(&skill_file, &content).await {
        tracing::warn!(
            "STEM: failed to write skill file {}: {e:#}",
            skill_file.display()
        );
        return;
    }
    tracing::info!(
        "STEM: generated quarantined skill '{skill_name}' from pattern '{seq}' (occurrences={occ})"
    );
}

// ── ERL background task ───────────────────────────────────────────────────────

pub(super) struct ErlTaskArgs {
    pub provider: zeph_llm::any::AnyProvider,
    pub memory: std::sync::Arc<SemanticMemory>,
    pub skill_name: String,
    pub task_summary: String,
    pub tool_calls_str: String,
    pub dedup_threshold: f32,
    pub status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

pub(super) async fn erl_reflection_task(args: ErlTaskArgs) {
    let _clear_status = defer_clear_status(args.status_tx.clone());
    let prompt = zeph_skills::erl::build_reflection_extract_prompt(
        &args.task_summary,
        &args.tool_calls_str,
        "success",
    );
    let messages = vec![Message {
        role: Role::User,
        content: prompt,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let result = match args
        .provider
        .chat_typed_erased::<zeph_skills::erl::ReflectionResult>(&messages)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("ERL: heuristic extraction LLM call failed: {e:#}");
            return;
        }
    };
    for entry in result.heuristics {
        let text = entry.text.trim().to_string();
        if text.is_empty() || text.len() > 512 {
            continue;
        }
        let effective_skill = entry
            .skill_name
            .as_deref()
            .or(Some(args.skill_name.as_str()))
            .filter(|s| !s.is_empty());
        let existing = match args
            .memory
            .sqlite()
            .load_all_heuristics_for_skill(effective_skill)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("ERL: load_all_heuristics_for_skill failed: {e:#}");
                continue;
            }
        };
        let duplicate = existing.iter().find(|(_, existing_text)| {
            zeph_skills::erl::text_similarity(&text, existing_text) >= args.dedup_threshold
        });
        if let Some((id, _)) = duplicate {
            if let Err(e) = args
                .memory
                .sqlite()
                .increment_heuristic_use_count(*id)
                .await
            {
                tracing::warn!("ERL: increment_heuristic_use_count failed: {e:#}");
            }
        } else {
            match args
                .memory
                .sqlite()
                .insert_skill_heuristic(effective_skill, &text, 0.5)
                .await
            {
                Ok(id) => tracing::debug!(id, skill = ?effective_skill, "ERL: stored heuristic"),
                Err(e) => tracing::warn!("ERL: insert_skill_heuristic failed: {e:#}"),
            }
        }
    }
}
