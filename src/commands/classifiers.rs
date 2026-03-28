// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Subcommands for the `zeph classifiers` command group.
#[derive(clap::Subcommand)]
pub(crate) enum ClassifiersCommand {
    /// Pre-download configured classifier model weights to the `HuggingFace` Hub cache.
    ///
    /// Run this before starting the agent to avoid a slow first-inference download.
    /// Model files (~100-280MB) are stored in the `HuggingFace` Hub cache directory
    /// (`~/.cache/huggingface/hub/` by default).
    Download {
        /// `HuggingFace` repo ID override. When set, downloads exactly this model.
        /// When omitted, uses the value from `[classifiers].*_model` in config (see `--model`).
        #[arg(long, value_name = "REPO_ID")]
        repo: Option<String>,

        /// Download timeout in seconds (default: 600).
        #[arg(long, default_value = "600")]
        timeout_secs: u64,

        /// Which model to download: "injection" (default), "pii", or "all".
        ///
        /// "injection" downloads `classifiers.injection_model`.
        /// "pii" downloads `classifiers.pii_model`.
        /// "all" downloads both.
        #[arg(long, default_value = "all")]
        model: String,
    },
}

/// Handle `zeph classifiers` subcommands.
///
/// # Errors
///
/// Returns an error if the download fails or times out.
#[cfg(feature = "classifiers")]
pub(crate) fn handle_classifiers_command(
    cmd: &ClassifiersCommand,
    config: &zeph_core::config::Config,
) -> anyhow::Result<()> {
    match cmd {
        ClassifiersCommand::Download {
            repo,
            timeout_secs,
            model,
        } => {
            let timeout = std::time::Duration::from_secs(*timeout_secs);
            let download_injection = matches!(model.as_str(), "injection" | "all");
            let download_pii = matches!(model.as_str(), "pii" | "all");

            if !download_injection && !download_pii {
                anyhow::bail!("Unknown model type '{model}'. Use 'injection', 'pii', or 'all'.");
            }

            if download_injection {
                let repo_id = repo
                    .as_deref()
                    .unwrap_or(&config.classifiers.injection_model);
                eprintln!("Downloading injection model: {repo_id}");
                eprintln!("This may take several minutes on first run (~100-280 MB).");
                zeph_llm::classifier::candle::download_model(
                    repo_id,
                    config.classifiers.hf_token.as_deref(),
                    timeout,
                )?;
                eprintln!("Injection model cached: {repo_id}");
            }

            if download_pii {
                let pii_repo_id = repo.as_deref().unwrap_or(&config.classifiers.pii_model);
                eprintln!("Downloading PII model: {pii_repo_id}");
                eprintln!("This may take several minutes on first run (~280 MB).");
                zeph_llm::classifier::candle_pii::download_pii_model(
                    pii_repo_id,
                    config.classifiers.hf_token.as_deref(),
                    timeout,
                )?;
                eprintln!("PII model cached: {pii_repo_id}");
            }

            Ok(())
        }
    }
}

/// Stub handler when the `classifiers` feature is disabled.
#[cfg(not(feature = "classifiers"))]
pub(crate) fn handle_classifiers_command(
    _cmd: &ClassifiersCommand,
    _config: &zeph_core::config::Config,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "The `classifiers` feature is not enabled in this build. \
         Recompile with `--features classifiers` to use classifier commands."
    )
}
