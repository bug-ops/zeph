// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::FileConfig;
use crate::executor::{
    ClaimSource, DiffData, ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params,
};
use crate::registry::{InvocationHint, ToolDef};
use zeph_common::ToolName;

#[derive(Deserialize, JsonSchema)]
pub(crate) struct ReadParams {
    /// File path
    path: String,
    /// Line offset
    offset: Option<u32>,
    /// Max lines
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteParams {
    /// File path
    path: String,
    /// Content to write
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct EditParams {
    /// File path
    path: String,
    /// Text to find
    old_string: String,
    /// Replacement text
    new_string: String,
}

#[derive(Deserialize, JsonSchema)]
struct FindPathParams {
    /// Glob pattern
    pattern: String,
    /// Maximum number of results to return. Defaults to 200.
    max_results: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct GrepParams {
    /// Regex pattern
    pattern: String,
    /// Search path
    path: Option<String>,
    /// Case sensitive
    case_sensitive: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ListDirectoryParams {
    /// Directory path
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct CreateDirectoryParams {
    /// Directory path to create (including parents)
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct DeletePathParams {
    /// Path to delete
    path: String,
    /// Delete non-empty directories recursively
    #[serde(default)]
    recursive: bool,
}

#[derive(Deserialize, JsonSchema)]
struct MovePathParams {
    /// Source path
    source: String,
    /// Destination path
    destination: String,
}

#[derive(Deserialize, JsonSchema)]
struct CopyPathParams {
    /// Source path
    source: String,
    /// Destination path
    destination: String,
}

/// File operations executor sandboxed to allowed paths.
#[derive(Debug)]
pub struct FileExecutor {
    allowed_paths: Vec<PathBuf>,
    read_deny_globs: Option<globset::GlobSet>,
    read_allow_globs: Option<globset::GlobSet>,
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s
        .strip_prefix("~/")
        .or_else(|| if s == "~" { Some("") } else { None })
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path
}

fn build_globset(patterns: &[String]) -> Option<globset::GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        match globset::Glob::new(pattern) {
            Ok(g) => {
                builder.add(g);
            }
            Err(e) => {
                tracing::warn!(pattern = %pattern, err = %e, "invalid file sandbox glob pattern, skipping");
            }
        }
    }
    builder.build().ok().filter(|s| !s.is_empty())
}

impl FileExecutor {
    #[must_use]
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        let paths = if allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            allowed_paths.into_iter().map(expand_tilde).collect()
        };
        Self {
            allowed_paths: paths
                .into_iter()
                .map(|p| p.canonicalize().unwrap_or(p))
                .collect(),
            read_deny_globs: None,
            read_allow_globs: None,
        }
    }

    /// Apply per-path read allow/deny sandbox rules from config.
    #[must_use]
    pub fn with_read_sandbox(mut self, config: &FileConfig) -> Self {
        self.read_deny_globs = build_globset(&config.deny_read);
        self.read_allow_globs = build_globset(&config.allow_read);
        self
    }

    /// Check if the canonical path is permitted by the deny/allow glob rules.
    ///
    /// Always matches against the canonicalized path to prevent symlink bypass (CR-02, MJ-01).
    fn check_read_sandbox(&self, canonical: &Path) -> Result<(), ToolError> {
        let Some(ref deny) = self.read_deny_globs else {
            return Ok(());
        };
        if deny.is_match(canonical)
            && !self
                .read_allow_globs
                .as_ref()
                .is_some_and(|allow| allow.is_match(canonical))
        {
            return Err(ToolError::SandboxViolation {
                path: canonical.display().to_string(),
            });
        }
        Ok(())
    }

    fn validate_path(&self, path: &Path) -> Result<PathBuf, ToolError> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let normalized = normalize_path(&resolved);
        let canonical = resolve_via_ancestors(&normalized);
        if !self.allowed_paths.iter().any(|a| canonical.starts_with(a)) {
            return Err(ToolError::SandboxViolation {
                path: canonical.display().to_string(),
            });
        }
        Ok(canonical)
    }

    /// Execute a tool call by `tool_id` and params.
    ///
    /// # Errors
    ///
    /// Returns `ToolError` on sandbox violations or I/O failures.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "tool.file", skip_all, fields(operation = %tool_id))
    )]
    pub fn execute_file_tool(
        &self,
        tool_id: &str,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<ToolOutput>, ToolError> {
        match tool_id {
            "read" => {
                let p: ReadParams = deserialize_params(params)?;
                self.handle_read(&p)
            }
            "write" => {
                let p: WriteParams = deserialize_params(params)?;
                self.handle_write(&p)
            }
            "edit" => {
                let p: EditParams = deserialize_params(params)?;
                self.handle_edit(&p)
            }
            "find_path" => {
                let p: FindPathParams = deserialize_params(params)?;
                self.handle_find_path(&p)
            }
            "grep" => {
                let p: GrepParams = deserialize_params(params)?;
                self.handle_grep(&p)
            }
            "list_directory" => {
                let p: ListDirectoryParams = deserialize_params(params)?;
                self.handle_list_directory(&p)
            }
            "create_directory" => {
                let p: CreateDirectoryParams = deserialize_params(params)?;
                self.handle_create_directory(&p)
            }
            "delete_path" => {
                let p: DeletePathParams = deserialize_params(params)?;
                self.handle_delete_path(&p)
            }
            "move_path" => {
                let p: MovePathParams = deserialize_params(params)?;
                self.handle_move_path(&p)
            }
            "copy_path" => {
                let p: CopyPathParams = deserialize_params(params)?;
                self.handle_copy_path(&p)
            }
            _ => Ok(None),
        }
    }

    fn handle_read(&self, params: &ReadParams) -> Result<Option<ToolOutput>, ToolError> {
        let path = self.validate_path(Path::new(&params.path))?;
        self.check_read_sandbox(&path)?;
        let content = std::fs::read_to_string(&path)?;

        let offset = params.offset.unwrap_or(0) as usize;
        let limit = params.limit.map_or(usize::MAX, |l| l as usize);

        let selected: Vec<String> = content
            .lines()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, line)| format!("{:>4}\t{line}", offset + i + 1))
            .collect();

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("read"),
            summary: selected.join("\n"),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_write(&self, params: &WriteParams) -> Result<Option<ToolOutput>, ToolError> {
        let path = self.validate_path(Path::new(&params.path))?;
        let old_content = std::fs::read_to_string(&path).unwrap_or_default();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &params.content)?;

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("write"),
            summary: format!("Wrote {} bytes to {}", params.content.len(), params.path),
            blocks_executed: 1,
            filter_stats: None,
            diff: Some(DiffData {
                file_path: params.path.clone(),
                old_content,
                new_content: params.content.clone(),
            }),
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_edit(&self, params: &EditParams) -> Result<Option<ToolOutput>, ToolError> {
        let path = self.validate_path(Path::new(&params.path))?;
        let content = std::fs::read_to_string(&path)?;

        if !content.contains(&params.old_string) {
            return Err(ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("old_string not found in {}", params.path),
            )));
        }

        let new_content = content.replacen(&params.old_string, &params.new_string, 1);
        std::fs::write(&path, &new_content)?;

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("edit"),
            summary: format!("Edited {}", params.path),
            blocks_executed: 1,
            filter_stats: None,
            diff: Some(DiffData {
                file_path: params.path.clone(),
                old_content: content,
                new_content,
            }),
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_find_path(&self, params: &FindPathParams) -> Result<Option<ToolOutput>, ToolError> {
        let limit = params.max_results.unwrap_or(200).max(1);
        let mut matches: Vec<String> = glob::glob(&params.pattern)
            .map_err(|e| {
                ToolError::Execution(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                ))
            })?
            .filter_map(Result::ok)
            .filter(|p| {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
                self.allowed_paths.iter().any(|a| canonical.starts_with(a))
            })
            .map(|p| p.display().to_string())
            .take(limit + 1)
            .collect();

        let truncated = matches.len() > limit;
        if truncated {
            matches.truncate(limit);
        }

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("find_path"),
            summary: if matches.is_empty() {
                format!("No files matching: {}", params.pattern)
            } else if truncated {
                format!(
                    "{}\n... and more results (showing first {limit})",
                    matches.join("\n")
                )
            } else {
                matches.join("\n")
            },
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_grep(&self, params: &GrepParams) -> Result<Option<ToolOutput>, ToolError> {
        let search_path = params.path.as_deref().unwrap_or(".");
        let case_sensitive = params.case_sensitive.unwrap_or(true);
        let path = self.validate_path(Path::new(search_path))?;

        let regex = if case_sensitive {
            regex::Regex::new(&params.pattern)
        } else {
            regex::RegexBuilder::new(&params.pattern)
                .case_insensitive(true)
                .build()
        }
        .map_err(|e| {
            ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;

        let sandbox = |p: &Path| self.check_read_sandbox(p);
        let mut results = Vec::new();
        grep_recursive(&path, &regex, &mut results, 100, &sandbox)?;

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("grep"),
            summary: if results.is_empty() {
                format!("No matches for: {}", params.pattern)
            } else {
                results.join("\n")
            },
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_list_directory(
        &self,
        params: &ListDirectoryParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path = self.validate_path(Path::new(&params.path))?;

        if !path.is_dir() {
            return Err(ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("{} is not a directory", params.path),
            )));
        }

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut symlinks = Vec::new();

        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Use symlink_metadata (lstat) to detect symlinks without following them.
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_symlink() {
                symlinks.push(format!("[symlink] {name}"));
            } else if meta.is_dir() {
                dirs.push(format!("[dir]  {name}"));
            } else {
                files.push(format!("[file] {name}"));
            }
        }

        dirs.sort();
        files.sort();
        symlinks.sort();

        let mut entries = dirs;
        entries.extend(files);
        entries.extend(symlinks);

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("list_directory"),
            summary: if entries.is_empty() {
                format!("Empty directory: {}", params.path)
            } else {
                entries.join("\n")
            },
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_create_directory(
        &self,
        params: &CreateDirectoryParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path = self.validate_path(Path::new(&params.path))?;
        std::fs::create_dir_all(&path)?;

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("create_directory"),
            summary: format!("Created directory: {}", params.path),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_delete_path(
        &self,
        params: &DeletePathParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path = self.validate_path(Path::new(&params.path))?;

        // Refuse to delete the sandbox root itself
        if self.allowed_paths.iter().any(|a| &path == a) {
            return Err(ToolError::SandboxViolation {
                path: path.display().to_string(),
            });
        }

        if path.is_dir() {
            if params.recursive {
                // Accepted risk: remove_dir_all has no depth/size guard within the sandbox.
                // Resource exhaustion is bounded by the filesystem and OS limits.
                std::fs::remove_dir_all(&path)?;
            } else {
                // remove_dir only succeeds on empty dirs
                std::fs::remove_dir(&path)?;
            }
        } else {
            std::fs::remove_file(&path)?;
        }

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("delete_path"),
            summary: format!("Deleted: {}", params.path),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_move_path(&self, params: &MovePathParams) -> Result<Option<ToolOutput>, ToolError> {
        let src = self.validate_path(Path::new(&params.source))?;
        let dst = self.validate_path(Path::new(&params.destination))?;
        std::fs::rename(&src, &dst)?;

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("move_path"),
            summary: format!("Moved: {} -> {}", params.source, params.destination),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }

    fn handle_copy_path(&self, params: &CopyPathParams) -> Result<Option<ToolOutput>, ToolError> {
        let src = self.validate_path(Path::new(&params.source))?;
        let dst = self.validate_path(Path::new(&params.destination))?;

        if src.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dst)?;
        }

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("copy_path"),
            summary: format!("Copied: {} -> {}", params.source, params.destination),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
        }))
    }
}

