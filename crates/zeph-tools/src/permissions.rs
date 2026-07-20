// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};

use glob::Pattern;

pub(crate) use zeph_config::tools::{
    AutonomyLevel, PermissionAction, PermissionRule, PermissionsConfig,
};

/// Read-only tool allowlist and its `is_readonly_tool` predicate.
///
/// Canonical definition lives in `zeph_common::tool_classification` — shared with
/// `zeph-orchestration`, which uses it to classify tool calls in a task's real execution
/// trace as read vs. write-type (#6397). Re-exported here under the historical names so
/// existing call sites in this crate (`permissions::READONLY_TOOLS`,
/// `permissions::is_readonly_tool`) keep working unchanged.
pub(crate) use zeph_common::tool_classification::{READONLY_TOOLS, is_readonly_tool};

/// Tool permission policy: maps `tool_id` → ordered list of rules.
/// First matching rule wins; default is `Ask`.
///
/// Runtime enforcement is currently implemented for `bash` (`ShellExecutor`).
/// Other tools rely on prompt filtering via `ToolRegistry::format_for_prompt_filtered`.
#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    rules: HashMap<String, Vec<PermissionRule>>,
    autonomy_level: AutonomyLevel,
}

impl PermissionPolicy {
    #[must_use]
    pub fn new(rules: HashMap<String, Vec<PermissionRule>>) -> Self {
        Self {
            rules,
            autonomy_level: AutonomyLevel::default(),
        }
    }

    /// Set autonomy level (builder pattern).
    #[must_use]
    pub fn with_autonomy(mut self, level: AutonomyLevel) -> Self {
        self.autonomy_level = level;
        self
    }

    /// Check permission for a tool invocation. First matching glob wins.
    #[must_use]
    pub fn check(&self, tool_id: &str, input: &str) -> PermissionAction {
        match self.autonomy_level {
            AutonomyLevel::ReadOnly => {
                if READONLY_TOOLS.contains(&tool_id) {
                    PermissionAction::Allow
                } else {
                    PermissionAction::Deny
                }
            }
            AutonomyLevel::Full => PermissionAction::Allow,
            AutonomyLevel::Supervised => {
                let Some(rules) = self.rules.get(tool_id) else {
                    return PermissionAction::Ask;
                };
                let normalized = input.to_lowercase();
                for rule in rules {
                    if let Ok(pat) = Pattern::new(&rule.pattern.to_lowercase())
                        && pat.matches(&normalized)
                    {
                        return rule.action;
                    }
                }
                PermissionAction::Ask
            }
            _ => PermissionAction::Deny,
        }
    }

    /// Build policy from legacy `blocked_commands` / `confirm_patterns` for "bash" tool.
    #[must_use]
    pub fn from_legacy(blocked: &[String], confirm: &[String]) -> Self {
        let mut rules = Vec::with_capacity(blocked.len() + confirm.len());
        for cmd in blocked {
            rules.push(PermissionRule {
                pattern: format!("*{cmd}*"),
                action: PermissionAction::Deny,
            });
        }
        for pat in confirm {
            rules.push(PermissionRule {
                pattern: format!("*{pat}*"),
                action: PermissionAction::Ask,
            });
        }
        // Allow everything not explicitly blocked or requiring confirmation.
        rules.push(PermissionRule {
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        });
        let mut map = HashMap::new();
        map.insert("bash".to_owned(), rules);
        Self {
            rules: map,
            autonomy_level: AutonomyLevel::default(),
        }
    }

    /// Returns true if all rules for a `tool_id` are Deny.
    #[must_use]
    pub fn is_fully_denied(&self, tool_id: &str) -> bool {
        self.rules.get(tool_id).is_some_and(|rules| {
            !rules.is_empty() && rules.iter().all(|r| r.action == PermissionAction::Deny)
        })
    }

    /// Returns a reference to the internal rules map.
    #[must_use]
    pub fn rules(&self) -> &HashMap<String, Vec<PermissionRule>> {
        &self.rules
    }

