// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fmt::Write;

use zeph_skills::SkillTrustLevel;

use super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    /// Handle `/skill trust [name [level]]`.
    pub(super) async fn handle_skill_trust_command(
        &mut self,
        args: &[&str],
    ) -> Result<(), super::error::AgentError> {
        let Some(memory) = &self.memory_state.persistence.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };

        match args.first().copied() {
            None => {
                // List all trust levels
                let rows = memory.sqlite().load_all_skill_trust().await?;
                if rows.is_empty() {
                    self.channel.send("No skill trust data recorded.").await?;
                    return Ok(());
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
                self.channel.send(&output).await?;
            }
            Some(name) => {
                if let Some(level_str) = args.get(1).copied() {
                    // Set trust level
                    let level = match level_str {
                        "trusted" => SkillTrustLevel::Trusted,
                        "verified" => SkillTrustLevel::Verified,
                        "quarantined" => SkillTrustLevel::Quarantined,
                        "blocked" => SkillTrustLevel::Blocked,
                        _ => {
                            self.channel
                                .send("Invalid trust level. Use: trusted, verified, quarantined, blocked")
                                .await?;
                            return Ok(());
                        }
                    };
                    let updated = memory
                        .sqlite()
                        .set_skill_trust_level(name, &level.to_string())
                        .await?;
                    if updated {
                        self.channel
                            .send(&format!("Trust level for \"{name}\" set to {level}."))
                            .await?;
                    } else {
                        self.channel
                            .send(&format!("Skill \"{name}\" not found in trust database."))
                            .await?;
                    }
                } else {
                    // Show single skill trust
                    let row = memory.sqlite().load_skill_trust(name).await?;
                    match row {
                        Some(r) => {
                            self.channel
                                .send(&format!(
                                    "{}: level={}, source={}, hash={}",
                                    r.skill_name, r.trust_level, r.source_kind, r.blake3_hash
                                ))
                                .await?;
                        }
                        None => {
                            self.channel
                                .send(&format!("No trust data for \"{name}\"."))
                                .await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle `/skill block <name>`.
    pub(super) async fn handle_skill_block(
        &mut self,
        name: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        let Some(name) = name else {
            self.channel.send("Usage: /skill block <name>").await?;
            return Ok(());
        };
        let Some(memory) = &self.memory_state.persistence.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };
        let updated = memory
            .sqlite()
            .set_skill_trust_level(name, "blocked")
            .await?;
        if updated {
            self.channel
                .send(&format!("Skill \"{name}\" blocked."))
                .await?;
        } else {
            self.channel
                .send(&format!("Skill \"{name}\" not found in trust database."))
                .await?;
        }
        Ok(())
    }

    /// Handle `/skill unblock <name>`.
    pub(super) async fn handle_skill_unblock(
        &mut self,
        name: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        let Some(name) = name else {
            self.channel.send("Usage: /skill unblock <name>").await?;
            return Ok(());
        };
        let Some(memory) = &self.memory_state.persistence.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };
        let updated = memory
            .sqlite()
            .set_skill_trust_level(name, "quarantined")
            .await?;
        if updated {
            self.channel
                .send(&format!("Skill \"{name}\" unblocked (set to quarantined)."))
                .await?;
        } else {
            self.channel
                .send(&format!("Skill \"{name}\" not found in trust database."))
                .await?;
        }
        Ok(())
    }

    /// Handle `/skill scan` — scan all loaded skills for injection patterns.
    ///
    /// Results are advisory only. Does not change trust levels or block tool calls.
    pub(super) async fn handle_skill_scan(&mut self) -> Result<(), super::error::AgentError> {
        // Scope the lock guard so it is dropped before the first await point.
        let findings = {
            let registry = self.skill_state.registry.read();
            registry.scan_loaded()
        };

        if findings.is_empty() {
            self.channel
                .send("Skill scan complete: no injection patterns detected.")
                .await?;
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
            self.channel.send(&output).await?;
        }

        Ok(())
    }

    pub(super) async fn build_skill_trust_map(&self) -> HashMap<String, SkillTrustLevel> {
        let Some(memory) = &self.memory_state.persistence.memory else {
            return HashMap::new();
        };
        let Ok(rows) = memory.sqlite().load_all_skill_trust().await else {
            return HashMap::new();
        };
        rows.into_iter()
            .filter_map(|r| {
                let level = match r.trust_level.as_str() {
                    "trusted" => SkillTrustLevel::Trusted,
                    "verified" => SkillTrustLevel::Verified,
                    "quarantined" => SkillTrustLevel::Quarantined,
                    "blocked" => SkillTrustLevel::Blocked,
                    _ => return None,
                };
                Some((r.skill_name, level))
            })
            .collect()
    }
}
