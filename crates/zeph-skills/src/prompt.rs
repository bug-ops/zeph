use std::borrow::Borrow;
use std::fmt::Write;

use crate::loader::Skill;

#[must_use]
pub fn format_skills_prompt<S: Borrow<Skill>>(skills: &[S]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("<available_skills>\n");

    for skill in skills {
        let skill = skill.borrow();
        let _ = write!(
            out,
            "  <skill name=\"{}\">\n    <description>{}</description>\n    <instructions>\n{}\n    </instructions>\n  </skill>\n",
            skill.name, skill.description, skill.body,
        );
    }

    out.push_str("</available_skills>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_skills_returns_empty_string() {
        let empty: &[Skill] = &[];
        assert_eq!(format_skills_prompt(empty), "");
    }

    #[test]
    fn single_skill_format() {
        let skills = vec![Skill {
            name: "test".into(),
            description: "A test.".into(),
            body: "# Hello\nworld".into(),
        }];

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
            Skill {
                name: "a".into(),
                description: "desc a".into(),
                body: "body a".into(),
            },
            Skill {
                name: "b".into(),
                description: "desc b".into(),
                body: "body b".into(),
            },
        ];

        let output = format_skills_prompt(&skills);
        assert!(output.contains("<skill name=\"a\">"));
        assert!(output.contains("<skill name=\"b\">"));
    }
}
