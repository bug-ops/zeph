// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod acp;
mod agent_setup;
mod channel;
mod cli;
mod commands;
mod daemon;
mod db_url;
mod gateway_spawn;
mod init;
#[cfg(feature = "prometheus")]
mod metrics_export;
mod runner;
mod scheduler;
#[cfg(feature = "scheduler")]
mod scheduler_executor;
mod tracing_init;
mod tui_bridge;
mod tui_remote;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Box::pin(runner::run(Cli::parse())).await
}

#[cfg(test)]
mod tests;
