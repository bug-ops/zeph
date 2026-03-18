// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::default_log_file;

fn default_log_level() -> String {
    "info".to_owned()
}

fn default_log_max_files() -> usize {
    7
}

/// Log file rotation strategy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogRotation {
    #[default]
    Daily,
    Hourly,
    Never,
}

/// Configuration for file-based logging.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Path to the log file. Empty string disables file logging.
    #[serde(default = "default_log_file")]
    pub file: String,
    /// Log level for the file sink (does not affect stderr/`RUST_LOG`).
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Rotation strategy: daily, hourly, or never.
    pub rotation: LogRotation,
    /// Maximum number of rotated log files to retain.
    #[serde(default = "default_log_max_files")]
    pub max_files: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            file: default_log_file(),
            level: default_log_level(),
            rotation: LogRotation::default(),
            max_files: default_log_max_files(),
        }
    }
}
