// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::bootstrap::AppBuilder;

pub(crate) async fn handle_search(
    query: String,
    limit: Option<usize>,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    // Non-interactive data command (single web_search call), not a troubleshooting
    // session with customization sources to isolate — `safe_mode` is deliberately
    // `false`, not omitted.
    let app = AppBuilder::new(config_path, None, None, None, false, false).await?;
    let config = app.config();

    let web_search_api_key = config
        .secrets
        .web_search_api_key
        .as_ref()
        .map(|s| zeph_common::secret::Secret::new(s.expose()));
    let Some(mut executor) = zeph_tools::WebSearchExecutor::new(
        &config.tools.search,
        &config.tools.scrape,
        web_search_api_key,
    ) else {
        anyhow::bail!(
            "web_search is not usable: set `[tools.search] enabled = true` in config.toml and \
             store an API key with `zeph vault set {} <key>`",
            config.tools.search.api_key_vault_key
        );
    };
    executor = executor.with_egress_config(config.tools.egress.clone());
    // Cross-mode consistency (spec 006-1-web-search §8 scenario 9): the CLI path gets the
    // same audit/egress persistence as the TUI/LLM-invoked paths (agent_setup.rs, acp.rs,
    // daemon.rs, serve/deps.rs) — no separate drain task is needed here since
    // `AuditLogger::log_egress` persists synchronously per call; the async telemetry
    // channel (`with_egress_tx`) exists for the TUI live-status panel only.
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit, false).await
    {
        executor = executor.with_audit(std::sync::Arc::new(logger));
    }

    let mut params = serde_json::Map::new();
    params.insert("query".to_owned(), serde_json::Value::String(query));
    if let Some(limit) = limit {
        params.insert("limit".to_owned(), serde_json::Value::Number(limit.into()));
    }
    let call = zeph_tools::ToolCall {
        tool_id: "web_search".into(),
        params,
        caller_id: None,
        context: None,
        tool_call_id: String::new(),
        skill_name: None,
    };

    let output = zeph_tools::ToolExecutor::execute_tool_call(&executor, &call)
        .await
        .map_err(|e| anyhow::anyhow!("web_search failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("web_search executor did not handle the call"))?;

    println!("{}", output.summary);
    Ok(())
}
