//! Tool execution abstraction and shell backend.

pub mod audit;
pub mod composite;
pub mod config;
pub mod executor;
pub mod file;
pub mod registry;
pub mod scrape;
pub mod shell;

pub use audit::{AuditEntry, AuditLogger, AuditResult};
pub use composite::CompositeExecutor;
pub use config::{AuditConfig, ScrapeConfig, ShellConfig, ToolsConfig};
pub use executor::{
    MAX_TOOL_OUTPUT_CHARS, ToolCall, ToolError, ToolEvent, ToolEventTx, ToolExecutor, ToolOutput,
    truncate_tool_output,
};
pub use file::FileExecutor;
pub use registry::ToolRegistry;
pub use scrape::WebScrapeExecutor;
pub use shell::ShellExecutor;
