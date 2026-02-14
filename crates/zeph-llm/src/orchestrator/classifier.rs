use crate::provider::{Message, Role};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskType {
    Coding,
    Creative,
    Analysis,
    Translation,
    Summarization,
    General,
}

impl TaskType {
    #[must_use]
    pub fn classify(messages: &[Message]) -> Self {
        let last_user_msg = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.to_lowercase())
            .unwrap_or_default();

        if contains_code_indicators(&last_user_msg) {
            Self::Coding
        } else if contains_translation_indicators(&last_user_msg) {
            Self::Translation
        } else if contains_summary_indicators(&last_user_msg) {
            Self::Summarization
        } else if contains_creative_indicators(&last_user_msg) {
            Self::Creative
        } else if contains_analysis_indicators(&last_user_msg) {
            Self::Analysis
        } else {
            Self::General
        }
    }

    #[must_use]
    pub fn parse_str(s: &str) -> Self {
        match s {
            "coding" => Self::Coding,
            "creative" => Self::Creative,
            "analysis" => Self::Analysis,
            "translation" => Self::Translation,
            "summarization" => Self::Summarization,
            _ => Self::General,
        }
    }
}

fn contains_code_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "code",
        "function",
        "implement",
        "debug",
        "compile",
        "syntax",
        "refactor",
        "algorithm",
        "class",
        "struct",
        "enum",
        "trait",
        "bug",
        "error",
        "fix",
        "rust",
        "python",
        "javascript",
        "typescript",
        "```",
        "fn ",
        "def ",
        "async fn",
        "pub fn",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_translation_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "translate",
        "translation",
        "переведи",
        "перевод",
        "to english",
        "to russian",
        "to spanish",
        "to french",
        "на английский",
        "на русский",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_summary_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "summarize",
        "summary",
        "tldr",
        "tl;dr",
        "brief",
        "кратко",
        "резюме",
        "суммируй",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_creative_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "write a story",
        "poem",
        "creative",
        "imagine",
        "fiction",
        "narrative",
        "compose",
        "стих",
        "рассказ",
        "сочини",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_analysis_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "analyze",
        "analysis",
        "compare",
        "evaluate",
        "assess",
        "review",
        "critique",
        "examine",
        "pros and cons",
        "анализ",
        "сравни",
        "оцени",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(content: &str) -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: content.into(),
            parts: vec![],
        }]
    }

    #[test]
    fn classify_coding() {
        assert_eq!(
            TaskType::classify(&user_msg("write a function to sort")),
            TaskType::Coding
        );
        assert_eq!(
            TaskType::classify(&user_msg("debug this error")),
            TaskType::Coding
        );
        assert_eq!(
            TaskType::classify(&user_msg("implement a struct")),
            TaskType::Coding
        );
    }

    #[test]
    fn classify_translation() {
        assert_eq!(
            TaskType::classify(&user_msg("translate this to english")),
            TaskType::Translation
        );
    }

    #[test]
    fn classify_summarization() {
        assert_eq!(
            TaskType::classify(&user_msg("summarize this article")),
            TaskType::Summarization
        );
        assert_eq!(
            TaskType::classify(&user_msg("give me a tldr")),
            TaskType::Summarization
        );
    }

    #[test]
    fn classify_creative() {
        assert_eq!(
            TaskType::classify(&user_msg("write a story about a dragon")),
            TaskType::Creative
        );
        assert_eq!(
            TaskType::classify(&user_msg("compose a poem")),
            TaskType::Creative
        );
    }

    #[test]
    fn classify_analysis() {
        assert_eq!(
            TaskType::classify(&user_msg("analyze this data")),
            TaskType::Analysis
        );
        assert_eq!(
            TaskType::classify(&user_msg("compare these two approaches")),
            TaskType::Analysis
        );
    }

    #[test]
    fn classify_general() {
        assert_eq!(TaskType::classify(&user_msg("hello")), TaskType::General);
        assert_eq!(
            TaskType::classify(&user_msg("what time is it")),
            TaskType::General
        );
    }

    #[test]
    fn classify_empty_messages() {
        assert_eq!(TaskType::classify(&[]), TaskType::General);
    }

    #[test]
    fn task_type_from_str() {
        assert_eq!(TaskType::parse_str("coding"), TaskType::Coding);
        assert_eq!(TaskType::parse_str("creative"), TaskType::Creative);
        assert_eq!(TaskType::parse_str("analysis"), TaskType::Analysis);
        assert_eq!(TaskType::parse_str("translation"), TaskType::Translation);
        assert_eq!(
            TaskType::parse_str("summarization"),
            TaskType::Summarization
        );
        assert_eq!(TaskType::parse_str("general"), TaskType::General);
        assert_eq!(TaskType::parse_str("unknown"), TaskType::General);
    }

    #[test]
    fn task_type_debug() {
        let task = TaskType::Coding;
        assert_eq!(format!("{task:?}"), "Coding");
    }

    #[test]
    fn task_type_copy_and_eq() {
        let a = TaskType::Creative;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn task_type_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(TaskType::Coding);
        set.insert(TaskType::Coding);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn classify_uses_last_user_message() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "write a function".into(),
                parts: vec![],
            },
            Message {
                role: Role::Assistant,
                content: "here it is".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "translate to spanish".into(),
                parts: vec![],
            },
        ];
        assert_eq!(TaskType::classify(&messages), TaskType::Translation);
    }

    #[test]
    fn classify_ignores_system_messages() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "you write code".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "hello there".into(),
                parts: vec![],
            },
        ];
        assert_eq!(TaskType::classify(&messages), TaskType::General);
    }

    #[test]
    fn classify_code_indicators_comprehensive() {
        for keyword in &[
            "algorithm",
            "refactor",
            "compile",
            "syntax",
            "pub fn",
            "```",
        ] {
            let msgs = user_msg(keyword);
            assert_eq!(
                TaskType::classify(&msgs),
                TaskType::Coding,
                "failed for: {keyword}"
            );
        }
    }

    #[test]
    fn classify_summary_indicators() {
        assert_eq!(
            TaskType::classify(&user_msg("give me a tl;dr")),
            TaskType::Summarization
        );
        assert_eq!(
            TaskType::classify(&user_msg("brief overview please")),
            TaskType::Summarization
        );
    }

    #[test]
    fn classify_creative_indicators() {
        assert_eq!(
            TaskType::classify(&user_msg("imagine a world where")),
            TaskType::Creative
        );
    }

    #[test]
    fn classify_analysis_indicators() {
        assert_eq!(
            TaskType::classify(&user_msg("evaluate this approach")),
            TaskType::Analysis
        );
        assert_eq!(
            TaskType::classify(&user_msg("pros and cons of X")),
            TaskType::Analysis
        );
    }

    #[test]
    fn classify_russian_translation_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("переведи на английский")),
            TaskType::Translation
        );
    }

    #[test]
    fn classify_russian_summary_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("кратко опиши")),
            TaskType::Summarization
        );
    }

    #[test]
    fn classify_russian_creative_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("сочини рассказ")),
            TaskType::Creative
        );
    }

    #[test]
    fn classify_russian_analysis_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("анализ данных")),
            TaskType::Analysis
        );
    }

    #[test]
    fn classify_code_with_backticks() {
        assert_eq!(
            TaskType::classify(&user_msg("here is some code ```rust let x = 5;```")),
            TaskType::Coding
        );
    }
}
