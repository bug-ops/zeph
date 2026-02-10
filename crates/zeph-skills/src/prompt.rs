use std::fmt::Write;

use crate::loader::Skill;

#[must_use]
pub fn format_skills_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("<available_skills>\n");

    for skill in skills {
        let _ = write!(
            out,
            "  <skill name=\"{}\">\n    <description>{}</description>\n    <instructions>\n{}\n    </instructions>\n  </skill>\n",
            skill.name(),
            skill.description(),
            skill.body,
        );
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

    #[test]
    fn empty_skills_returns_empty_string() {
        let empty: &[Skill] = &[];
        assert_eq!(format_skills_prompt(empty), "");
    }

    #[test]
    fn single_skill_format() {
        let skills = vec![make_skill("test", "A test.", "# Hello\nworld")];

        let output = format_skills_prompt(&skills);
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

        let output = format_skills_prompt(&skills);
        assert!(output.contains("<skill name=\"a\">"));
        assert!(output.contains("<skill name=\"b\">"));
    }
}
