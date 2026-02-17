//! Tool execution abstraction and shell backend.

pub mod anomaly;
pub mod audit;
pub mod composite;
pub mod config;
pub mod executor;
pub mod file;
pub mod filter;
pub mod overflow;
pub mod permissions;
pub mod registry;
pub mod scrape;
pub mod shell;
pub mod trust_gate;

pub use anomaly::{AnomalyDetector, AnomalySeverity};
pub use audit::{AuditEntry, AuditLogger, AuditResult};
pub use composite::CompositeExecutor;
pub use config::{AuditConfig, ScrapeConfig, ShellConfig, ToolsConfig};
pub use executor::{
    DiffData, FilterStats, MAX_TOOL_OUTPUT_CHARS, ToolCall, ToolError, ToolEvent, ToolEventTx,
    ToolExecutor, ToolOutput, truncate_tool_output,
};
pub use file::FileExecutor;
pub use filter::{
    CommandMatcher, FilterConfidence, FilterConfig, FilterMetrics, FilterResult, OutputFilter,
    OutputFilterRegistry, sanitize_output, strip_ansi,
};
pub use overflow::{cleanup_overflow_files, save_overflow};
pub use permissions::{
    AutonomyLevel, PermissionAction, PermissionPolicy, PermissionRule, PermissionsConfig,
};
pub use registry::ToolRegistry;
pub use scrape::WebScrapeExecutor;
pub use shell::ShellExecutor;
pub use trust_gate::TrustGateExecutor;
