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
        /// `HuggingFace` repo ID of the injection model.
        /// Defaults to the value from `[classifiers].injection_model` in config.
        #[arg(long, value_name = "REPO_ID")]
        repo: Option<String>,

        /// Download timeout in seconds (default: 600).
        #[arg(long, default_value = "600")]
        timeout_secs: u64,
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
        ClassifiersCommand::Download { repo, timeout_secs } => {
            let repo_id = repo
                .as_deref()
                .unwrap_or(&config.classifiers.injection_model);
            let timeout = std::time::Duration::from_secs(*timeout_secs);

            eprintln!("Downloading classifier model: {repo_id}");
            eprintln!("This may take several minutes on first run (~100-280 MB).");

            zeph_llm::classifier::candle::download_model(repo_id, timeout)?;

            eprintln!("Model cached successfully: {repo_id}");
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
