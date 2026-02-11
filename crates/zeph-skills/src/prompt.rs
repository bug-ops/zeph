use std::fmt::Write;

use crate::loader::Skill;
use crate::resource::discover_resources;

const OS_NAMES: &[&str] = &["linux", "macos", "windows"];

fn should_include_reference(filename: &str, os_family: &str) -> bool {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    if OS_NAMES.contains(&stem) {
        stem == os_family
    } else {
        true
    }
}

#[must_use]
pub fn format_skills_prompt(skills: &[Skill], os_family: &str) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("<available_skills>\n");

    for skill in skills {
        let _ = write!(
            out,
            "  <skill name=\"{}\">\n    <description>{}</description>\n    <instructions>\n{}",
            skill.name(),
            skill.description(),
            skill.body,
        );

        let resources = discover_resources(&skill.meta.skill_dir);
        for ref_path in &resources.references {
            let Some(filename) = ref_path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            if !should_include_reference(filename, os_family) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(ref_path) {
                let _ = write!(
                    out,
                    "\n<reference name=\"{filename}\">\n{content}\n</reference>",
                );
            }
        }

        out.push_str("\n    </instructions>\n  </skill>\n");
    }

    out.push_str("</available_skills>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::loader::SkillMeta;

    fn make_skill(name: &str, description: &str, body: &str) -> Skill {
        Skill {
            meta: SkillMeta {
                name: name.into(),
                description: description.into(),
                compatibility: None,
                license: None,
                metadata: Vec::new(),
                allowed_tools: Vec::new(),
                skill_dir: PathBuf::new(),
            },
            body: body.into(),
        }
    }

    fn make_skill_with_dir(name: &str, description: &str, body: &str, dir: PathBuf) -> Skill {
        Skill {
            meta: SkillMeta {
                name: name.into(),
                description: description.into(),
                compatibility: None,
                license: None,
                metadata: Vec::new(),
                allowed_tools: Vec::new(),
                skill_dir: dir,
            },
            body: body.into(),
        }
    }

    #[test]
    fn empty_skills_returns_empty_string() {
        let empty: &[Skill] = &[];
        assert_eq!(format_skills_prompt(empty, "linux"), "");
    }

    #[test]
    fn single_skill_format() {
        let skills = vec![make_skill("test", "A test.", "# Hello\nworld")];

        let output = format_skills_prompt(&skills, "linux");
        assert!(output.starts_with("<available_skills>"));
        assert!(output.ends_with("</available_skills>"));
        assert!(output.contains("<skill name=\"test\">"));
        assert!(output.contains("<description>A test.</description>"));
        assert!(output.contains("# Hello\nworld"));
    }

    #[test]
    fn multiple_skills() {
        let skills = vec![
            make_skill("a", "desc a", "body a"),
            make_skill("b", "desc b", "body b"),
        ];

        let output = format_skills_prompt(&skills, "linux");
        assert!(output.contains("<skill name=\"a\">"));
        assert!(output.contains("<skill name=\"b\">"));
    }

    #[test]
    fn should_include_os_matching_reference() {
        assert!(should_include_reference("linux.md", "linux"));
        assert!(!should_include_reference("linux.md", "macos"));
        assert!(!should_include_reference("linux.md", "windows"));

        assert!(should_include_reference("macos.md", "macos"));
        assert!(!should_include_reference("macos.md", "linux"));

        assert!(should_include_reference("windows.md", "windows"));
        assert!(!should_include_reference("windows.md", "linux"));
    }

    #[test]
    fn should_include_generic_reference() {
        assert!(should_include_reference("api.md", "linux"));
        assert!(should_include_reference("api.md", "macos"));
        assert!(should_include_reference("usage.md", "windows"));
    }

    #[test]
    fn references_injected_for_matching_os() {
        let dir = tempfile::tempdir().unwrap();
        let refs = dir.path().join("references");
        std::fs::create_dir(&refs).unwrap();
        std::fs::write(refs.join("linux.md"), "# Linux commands").unwrap();
        std::fs::write(refs.join("macos.md"), "# macOS commands").unwrap();
        std::fs::write(refs.join("common.md"), "# Common docs").unwrap();

        let skills = vec![make_skill_with_dir(
            "test",
            "desc",
            "body",
            dir.path().to_path_buf(),
        )];

        let output = format_skills_prompt(&skills, "linux");
        assert!(output.contains("# Linux commands"));
        assert!(!output.contains("# macOS commands"));
        assert!(output.contains("# Common docs"));
        assert!(output.contains("<reference name=\"linux.md\">"));
        assert!(output.contains("<reference name=\"common.md\">"));
    }

    #[test]
    fn no_references_dir_produces_body_only() {
        let dir = tempfile::tempdir().unwrap();
        let skills = vec![make_skill_with_dir(
            "test",
            "desc",
            "skill body",
            dir.path().to_path_buf(),
        )];

        let output = format_skills_prompt(&skills, "macos");
        assert!(output.contains("skill body"));
        assert!(!output.contains("<reference"));
    }
}
