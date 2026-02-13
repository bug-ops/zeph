use crate::provider::{Message, Role};

#[derive(Debug, Clone, Copy)]
pub enum ChatTemplate {
    Llama3,
    ChatML,
    Mistral,
    Phi3,
    Raw,
}

impl ChatTemplate {
    #[must_use]
    pub fn parse_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "llama3" | "llama" => Self::Llama3,
            "chatml" | "chat-ml" => Self::ChatML,
            "mistral" => Self::Mistral,
            "phi3" | "phi" => Self::Phi3,
            _ => Self::Raw,
        }
    }

    #[must_use]
    pub fn format(&self, messages: &[Message]) -> String {
        match self {
            Self::Llama3 => format_llama3(messages),
            Self::ChatML => format_chatml(messages),
            Self::Mistral => format_mistral(messages),
            Self::Phi3 => format_phi3(messages),
            Self::Raw => format_raw(messages),
        }
    }
}

fn role_tag(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn format_llama3(messages: &[Message]) -> String {
    let mut out = String::from("<|begin_of_text|>");
    for msg in messages {
        out.push_str("<|start_header_id|>");
        out.push_str(role_tag(msg.role));
        out.push_str("<|end_header_id|>\n\n");
        out.push_str(msg.to_llm_content());
        out.push_str("<|eot_id|>");
    }
    out.push_str("<|start_header_id|>assistant<|end_header_id|>\n\n");
    out
}

fn format_chatml(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        out.push_str("<|im_start|>");
        out.push_str(role_tag(msg.role));
        out.push('\n');
        out.push_str(msg.to_llm_content());
        out.push_str("<|im_end|>\n");
    }
    out.push_str("<|im_start|>assistant\n");
    out
}

fn format_mistral(messages: &[Message]) -> String {
    let mut out = String::new();
    let mut system_text = String::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if !system_text.is_empty() {
                    system_text.push('\n');
                }
                system_text.push_str(msg.to_llm_content());
            }
            Role::User => {
                out.push_str("[INST] ");
                if !system_text.is_empty() {
                    out.push_str(&system_text);
                    out.push_str("\n\n");
                    system_text.clear();
                }
                out.push_str(msg.to_llm_content());
                out.push_str(" [/INST]");
            }
            Role::Assistant => {
                out.push_str(msg.to_llm_content());
                out.push_str("</s>");
            }
        }
    }
    out
}

fn format_phi3(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        out.push_str("<|");
        out.push_str(role_tag(msg.role));
        out.push_str("|>\n");
        out.push_str(msg.to_llm_content());
        out.push_str("<|end|>\n");
    }
    out.push_str("<|assistant|>\n");
    out
}

fn format_raw(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(msg.to_llm_content());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::System,
                content: "You are helpful.".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
                parts: vec![],
            },
        ]
    }

    #[test]
    fn llama3_template() {
        let out = ChatTemplate::Llama3.format(&sample_messages());
        assert!(out.starts_with("<|begin_of_text|>"));
        assert!(out.contains("<|start_header_id|>system<|end_header_id|>"));
        assert!(out.contains("You are helpful."));
        assert!(out.contains("<|start_header_id|>user<|end_header_id|>"));
        assert!(out.contains("Hi"));
        assert!(out.ends_with("<|start_header_id|>assistant<|end_header_id|>\n\n"));
    }

    #[test]
    fn chatml_template() {
        let out = ChatTemplate::ChatML.format(&sample_messages());
        assert!(out.contains("<|im_start|>system\nYou are helpful.<|im_end|>"));
        assert!(out.contains("<|im_start|>user\nHi<|im_end|>"));
        assert!(out.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn mistral_template() {
        let out = ChatTemplate::Mistral.format(&sample_messages());
        assert!(out.contains("[INST] You are helpful.\n\nHi [/INST]"));
    }

    #[test]
    fn phi3_template() {
        let out = ChatTemplate::Phi3.format(&sample_messages());
        assert!(out.contains("<|system|>\nYou are helpful.<|end|>"));
        assert!(out.contains("<|user|>\nHi<|end|>"));
        assert!(out.ends_with("<|assistant|>\n"));
    }

    #[test]
    fn raw_template() {
        let out = ChatTemplate::Raw.format(&sample_messages());
        assert_eq!(out, "You are helpful.\nHi");
    }

    #[test]
    fn from_str_parses_variants() {
        assert!(matches!(
            ChatTemplate::parse_str("llama3"),
            ChatTemplate::Llama3
        ));
        assert!(matches!(
            ChatTemplate::parse_str("chatml"),
            ChatTemplate::ChatML
        ));
        assert!(matches!(
            ChatTemplate::parse_str("mistral"),
            ChatTemplate::Mistral
        ));
        assert!(matches!(
            ChatTemplate::parse_str("phi3"),
            ChatTemplate::Phi3
        ));
        assert!(matches!(
            ChatTemplate::parse_str("unknown"),
            ChatTemplate::Raw
        ));
    }

    #[test]
    fn mistral_multi_turn() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "System prompt.".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "Hello".into(),
                parts: vec![],
            },
            Message {
                role: Role::Assistant,
                content: "Hi there".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "How are you?".into(),
                parts: vec![],
            },
        ];
        let out = ChatTemplate::Mistral.format(&messages);
        assert!(out.contains("[INST] System prompt.\n\nHello [/INST]"));
        assert!(out.contains("Hi there</s>"));
        assert!(out.contains("[INST] How are you? [/INST]"));
    }
}
