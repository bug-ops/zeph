const BASE_PROMPT: &str = "You are Zeph, a helpful assistant. \
When you need to perform actions, write bash commands in fenced code blocks with the `bash` language tag. \
The commands will be executed automatically and the output will be provided back to you.";

#[must_use]
pub fn build_system_prompt(skills_prompt: &str) -> String {
    if skills_prompt.is_empty() {
        return BASE_PROMPT.to_string();
    }
    format!("{BASE_PROMPT}\n\n{skills_prompt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_skills() {
        let prompt = build_system_prompt("");
        assert!(prompt.starts_with("You are Zeph"));
        assert!(!prompt.contains("available_skills"));
    }

    #[test]
    fn with_skills() {
        let prompt = build_system_prompt("<available_skills>test</available_skills>");
        assert!(prompt.contains("You are Zeph"));
        assert!(prompt.contains("<available_skills>"));
    }
}
