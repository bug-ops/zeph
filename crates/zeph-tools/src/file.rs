use std::collections::HashMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::{InvocationHint, ToolDef};

// Schema-only: fields are read by schemars derive, not by Rust code directly.
#[derive(JsonSchema)]
#[allow(dead_code)]
pub(crate) struct ReadParams {
    /// File path
    path: String,
    /// Line offset
    offset: Option<u32>,
    /// Max lines
    limit: Option<u32>,
}

// Schema-only: fields are read by schemars derive, not by Rust code directly.
#[derive(JsonSchema)]
#[allow(dead_code)]
struct WriteParams {
    /// File path
    path: String,
    /// Content to write
    content: String,
}

// Schema-only: fields are read by schemars derive, not by Rust code directly.
#[derive(JsonSchema)]
#[allow(dead_code)]
struct EditParams {
    /// File path
    path: String,
    /// Text to find
    old_string: String,
    /// Replacement text
    new_string: String,
}

// Schema-only: fields are read by schemars derive, not by Rust code directly.
#[derive(JsonSchema)]
#[allow(dead_code)]
struct GlobParams {
    /// Glob pattern
    pattern: String,
}

// Schema-only: fields are read by schemars derive, not by Rust code directly.
#[derive(JsonSchema)]
#[allow(dead_code)]
struct GrepParams {
    /// Regex pattern
    pattern: String,
    /// Search path
    path: Option<String>,
    /// Case sensitive
    case_sensitive: Option<bool>,
}

/// File operations executor sandboxed to allowed paths.
#[derive(Debug)]
pub struct FileExecutor {
    allowed_paths: Vec<PathBuf>,
}

impl FileExecutor {
    #[must_use]
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        let paths = if allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            allowed_paths
        };
        Self {
            allowed_paths: paths
                .into_iter()
                .map(|p| p.canonicalize().unwrap_or(p))
                .collect(),
        }
    }

    fn validate_path(&self, path: &Path) -> Result<PathBuf, ToolError> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let canonical = resolve_via_ancestors(&resolved);
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
    pub fn execute_file_tool(
        &self,
        tool_id: &str,
        params: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<ToolOutput>, ToolError> {
        match tool_id {
            "read" => self.handle_read(params),
            "write" => self.handle_write(params),
            "edit" => self.handle_edit(params),
            "glob" => self.handle_glob(params),
            "grep" => self.handle_grep(params),
            _ => Ok(None),
        }
    }

    fn handle_read(
        &self,
        params: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path_str = param_str(params, "path")?;
        let path = self.validate_path(Path::new(&path_str))?;

        let content = std::fs::read_to_string(&path)?;

        let offset = param_usize(params, "offset").unwrap_or(0);
        let limit = param_usize(params, "limit").unwrap_or(usize::MAX);

        let selected: Vec<String> = content
            .lines()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, line)| format!("{:>4}\t{line}", offset + i + 1))
            .collect();

        Ok(Some(ToolOutput {
            tool_name: "read".to_owned(),
            summary: selected.join("\n"),
            blocks_executed: 1,
            filter_stats: None,
        }))
    }

    fn handle_write(
        &self,
        params: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path_str = param_str(params, "path")?;
        let content = param_str(params, "content")?;
        let path = self.validate_path(Path::new(&path_str))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &content)?;

        Ok(Some(ToolOutput {
            tool_name: "write".to_owned(),
            summary: format!("Wrote {} bytes to {path_str}", content.len()),
            blocks_executed: 1,
            filter_stats: None,
        }))
    }

    fn handle_edit(
        &self,
        params: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path_str = param_str(params, "path")?;
        let old_string = param_str(params, "old_string")?;
        let new_string = param_str(params, "new_string")?;
        let path = self.validate_path(Path::new(&path_str))?;

        let content = std::fs::read_to_string(&path)?;
        if !content.contains(&old_string) {
            return Err(ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("old_string not found in {path_str}"),
            )));
        }

        let new_content = content.replacen(&old_string, &new_string, 1);
        std::fs::write(&path, &new_content)?;

        Ok(Some(ToolOutput {
            tool_name: "edit".to_owned(),
            summary: format!("Edited {path_str}"),
            blocks_executed: 1,
            filter_stats: None,
        }))
    }

    fn handle_glob(
        &self,
        params: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let pattern = param_str(params, "pattern")?;
        let matches: Vec<String> = glob::glob(&pattern)
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
            .collect();

        Ok(Some(ToolOutput {
            tool_name: "glob".to_owned(),
            summary: if matches.is_empty() {
                format!("No files matching: {pattern}")
            } else {
                matches.join("\n")
            },
            blocks_executed: 1,
            filter_stats: None,
        }))
    }

    fn handle_grep(
        &self,
        params: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let pattern = param_str(params, "pattern")?;
        let search_path = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let case_sensitive = params
            .get("case_sensitive")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        let path = self.validate_path(Path::new(search_path))?;

        let regex = if case_sensitive {
            regex::Regex::new(&pattern)
        } else {
            regex::RegexBuilder::new(&pattern)
                .case_insensitive(true)
                .build()
        }
        .map_err(|e| {
            ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;

        let mut results = Vec::new();
        grep_recursive(&path, &regex, &mut results, 100)?;

        Ok(Some(ToolOutput {
            tool_name: "grep".to_owned(),
            summary: if results.is_empty() {
                format!("No matches for: {pattern}")
            } else {
                results.join("\n")
            },
            blocks_executed: 1,
            filter_stats: None,
        }))
    }
}

impl ToolExecutor for FileExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        self.execute_file_tool(&call.tool_id, &call.params)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "read",
                description: "Read file contents with optional offset/limit",
                schema: schemars::schema_for!(ReadParams),
                invocation: InvocationHint::ToolCall,
            },
            ToolDef {
                id: "write",
                description: "Write content to a file",
                schema: schemars::schema_for!(WriteParams),
                invocation: InvocationHint::ToolCall,
            },
            ToolDef {
                id: "edit",
                description: "Replace a string in a file",
                schema: schemars::schema_for!(EditParams),
                invocation: InvocationHint::ToolCall,
            },
            ToolDef {
                id: "glob",
                description: "Find files matching a glob pattern",
                schema: schemars::schema_for!(GlobParams),
                invocation: InvocationHint::ToolCall,
            },
            ToolDef {
                id: "grep",
                description: "Search file contents with regex",
                schema: schemars::schema_for!(GrepParams),
                invocation: InvocationHint::ToolCall,
            },
        ]
    }
}

