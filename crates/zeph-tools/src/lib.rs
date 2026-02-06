//! Tool execution abstraction and shell backend.

pub mod config;
pub mod executor;
pub mod shell;

pub use config::{ShellConfig, ToolsConfig};
pub use executor::{ToolError, ToolExecutor, ToolOutput};
pub use shell::ShellExecutor;