impl ToolExecutor for FileExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "tool.file.execute_call", skip_all, fields(tool_id = %call.tool_id))
    )]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        self.execute_file_tool(call.tool_id.as_str(), &call.params)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "read".into(),
                description: "Read file contents with line numbers.\n\nParameters: path (string, required) - absolute or relative file path; offset (integer, optional) - start line (0-based); limit (integer, optional) - max lines to return\nReturns: file content with line numbers, or error if file not found\nErrors: SandboxViolation if path outside allowed dirs; Execution if file not found or unreadable\nExample: {\"path\": \"src/main.rs\", \"offset\": 10, \"limit\": 50}".into(),
                schema: schemars::schema_for!(ReadParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "write".into(),
                description: "Create or overwrite a file with the given content.\n\nParameters: path (string, required) - file path; content (string, required) - full file content\nReturns: confirmation message with bytes written\nErrors: SandboxViolation if path outside allowed dirs; Execution on I/O failure\nExample: {\"path\": \"output.txt\", \"content\": \"Hello, world!\"}".into(),
                schema: schemars::schema_for!(WriteParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "edit".into(),
                description: "Find and replace a text substring in a file.\n\nParameters: path (string, required) - file path; old_string (string, required) - exact text to find; new_string (string, required) - replacement text\nReturns: confirmation with match count, or error if old_string not found\nErrors: SandboxViolation; Execution if file not found or old_string has no matches\nExample: {\"path\": \"config.toml\", \"old_string\": \"debug = true\", \"new_string\": \"debug = false\"}".into(),
                schema: schemars::schema_for!(EditParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "find_path".into(),
                description: "Find files and directories matching a glob pattern.\n\nParameters: pattern (string, required) - glob pattern (e.g. \"**/*.rs\", \"src/*.toml\")\nReturns: newline-separated list of matching paths, or \"(no matches)\" if none found\nErrors: SandboxViolation if search root is outside allowed dirs\nExample: {\"pattern\": \"**/*.rs\"}".into(),
                schema: schemars::schema_for!(FindPathParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "grep".into(),
                description: "Search file contents for lines matching a regex pattern.\n\nParameters: pattern (string, required) - regex pattern; path (string, optional) - directory or file to search (default: cwd); case_sensitive (boolean, optional) - default true\nReturns: matching lines with file paths and line numbers, or \"(no matches)\"\nErrors: SandboxViolation; InvalidParams if regex is invalid\nExample: {\"pattern\": \"fn main\", \"path\": \"src/\"}".into(),
                schema: schemars::schema_for!(GrepParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "list_directory".into(),
                description: "List files and subdirectories in a directory.\n\nParameters: path (string, required) - directory path\nReturns: sorted listing with [dir]/[file] prefixes, or \"Empty directory\" if empty\nErrors: SandboxViolation; Execution if path is not a directory or does not exist\nExample: {\"path\": \"src/\"}".into(),
                schema: schemars::schema_for!(ListDirectoryParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "create_directory".into(),
                description: "Create a directory, including any missing parent directories.\n\nParameters: path (string, required) - directory path to create\nReturns: confirmation message\nErrors: SandboxViolation; Execution on I/O failure\nExample: {\"path\": \"src/utils/helpers\"}".into(),
                schema: schemars::schema_for!(CreateDirectoryParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "delete_path".into(),
                description: "Delete a file or directory.\n\nParameters: path (string, required) - path to delete; recursive (boolean, optional) - if true, delete non-empty directories recursively (default: false)\nReturns: confirmation message\nErrors: SandboxViolation; Execution if path not found or directory non-empty without recursive=true\nExample: {\"path\": \"tmp/old_file.txt\"}".into(),
                schema: schemars::schema_for!(DeletePathParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "move_path".into(),
                description: "Move or rename a file or directory.\n\nParameters: source (string, required) - current path; destination (string, required) - new path\nReturns: confirmation message\nErrors: SandboxViolation if either path is outside allowed dirs; Execution if source not found\nExample: {\"source\": \"old_name.rs\", \"destination\": \"new_name.rs\"}".into(),
                schema: schemars::schema_for!(MovePathParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "copy_path".into(),
                description: "Copy a file or directory to a new location.\n\nParameters: source (string, required) - path to copy; destination (string, required) - target path\nReturns: confirmation message\nErrors: SandboxViolation; Execution if source not found or I/O failure\nExample: {\"source\": \"template.rs\", \"destination\": \"new_module.rs\"}".into(),
                schema: schemars::schema_for!(CopyPathParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
        ]
    }
}

/// Lexically normalize a path by collapsing `.` and `..` components without
/// any filesystem access. This prevents `..` components from bypassing the
/// sandbox check inside `validate_path`.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    // On Windows, paths may have a drive prefix (e.g. `D:` or `\\?\D:`).
    // We track it separately so that `RootDir` (the `\` after the drive letter)
    // does not accidentally clear the prefix from the stack.
    let mut prefix: Option<std::ffi::OsString> = None;
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop the sentinel "/" root entry.
                if stack.last().is_some_and(|s| s != "/") {
                    stack.pop();
                }
            }
            Component::Normal(name) => stack.push(name.to_owned()),
            Component::RootDir => {
                if prefix.is_none() {
                    // Unix absolute path: treat "/" as the root sentinel.
                    stack.clear();
                    stack.push(std::ffi::OsString::from("/"));
                }
                // On Windows, RootDir follows the drive Prefix and is just the
                // path separator — the prefix is already recorded, so skip it.
            }
            Component::Prefix(p) => {
                stack.clear();
                prefix = Some(p.as_os_str().to_owned());
            }
        }
    }
    if let Some(drive) = prefix {
        // Windows: reconstruct "DRIVE:\" (absolute) then append normal components.
        let mut s = drive.to_string_lossy().into_owned();
        s.push('\\');
        let mut result = PathBuf::from(s);
        for part in &stack {
            result.push(part);
        }
        result
    } else {
        let mut result = PathBuf::new();
        for (i, part) in stack.iter().enumerate() {
            if i == 0 && part == "/" {
                result.push("/");
            } else {
                result.push(part);
            }
        }
        result
    }
}

/// Canonicalize a path by walking up to the nearest existing ancestor.
///
/// Walks up `path` until an existing ancestor is found, calls `canonicalize()` on it
/// (which follows symlinks), then re-appends the non-existing suffix. The sandbox check
/// in `validate_path` uses `starts_with` on the resulting canonical path, so symlinks
/// that resolve outside `allowed_paths` are correctly rejected.
fn resolve_via_ancestors(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = PathBuf::new();
    while !existing.exists() {
        if let Some(parent) = existing.parent() {
            if let Some(name) = existing.file_name() {
                if suffix.as_os_str().is_empty() {
                    suffix = PathBuf::from(name);
                } else {
                    suffix = PathBuf::from(name).join(&suffix);
                }
            }
            existing = parent;
        } else {
            break;
        }
    }
    let base = existing.canonicalize().unwrap_or(existing.to_path_buf());
    if suffix.as_os_str().is_empty() {
        base
    } else {
        base.join(&suffix)
    }
}

const IGNORED_DIRS: &[&str] = &[".git", "target", "node_modules", ".hg"];

fn grep_recursive(
    path: &Path,
    regex: &regex::Regex,
    results: &mut Vec<String>,
    limit: usize,
    sandbox: &impl Fn(&Path) -> Result<(), ToolError>,
) -> Result<(), ToolError> {
    if results.len() >= limit {
        return Ok(());
    }
    if path.is_file() {
        // Canonicalize before sandbox check to prevent symlink bypass (SEC-01).
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if sandbox(&canonical).is_err() {
            return Ok(());
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            for (i, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    results.push(format!("{}:{}: {line}", path.display(), i + 1));
                    if results.len() >= limit {
                        return Ok(());
                    }
                }
            }
        }
    } else if path.is_dir() {
        let entries = std::fs::read_dir(path)?;
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str());
            if name.is_some_and(|n| n.starts_with('.') || IGNORED_DIRS.contains(&n)) {
                continue;
            }
            grep_recursive(&p, regex, results, limit, sandbox)?;
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), ToolError> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        // Use symlink_metadata (lstat) so we classify symlinks without following them.
        // Symlinks are skipped to prevent escaping the sandbox via a symlink pointing
        // to a path outside allowed_paths.
        let meta = std::fs::symlink_metadata(entry.path())?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if meta.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if meta.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
        // Symlinks are intentionally skipped.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn make_params(
        pairs: &[(&str, serde_json::Value)],
    ) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn read_file() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
        let result = exec.execute_file_tool("read", &params).unwrap().unwrap();
        assert_eq!(result.tool_name, "read");
        assert!(result.summary.contains("line1"));
        assert!(result.summary.contains("line3"));
    }

    #[test]
    fn read_with_offset_and_limit() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("path", serde_json::json!(file.to_str().unwrap())),
            ("offset", serde_json::json!(1)),
            ("limit", serde_json::json!(2)),
        ]);
        let result = exec.execute_file_tool("read", &params).unwrap().unwrap();
        assert!(result.summary.contains('b'));
        assert!(result.summary.contains('c'));
        assert!(!result.summary.contains('a'));
        assert!(!result.summary.contains('d'));
    }

    #[test]
    fn write_file() {
        let dir = temp_dir();
        let file = dir.path().join("out.txt");

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("path", serde_json::json!(file.to_str().unwrap())),
            ("content", serde_json::json!("hello world")),
        ]);
        let result = exec.execute_file_tool("write", &params).unwrap().unwrap();
        assert!(result.summary.contains("11 bytes"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn edit_file() {
        let dir = temp_dir();
        let file = dir.path().join("edit.txt");
        fs::write(&file, "foo bar baz").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("path", serde_json::json!(file.to_str().unwrap())),
            ("old_string", serde_json::json!("bar")),
            ("new_string", serde_json::json!("qux")),
        ]);
        let result = exec.execute_file_tool("edit", &params).unwrap().unwrap();
        assert!(result.summary.contains("Edited"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "foo qux baz");
    }

    #[test]
    fn edit_not_found() {
        let dir = temp_dir();
        let file = dir.path().join("edit.txt");
        fs::write(&file, "foo bar").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("path", serde_json::json!(file.to_str().unwrap())),
            ("old_string", serde_json::json!("nonexistent")),
            ("new_string", serde_json::json!("x")),
        ]);
        let result = exec.execute_file_tool("edit", &params);
        assert!(result.is_err());
    }

    #[test]
    fn sandbox_violation() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!("/etc/passwd"))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn unknown_tool_returns_none() {
        let exec = FileExecutor::new(vec![]);
        let params = serde_json::Map::new();
        let result = exec.execute_file_tool("unknown", &params).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_path_finds_files() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.rs"), "").unwrap();
        fs::write(dir.path().join("b.rs"), "").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let pattern = format!("{}/*.rs", dir.path().display());
        let params = make_params(&[("pattern", serde_json::json!(pattern))]);
        let result = exec
            .execute_file_tool("find_path", &params)
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("a.rs"));
        assert!(result.summary.contains("b.rs"));
    }

    #[test]
    fn grep_finds_matches() {
        let dir = temp_dir();
        fs::write(
            dir.path().join("test.txt"),
            "hello world\nfoo bar\nhello again\n",
        )
        .unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("pattern", serde_json::json!("hello")),
            ("path", serde_json::json!(dir.path().to_str().unwrap())),
        ]);
        let result = exec.execute_file_tool("grep", &params).unwrap().unwrap();
        assert!(result.summary.contains("hello world"));
        assert!(result.summary.contains("hello again"));
        assert!(!result.summary.contains("foo bar"));
    }

    #[test]
    fn write_sandbox_bypass_nonexistent_path() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("path", serde_json::json!("/tmp/evil/escape.txt")),
            ("content", serde_json::json!("pwned")),
        ]);
        let result = exec.execute_file_tool("write", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
        assert!(!Path::new("/tmp/evil/escape.txt").exists());
    }

    #[test]
    fn find_path_filters_outside_sandbox() {
        let sandbox = temp_dir();
        let outside = temp_dir();
        fs::write(outside.path().join("secret.rs"), "secret").unwrap();

        let exec = FileExecutor::new(vec![sandbox.path().to_path_buf()]);
        let pattern = format!("{}/*.rs", outside.path().display());
        let params = make_params(&[("pattern", serde_json::json!(pattern))]);
        let result = exec
            .execute_file_tool("find_path", &params)
            .unwrap()
            .unwrap();
        assert!(!result.summary.contains("secret.rs"));
    }

    #[tokio::test]
    async fn tool_executor_execute_tool_call_delegates() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "content").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: ToolName::new("read"),
            params: make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]),
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.tool_name, "read");
        assert!(result.summary.contains("content"));
    }

    #[test]
    fn tool_executor_tool_definitions_lists_all() {
        let exec = FileExecutor::new(vec![]);
        let defs = exec.tool_definitions();
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
        assert!(ids.contains(&"read"));
        assert!(ids.contains(&"write"));
        assert!(ids.contains(&"edit"));
        assert!(ids.contains(&"find_path"));
        assert!(ids.contains(&"grep"));
        assert!(ids.contains(&"list_directory"));
        assert!(ids.contains(&"create_directory"));
        assert!(ids.contains(&"delete_path"));
        assert!(ids.contains(&"move_path"));
        assert!(ids.contains(&"copy_path"));
        assert_eq!(defs.len(), 10);
    }

    #[test]
    fn grep_relative_path_validated() {
        let sandbox = temp_dir();
        let exec = FileExecutor::new(vec![sandbox.path().to_path_buf()]);
        let params = make_params(&[
            ("pattern", serde_json::json!("password")),
            ("path", serde_json::json!("../../etc")),
        ]);
        let result = exec.execute_file_tool("grep", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn tool_definitions_returns_ten_tools() {
        let exec = FileExecutor::new(vec![]);
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 10);
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
        assert_eq!(
            ids,
            vec![
                "read",
                "write",
                "edit",
                "find_path",
                "grep",
                "list_directory",
                "create_directory",
                "delete_path",
                "move_path",
                "copy_path",
            ]
        );
    }

    #[test]
    fn tool_definitions_all_use_tool_call() {
        let exec = FileExecutor::new(vec![]);
        for def in exec.tool_definitions() {
            assert_eq!(def.invocation, InvocationHint::ToolCall);
        }
    }

    #[test]
    fn tool_definitions_read_schema_has_params() {
        let exec = FileExecutor::new(vec![]);
        let defs = exec.tool_definitions();
        let read = defs.iter().find(|d| d.id.as_ref() == "read").unwrap();
        let obj = read.schema.as_object().unwrap();
        let props = obj["properties"].as_object().unwrap();
        assert!(props.contains_key("path"));
        assert!(props.contains_key("offset"));
        assert!(props.contains_key("limit"));
    }

    #[test]
    fn missing_required_path_returns_invalid_params() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = serde_json::Map::new();
        let result = exec.execute_file_tool("read", &params);
        assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
    }

    // --- list_directory tests ---

    #[test]
    fn list_directory_returns_entries() {
        let dir = temp_dir();
        fs::write(dir.path().join("file.txt"), "").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(dir.path().to_str().unwrap()))]);
        let result = exec
            .execute_file_tool("list_directory", &params)
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("[dir]  subdir"));
        assert!(result.summary.contains("[file] file.txt"));
        // dirs listed before files
        let dir_pos = result.summary.find("[dir]").unwrap();
        let file_pos = result.summary.find("[file]").unwrap();
        assert!(dir_pos < file_pos);
    }

    #[test]
    fn list_directory_empty_dir() {
        let dir = temp_dir();
        let subdir = dir.path().join("empty");
        fs::create_dir(&subdir).unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(subdir.to_str().unwrap()))]);
        let result = exec
            .execute_file_tool("list_directory", &params)
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("Empty directory"));
    }

    #[test]
    fn list_directory_sandbox_violation() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!("/etc"))]);
        let result = exec.execute_file_tool("list_directory", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn list_directory_nonexistent_returns_error() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let missing = dir.path().join("nonexistent");
        let params = make_params(&[("path", serde_json::json!(missing.to_str().unwrap()))]);
        let result = exec.execute_file_tool("list_directory", &params);
        assert!(result.is_err());
    }

    #[test]
    fn list_directory_on_file_returns_error() {
        let dir = temp_dir();
        let file = dir.path().join("file.txt");
        fs::write(&file, "content").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
        let result = exec.execute_file_tool("list_directory", &params);
        assert!(result.is_err());
    }

    // --- create_directory tests ---

    #[test]
    fn create_directory_creates_nested() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let nested = dir.path().join("a/b/c");
        let params = make_params(&[("path", serde_json::json!(nested.to_str().unwrap()))]);
        let result = exec
            .execute_file_tool("create_directory", &params)
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("Created"));
        assert!(nested.is_dir());
    }

    #[test]
    fn create_directory_sandbox_violation() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!("/tmp/evil_dir"))]);
        let result = exec.execute_file_tool("create_directory", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    // --- delete_path tests ---

    #[test]
    fn delete_path_file() {
        let dir = temp_dir();
        let file = dir.path().join("del.txt");
        fs::write(&file, "bye").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
        exec.execute_file_tool("delete_path", &params)
            .unwrap()
            .unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn delete_path_empty_directory() {
        let dir = temp_dir();
        let subdir = dir.path().join("empty_sub");
        fs::create_dir(&subdir).unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(subdir.to_str().unwrap()))]);
        exec.execute_file_tool("delete_path", &params)
            .unwrap()
            .unwrap();
        assert!(!subdir.exists());
    }

    #[test]
    fn delete_path_non_empty_dir_without_recursive_fails() {
        let dir = temp_dir();
        let subdir = dir.path().join("nonempty");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("file.txt"), "x").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(subdir.to_str().unwrap()))]);
        let result = exec.execute_file_tool("delete_path", &params);
        assert!(result.is_err());
    }

    #[test]
    fn delete_path_recursive() {
        let dir = temp_dir();
        let subdir = dir.path().join("recurse");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("f.txt"), "x").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("path", serde_json::json!(subdir.to_str().unwrap())),
            ("recursive", serde_json::json!(true)),
        ]);
        exec.execute_file_tool("delete_path", &params)
            .unwrap()
            .unwrap();
        assert!(!subdir.exists());
    }

    #[test]
    fn delete_path_sandbox_violation() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!("/etc/hosts"))]);
        let result = exec.execute_file_tool("delete_path", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn delete_path_refuses_sandbox_root() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("path", serde_json::json!(dir.path().to_str().unwrap())),
            ("recursive", serde_json::json!(true)),
        ]);
        let result = exec.execute_file_tool("delete_path", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    // --- move_path tests ---

    #[test]
    fn move_path_renames_file() {
        let dir = temp_dir();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        fs::write(&src, "data").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("source", serde_json::json!(src.to_str().unwrap())),
            ("destination", serde_json::json!(dst.to_str().unwrap())),
        ]);
        exec.execute_file_tool("move_path", &params)
            .unwrap()
            .unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "data");
    }

    #[test]
    fn move_path_cross_sandbox_denied() {
        let sandbox = temp_dir();
        let outside = temp_dir();
        let src = sandbox.path().join("src.txt");
        fs::write(&src, "x").unwrap();

        let exec = FileExecutor::new(vec![sandbox.path().to_path_buf()]);
        let dst = outside.path().join("dst.txt");
        let params = make_params(&[
            ("source", serde_json::json!(src.to_str().unwrap())),
            ("destination", serde_json::json!(dst.to_str().unwrap())),
        ]);
        let result = exec.execute_file_tool("move_path", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    // --- copy_path tests ---

    #[test]
    fn copy_path_file() {
        let dir = temp_dir();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        fs::write(&src, "hello").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("source", serde_json::json!(src.to_str().unwrap())),
            ("destination", serde_json::json!(dst.to_str().unwrap())),
        ]);
        exec.execute_file_tool("copy_path", &params)
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(&src).unwrap(), "hello");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello");
    }

    #[test]
    fn copy_path_directory_recursive() {
        let dir = temp_dir();
        let src_dir = dir.path().join("src_dir");
        fs::create_dir(&src_dir).unwrap();
        fs::write(src_dir.join("a.txt"), "aaa").unwrap();

        let dst_dir = dir.path().join("dst_dir");

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("source", serde_json::json!(src_dir.to_str().unwrap())),
            ("destination", serde_json::json!(dst_dir.to_str().unwrap())),
        ]);
        exec.execute_file_tool("copy_path", &params)
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(dst_dir.join("a.txt")).unwrap(), "aaa");
    }

    #[test]
    fn copy_path_sandbox_violation() {
        let sandbox = temp_dir();
        let outside = temp_dir();
        let src = sandbox.path().join("src.txt");
        fs::write(&src, "x").unwrap();

        let exec = FileExecutor::new(vec![sandbox.path().to_path_buf()]);
        let dst = outside.path().join("dst.txt");
        let params = make_params(&[
            ("source", serde_json::json!(src.to_str().unwrap())),
            ("destination", serde_json::json!(dst.to_str().unwrap())),
        ]);
        let result = exec.execute_file_tool("copy_path", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    // CR-11: invalid glob pattern returns error
    #[test]
    fn find_path_invalid_pattern_returns_error() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("pattern", serde_json::json!("[invalid"))]);
        let result = exec.execute_file_tool("find_path", &params);
        assert!(result.is_err());
    }

    // CR-12: create_directory is idempotent on existing dir
    #[test]
    fn create_directory_idempotent() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let target = dir.path().join("exists");
        fs::create_dir(&target).unwrap();

        let params = make_params(&[("path", serde_json::json!(target.to_str().unwrap()))]);
        let result = exec.execute_file_tool("create_directory", &params);
        assert!(result.is_ok());
        assert!(target.is_dir());
    }

    // CR-13: move_path source sandbox violation
    #[test]
    fn move_path_source_sandbox_violation() {
        let sandbox = temp_dir();
        let outside = temp_dir();
        let src = outside.path().join("src.txt");
        fs::write(&src, "x").unwrap();

        let exec = FileExecutor::new(vec![sandbox.path().to_path_buf()]);
        let dst = sandbox.path().join("dst.txt");
        let params = make_params(&[
            ("source", serde_json::json!(src.to_str().unwrap())),
            ("destination", serde_json::json!(dst.to_str().unwrap())),
        ]);
        let result = exec.execute_file_tool("move_path", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    // CR-13: copy_path source sandbox violation
    #[test]
    fn copy_path_source_sandbox_violation() {
        let sandbox = temp_dir();
        let outside = temp_dir();
        let src = outside.path().join("src.txt");
        fs::write(&src, "x").unwrap();

        let exec = FileExecutor::new(vec![sandbox.path().to_path_buf()]);
        let dst = sandbox.path().join("dst.txt");
        let params = make_params(&[
            ("source", serde_json::json!(src.to_str().unwrap())),
            ("destination", serde_json::json!(dst.to_str().unwrap())),
        ]);
        let result = exec.execute_file_tool("copy_path", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    // CR-01: copy_dir_recursive skips symlinks
    #[cfg(unix)]
    #[test]
    fn copy_dir_skips_symlinks() {
        let dir = temp_dir();
        let src_dir = dir.path().join("src");
        fs::create_dir(&src_dir).unwrap();
        fs::write(src_dir.join("real.txt"), "real").unwrap();

        // Create a symlink inside src pointing outside sandbox
        let outside = temp_dir();
        std::os::unix::fs::symlink(outside.path(), src_dir.join("link")).unwrap();

        let dst_dir = dir.path().join("dst");
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[
            ("source", serde_json::json!(src_dir.to_str().unwrap())),
            ("destination", serde_json::json!(dst_dir.to_str().unwrap())),
        ]);
        exec.execute_file_tool("copy_path", &params)
            .unwrap()
            .unwrap();
        // Real file copied
        assert_eq!(
            fs::read_to_string(dst_dir.join("real.txt")).unwrap(),
            "real"
        );
        // Symlink not copied
        assert!(!dst_dir.join("link").exists());
    }

    // CR-04: list_directory detects symlinks
    #[cfg(unix)]
    #[test]
    fn list_directory_shows_symlinks() {
        let dir = temp_dir();
        let target = dir.path().join("target.txt");
        fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(dir.path().to_str().unwrap()))]);
        let result = exec
            .execute_file_tool("list_directory", &params)
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("[symlink] link"));
        assert!(result.summary.contains("[file] target.txt"));
    }

    #[test]
    fn tilde_path_is_expanded() {
        let exec = FileExecutor::new(vec![PathBuf::from("~/nonexistent_subdir_for_test")]);
        assert!(
            !exec.allowed_paths[0].to_string_lossy().starts_with('~'),
            "tilde was not expanded: {:?}",
            exec.allowed_paths[0]
        );
    }

    #[test]
    fn absolute_path_unchanged() {
        let exec = FileExecutor::new(vec![PathBuf::from("/tmp")]);
        // On macOS /tmp is a symlink to /private/tmp; canonicalize resolves it.
        // The invariant is that the result is absolute and tilde-free.
        let p = exec.allowed_paths[0].to_string_lossy();
        assert!(
            p.starts_with('/'),
            "expected absolute path, got: {:?}",
            exec.allowed_paths[0]
        );
        assert!(
            !p.starts_with('~'),
            "tilde must not appear in result: {:?}",
            exec.allowed_paths[0]
        );
    }

    #[test]
    fn tilde_only_expands_to_home() {
        let exec = FileExecutor::new(vec![PathBuf::from("~")]);
        assert!(
            !exec.allowed_paths[0].to_string_lossy().starts_with('~'),
            "bare tilde was not expanded: {:?}",
            exec.allowed_paths[0]
        );
    }

    #[test]
    fn empty_allowed_paths_uses_cwd() {
        let exec = FileExecutor::new(vec![]);
        assert!(
            !exec.allowed_paths.is_empty(),
            "expected cwd fallback, got empty allowed_paths"
        );
    }

    // --- normalize_path tests ---

    #[test]
    fn normalize_path_normal_path() {
        assert_eq!(
            normalize_path(Path::new("/tmp/sandbox/file.txt")),
            PathBuf::from("/tmp/sandbox/file.txt")
        );
    }

    #[test]
    fn normalize_path_collapses_dot() {
        assert_eq!(
            normalize_path(Path::new("/tmp/sandbox/./file.txt")),
            PathBuf::from("/tmp/sandbox/file.txt")
        );
    }

    #[test]
    fn normalize_path_collapses_dotdot() {
        assert_eq!(
            normalize_path(Path::new("/tmp/sandbox/nonexistent/../../etc/passwd")),
            PathBuf::from("/tmp/etc/passwd")
        );
    }

    #[test]
    fn normalize_path_nested_dotdot() {
        assert_eq!(
            normalize_path(Path::new("/tmp/sandbox/a/b/../../../etc/passwd")),
            PathBuf::from("/tmp/etc/passwd")
        );
    }

    #[test]
    fn normalize_path_at_sandbox_boundary() {
        assert_eq!(
            normalize_path(Path::new("/tmp/sandbox")),
            PathBuf::from("/tmp/sandbox")
        );
    }

    // --- validate_path dotdot bypass tests ---

    #[test]
    fn validate_path_dotdot_bypass_nonexistent_blocked() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        // /sandbox/nonexistent/../../etc/passwd normalizes to /etc/passwd — must be blocked
        let escape = format!("{}/nonexistent/../../etc/passwd", dir.path().display());
        let params = make_params(&[("path", serde_json::json!(escape))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(
            matches!(result, Err(ToolError::SandboxViolation { .. })),
            "expected SandboxViolation for dotdot bypass, got {result:?}"
        );
    }

    #[test]
    fn validate_path_dotdot_nested_bypass_blocked() {
        let dir = temp_dir();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let escape = format!("{}/a/b/../../../etc/shadow", dir.path().display());
        let params = make_params(&[("path", serde_json::json!(escape))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn validate_path_inside_sandbox_passes() {
        let dir = temp_dir();
        let file = dir.path().join("allowed.txt");
        fs::write(&file, "ok").unwrap();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_dot_components_inside_sandbox_passes() {
        let dir = temp_dir();
        let file = dir.path().join("sub/file.txt");
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(&file, "ok").unwrap();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let dotpath = format!("{}/sub/./file.txt", dir.path().display());
        let params = make_params(&[("path", serde_json::json!(dotpath))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(result.is_ok());
    }

    // --- #2489: per-path read allow/deny sandbox tests ---

    #[test]
    fn read_sandbox_deny_blocks_file() {
        let dir = temp_dir();
        let secret = dir.path().join(".env");
        fs::write(&secret, "SECRET=abc").unwrap();

        let config = crate::config::FileConfig {
            deny_read: vec!["**/.env".to_owned()],
            allow_read: vec![],
        };
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]).with_read_sandbox(&config);
        let params = make_params(&[("path", serde_json::json!(secret.to_str().unwrap()))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(
            matches!(result, Err(ToolError::SandboxViolation { .. })),
            "expected SandboxViolation, got: {result:?}"
        );
    }

    #[test]
    fn read_sandbox_allow_overrides_deny() {
        let dir = temp_dir();
        let public = dir.path().join("public.env");
        fs::write(&public, "VAR=ok").unwrap();

        let config = crate::config::FileConfig {
            deny_read: vec!["**/*.env".to_owned()],
            allow_read: vec![format!("**/public.env")],
        };
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]).with_read_sandbox(&config);
        let params = make_params(&[("path", serde_json::json!(public.to_str().unwrap()))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(
            result.is_ok(),
            "allow override should permit read: {result:?}"
        );
    }

    #[test]
    fn read_sandbox_empty_deny_allows_all() {
        let dir = temp_dir();
        let file = dir.path().join("data.txt");
        fs::write(&file, "data").unwrap();

        let config = crate::config::FileConfig::default();
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]).with_read_sandbox(&config);
        let params = make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]);
        let result = exec.execute_file_tool("read", &params);
        assert!(result.is_ok(), "empty deny should allow all: {result:?}");
    }

    #[test]
    fn read_sandbox_grep_skips_denied_files() {
        let dir = temp_dir();
        let allowed = dir.path().join("allowed.txt");
        let denied = dir.path().join(".env");
        fs::write(&allowed, "needle").unwrap();
        fs::write(&denied, "needle").unwrap();

        let config = crate::config::FileConfig {
            deny_read: vec!["**/.env".to_owned()],
            allow_read: vec![],
        };
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]).with_read_sandbox(&config);
        let params = make_params(&[
            ("pattern", serde_json::json!("needle")),
            ("path", serde_json::json!(dir.path().to_str().unwrap())),
        ]);
        let result = exec.execute_file_tool("grep", &params).unwrap().unwrap();
        // Should find match in allowed.txt but not in .env
        assert!(
            result.summary.contains("allowed.txt"),
            "expected match in allowed.txt: {}",
            result.summary
        );
        assert!(
            !result.summary.contains(".env"),
            "should not match in denied .env: {}",
            result.summary
        );
    }

    #[test]
    fn find_path_truncates_at_default_limit() {
        let dir = temp_dir();
        // Create 205 files.
        for i in 0..205u32 {
            fs::write(dir.path().join(format!("file_{i:04}.txt")), "").unwrap();
        }
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let pattern = dir.path().join("*.txt").to_str().unwrap().to_owned();
        let params = make_params(&[("pattern", serde_json::json!(pattern))]);
        let result = exec
            .execute_file_tool("find_path", &params)
            .unwrap()
            .unwrap();
        // Default limit is 200; summary should mention truncation.
        assert!(
            result.summary.contains("and more results"),
            "expected truncation notice: {}",
            &result.summary[..100.min(result.summary.len())]
        );
        // Should contain exactly 200 lines before the truncation notice.
        let lines: Vec<&str> = result.summary.lines().collect();
        assert_eq!(lines.len(), 201, "expected 200 paths + 1 truncation line");
    }

    #[test]
    fn find_path_respects_max_results() {
        let dir = temp_dir();
        for i in 0..10u32 {
            fs::write(dir.path().join(format!("f_{i}.txt")), "").unwrap();
        }
        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let pattern = dir.path().join("*.txt").to_str().unwrap().to_owned();
        let params = make_params(&[
            ("pattern", serde_json::json!(pattern)),
            ("max_results", serde_json::json!(5)),
        ]);
        let result = exec
            .execute_file_tool("find_path", &params)
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("and more results"));
        let paths: Vec<&str> = result
            .summary
            .lines()
            .filter(|l| {
                std::path::Path::new(l)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
            })
            .collect();
        assert_eq!(paths.len(), 5);
    }
}