/// Canonicalize a path by walking up to the nearest existing ancestor.
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
) -> Result<(), ToolError> {
    if results.len() >= limit {
        return Ok(());
    }
    if path.is_file() {
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
            grep_recursive(&p, regex, results, limit)?;
        }
    }
    Ok(())
}

fn param_str(params: &HashMap<String, serde_json::Value>, key: &str) -> Result<String, ToolError> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("missing required parameter: {key}"),
            ))
        })
}

fn param_usize(params: &HashMap<String, serde_json::Value>, key: &str) -> Option<usize> {
    #[allow(clippy::cast_possible_truncation)]
    params
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn make_params(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
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
        assert!(result.summary.contains("b"));
        assert!(result.summary.contains("c"));
        assert!(!result.summary.contains("a"));
        assert!(!result.summary.contains("d"));
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
        let params = HashMap::new();
        let result = exec.execute_file_tool("unknown", &params).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn glob_finds_files() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.rs"), "").unwrap();
        fs::write(dir.path().join("b.rs"), "").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let pattern = format!("{}/*.rs", dir.path().display());
        let params = make_params(&[("pattern", serde_json::json!(pattern))]);
        let result = exec.execute_file_tool("glob", &params).unwrap().unwrap();
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
    fn glob_filters_outside_sandbox() {
        let sandbox = temp_dir();
        let outside = temp_dir();
        fs::write(outside.path().join("secret.rs"), "secret").unwrap();

        let exec = FileExecutor::new(vec![sandbox.path().to_path_buf()]);
        let pattern = format!("{}/*.rs", outside.path().display());
        let params = make_params(&[("pattern", serde_json::json!(pattern))]);
        let result = exec.execute_file_tool("glob", &params).unwrap().unwrap();
        assert!(!result.summary.contains("secret.rs"));
    }

    #[tokio::test]
    async fn tool_executor_execute_tool_call_delegates() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "content").unwrap();

        let exec = FileExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "read".to_owned(),
            params: make_params(&[("path", serde_json::json!(file.to_str().unwrap()))]),
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.tool_name, "read");
        assert!(result.summary.contains("content"));
    }

    #[test]
    fn tool_executor_tool_definitions_lists_all() {
        let exec = FileExecutor::new(vec![]);
        let defs = exec.tool_definitions();
        let ids: Vec<&str> = defs.iter().map(|d| d.id).collect();
        assert!(ids.contains(&"read"));
        assert!(ids.contains(&"write"));
        assert!(ids.contains(&"edit"));
        assert!(ids.contains(&"glob"));
        assert!(ids.contains(&"grep"));
        assert_eq!(defs.len(), 5);
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
    fn tool_definitions_returns_five_tools() {
        let exec = FileExecutor::new(vec![]);
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 5);
        let ids: Vec<&str> = defs.iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["read", "write", "edit", "glob", "grep"]);
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
        let read = defs.iter().find(|d| d.id == "read").unwrap();
        let obj = read.schema.as_object().unwrap();
        let props = obj["properties"].as_object().unwrap();
        assert!(props.contains_key("path"));
        assert!(props.contains_key("offset"));
        assert!(props.contains_key("limit"));
    }
}
