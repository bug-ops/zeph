// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Input, Password, Select};
use zeph_core::config::ProviderKind;
use zeph_llm::{GeminiThinkingLevel, ThinkingConfig, ThinkingEffort};

use super::WizardState;

#[allow(clippy::too_many_lines)]
pub(super) fn step_llm(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 2/10: LLM Provider ==\n");

    let use_age = state.vault_backend == "age";

    step_llm_provider(state, use_age)?;

    state.embedding_model = Some(
        Input::new()
            .with_prompt("Embedding model")
            .default("qwen3-embedding".into())
            .interact_text()?,
    );

    if state.provider == Some(ProviderKind::Ollama) {
        let use_vision = Confirm::new()
            .with_prompt("Use a separate model for vision (image input)?")
            .default(false)
            .interact()?;
        if use_vision {
            state.vision_model = Some(
                Input::new()
                    .with_prompt("Vision model name (e.g. llava:13b)")
                    .interact_text()?,
            );
        }
    }

    println!();
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn step_llm_provider(state: &mut WizardState, use_age: bool) -> anyhow::Result<()> {
    let providers = [
        "Ollama (local)",
        "Claude (API)",
        "OpenAI (API)",
        "Gemini (API)",
        "Compatible (custom)",
    ];
    let selection = Select::new()
        .with_prompt("Select LLM provider")
        .items(providers)
        .default(0)
        .interact()?;

    match selection {
        0 => {
            state.provider = Some(ProviderKind::Ollama);
            state.base_url = Some(
                Input::new()
                    .with_prompt("Ollama base URL")
                    .default("http://localhost:11434".into())
                    .interact_text()?,
            );
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("qwen3:8b".into())
                    .interact_text()?,
            );
        }
        1 => {
            state.provider = Some(ProviderKind::Claude);
            if !use_age {
                let raw = Password::new().with_prompt("Claude API key").interact()?;
                state.api_key = if raw.is_empty() { None } else { Some(raw) };
            }
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("claude-sonnet-4-5-20250929".into())
                    .interact_text()?,
            );
            let thinking_mode = Select::new()
                .with_prompt("Enable thinking?")
                .items(["No", "Extended", "Adaptive"])
                .default(0)
                .interact()?;
            state.thinking = match thinking_mode {
                1 => {
                    let budget: u32 = Input::new()
                        .with_prompt("Budget tokens (1024-128000)")
                        .default(10_000)
                        .interact_text()?;
                    Some(ThinkingConfig::Extended {
                        budget_tokens: budget,
                    })
                }
                2 => {
                    let effort_idx = Select::new()
                        .with_prompt("Effort level")
                        .items(["Low", "Medium", "High"])
                        .default(1)
                        .interact()?;
                    let effort = match effort_idx {
                        0 => ThinkingEffort::Low,
                        2 => ThinkingEffort::High,
                        _ => ThinkingEffort::Medium,
                    };
                    Some(ThinkingConfig::Adaptive {
                        effort: Some(effort),
                    })
                }
                _ => None,
            };
            state.enable_extended_context = Confirm::new()
                .with_prompt("Enable 1M extended context? (long-context pricing above 200K tokens)")
                .default(false)
                .interact()?;
        }
        2 => {
            state.provider = Some(ProviderKind::OpenAi);
            if !use_age {
                let raw = Password::new().with_prompt("OpenAI API key").interact()?;
                state.api_key = if raw.is_empty() { None } else { Some(raw) };
            }
            state.base_url = Some(
                Input::new()
                    .with_prompt("Base URL")
                    .default("https://api.openai.com/v1".into())
                    .interact_text()?,
            );
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("gpt-4o".into())
                    .interact_text()?,
            );
        }
        3 => {
            state.provider = Some(ProviderKind::Gemini);
            if !use_age {
                let raw = Password::new().with_prompt("Gemini API key").interact()?;
                state.api_key = if raw.is_empty() { None } else { Some(raw) };
            }
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("gemini-2.0-flash".into())
                    .interact_text()?,
            );
            let thinking_opts = [
                "skip (no thinking_level)",
                "minimal",
                "low",
                "medium",
                "high",
            ];
            let thinking_sel = Select::new()
                .with_prompt("Thinking level (for Gemini 3+ thinking models; skip for 2.x)")
                .items(thinking_opts)
                .default(0)
                .interact()?;
            state.gemini_thinking_level = match thinking_sel {
                1 => Some(GeminiThinkingLevel::Minimal),
                2 => Some(GeminiThinkingLevel::Low),
                3 => Some(GeminiThinkingLevel::Medium),
                4 => Some(GeminiThinkingLevel::High),
                _ => None,
            };
        }
        4 => {
            state.provider = Some(ProviderKind::Compatible);
            state.compatible_name =
                Some(Input::new().with_prompt("Provider name").interact_text()?);
            state.base_url = Some(Input::new().with_prompt("Base URL").interact_text()?);
            state.model = Some(Input::new().with_prompt("Model name").interact_text()?);
            if !use_age {
                state.api_key = Some(
                    Password::new()
                        .with_prompt("API key (leave empty if none)")
                        .allow_empty_password(true)
                        .interact()?,
                );
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}
