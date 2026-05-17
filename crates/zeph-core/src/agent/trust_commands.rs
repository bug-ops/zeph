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
                    if updated {
                        Ok(format!("Trust level for \"{name}\" set to {level}."))
                    } else {
                        Ok(format!("Skill \"{name}\" not found in trust database."))
                    }
                } else {
                    let row = memory.sqlite().load_skill_trust(name).await?;
                    match row {
                        Some(r) => Ok(format!(
                            "{}: level={}, source={}, hash={}",
                            r.skill_name, r.trust_level, r.source_kind, r.blake3_hash
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
        let Ok(rows) = memory.sqlite().load_all_skill_trust().await else {
            return HashMap::new();
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
