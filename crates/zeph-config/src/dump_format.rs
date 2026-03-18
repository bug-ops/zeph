// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

/// Output format for debug dump files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DumpFormat {
    /// Write LLM requests as pretty-printed internal zeph-llm JSON (`{id}-request.json`).
    #[default]
    Json,
    /// Write LLM requests as the actual API payload sent to the provider (`{id}-request.json`):
    /// system extracted, `agent_invisible` messages filtered, parts rendered as content blocks.
    Raw,
    /// Emit OpenTelemetry-compatible OTLP JSON trace spans (`trace.json` at session end).
    /// Legacy numbered dump files are NOT written unless `[debug.traces] legacy_files = true`.
    Trace,
}

impl std::str::FromStr for DumpFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "json" => Ok(Self::Json),
            "raw" => Ok(Self::Raw),
            "trace" => Ok(Self::Trace),
            other => Err(format!(
                "unknown dump format `{other}`, expected json|raw|trace"
            )),
        }
    }
}