    /// Returns the configured autonomy level.
    #[must_use]
    pub fn autonomy_level(&self) -> AutonomyLevel {
        self.autonomy_level
    }

    /// Derive the subset of `universe` this policy does not wholesale-deny.
    ///
    /// Intended as a defense-in-depth / tool-visibility narrowing signal for sub-agent
    /// spawns (#6527): a session whose own `[tool.permissions]` rules wholesale-deny a
    /// tool (a catch-all `Deny` not preceded by an `Allow`/`Ask` rule for that tool)
    /// should not hand that tool's definition to a spawned sub-agent either. This is
    /// **not** the runtime security boundary — every sub-agent tool call already passes
    /// through the parent's own `TrustGate`-wrapped executor transitively, so a
    /// wholesale-denied tool is already blocked for the child at call time regardless of
    /// this method's output. This method only controls what the child's LLM *sees* in
    /// its tool catalog, avoiding wasted turns on tools that would be denied anyway.
    ///
    /// Returns `None` when no narrowing is warranted:
    /// - `autonomy_level` is not [`AutonomyLevel::Supervised`] (`Full` ignores rules
    ///   entirely; `ReadOnly` also ignores the rules map — see below).
    /// - No tool in `universe` is actually wholesale-denied.
    ///
    /// Returning `Some(universe)` when nothing is denied would be a behavior change for
    /// callers that replace [`ToolPolicy::InheritAll`](https://docs.rs/zeph-subagent) with
    /// the returned set: an unrestricted child's tool list would be frozen at spawn time,
    /// making dynamically-added MCP tools invisible. So `None` is returned unless at
    /// least one tool is genuinely narrowed.
    ///
    /// `ReadOnly` asymmetry: under `ReadOnly`, [`check`][Self::check] ignores the rules
    /// map entirely (only [`READONLY_TOOLS`] matters), so there is nothing in the rules to
    /// propagate here — this method returns `None`, leaving the child's tool *list*
    /// un-narrowed. This is inconsistent with the `Supervised` path (which does narrow the
    /// list), but not a security gap: the parent's `TrustGate` still denies non-read-only
    /// tool calls for the child at runtime, exactly as it does for the parent itself.
    ///
    /// A tool is "wholesale-denied" when the first rule that would match ANY input for
    /// that tool is a catch-all `Deny` (pattern `""`, `"*"`, or `"**"`), mirroring
    /// [`check`][Self::check]'s first-match-wins semantics. This deliberately treats `*`
    /// as catch-all even though `glob::Pattern`'s `*` does not cross `/` at the
    /// enforcement layer (so a `("*", Deny)` rule technically still lets a `/`-bearing
    /// input like `rm -rf /home` fall through to `Ask` in `check`) — operator intent
    /// ("deny this tool") wins over that glob quirk for allowlist derivation. This can
    /// over-restrict the child's visible tool list relative to what the parent could
    /// still do for `/`-bearing inputs, which is acceptable for a hygiene-only signal.
    ///
    /// Rule-map keys are matched case-insensitively against `normalize_tool_id`-style
    /// bare tool ids (lowercased, stripped of any `(...)` argument suffix) so a
    /// differently-cased or parenthesized config key like `"Bash(cargo *)"` still matches
    /// the runtime tool id `"bash"` in `universe`.
    #[must_use]
    pub fn effective_tool_allowlist(
        &self,
        universe: impl IntoIterator<Item = String>,
    ) -> Option<HashSet<String>> {
        if self.autonomy_level != AutonomyLevel::Supervised {
            return None;
        }

        let mut normalized_rules: HashMap<String, &Vec<PermissionRule>> = HashMap::new();
        for (tool_id, rules) in &self.rules {
            normalized_rules.insert(normalize_tool_id(tool_id), rules);
        }

        let universe: HashSet<String> = universe
            .into_iter()
            .map(|t| normalize_tool_id(&t))
            .collect();
        let kept: HashSet<String> = universe
            .iter()
            .filter(|tool| {
                !normalized_rules
                    .get(tool.as_str())
                    .is_some_and(|rules| is_wholesale_denied(rules))
            })
            .cloned()
            .collect();

        if kept == universe { None } else { Some(kept) }
    }
}

