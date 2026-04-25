// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

#[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — deferred to a future structural refactor
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let collapsed = area.height < 30;

    let mut lines: Vec<Line<'_>> = Vec::new();

    // LLM section
    lines.push(Line::from("  LLM"));
    lines.push(Line::from(format!(
        "    Provider: {}",
        metrics.provider_name
    )));
    lines.push(Line::from(format!("    Model: {}", metrics.model_name)));
    if !metrics.embedding_model.is_empty() {
        lines.push(Line::from(format!(
            "    Embed: {}",
            metrics.embedding_model
        )));
    }
    lines.push(Line::from(format!(
        "    Context: {} | Latency: {}ms",
        metrics.context_tokens, metrics.last_llm_latency_ms
    )));
    if metrics.extended_context {
        lines.push(Line::from("    Max context: 1M"));
    }

    // Session section
    if collapsed {
        lines.push(Line::from(format!(
            "  Session: {} tok | {} calls",
            metrics.total_tokens, metrics.api_calls
        )));
    } else {
        lines.push(Line::from("  Session"));
        lines.push(Line::from(format!(
            "    Tokens: {} | API: {}",
            metrics.total_tokens, metrics.api_calls
        )));
        if let Some(budget) = metrics.token_budget {
            if let Some(threshold) = metrics.compaction_threshold {
                lines.push(Line::from(format!(
                    "    Budget: {budget} | Compact: {threshold}"
                )));
            } else {
                lines.push(Line::from(format!("    Budget: {budget}")));
            }
        }
        if metrics.cache_creation_tokens > 0 || metrics.cache_read_tokens > 0 {
            lines.push(Line::from(format!(
                "    Cache W:{} R:{}",
                metrics.cache_creation_tokens, metrics.cache_read_tokens
            )));
        }
        if metrics.filter_applications > 0 {
            #[allow(clippy::cast_precision_loss)]
            let hit_pct = if metrics.filter_total_commands > 0 {
                metrics.filter_filtered_commands as f64 / metrics.filter_total_commands as f64
                    * 100.0
            } else {
                0.0
            };
            lines.push(Line::from(format!(
                "    Filter: {}/{} ({hit_pct:.0}% hit)",
                metrics.filter_filtered_commands, metrics.filter_total_commands,
            )));
            #[allow(clippy::cast_precision_loss)]
            let pct = if metrics.filter_raw_tokens > 0 {
                metrics.filter_saved_tokens as f64 / metrics.filter_raw_tokens as f64 * 100.0
            } else {
                0.0
            };
            lines.push(Line::from(format!(
                "    Filter saved: {} tok ({pct:.0}%)",
                metrics.filter_saved_tokens,
            )));
        }
    }

    // Infra section
    if collapsed {
        let mut infra_parts: Vec<String> = Vec::new();
        if !metrics.vault_backend.is_empty() {
            infra_parts.push(format!("vault:{}", metrics.vault_backend));
        }
        if !metrics.active_channel.is_empty() {
            infra_parts.push(format!("ch:{}", metrics.active_channel));
        }
        if !infra_parts.is_empty() {
            lines.push(Line::from(format!("  Infra: {}", infra_parts.join(" | "))));
        }
    } else {
        lines.push(Line::from("  Infra"));
        match (
            metrics.vault_backend.as_str(),
            metrics.active_channel.as_str(),
        ) {
            ("", "") => {}
            (v, "") => lines.push(Line::from(format!("    Vault: {v}"))),
            ("", c) => lines.push(Line::from(format!("    Channel: {c}"))),
            (v, c) => lines.push(Line::from(format!("    Vault: {v} | Channel: {c}"))),
        }

        let mut flags: Vec<&str> = Vec::new();
        if metrics.self_learning_enabled {
            flags.push("Learning: ON");
        }
        if metrics.cache_enabled {
            flags.push("Cache: ON");
        }
        if metrics.autosave_enabled {
            flags.push("Autosave: ON");
        }
        if !flags.is_empty() {
            lines.push(Line::from(format!("    {}", flags.join(" | "))));
        }
        if metrics.mcp_server_count > 0 {
            lines.push(Line::from(format!(
                "    MCP: {}/{} connected, {} tools",
                metrics.mcp_connected_count, metrics.mcp_server_count, metrics.mcp_tool_count
            )));
        }
    }

    // Turn latency section — only shown after at least one turn has completed (#2820).
    if metrics.timing_sample_count > 0 {
        let last = &metrics.last_turn_timings;
        let avg = &metrics.avg_turn_timings;
        let max = &metrics.max_turn_timings;
        lines.push(Line::from("  Turn Latency"));
        lines.push(Line::from(format!(
            "    ctx:{}ms llm:{}ms tool:{}ms persist:{}ms",
            last.prepare_context_ms, last.llm_chat_ms, last.tool_exec_ms, last.persist_message_ms,
        )));
        if metrics.timing_sample_count > 1 {
            lines.push(Line::from(format!(
                "    avg ctx:{}ms llm:{}ms (n={})",
                avg.prepare_context_ms, avg.llm_chat_ms, metrics.timing_sample_count,
            )));
            lines.push(Line::from(format!(
                "    max ctx:{}ms llm:{}ms tool:{}ms",
                max.prepare_context_ms, max.llm_chat_ms, max.tool_exec_ms,
            )));
        }
    }

    // Classifier latency section — only shown when at least one task has been called.
    let clf = &metrics.classifier;
    let has_classifier_data =
        clf.injection.call_count > 0 || clf.pii.call_count > 0 || clf.feedback.call_count > 0;
    if has_classifier_data {
        lines.push(Line::from("  Classifiers"));
        for (name, snap) in [
            ("injection", &clf.injection),
            ("pii", &clf.pii),
            ("feedback", &clf.feedback),
        ] {
            if snap.call_count > 0 {
                lines.push(Line::from(format!(
                    "    [{name}] calls:{} p50:{}ms p95:{}ms",
                    snap.call_count,
                    snap.p50_ms.unwrap_or(0),
                    snap.p95_ms.unwrap_or(0),
                )));
            }
        }
    }

    let resources = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(" Resources "),
    );
    frame.render_widget(resources, area);
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::metrics::MetricsSnapshot;
    use crate::test_utils::render_to_string;

    #[test]
    fn resources_with_provider() {
        let metrics = MetricsSnapshot {
            provider_name: "claude".into(),
            model_name: "opus-4".into(),
            context_tokens: 8000,
            total_tokens: 12000,
            api_calls: 5,
            last_llm_latency_ms: 250,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(35, 12, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn resources_with_extended_context() {
        let metrics = MetricsSnapshot {
            provider_name: "claude".into(),
            model_name: "claude-sonnet-4-6".into(),
            context_tokens: 50000,
            total_tokens: 75000,
            api_calls: 3,
            last_llm_latency_ms: 400,
            extended_context: true,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(35, 13, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            output.contains("Max context: 1M"),
            "resources panel must contain 'Max context: 1M' when extended_context is true; got: {output:?}"
        );
        assert_snapshot!(output);
    }

    #[test]
    fn resources_shows_embedding_model_when_set() {
        let metrics = MetricsSnapshot {
            embedding_model: "nomic-embed-text".into(),
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(35, 30, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            output.contains("Embed: nomic-embed-text"),
            "resources panel must contain embedding model; got: {output:?}"
        );
    }

    #[test]
    fn resources_omits_embedding_model_when_empty() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(35, 30, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            !output.contains("Embed:"),
            "resources panel must not contain Embed: when embedding_model is empty; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_token_budget_with_compaction_threshold_none() {
        let metrics = MetricsSnapshot {
            token_budget: Some(200_000),
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(35, 30, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            output.contains("Budget: 200000"),
            "resources panel must show token budget; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_self_learning_flag() {
        let metrics = MetricsSnapshot {
            self_learning_enabled: true,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(35, 30, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            output.contains("Learning: ON"),
            "resources panel must show 'Learning: ON' when self_learning_enabled; got: {output:?}"
        );
    }

    #[test]
    fn resources_with_full_infra() {
        let metrics = MetricsSnapshot {
            provider_name: "claude".into(),
            model_name: "claude-sonnet-4-6".into(),
            context_tokens: 10000,
            total_tokens: 15000,
            api_calls: 7,
            last_llm_latency_ms: 180,
            embedding_model: "nomic-embed-text".into(),
            token_budget: Some(200_000),
            compaction_threshold: Some(120_000),
            vault_backend: "age".into(),
            active_channel: "tui".into(),
            self_learning_enabled: true,
            cache_enabled: true,
            autosave_enabled: true,
            mcp_server_count: 2,
            mcp_connected_count: 2,
            mcp_tool_count: 14,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(40, 30, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            output.contains("Vault: age"),
            "expected vault backend; got: {output:?}"
        );
        assert!(
            output.contains("Channel: tui"),
            "expected channel; got: {output:?}"
        );
        assert!(
            output.contains("Learning: ON"),
            "expected learning flag; got: {output:?}"
        );
        assert_snapshot!(output);
    }

    #[test]
    fn resources_collapsed_when_small_height() {
        let metrics = MetricsSnapshot {
            provider_name: "claude".into(),
            model_name: "claude-sonnet-4-6".into(),
            vault_backend: "age".into(),
            active_channel: "tui".into(),
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(40, 20, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            output.contains("vault:age"),
            "collapsed mode should show vault inline; got: {output:?}"
        );
    }
}
