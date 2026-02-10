//! Tool execution abstraction and shell backend.

pub mod audit;
pub mod composite;
pub mod config;
pub mod executor;
pub mod scrape;
pub mod shell;

pub use audit::{AuditEntry, AuditLogger, AuditResult};
pub use composite::CompositeExecutor;
pub use config::{AuditConfig, ScrapeConfig, ShellConfig, ToolsConfig};
pub use executor::{ToolError, ToolExecutor, ToolOutput};
pub use scrape::WebScrapeExecutor;
pub use shell::ShellExecutor;