/// Lowercase and strip any `(...)` argument suffix, mirroring
/// `zeph_subagent::filter::normalize_tool_id` without introducing a `zeph-tools` →
/// `zeph-subagent` dependency (the reverse dependency already exists).
fn normalize_tool_id(s: &str) -> String {
    let base = s.split('(').next().unwrap_or(s);
    base.trim().to_lowercase()
}

/// Returns `true` if the first rule that would match any input for this tool is a
/// catch-all [`PermissionAction::Deny`] — i.e. no input can reach `Allow` or `Ask`.
/// Mirrors [`PermissionPolicy::check`]'s first-match-wins glob evaluation without
/// depending on a specific probe input.
fn is_wholesale_denied(rules: &[PermissionRule]) -> bool {
    for rule in rules {
        match rule.action {
            PermissionAction::Deny if is_catch_all(&rule.pattern) => return true,
            PermissionAction::Deny => {}
            _ => return false,
        }
    }
    false
}

/// A pattern that matches every input, regardless of `glob::Pattern`'s `/`-crossing
/// quirk — see [`PermissionPolicy::effective_tool_allowlist`] doc comment for why `*` is
/// treated as catch-all here despite not being one at the `check()` enforcement layer.
fn is_catch_all(pattern: &str) -> bool {
    matches!(pattern.trim(), "" | "*" | "**")
}

impl From<PermissionsConfig> for PermissionPolicy {
    fn from(config: PermissionsConfig) -> Self {
        Self {
            rules: config.tools,
            autonomy_level: AutonomyLevel::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with_rules(tool_id: &str, rules: Vec<(&str, PermissionAction)>) -> PermissionPolicy {
        let rules = rules
            .into_iter()
            .map(|(pattern, action)| PermissionRule {
                pattern: pattern.to_owned(),
                action,
            })
            .collect();
        let mut map = HashMap::new();
        map.insert(tool_id.to_owned(), rules);
        PermissionPolicy::new(map)
    }

    #[test]
    fn allow_rule_matches_glob() {
        let policy = policy_with_rules("bash", vec![("echo *", PermissionAction::Allow)]);
        assert_eq!(policy.check("bash", "echo hello"), PermissionAction::Allow);
    }

    #[test]
    fn deny_rule_blocks() {
        let policy = policy_with_rules("bash", vec![("*rm -rf*", PermissionAction::Deny)]);
        assert_eq!(policy.check("bash", "rm -rf /tmp"), PermissionAction::Deny);
    }

    #[test]
    fn ask_rule_returns_ask() {
        let policy = policy_with_rules("bash", vec![("*git push*", PermissionAction::Ask)]);
        assert_eq!(
            policy.check("bash", "git push origin main"),
            PermissionAction::Ask
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let policy = policy_with_rules(
            "bash",
            vec![
                ("*safe*", PermissionAction::Allow),
                ("*", PermissionAction::Deny),
            ],
        );
        assert_eq!(
            policy.check("bash", "safe command"),
            PermissionAction::Allow
        );
        assert_eq!(
            policy.check("bash", "dangerous command"),
            PermissionAction::Deny
        );
    }

    #[test]
    fn no_rules_returns_default_ask() {
        let policy = PermissionPolicy::default();
        assert_eq!(policy.check("bash", "anything"), PermissionAction::Ask);
    }

    #[test]
    fn wildcard_pattern() {
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Allow)]);
        assert_eq!(policy.check("bash", "any command"), PermissionAction::Allow);
    }

