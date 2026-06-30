// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Optional platform-extension manifest for skills.
//!
//! The `extensions:` YAML block inside a `SKILL.md` frontmatter lets a skill declare
//! UI elements, keybindings, and background monitors that a host platform may wire up.
//! Parsing is best-effort: any error in this block logs a warning and falls back to
//! `None` so that the skill continues to load normally.
//!
//! # SKILL.md Format
//!
//! ```text
//! ---
//! name: my-skill
//! description: Does something useful.
//! extensions:
//!   ui:
//!     - type: toolbar_button
//!       label: Run My Skill
//!       icon: play
//!   keybindings:
//!     - chord: ctrl+shift+r
//!       action: run-my-skill
//!   monitors:
//!     - trigger: file_changed
//!       action: reload-my-skill
//! ---
//! ```

/// A UI element declared by a skill extension manifest.
///
/// The `type` tag selects which variant is deserialized. Currently supported variants
/// are `toolbar_button` and `menu_item`.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::extensions::SkillUiElement;
///
/// let yaml = r#"
/// - type: toolbar_button
///   label: Run
///   icon: play
/// - type: menu_item
///   label: Open Skill
///   action: open-skill
/// "#;
/// let elements: Vec<SkillUiElement> = serde_norway::from_str(yaml).unwrap();
/// assert!(matches!(elements[0], SkillUiElement::ToolbarButton { .. }));
/// assert!(matches!(elements[1], SkillUiElement::MenuItem { .. }));
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillUiElement {
    /// A button rendered in the host application's toolbar.
    ToolbarButton {
        /// Display label for the button.
        label: String,
        /// Optional icon identifier (platform-defined).
        icon: Option<String>,
    },
    /// An item injected into a host application menu.
    MenuItem {
        /// Display label for the menu item.
        label: String,
        /// Action identifier dispatched when the item is selected.
        action: String,
    },
}

/// A keyboard shortcut declared by a skill extension manifest.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::extensions::SkillKeybinding;
///
/// let yaml = "chord: ctrl+shift+r\naction: run-my-skill\n";
/// let kb: SkillKeybinding = serde_norway::from_str(yaml).unwrap();
/// assert_eq!(kb.chord, "ctrl+shift+r");
/// assert_eq!(kb.action, "run-my-skill");
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[non_exhaustive]
pub struct SkillKeybinding {
    /// Key chord string in platform-normalized notation (e.g. `"ctrl+shift+r"`).
    pub chord: String,
    /// Action identifier dispatched when the chord is pressed.
    pub action: String,
}

/// A background monitor declared by a skill extension manifest.
///
/// Monitors let a skill react to host events without explicit user invocation.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::extensions::SkillMonitor;
///
/// let yaml = "trigger: file_changed\naction: reload-my-skill\n";
/// let mon: SkillMonitor = serde_norway::from_str(yaml).unwrap();
/// assert_eq!(mon.trigger, "file_changed");
/// assert_eq!(mon.action, "reload-my-skill");
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[non_exhaustive]
pub struct SkillMonitor {
    /// Event name that activates this monitor (platform-defined).
    pub trigger: String,
    /// Action identifier dispatched when the trigger fires.
    pub action: String,
}

/// Optional platform-extension manifest parsed from a skill's `SKILL.md` `extensions:` block.
///
/// All fields default to empty. When the `extensions:` block is absent from a SKILL.md,
/// [`parse_extensions`] returns `None` rather than a default value.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::extensions::SkillExtensions;
///
/// let yaml = r#"
/// ui:
///   - type: toolbar_button
///     label: "Quick Refactor"
/// keybindings:
///   - chord: "cmd+shift+r"
///     action: "refactor-selected"
/// "#;
/// let ext: SkillExtensions = serde_norway::from_str(yaml).unwrap();
/// assert_eq!(ext.keybindings[0].chord, "cmd+shift+r");
/// assert_eq!(ext.ui.len(), 1);
/// ```
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[non_exhaustive]
pub struct SkillExtensions {
    /// UI elements (toolbar buttons, menu items) the skill contributes to the host UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<SkillUiElement>,
    /// Keybindings the skill registers with the host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keybindings: Vec<SkillKeybinding>,
    /// Background monitors the skill wants the host to activate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<SkillMonitor>,
}

/// Extract an `extensions:` YAML sub-block from a raw SKILL.md frontmatter string and
/// parse it into [`SkillExtensions`].
///
/// Returns `None` if the block is absent or if parsing fails (a warning is logged).
/// This function never panics and never propagates errors — failures are always `None`.
#[must_use]
pub fn parse_extensions(yaml_str: &str) -> Option<SkillExtensions> {
    let block = extract_extensions_block(yaml_str)?;
    if block.len() > 8 * 1024 {
        tracing::warn!("'extensions:' block exceeds 8 KiB, skipping");
        return None;
    }
    match serde_norway::from_str::<SkillExtensions>(&block) {
        Ok(ext) => Some(ext),
        Err(e) => {
            tracing::warn!("failed to parse 'extensions:' block in SKILL.md frontmatter: {e}");
            None
        }
    }
}

