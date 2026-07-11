// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fmt::Write;

use zeph_skills::SkillTrustLevel;

use crate::skill_invoker::SkillTrustSnapshot;

use super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    pub(super) async fn handle_skill_trust_command_as_string(
        &mut self,
        args: &[&str],
    ) -> Result<String, super::error::AgentError> {
        // Clone Arc before .await to avoid holding &self across suspension points.
        let memory = self.services.memory.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };

        match args.first().copied() {
            None => {
                let rows = memory.sqlite().load_all_skill_trust().await?;
                if rows.is_empty() {
                    return Ok("No skill trust data recorded.".to_owned());
                }
                let mut output = String::from("Skill trust levels:\n\n");
                for row in &rows {
                    let _ = writeln!(
                        output,
                        "- {} [{}] (source: {}, hash: {}..)",
                        row.skill_name,
                        row.trust_level,
                        row.source_kind,
                        &row.blake3_hash[..row.blake3_hash.len().min(8)]
                    );
                }
                Ok(output)
            }
            Some(name) => {
                if let Some(level_str) = args.get(1).copied() {
                    let Ok(level) = level_str.parse::<SkillTrustLevel>() else {
                        return Ok(
                            "Invalid trust level. Use: trusted, verified, quarantined, blocked"
                                .to_owned(),
                        );
                    };
                    let updated = memory.sqlite().set_skill_trust_level(name, level).await?;
                    if !updated {
                        return Ok(format!("Skill \"{name}\" not found in trust database."));
                    }
                    let mut output = format!("Trust level for \"{name}\" set to {level}.");
                    // #6080: `--require-check` arms the per-invocation blake3 integrity
                    // re-check (`SkillTrustGate::resolve_body`), previously unreachable from
                    // any production entry point. Scan the whole remaining slice rather than
                    // indexing a fixed position — a security toggle must not silently fail to
                    // arm just because the flag isn't the 3rd token (review finding, #6080).
                    if args[2..].contains(&"--require-check") {
                        memory.sqlite().set_requires_trust_check(name, true).await?;
                        let _ = write!(
                            output,
                            "\nPer-invocation integrity re-check enabled for \"{name}\"."
                        );
                    }
                    Ok(output)
                } else {
                    let row = memory.sqlite().load_skill_trust(name).await?;
                    match row {
                        Some(r) => Ok(format!(
                            "{}: level={}, source={}, hash={}, requires_trust_check={}",
                            r.skill_name,
                            r.trust_level,
                            r.source_kind,
                            r.blake3_hash,
                            r.requires_trust_check
                        )),
                        None => Ok(format!("No trust data for \"{name}\".")),
                    }
                }
            }
        }
    }

    pub(super) async fn handle_skill_block_as_string(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, super::error::AgentError> {
        let Some(name) = name else {
            return Ok("Usage: /skill block <name>".to_owned());
        };
        let memory = self.services.memory.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };
        let updated = memory
            .sqlite()
            .set_skill_trust_level(name, SkillTrustLevel::Blocked)
            .await?;
        if updated {
            Ok(format!("Skill \"{name}\" blocked."))
        } else {
            Ok(format!("Skill \"{name}\" not found in trust database."))
        }
    }

    pub(super) async fn handle_skill_unblock_as_string(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, super::error::AgentError> {
        let Some(name) = name else {
            return Ok("Usage: /skill unblock <name>".to_owned());
        };
        let memory = self.services.memory.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };
        let updated = memory
            .sqlite()
            .set_skill_trust_level(name, SkillTrustLevel::Quarantined)
            .await?;
        if updated {
            Ok(format!("Skill \"{name}\" unblocked (set to quarantined)."))
        } else {
            Ok(format!("Skill \"{name}\" not found in trust database."))
        }
    }

    pub(super) fn handle_skill_scan_as_string(&mut self) -> String {
        // Scope the lock guard so it is dropped before the first await point.
        let findings = {
            let registry = self.services.skill.registry.read();
            registry.scan_loaded()
        };

        if findings.is_empty() {
            "Skill scan complete: no injection patterns detected.".to_owned()
        } else {
            let mut output = format!(
                "Skill scan complete: {} skill(s) with potential injection patterns (advisory):\n\n",
                findings.len()
            );
            for (name, result) in &findings {
                use std::fmt::Write as _;
                let _ = writeln!(
                    output,
                    "- {} ({} pattern(s)): {}",
                    name,
                    result.pattern_count,
                    result.matched_patterns.join(", ")
                );
            }
            output.push_str(
                "\nNote: scan results are advisory. Use `/skill trust` to adjust trust levels.",
            );
            output
        }
    }

    pub(super) async fn build_skill_trust_map(&mut self) -> HashMap<String, SkillTrustSnapshot> {
        // Clone Arc before .await so no &self fields are held across suspension points.
        let memory = self.services.memory.persistence.memory.clone();
        let Some(memory) = memory else {
            return HashMap::new();
        };
        let rows = match memory.sqlite().load_all_skill_trust().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "build_skill_trust_map: load_all_skill_trust failed, skills will render \
                     with no trust-map entry this turn"
                );
                return HashMap::new();
            }
        };
        rows.into_iter()
            .map(|r| {
                (
                    r.skill_name,
                    SkillTrustSnapshot {
                        trust_level: r.trust_level,
                        requires_trust_check: r.requires_trust_check,
                        blake3_hash: r.blake3_hash,
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_memory::semantic::SemanticMemory;
    use zeph_memory::store::SourceKind;

    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    async fn test_memory() -> Arc<SemanticMemory> {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        Arc::new(
            SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                provider,
                "test-model",
            )
            .await
            .unwrap(),
        )
    }

    fn agent_with_memory(memory: Arc<SemanticMemory>) -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory,
            zeph_memory::ConversationId(1),
            50,
            5,
            50,
        )
    }

    #[tokio::test]
    async fn trust_require_check_flag_arms_per_invocation_check() {
        let memory = test_memory().await;
        memory
            .sqlite()
            .upsert_skill_trust(
                "git",
                SkillTrustLevel::Quarantined,
                SourceKind::Local,
                None,
                None,
                "hash1",
            )
            .await
            .unwrap();
        let mut agent = agent_with_memory(memory.clone());

        let out = agent
            .handle_skill_trust_command_as_string(&["git", "trusted", "--require-check"])
            .await
            .unwrap();
        assert!(out.contains("Trust level for \"git\" set to trusted"));
        assert!(out.contains("Per-invocation integrity re-check enabled"));

        let row = memory
            .sqlite()
            .load_skill_trust("git")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.trust_level, SkillTrustLevel::Trusted);
        assert!(
            row.requires_trust_check,
            "--require-check must persist requires_trust_check=true"
        );
    }

    #[tokio::test]
    async fn trust_require_check_flag_arms_even_when_not_the_third_token() {
        // Regression test (review finding): the flag must be found by scanning the whole
        // remaining slice, not by indexing a fixed position — otherwise a security toggle
        // silently fails to arm on any extra/reordered trailing token while still reporting
        // success.
        let memory = test_memory().await;
        memory
            .sqlite()
            .upsert_skill_trust(
                "git",
                SkillTrustLevel::Quarantined,
                SourceKind::Local,
                None,
                None,
                "hash1",
            )
            .await
            .unwrap();
        let mut agent = agent_with_memory(memory.clone());

        let out = agent
            .handle_skill_trust_command_as_string(&["git", "trusted", "extra", "--require-check"])
            .await
            .unwrap();
        assert!(out.contains("Trust level for \"git\" set to trusted"));
        assert!(
            out.contains("Per-invocation integrity re-check enabled"),
            "flag must arm even when it isn't the 3rd token: {out}"
        );

        let row = memory
            .sqlite()
            .load_skill_trust("git")
            .await
            .unwrap()
            .unwrap();
        assert!(row.requires_trust_check);
    }

    #[tokio::test]
    async fn trust_without_flag_leaves_requires_trust_check_false() {
        let memory = test_memory().await;
        memory
            .sqlite()
            .upsert_skill_trust(
                "git",
                SkillTrustLevel::Quarantined,
                SourceKind::Local,
                None,
                None,
                "hash1",
            )
            .await
            .unwrap();
        let mut agent = agent_with_memory(memory.clone());

        let out = agent
            .handle_skill_trust_command_as_string(&["git", "trusted"])
            .await
            .unwrap();
        assert!(out.contains("Trust level for \"git\" set to trusted"));
        assert!(!out.contains("Per-invocation integrity re-check enabled"));

        let row = memory
            .sqlite()
            .load_skill_trust("git")
            .await
            .unwrap()
            .unwrap();
        assert!(!row.requires_trust_check);
    }

    #[tokio::test]
    async fn trust_info_display_includes_requires_trust_check() {
        let memory = test_memory().await;
        memory
            .sqlite()
            .upsert_skill_trust(
                "git",
                SkillTrustLevel::Trusted,
                SourceKind::Local,
                None,
                None,
                "hash1",
            )
            .await
            .unwrap();
        memory
            .sqlite()
            .set_requires_trust_check("git", true)
            .await
            .unwrap();
        let mut agent = agent_with_memory(memory);

        let out = agent
            .handle_skill_trust_command_as_string(&["git"])
            .await
            .unwrap();
        assert!(out.contains("requires_trust_check=true"));
    }
}