    #[test]
    fn case_sensitive_tool_id() {
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Deny)]);
        assert_eq!(policy.check("BASH", "cmd"), PermissionAction::Ask);
        assert_eq!(policy.check("bash", "cmd"), PermissionAction::Deny);
    }

    #[test]
    fn no_matching_rule_falls_through_to_ask() {
        let policy = policy_with_rules("bash", vec![("echo *", PermissionAction::Allow)]);
        assert_eq!(policy.check("bash", "ls -la"), PermissionAction::Ask);
    }

    #[test]
    fn from_legacy_creates_deny_and_ask_rules() {
        let policy = PermissionPolicy::from_legacy(&["sudo".to_owned()], &["rm ".to_owned()]);
        assert_eq!(policy.check("bash", "sudo apt"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "rm file"), PermissionAction::Ask);
        assert_eq!(
            policy.check("bash", "find . -name foo"),
            PermissionAction::Allow
        );
        assert_eq!(policy.check("bash", "ls -la"), PermissionAction::Allow);
    }

    #[test]
    fn is_fully_denied_all_deny() {
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Deny)]);
        assert!(policy.is_fully_denied("bash"));
    }

    #[test]
    fn is_fully_denied_mixed() {
        let policy = policy_with_rules(
            "bash",
            vec![
                ("echo *", PermissionAction::Allow),
                ("*", PermissionAction::Deny),
            ],
        );
        assert!(!policy.is_fully_denied("bash"));
    }

    #[test]
    fn is_fully_denied_no_rules() {
        let policy = PermissionPolicy::default();
        assert!(!policy.is_fully_denied("bash"));
    }

    #[test]
    fn case_insensitive_input_matching() {
        let policy = policy_with_rules("bash", vec![("*sudo*", PermissionAction::Deny)]);
        assert_eq!(policy.check("bash", "SUDO apt"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "Sudo apt"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "sudo apt"), PermissionAction::Deny);
    }

    #[test]
    fn permissions_config_deserialize() {
        let toml_str = r#"
            [[bash]]
            pattern = "*sudo*"
            action = "deny"

            [[bash]]
            pattern = "*"
            action = "ask"
        "#;
        let config: PermissionsConfig = toml::from_str(toml_str).unwrap();
        let policy = PermissionPolicy::from(config);
        assert_eq!(policy.check("bash", "sudo rm"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "echo hi"), PermissionAction::Ask);
    }

    #[test]
    fn autonomy_level_deserialize() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Wrapper {
            level: AutonomyLevel,
        }
        let w: Wrapper = toml::from_str(r#"level = "readonly""#).unwrap();
        assert_eq!(w.level, AutonomyLevel::ReadOnly);
        let w: Wrapper = toml::from_str(r#"level = "supervised""#).unwrap();
        assert_eq!(w.level, AutonomyLevel::Supervised);
        let w: Wrapper = toml::from_str(r#"level = "full""#).unwrap();
        assert_eq!(w.level, AutonomyLevel::Full);
    }

    #[test]
    fn autonomy_level_default_is_supervised() {
        assert_eq!(AutonomyLevel::default(), AutonomyLevel::Supervised);
    }

    #[test]
    fn is_readonly_tool_matches_allowlist() {
        for tool in READONLY_TOOLS {
            assert!(is_readonly_tool(tool), "{tool} should be a readonly tool");
        }
        assert!(!is_readonly_tool("bash"));
        assert!(!is_readonly_tool("diagnostics"));
        assert!(!is_readonly_tool("write"));
    }

    #[test]
    fn readonly_allows_readonly_tools() {
        let policy = PermissionPolicy::default().with_autonomy(AutonomyLevel::ReadOnly);
        for tool in &[
            "read",
            "find_path",
            "grep",
            "list_directory",
            "web_scrape",
            "fetch",
        ] {
            assert_eq!(
                policy.check(tool, "any input"),
                PermissionAction::Allow,
                "expected Allow for read-only tool {tool}"
            );
        }
    }

    #[test]
    fn readonly_denies_write_tools() {
        let policy = PermissionPolicy::default().with_autonomy(AutonomyLevel::ReadOnly);
        assert_eq!(policy.check("bash", "rm -rf /"), PermissionAction::Deny);
        assert_eq!(
            policy.check("file_write", "foo.txt"),
            PermissionAction::Deny
        );
    }

    #[test]
    fn full_allows_everything() {
        let policy = PermissionPolicy::default().with_autonomy(AutonomyLevel::Full);
        assert_eq!(policy.check("bash", "rm -rf /"), PermissionAction::Allow);
        assert_eq!(
            policy.check("file_write", "foo.txt"),
            PermissionAction::Allow
        );
    }

    #[test]
    fn supervised_uses_rules() {
        let policy = policy_with_rules("bash", vec![("*sudo*", PermissionAction::Deny)])
            .with_autonomy(AutonomyLevel::Supervised);
        assert_eq!(policy.check("bash", "sudo rm"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "echo hi"), PermissionAction::Ask);
    }

    #[test]
    fn from_legacy_preserves_supervised_behavior() {
        let policy = PermissionPolicy::from_legacy(&["sudo".to_owned()], &["rm ".to_owned()]);
        assert_eq!(policy.check("bash", "sudo apt"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "rm file"), PermissionAction::Ask);
        assert_eq!(policy.check("bash", "echo hello"), PermissionAction::Allow);
    }

    // ── effective_tool_allowlist tests (#6527) ─────────────────────────────

    fn universe(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn effective_allowlist_empty_rules_returns_none() {
        let policy = PermissionPolicy::default().with_autonomy(AutonomyLevel::Supervised);
        assert_eq!(
            policy.effective_tool_allowlist(universe(&["bash", "read"])),
            None,
            "no rules at all -> no narrowing needed"
        );
    }

    #[test]
    fn effective_allowlist_star_catch_all_deny_removes_tool() {
        // Deliberate divergence from glob::Pattern's `*`-does-not-cross-`/` semantics
        // (M2, critic): operator intent for "deny this tool" wins over literal glob
        // matching. Do not "fix" this back to exact glob semantics.
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Deny)])
            .with_autonomy(AutonomyLevel::Supervised);
        let result = policy
            .effective_tool_allowlist(universe(&["bash", "read"]))
            .expect("bash must be wholesale-denied");
        assert!(!result.contains("bash"));
        assert!(result.contains("read"));
    }

    #[test]
    fn effective_allowlist_double_star_and_empty_pattern_are_catch_all() {
        for pattern in ["**", ""] {
            let policy = policy_with_rules("bash", vec![(pattern, PermissionAction::Deny)])
                .with_autonomy(AutonomyLevel::Supervised);
            let result = policy
                .effective_tool_allowlist(universe(&["bash", "read"]))
                .unwrap_or_else(|| panic!("pattern {pattern:?} must be treated as catch-all"));
            assert!(
                !result.contains("bash"),
                "pattern {pattern:?} must deny bash"
            );
        }
    }

    #[test]
    fn effective_allowlist_narrower_deny_only_keeps_tool() {
        // A single narrower Deny (not catch-all) must not wholesale-deny the tool —
        // reusing is_fully_denied here would over-restrict (architect §2, rejected
        // alternative).
        let policy = policy_with_rules("bash", vec![("*rm -rf*", PermissionAction::Deny)])
            .with_autonomy(AutonomyLevel::Supervised);
        assert_eq!(
            policy.effective_tool_allowlist(universe(&["bash", "read"])),
            None,
            "narrower deny alone must not wholesale-deny bash"
        );
    }

    #[test]
    fn effective_allowlist_allow_before_catch_all_deny_keeps_tool() {
        let policy = policy_with_rules(
            "bash",
            vec![
                ("echo *", PermissionAction::Allow),
                ("*", PermissionAction::Deny),
            ],
        )
        .with_autonomy(AutonomyLevel::Supervised);
        assert_eq!(
            policy.effective_tool_allowlist(universe(&["bash", "read"])),
            None,
            "an earlier Allow rule means the tool is not wholesale-denied"
        );
    }

    #[test]
    fn effective_allowlist_ask_before_catch_all_deny_keeps_tool() {
        let policy = policy_with_rules(
            "bash",
            vec![
                ("*sudo*", PermissionAction::Ask),
                ("*", PermissionAction::Deny),
            ],
        )
        .with_autonomy(AutonomyLevel::Supervised);
        assert_eq!(
            policy.effective_tool_allowlist(universe(&["bash", "read"])),
            None,
            "an earlier Ask rule means the tool is not wholesale-denied"
        );
    }

    #[test]
    fn effective_allowlist_full_autonomy_returns_none() {
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Deny)])
            .with_autonomy(AutonomyLevel::Full);
        assert_eq!(
            policy.effective_tool_allowlist(universe(&["bash", "read"])),
            None,
            "Full autonomy ignores rules entirely"
        );
    }

    #[test]
    fn effective_allowlist_readonly_autonomy_returns_none() {
        // M1 (critic, decision made): ReadOnly's check() ignores the rules map entirely,
        // so there is nothing to propagate — this is a documented asymmetry vs.
        // Supervised, not a security gap (TrustGate backstops the child at runtime).
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Deny)])
            .with_autonomy(AutonomyLevel::ReadOnly);
        assert_eq!(
            policy.effective_tool_allowlist(universe(&["bash", "read"])),
            None
        );
    }

    #[test]
    fn effective_allowlist_no_wholesale_deny_returns_none_not_full_universe() {
        // §2a (architect, critic-confirmed): returning Some(universe) when nothing is
        // denied would freeze InheritAll children's tool list at spawn time, hiding
        // dynamically-added MCP tools. Must return None instead.
        let policy = policy_with_rules("bash", vec![("*sudo*", PermissionAction::Deny)])
            .with_autonomy(AutonomyLevel::Supervised);
        assert_eq!(
            policy.effective_tool_allowlist(universe(&["bash", "read", "write"])),
            None
        );
    }

    #[test]
    fn effective_allowlist_normalizes_mixed_case_parenthesized_rule_key() {
        // M3 (critic, must fix): rules() is keyed by raw config tool_ids; the universe is
        // normalized. A rule key like "Bash(cargo *)" must still match the runtime id
        // "bash", or the deny is silently missed (under-restriction).
        let mut map = HashMap::new();
        map.insert(
            "Bash(cargo *)".to_owned(),
            vec![PermissionRule {
                pattern: "*".to_owned(),
                action: PermissionAction::Deny,
            }],
        );
        let policy = PermissionPolicy::new(map).with_autonomy(AutonomyLevel::Supervised);
        let result = policy
            .effective_tool_allowlist(universe(&["bash", "read"]))
            .expect("mixed-case parenthesized rule key must still match normalized 'bash'");
        assert!(!result.contains("bash"));
        assert!(result.contains("read"));
    }

    #[test]
    fn effective_allowlist_multiple_tools_mixed_deny() {
        let mut map = HashMap::new();
        map.insert(
            "bash".to_owned(),
            vec![PermissionRule {
                pattern: "*".to_owned(),
                action: PermissionAction::Deny,
            }],
        );
        map.insert(
            "fetch".to_owned(),
            vec![PermissionRule {
                pattern: "*".to_owned(),
                action: PermissionAction::Deny,
            }],
        );
        let policy = PermissionPolicy::new(map).with_autonomy(AutonomyLevel::Supervised);
        let result = policy
            .effective_tool_allowlist(universe(&["bash", "fetch", "read"]))
            .expect("at least one wholesale-denied tool must narrow the set");
        assert_eq!(result.len(), 1);
        assert!(result.contains("read"));
    }
}