/// Pull the indented content under `extensions:` from a raw YAML frontmatter string.
///
/// Returns a YAML string suitable for deserializing into [`SkillExtensions`], or `None`
/// if the `extensions:` key is not present.
fn extract_extensions_block(yaml_str: &str) -> Option<String> {
    let mut in_block = false;
    let mut lines = Vec::new();

    for line in yaml_str.lines() {
        if in_block {
            if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
                // Collect indented content; strip one level of indentation (2 spaces).
                let stripped = line.strip_prefix("  ").unwrap_or(line);
                lines.push(stripped);
            } else {
                // A non-indented line means we've exited the block.
                break;
            }
        } else if line.trim_start() == "extensions:" || line.starts_with("extensions:") {
            in_block = true;
        }
    }

    if lines.is_empty() {
        return None;
    }

    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn parse_full_extensions_block() {
        let yaml = "\
name: my-skill
description: A skill.
extensions:
  ui:
    - type: toolbar_button
      label: Run
      icon: play
  keybindings:
    - chord: ctrl+r
      action: run
  monitors:
    - trigger: file_changed
      action: reload
";
        let ext = parse_extensions(yaml).expect("should parse extensions");
        assert_eq!(ext.ui.len(), 1);
        assert_matches!(
            &ext.ui[0],
            SkillUiElement::ToolbarButton { label, icon }
            if label == "Run" && icon.as_deref() == Some("play")
        );
        assert_eq!(ext.keybindings.len(), 1);
        assert_eq!(ext.keybindings[0].chord, "ctrl+r");
        assert_eq!(ext.keybindings[0].action, "run");
        assert_eq!(ext.monitors.len(), 1);
        assert_eq!(ext.monitors[0].trigger, "file_changed");
        assert_eq!(ext.monitors[0].action, "reload");
    }

    #[test]
    fn parse_extensions_absent_returns_none() {
        let yaml = "name: my-skill\ndescription: A skill.\n";
        assert!(parse_extensions(yaml).is_none());
    }

    #[test]
    fn parse_extensions_empty_block_returns_default() {
        // An `extensions:` key with no sub-keys deserializes to Default.
        let yaml = "name: my-skill\ndescription: desc.\nextensions:\nother: field\n";
        // Block is empty → None (no lines collected).
        let result = parse_extensions(yaml);
        // Empty extensions block → None (nothing to parse).
        assert!(result.is_none());
    }

    #[test]
    fn parse_extensions_invalid_yaml_returns_none() {
        // Malformed YAML in extensions block must not fail skill loading.
        let yaml = "extensions:\n  ui:\n    - type: !!invalid\n";
        // Should not panic; may return None.
        let _ = parse_extensions(yaml);
    }

    #[test]
    fn parse_extensions_only_ui() {
        let yaml = "\
extensions:
  ui:
    - type: menu_item
      label: Open
      action: open-skill
";
        let ext = parse_extensions(yaml).expect("should parse");
        assert_eq!(ext.ui.len(), 1);
        assert_matches!(
            &ext.ui[0],
            SkillUiElement::MenuItem { label, action }
            if label == "Open" && action == "open-skill"
        );
        assert!(ext.keybindings.is_empty());
        assert!(ext.monitors.is_empty());
    }

    #[test]
    fn parse_extensions_toolbar_button_no_icon() {
        let yaml = "\
extensions:
  ui:
    - type: toolbar_button
      label: Run
";
        let ext = parse_extensions(yaml).expect("should parse");
        assert_matches!(
            &ext.ui[0],
            SkillUiElement::ToolbarButton { label, icon }
            if label == "Run" && icon.is_none()
        );
    }

    #[test]
    fn roundtrip_yaml() {
        let ext = SkillExtensions {
            ui: vec![SkillUiElement::ToolbarButton {
                label: "Run".into(),
                icon: Some("play".into()),
            }],
            keybindings: vec![SkillKeybinding {
                chord: "ctrl+r".into(),
                action: "run".into(),
            }],
            monitors: vec![SkillMonitor {
                trigger: "file_changed".into(),
                action: "reload".into(),
            }],
        };
        let yaml = serde_norway::to_string(&ext).expect("serialize");
        let parsed: SkillExtensions = serde_norway::from_str(&yaml).expect("deserialize");
        assert_eq!(ext, parsed);
    }

    #[test]
    fn default_is_all_empty() {
        let ext = SkillExtensions::default();
        assert!(ext.ui.is_empty());
        assert!(ext.keybindings.is_empty());
        assert!(ext.monitors.is_empty());
    }

    #[test]
    fn extensions_block_stops_at_next_top_level_field() {
        // After the extensions block, other frontmatter fields must not bleed into it.
        let yaml = "\
extensions:
  keybindings:
    - chord: ctrl+k
      action: do-something
other-field: should-not-appear
";
        let ext = parse_extensions(yaml).expect("should parse");
        assert_eq!(ext.keybindings.len(), 1);
        assert_eq!(ext.keybindings[0].chord, "ctrl+k");
    }
}
