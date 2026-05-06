// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Input, Password, Select};
use zeph_config::{GeminiThinkingLevel, GonkaNode, ThinkingConfig, ThinkingEffort};
use zeph_core::config::ProviderKind;
use zeroize::Zeroizing;

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
        "Gonka (decentralized \u{2014} via GonkaGate)",
        "Gonka (native \u{2014} requires GNK staking)",
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
        5 => {
            state.provider = Some(ProviderKind::Compatible);
            state.compatible_name = Some("gonkagate".into());
            state.base_url = Some("https://api.gonkagate.com/v1".into());

            let models = ["Qwen/Qwen3-235B-A22B-Instruct-2507-FP8", "Custom..."];
            let model_sel = Select::new()
                .with_prompt("Select model")
                .items(models)
                .default(0)
                .interact()?;
            state.model = Some(match model_sel {
                0 => models[0].to_owned(),
                _ => Input::new().with_prompt("Model name").interact_text()?,
            });

            if !use_age {
                let raw = Password::new()
                    .with_prompt("GonkaGate API key (starts with gp-...)")
                    .interact()?;
                state.api_key = Some(raw);
            }
        }
        6 => {
            step_gonka_native(state, use_age)?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn step_gonka_native(state: &mut WizardState, use_age: bool) -> anyhow::Result<()> {
    if !use_age {
        anyhow::bail!(
            "Gonka native provider requires the age vault backend for secure key storage.\n\
             Please re-run the wizard and select the age vault backend first."
        );
    }

    state.provider = Some(ProviderKind::Gonka);

    let hex_key = pick_hex_key()?;

    // Validate hex length.
    if hex_key.len() != 64 || !hex_key.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("Private key must be exactly 64 lowercase hex characters");
    }

    // Derive address to show the user.
    #[cfg(feature = "gonka")]
    let derived_address = {
        use zeph_llm::gonka::RequestSigner;
        let signer = RequestSigner::from_hex(&hex_key, "gonka")
            .map_err(|e| anyhow::anyhow!("invalid private key: {e}"))?;
        signer.address().to_owned()
    };
    #[cfg(not(feature = "gonka"))]
    let derived_address = String::from("<gonka feature not compiled in>");

    println!("\n  Derived address: {derived_address}");

    let nodes = configure_gonka_nodes()?;

    state.model = Some(
        Input::new()
            .with_prompt("Model name")
            .default("gpt-4o".into())
            .interact_text()?,
    );

    state.gonka_private_key = Some(hex_key);
    state.gonka_address = Some(derived_address);
    state.gonka_nodes = nodes;

    Ok(())
}

fn pick_hex_key() -> anyhow::Result<Zeroizing<String>> {
    let inferenced_available = std::process::Command::new("which")
        .arg("inferenced")
        .output()
        .is_ok_and(|o| o.status.success());

    if !inferenced_available {
        println!(
            "\n  inferenced CLI not found. Download from:\n  \
             https://github.com/gonka-ai/gonka/releases\n"
        );
        println!("Alternatively, you can paste a raw hex private key (64 hex characters).");
    }

    if inferenced_available {
        let key_names = get_inferenced_keys()?;
        let key_name = if key_names.is_empty() {
            let name: String = Input::new()
                .with_prompt("New key name")
                .default("zeph".into())
                .interact_text()?;
            create_inferenced_key(&name)?;
            name
        } else {
            let create_new = Confirm::new()
                .with_prompt("Create a new key?")
                .default(false)
                .interact()?;
            if create_new {
                let name: String = Input::new()
                    .with_prompt("New key name")
                    .default("zeph".into())
                    .interact_text()?;
                create_inferenced_key(&name)?;
                name
            } else {
                let items: Vec<&str> = key_names.iter().map(String::as_str).collect();
                let idx = Select::new()
                    .with_prompt("Select existing key")
                    .items(&items)
                    .default(0)
                    .interact()?;
                key_names[idx].clone()
            }
        };
        export_inferenced_key_hex(&key_name).map(Zeroizing::new)
    } else {
        let raw = Password::new()
            .with_prompt("Private key hex (64 hex chars, input hidden)")
            .interact()?;
        Ok(Zeroizing::new(raw.trim().to_owned()))
    }
}

fn configure_gonka_nodes() -> anyhow::Result<Vec<GonkaNode>> {
    println!("\nConfigure Gonka nodes (press Enter to use default seed nodes):");
    let default_seeds = vec![
        (
            "https://node1.gonka.ai".to_owned(),
            "gonka1node1placeholder000000000000000000000000".to_owned(),
        ),
        (
            "https://node2.gonka.ai".to_owned(),
            "gonka1node2placeholder000000000000000000000000".to_owned(),
        ),
        (
            "https://node3.gonka.ai".to_owned(),
            "gonka1node3placeholder000000000000000000000000".to_owned(),
        ),
    ];
    let use_defaults = Confirm::new()
        .with_prompt("Use default seed nodes?")
        .default(true)
        .interact()?;

    if use_defaults {
        return Ok(default_seeds
            .into_iter()
            .map(|(url, address)| GonkaNode {
                url,
                address,
                name: None,
            })
            .collect());
    }

    let mut nodes = Vec::new();
    loop {
        let url: String = Input::new()
            .with_prompt("Node URL (leave empty to finish)")
            .allow_empty(true)
            .interact_text()?;
        if url.is_empty() {
            break;
        }
        let address: String = Input::new()
            .with_prompt("Node on-chain address (bech32)")
            .interact_text()?;
        nodes.push(GonkaNode {
            url,
            address,
            name: None,
        });
    }
    if nodes.is_empty() {
        anyhow::bail!("At least one Gonka node is required");
    }
    Ok(nodes)
}

fn get_inferenced_keys() -> anyhow::Result<Vec<String>> {
    let output = std::process::Command::new("inferenced")
        .args(["keys", "list"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let names: Vec<String> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_owned())
        .collect();
    Ok(names)
}

fn create_inferenced_key(name: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("inferenced")
        .args(["keys", "add", name])
        .status()?;
    if !status.success() {
        anyhow::bail!("inferenced keys add failed");
    }
    Ok(())
}

fn export_inferenced_key_hex(name: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new("inferenced")
        .args(["keys", "export", name, "--unarmored-hex", "--unsafe"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("inferenced keys export failed: {stderr}");
    }
    let hex = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_lowercase();
    Ok(hex)
}
