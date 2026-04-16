// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! IDE-proxied filesystem executor via ACP `fs/*` methods.
//!
//! When the IDE advertises `fs.readTextFile` and/or `fs.writeTextFile` during
//! the ACP `initialize()` handshake, the agent can delegate file I/O to the IDE
//! rather than performing it directly. This allows the IDE to apply its own
//! access controls, open unsaved buffers, and show diff previews.
//!
//! # Security
//!
//! Write operations enforce a 10 MiB content limit and binary file detection
//! (null byte check) before forwarding to the IDE. An optional
//! [`AcpPermissionGate`] can request explicit user confirmation for writes.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use acp::Client as _;
use agent_client_protocol as acp;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use zeph_tools::{
    DiffData, ToolCall, ToolError, ToolOutput,
    executor::deserialize_params,
    registry::{InvocationHint, ToolDef},
};

use crate::error::AcpError;
use crate::permission::AcpPermissionGate;

const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024; // REQ-P31-5: 10 MiB

fn is_binary(content: &[u8]) -> bool {
    content.contains(&0) // REQ-P31-6: null byte detection
}

// Same-process comparison only: `DefaultHasher` is not stable across processes or versions.
fn hash_content(content: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn compute_diff_data(old: &str, new: &str, path: &str) -> DiffData {
    DiffData {
        file_path: path.to_owned(),
        old_content: old.to_owned(),
        new_content: new.to_owned(),
    }
}

enum FsRequest {
    Read {
        session_id: acp::SessionId,
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
        reply: oneshot::Sender<Result<String, AcpError>>,
    },
    Write {
        session_id: acp::SessionId,
        path: PathBuf,
        content: String,
        reply: oneshot::Sender<Result<(), AcpError>>,
    },
    ReadForDiff {
        session_id: acp::SessionId,
        path: PathBuf,
        reply: oneshot::Sender<Result<Option<String>, AcpError>>,
    },
}

/// IDE-proxied file system executor.
///
/// Routes `read_file` / `write_file` tool calls to the IDE via ACP `fs/*` methods.
/// Only constructed when the IDE advertises `fs.readTextFile` or `fs.writeTextFile`
/// capability.
#[derive(Clone)]
pub struct AcpFileExecutor {
    session_id: acp::SessionId,
    request_tx: mpsc::UnboundedSender<FsRequest>,
    can_read: bool,
    can_write: bool,
    cwd: PathBuf,
    permission_gate: Option<AcpPermissionGate>,
}

impl AcpFileExecutor {
    /// Create the executor and the `LocalSet`-side handler future.
    ///
    /// `can_read` / `can_write` gate which tool definitions are advertised.
    /// `permission_gate` is used to request user confirmation before writing files.
    pub fn new<C>(
        conn: Rc<C>,
        session_id: acp::SessionId,
        can_read: bool,
        can_write: bool,
        cwd: PathBuf,
        permission_gate: Option<AcpPermissionGate>,
    ) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let (tx, rx) = mpsc::unbounded_channel::<FsRequest>();
        let handler = async move { run_fs_handler(conn, rx).await };
        (
            Self {
                session_id,
                request_tx: tx,
                can_read,
                can_write,
                cwd,
                permission_gate,
            },
            handler,
        )
    }

    /// Resolve a potentially relative path to an absolute path
    fn resolve_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    async fn read(
        &self,
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, AcpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(FsRequest::Read {
                session_id: self.session_id.clone(),
                path,
                line,
                limit,
                reply: reply_tx,
            })
            .map_err(|_| AcpError::ChannelClosed)?;
        reply_rx.await.map_err(|_| AcpError::ChannelClosed)?
    }

    async fn write(&self, path: PathBuf, content: String) -> Result<(), AcpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(FsRequest::Write {
                session_id: self.session_id.clone(),
                path,
                content,
                reply: reply_tx,
            })
            .map_err(|_| AcpError::ChannelClosed)?;
        reply_rx.await.map_err(|_| AcpError::ChannelClosed)?
    }

    async fn read_for_diff(&self, path: PathBuf) -> Result<Option<String>, AcpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(FsRequest::ReadForDiff {
                session_id: self.session_id.clone(),
                path,
                reply: reply_tx,
            })
            .map_err(|_| AcpError::ChannelClosed)?;
        reply_rx.await.map_err(|_| AcpError::ChannelClosed)?
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReadFileParams {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteFileParams {
    path: String,
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct ListDirectoryParams {
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct FindPathParams {
    /// Directory to search in. Must be an absolute path within the project sandbox.
    path: String,
    /// Glob pattern to match file names (e.g. `*.rs`, `config*.toml`).
    pattern: String,
}

/// Verify that `resolved` is contained within `sandbox` after symlink resolution.
///
/// For existing paths: canonicalize and check prefix.
/// For non-existent paths (e.g. new files): canonicalize the parent directory instead.
///
/// # Errors
///
/// Returns `ToolError::SandboxViolation` if the path escapes the sandbox or the parent
/// directory cannot be canonicalized.
fn validate_within_sandbox(resolved: &Path, sandbox: &Path) -> Result<(), ToolError> {
    let sandbox_canonical = sandbox
        .canonicalize()
        .unwrap_or_else(|_| sandbox.to_path_buf());
    match resolved.canonicalize() {
        Ok(canonical) => {
            if canonical.starts_with(&sandbox_canonical) {
                Ok(())
            } else {
                Err(ToolError::SandboxViolation {
                    path: resolved.display().to_string(),
                })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Walk up ancestors to find the first existing directory.
            let mut ancestor = resolved.parent();
            while let Some(dir) = ancestor {
                match dir.canonicalize() {
                    Ok(canonical) => {
                        if canonical.starts_with(&sandbox_canonical) {
                            return Ok(());
                        }
                        return Err(ToolError::SandboxViolation {
                            path: resolved.display().to_string(),
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        ancestor = dir.parent();
                    }
                    Err(_) => {
                        return Err(ToolError::SandboxViolation {
                            path: resolved.display().to_string(),
                        });
                    }
                }
            }
            Err(ToolError::SandboxViolation {
                path: resolved.display().to_string(),
            })
        }
        Err(_) => Err(ToolError::SandboxViolation {
            path: resolved.display().to_string(),
        }),
    }
}

fn validate_path(raw: &str) -> Result<PathBuf, ToolError> {
    let path = PathBuf::from(raw);
    // Reject obvious traversal components (agent shouldn't try to escape workspace).
    if path.components().any(|c| c.as_os_str() == "..") {
        return Err(ToolError::SandboxViolation {
            path: raw.to_owned(),
        });
    }
    // Symlink resolution is intentionally delegated to the IDE: the agent sends the path
    // as-is via the ACP protocol and the IDE enforces its own sandbox (workspace root,
    // read-only mounts, etc.). The agent trusts the IDE's file-system sandbox boundary.
    Ok(path)
}

impl zeph_tools::ToolExecutor for AcpFileExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        let mut defs = Vec::new();
        if self.can_read {
            defs.push(ToolDef {
                id: "read_file".into(),
                description: "Read a file from the IDE workspace with line numbers.\n\nParameters: path (string, required) - file path relative to workspace root; offset (integer, optional) - start line; limit (integer, optional) - max lines\nReturns: file content with line numbers, structured for IDE display\nErrors: file not found; path outside workspace; I/O failure\nExample: {\"path\": \"src/main.rs\", \"offset\": 0, \"limit\": 100}".into(),
                schema: schemars::schema_for!(ReadFileParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            });
            defs.push(ToolDef {
                id: "list_directory".into(),
                description: "List files and directories at the given path in the IDE workspace.\n\nParameters: path (string, required) - directory path relative to workspace root\nReturns: sorted listing with type indicators\nErrors: path not found; path outside workspace\nExample: {\"path\": \"src/\"}".into(),
                schema: schemars::schema_for!(ListDirectoryParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            });
            defs.push(ToolDef {
                id: "find_path".into(),
                description: "Find files matching a glob pattern in the IDE workspace.\n\nParameters: pattern (string, required) - glob pattern\nReturns: matching file paths relative to workspace root\nErrors: path outside workspace\nExample: {\"pattern\": \"**/*.rs\"}".into(),
                schema: schemars::schema_for!(FindPathParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            });
        }
        // REQ-P31-1: write_file requires a permission gate (diff preview must have an approver).
        if self.can_write && self.permission_gate.is_some() {
            defs.push(ToolDef {
                id: "write_file".into(),
                description: "Create or overwrite a file in the IDE workspace.\n\nParameters: path (string, required) - file path; content (string, required) - file content\nReturns: confirmation with bytes written\nErrors: permission denied; path outside workspace; I/O failure\nExample: {\"path\": \"output.txt\", \"content\": \"Hello\"}".into(),
                schema: schemars::schema_for!(WriteFileParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            });
        }
        defs
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        match call.tool_id.as_str() {
            "read_file" if self.can_read => {
                let params: ReadFileParams = deserialize_params(&call.params)?;
                let path = validate_path(&params.path)?;
                let resolved = self.resolve_path(&path);
                // Defense-in-depth: reject paths that escape cwd. The IDE enforces its own
                // sandbox; we use parent-dir canonicalization to handle non-existent paths
                // and resolve symlinks in the directory component.
                validate_within_sandbox(&resolved, &self.cwd)?;
                let resolved_str = resolved.to_string_lossy().into_owned();
                let content = self
                    .read(resolved, params.line, params.limit)
                    .await
                    .map_err(|e| ToolError::InvalidParams {
                        message: e.to_string(),
                    })?;
                let total_lines = content.lines().count();
                let start_line = params.line.unwrap_or(1);
                let raw_response = Some(serde_json::json!({
                    "type": "text",
                    "file": {
                        "filePath": &resolved_str,
                        "content": &content,
                        "numLines": total_lines,
                        "startLine": start_line,
                        "totalLines": total_lines
                    }
                }));
                Ok(Some(ToolOutput {
                    tool_name: zeph_tools::ToolName::new("read_file"),
                    summary: content,
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: Some(vec![resolved_str]),
                    raw_response,
                    claim_source: Some(zeph_tools::ClaimSource::FileSystem),
                }))
            }
            "write_file" if self.can_write => {
                let params: WriteFileParams = deserialize_params(&call.params)?;
                self.handle_write_file(params).await
            }
            "list_directory" if self.can_read => {
                let params: ListDirectoryParams = deserialize_params(&call.params)?;
                self.handle_list_directory(params)
            }
            "find_path" if self.can_read => {
                let params: FindPathParams = deserialize_params(&call.params)?;
                self.handle_find_path(&params)
            }
            _ => Ok(None),
        }
    }
}

impl AcpFileExecutor {
    async fn handle_write_file(
        &self,
        params: WriteFileParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        // REQ-P31-5: size check before any work
        if params.content.len() > MAX_WRITE_BYTES {
            return Err(ToolError::InvalidParams {
                message: format!("content exceeds {MAX_WRITE_BYTES} byte limit"),
            });
        }
        // REQ-P31-6: binary detection on new content
        if is_binary(params.content.as_bytes()) {
            return Err(ToolError::InvalidParams {
                message: "binary content not supported for write_file".into(),
            });
        }
        let path = validate_path(&params.path)?;
        let resolved = self.resolve_path(&path);
        validate_within_sandbox(&resolved, &self.cwd)?;

        // Read current file for diff (None if new file).
        let old_content =
            self.read_for_diff(resolved.clone())
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: e.to_string(),
                })?;

        // REQ-P31-6: binary detection on existing content
        if let Some(ref old) = old_content
            && is_binary(old.as_bytes())
        {
            return Err(ToolError::InvalidParams {
                message: "existing file is binary; cannot diff".into(),
            });
        }

        // Hash old content for TOCTOU guard (REQ-P31-3)
        let old_hash = old_content.as_deref().map(hash_content);

        if self.permission_gate.is_none() {
            tracing::warn!(
                path = %resolved.display(),
                "AcpFileExecutor: write_file called without permission gate"
            );
        }

        // REQ-P31-2: show diff preview and require approval
        if let Some(gate) = &self.permission_gate {
            let diff = acp::Diff::new(resolved.clone(), params.content.clone())
                .old_text(old_content.clone());
            let fields = acp::ToolCallUpdateFields::new()
                .title("write_file".to_owned())
                .content(vec![acp::ToolCallContent::Diff(diff)])
                .raw_input(serde_json::json!({ "path": params.path }));
            let tool_call = acp::ToolCallUpdate::new("write_file".to_owned(), fields);
            let allowed = gate
                .check_permission(self.session_id.clone(), tool_call)
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: e.to_string(),
                })?;
            if !allowed {
                return Err(ToolError::Blocked {
                    command: "write_file: diff rejected".to_owned(),
                });
            }
        }

        // REQ-P31-3: TOCTOU guard — re-read and compare hash
        let current_content =
            self.read_for_diff(resolved.clone())
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: e.to_string(),
                })?;
        if old_hash != current_content.as_deref().map(hash_content) {
            return Err(ToolError::InvalidParams {
                message: "file changed between diff preview and write; aborting".into(),
            });
        }

        let diff_data = Some(compute_diff_data(
            old_content.as_deref().unwrap_or(""),
            &params.content,
            &params.path,
        ));
        self.write(resolved, params.content.clone())
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: e.to_string(),
            })?;
        Ok(Some(ToolOutput {
            tool_name: zeph_tools::ToolName::new("write_file"),
            summary: format!("wrote {}", params.path),
            blocks_executed: 1,
            filter_stats: None,
            diff: diff_data,
            streamed: false,
            terminal_id: None,
            locations: Some(vec![params.path]),
            raw_response: None,
            claim_source: Some(zeph_tools::ClaimSource::FileSystem),
        }))
    }

    fn handle_list_directory(
        &self,
        params: ListDirectoryParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path = validate_path(&params.path)?;
        let dir = self.resolve_path(&path);
        validate_within_sandbox(&dir, &self.cwd)?;
        let entries = std::fs::read_dir(&dir).map_err(|e| ToolError::InvalidParams {
            message: format!("cannot read directory {}: {e}", params.path),
        })?;
        let mut items: Vec<serde_json::Value> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| ToolError::InvalidParams {
                message: format!("directory entry error: {e}"),
            })?;
            // Use symlink_metadata to avoid following symlinks outside the sandbox.
            let meta = entry
                .path()
                .symlink_metadata()
                .map_err(|e| ToolError::InvalidParams {
                    message: format!("metadata error: {e}"),
                })?;
            // Skip symlinks whose canonical target escapes the sandbox.
            if meta.file_type().is_symlink()
                && validate_within_sandbox(&entry.path(), &self.cwd).is_err()
            {
                continue;
            }
            items.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "is_dir": meta.is_dir(),
                "size": meta.len(),
                "is_symlink": meta.file_type().is_symlink(),
            }));
        }
        items.sort_by(|a, b| {
            let a_name = a["name"].as_str().unwrap_or("");
            let b_name = b["name"].as_str().unwrap_or("");
            a_name.cmp(b_name)
        });
        let summary = serde_json::to_string(&items).unwrap_or_default();
        Ok(Some(ToolOutput {
            tool_name: zeph_tools::ToolName::new("list_directory"),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: Some(vec![params.path]),
            raw_response: None,
            claim_source: Some(zeph_tools::ClaimSource::FileSystem),
        }))
    }

    fn handle_find_path(&self, params: &FindPathParams) -> Result<Option<ToolOutput>, ToolError> {
        const MAX_RESULTS: usize = 1000;

        let path = validate_path(&params.path)?;
        let base = self.resolve_path(&path);

        // Reject traversal components in the pattern to prevent escaping the base directory.
        if params
            .pattern
            .split('/')
            .any(|seg| seg == ".." || seg.starts_with('/'))
        {
            return Err(ToolError::SandboxViolation {
                path: params.pattern.clone(),
            });
        }

        validate_within_sandbox(&base, &self.cwd)?;

        let glob_str = format!("{}/{}", params.path, params.pattern);
        let mut matches: Vec<String> = Vec::new();
        for entry in glob::glob(&glob_str).map_err(|e| ToolError::InvalidParams {
            message: format!("invalid glob pattern: {e}"),
        })? {
            if matches.len() >= MAX_RESULTS {
                break;
            }
            if let Ok(p) = entry {
                // Skip paths that escape the sandbox via symlinks.
                if validate_within_sandbox(&p, &self.cwd).is_err() {
                    continue;
                }
                matches.push(p.display().to_string());
            }
        }

        let summary = matches.join("\n");
        Ok(Some(ToolOutput {
            tool_name: zeph_tools::ToolName::new("find_path"),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(zeph_tools::ClaimSource::FileSystem),
        }))
    }
}

async fn run_fs_handler<C>(conn: Rc<C>, mut rx: mpsc::UnboundedReceiver<FsRequest>)
where
    C: acp::Client,
{
    while let Some(req) = rx.recv().await {
        match req {
            FsRequest::Read {
                session_id,
                path,
                line,
                limit,
                reply,
            } => {
                let req = acp::ReadTextFileRequest::new(session_id, path)
                    .line(line)
                    .limit(limit);
                let result = conn
                    .read_text_file(req)
                    .await
                    .map(|r| r.content)
                    .map_err(|e| AcpError::ClientError(e.to_string()));
                reply.send(result).ok();
            }
            FsRequest::Write {
                session_id,
                path,
                content,
                reply,
            } => {
                let result = conn
                    .write_text_file(acp::WriteTextFileRequest::new(session_id, path, content))
                    .await
                    .map(|_| ())
                    .map_err(|e| AcpError::ClientError(e.to_string()));
                reply.send(result).ok();
            }
            FsRequest::ReadForDiff {
                session_id,
                path,
                reply,
            } => {
                let req = acp::ReadTextFileRequest::new(session_id, path);
                let result = match conn.read_text_file(req).await {
                    Ok(r) => Ok(Some(r.content)),
                    Err(e) if e.code == acp::ErrorCode::ResourceNotFound => Ok(None),
                    Err(e) => Err(AcpError::ClientError(e.to_string())),
                };
                reply.send(result).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use zeph_tools::ToolExecutor as _;

    use super::*;

    fn test_cwd() -> PathBuf {
        std::env::temp_dir()
    }

    fn test_path(name: &str) -> String {
        test_cwd().join(name).to_string_lossy().into_owned()
    }

    /// Minimal client for constructing `AcpPermissionGate` in tests that don't need real perms.
    struct NoopPermClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for NoopPermClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "allow_once",
                )),
            ))
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    struct FakeClient {
        content: String,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for FakeClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Ok(acp::ReadTextFileResponse::new(self.content.clone()))
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Ok(acp::WriteTextFileResponse::new())
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn read_file_tool_call_returns_content() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: "hello world".to_owned(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, true, false, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("test.txt")));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("read_file"),
                    params,
                    caller_id: None,
                };

                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert_eq!(result.summary, "hello world");
                assert_eq!(
                    result.locations.as_deref(),
                    Some(&[test_path("test.txt")][..])
                );
            })
            .await;
    }

    #[tokio::test]
    async fn write_file_tool_call_succeeds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, false, true, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("data"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };

                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert!(result.summary.contains(&test_path("out.txt")));
                assert_eq!(
                    result.locations.as_deref(),
                    Some(&[test_path("out.txt")][..])
                );
            })
            .await;
    }

    #[tokio::test]
    async fn unknown_tool_returns_none() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpFileExecutor::new(conn, sid, true, true, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("unknown"),
                    params: serde_json::Map::new(),
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap();
                assert!(result.is_none());
            })
            .await;
    }

    #[test]
    fn tool_definitions_gated_by_capabilities() {
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec_read_only = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx.clone(),
            can_read: true,
            can_write: false,
            cwd: test_cwd(),
            permission_gate: None,
        };
        let defs = exec_read_only.tool_definitions();
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
        assert!(ids.contains(&"read_file"));
        assert!(ids.contains(&"list_directory"));
        assert!(ids.contains(&"find_path"));
        assert!(!ids.contains(&"write_file"));
        assert!(defs[0].description.contains("IDE workspace"));

        // REQ-P31-1: write_file not advertised without permission gate.
        let exec_write_no_gate = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx.clone(),
            can_read: false,
            can_write: true,
            cwd: test_cwd(),
            permission_gate: None,
        };
        let defs = exec_write_no_gate.tool_definitions();
        assert_eq!(
            defs.len(),
            0,
            "write_file must not appear without permission gate"
        );

        let tmp_dir = tempfile::tempdir().unwrap();
        let perm_file = tmp_dir.path().join("perms.toml");
        let perm_conn = Rc::new(NoopPermClient);
        let (gate, _handler) = AcpPermissionGate::new(perm_conn, Some(perm_file));
        let exec_write_with_gate = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: false,
            can_write: true,
            cwd: test_cwd(),
            permission_gate: Some(gate),
        };
        let defs = exec_write_with_gate.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "write_file");
        assert!(defs[0].description.contains("IDE workspace"));
    }

    #[tokio::test]
    async fn list_directory_returns_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "hello").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: dir.path().to_path_buf(),
            permission_gate: None,
        };

        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("list_directory"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("file.txt"));
        assert!(result.summary.contains("subdir"));
    }

    #[tokio::test]
    async fn find_path_matches_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("bar.toml"), "[package]").unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: dir.path().to_path_buf(),
            permission_gate: None,
        };

        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.rs"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("find_path"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("foo.rs"));
        assert!(!result.summary.contains("bar.toml"));
    }

    #[tokio::test]
    async fn read_file_when_capability_disabled_returns_none() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: "ignored".to_owned(),
                });
                let sid = acp::SessionId::new("s1");
                // can_read = false
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, false, true, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("test.txt")));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("read_file"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap();
                assert!(result.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn write_file_when_capability_disabled_returns_none() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                // can_write = false
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, true, false, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("data"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap();
                assert!(result.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn list_directory_nonexistent_returns_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nonexistent = tmp.path().join("nonexistent_dir_zeph");
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: tmp.path().to_path_buf(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::json!(nonexistent.to_string_lossy()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("list_directory"),
            params,
            caller_id: None,
        };
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[tokio::test]
    async fn list_directory_empty_dir_returns_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: dir.path().to_path_buf(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("list_directory"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary, "[]");
    }

    #[tokio::test]
    async fn find_path_no_matches_returns_empty_summary() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: dir.path().to_path_buf(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.nomatch"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("find_path"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary, "");
    }

    #[tokio::test]
    async fn find_path_invalid_glob_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: dir.path().to_path_buf(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("[invalid"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("find_path"),
            params,
            caller_id: None,
        };
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[tokio::test]
    async fn list_directory_capability_disabled_returns_none() {
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: false,
            can_write: false,
            cwd: test_cwd(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert("path".to_owned(), serde_json::json!(test_path("some_dir")));
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("list_directory"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_path_capability_disabled_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: false,
            can_write: false,
            cwd: test_cwd(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.rs"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("find_path"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_path_traversal_in_pattern_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: test_cwd(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("../../etc/passwd"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("find_path"),
            params,
            caller_id: None,
        };
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation { .. }));
    }

    #[tokio::test]
    async fn find_path_missing_path_param_returns_error() {
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: test_cwd(),
            permission_gate: None,
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.rs"));
        // no "path" key — should error, not default to "."
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("find_path"),
            params,
            caller_id: None,
        };
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn validate_path_rejects_traversal() {
        let traversal = if cfg!(windows) {
            "C:\\tmp\\..\\etc\\passwd"
        } else {
            "/tmp/../etc/passwd"
        };
        let err = validate_path(traversal).unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation { .. }));
    }

    #[test]
    fn validate_path_accepts_relative() {
        // Relative paths are now accepted; resolve_path joins them with cwd.
        let path = validate_path("relative/path.txt").unwrap();
        assert_eq!(path, PathBuf::from("relative/path.txt"));
    }

    #[test]
    fn validate_path_accepts_absolute() {
        let path = validate_path(&test_path("safe.txt")).unwrap();
        assert!(path.is_absolute());
    }

    #[tokio::test]
    async fn read_file_resolves_relative_path_against_cwd() {
        // Relative paths are joined with cwd; the FakeClient mirrors the path back
        // so we verify the resolved absolute path is forwarded correctly.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: "data".to_owned(),
                });
                let sid = acp::SessionId::new("s1");
                let cwd = std::env::current_dir().unwrap_or_else(|_| test_cwd());
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, true, false, cwd.clone(), None);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!("relative/path.txt"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("read_file"),
                    params,
                    caller_id: None,
                };
                // Should succeed: relative path is resolved to cwd/relative/path.txt.
                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert_eq!(result.summary, "data");
                // locations must carry the absolute resolved path
                let locations = result.locations.unwrap();
                assert_eq!(locations.len(), 1);
                assert!(
                    std::path::Path::new(&locations[0]).is_absolute(),
                    "location must be absolute, got: {}",
                    locations[0]
                );
                assert!(
                    locations[0].ends_with("relative/path.txt")
                        || locations[0].ends_with("relative\\path.txt"),
                    "expected path ending with relative/path.txt, got: {}",
                    locations[0]
                );
            })
            .await;
    }

    #[tokio::test]
    async fn write_file_rejects_traversal_path() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, false, true, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                let traversal = if cfg!(windows) {
                    "C:\\tmp\\..\\etc\\passwd"
                } else {
                    "/tmp/../etc/passwd"
                };
                params.insert("path".to_owned(), serde_json::json!(traversal));
                params.insert("content".to_owned(), serde_json::json!("evil"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::SandboxViolation { .. }));
            })
            .await;
    }

    // --- P0.1: permission gate tests ---

    struct AlwaysRejectPermClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AlwaysRejectPermClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "reject_once",
                )),
            ))
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Ok(acp::ReadTextFileResponse::new(String::new()))
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Ok(acp::WriteTextFileResponse::new())
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    struct AlwaysAllowPermClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AlwaysAllowPermClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "allow_once",
                )),
            ))
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Ok(acp::ReadTextFileResponse::new(String::new()))
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Ok(acp::WriteTextFileResponse::new())
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn write_file_permission_denied_returns_blocked_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AlwaysRejectPermClient);
                let (gate, gate_handler) = AcpPermissionGate::new(Rc::clone(&conn), None);
                tokio::task::spawn_local(gate_handler);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid.clone(), false, true, test_cwd(), Some(gate));
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("data"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    #[tokio::test]
    async fn write_file_permission_allowed_succeeds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(AlwaysAllowPermClient);
                let (gate, gate_handler) = AcpPermissionGate::new(Rc::clone(&conn), None);
                tokio::task::spawn_local(gate_handler);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid.clone(), false, true, test_cwd(), Some(gate));
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("data"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert!(result.summary.contains("out.txt"));
            })
            .await;
    }

    #[tokio::test]
    async fn write_file_no_gate_succeeds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, false, true, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("data"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert!(result.summary.contains("out.txt"));
            })
            .await;
    }

    // --- P0.2: symlink sandbox tests ---

    #[test]
    fn validate_within_sandbox_allows_inside() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("safe.txt");
        std::fs::write(&file, "ok").unwrap();
        assert!(validate_within_sandbox(&file, dir.path()).is_ok());
    }

    #[test]
    fn validate_within_sandbox_rejects_escape() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("escape.txt");
        std::fs::write(&file, "evil").unwrap();
        assert!(validate_within_sandbox(&file, sandbox.path()).is_err());
    }

    #[test]
    fn validate_within_sandbox_nonexistent_file_parent_inside() {
        let dir = tempfile::tempdir().unwrap();
        let new_file = dir.path().join("new_file.txt");
        // File does not exist, but parent (dir) is inside sandbox.
        assert!(validate_within_sandbox(&new_file, dir.path()).is_ok());
    }

    #[test]
    fn validate_within_sandbox_nonexistent_file_parent_outside() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let new_file = outside.path().join("new_file.txt");
        // File does not exist, parent is outside sandbox.
        assert!(validate_within_sandbox(&new_file, sandbox.path()).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_directory_symlink_escape_filtered() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        // Create a symlink inside sandbox pointing outside.
        let link = sandbox.path().join("escape_link");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), &link).unwrap();
        // Create a normal file inside sandbox.
        std::fs::write(sandbox.path().join("normal.txt"), "ok").unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: sandbox.path().to_path_buf(),
            permission_gate: None,
        };

        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::json!(sandbox.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("list_directory"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(
            result.summary.contains("normal.txt"),
            "normal file must appear"
        );
        assert!(
            !result.summary.contains("escape_link"),
            "symlink escaping sandbox must be filtered out"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_path_symlink_escape_filtered() {
        let sandbox = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        // Create a symlink inside sandbox pointing outside.
        let link = sandbox.path().join("escape_link.txt");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), &link).unwrap();
        std::fs::write(sandbox.path().join("normal.txt"), "ok").unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
            cwd: sandbox.path().to_path_buf(),
            permission_gate: None,
        };

        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.txt"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(sandbox.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: zeph_tools::ToolName::new("find_path"),
            params,
            caller_id: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(
            result.summary.contains("normal.txt"),
            "normal file must appear"
        );
        assert!(
            !result.summary.contains("escape_link.txt"),
            "symlinked path escaping sandbox must be filtered out"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_via_symlink_outside_sandbox_rejected() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sandbox = tempfile::tempdir().unwrap();
                let outside = tempfile::tempdir().unwrap();
                std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

                let link = sandbox.path().join("escape_link.txt");
                std::os::unix::fs::symlink(outside.path().join("secret.txt"), &link).unwrap();

                let conn = Rc::new(FakeClient {
                    content: "should not reach".to_owned(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpFileExecutor::new(
                    conn,
                    sid,
                    true,
                    false,
                    sandbox.path().to_path_buf(),
                    None,
                );
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(link.to_str().unwrap()));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("read_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::SandboxViolation { .. }));
            })
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_via_symlink_outside_sandbox_rejected() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sandbox = tempfile::tempdir().unwrap();
                let outside = tempfile::tempdir().unwrap();
                std::fs::write(outside.path().join("target.txt"), "original").unwrap();

                let link = sandbox.path().join("escape_link.txt");
                std::os::unix::fs::symlink(outside.path().join("target.txt"), &link).unwrap();

                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpFileExecutor::new(
                    conn,
                    sid,
                    false,
                    true,
                    sandbox.path().to_path_buf(),
                    None,
                );
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(link.to_str().unwrap()));
                params.insert("content".to_owned(), serde_json::json!("evil"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::SandboxViolation { .. }));
            })
            .await;
    }

    #[test]
    fn is_binary_detects_null_byte() {
        assert!(is_binary(b"hello\x00world"));
        assert!(!is_binary(b"plain text\nno nulls"));
    }

    #[test]
    fn hash_content_is_deterministic() {
        let h1 = hash_content("hello");
        let h2 = hash_content("hello");
        let h3 = hash_content("world");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn compute_diff_data_captures_both_sides() {
        let d = compute_diff_data("old\n", "new\n", "file.txt");
        assert_eq!(d.file_path, "file.txt");
        assert_eq!(d.old_content, "old\n");
        assert_eq!(d.new_content, "new\n");
    }

    #[tokio::test]
    async fn write_file_size_limit_rejected() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, false, true, test_cwd(), None);
                tokio::task::spawn_local(handler);

                let oversized = "x".repeat(MAX_WRITE_BYTES + 1);
                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("big.txt")));
                params.insert("content".to_owned(), serde_json::json!(oversized));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::InvalidParams { .. }));
            })
            .await;
    }

    #[tokio::test]
    async fn write_file_binary_content_rejected() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: String::new(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) =
                    AcpFileExecutor::new(conn, sid, false, true, test_cwd(), None);
                tokio::task::spawn_local(handler);

                // Embed a null byte to trigger binary detection.
                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("bin.txt")));
                params.insert(
                    "content".to_owned(),
                    serde_json::json!("hello\u{0000}world"),
                );
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::InvalidParams { .. }));
            })
            .await;
    }

    struct DiffApproveClient {
        old_content: String,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for DiffApproveClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "allow_once",
                )),
            ))
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Ok(acp::ReadTextFileResponse::new(self.old_content.clone()))
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Ok(acp::WriteTextFileResponse::new())
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn write_file_with_permission_gate_shows_diff_and_succeeds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(DiffApproveClient {
                    old_content: "old content\n".into(),
                });
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) =
                    AcpPermissionGate::new(perm_conn.clone(), Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let (exec, handler) =
                    AcpFileExecutor::new(perm_conn, sid, false, true, test_cwd(), Some(gate));
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("new content\n"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert!(result.summary.contains("wrote"));
                assert!(result.diff.is_some());
                let diff = result.diff.unwrap();
                assert_eq!(diff.old_content, "old content\n");
                assert_eq!(diff.new_content, "new content\n");
            })
            .await;
    }

    struct DiffRejectClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for DiffRejectClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "reject_once",
                )),
            ))
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Ok(acp::ReadTextFileResponse::new("current\n".to_owned()))
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            panic!("write should not be called when diff rejected")
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn write_file_diff_rejected_returns_blocked() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(DiffRejectClient);
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) =
                    AcpPermissionGate::new(perm_conn.clone(), Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let (exec, handler) =
                    AcpFileExecutor::new(perm_conn, sid, false, true, test_cwd(), Some(gate));
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("new\n"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    struct NotFoundReadClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for NotFoundReadClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "allow_once",
                )),
            ))
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Err(acp::Error::resource_not_found(None))
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Ok(acp::WriteTextFileResponse::new())
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    /// Simulates a file being modified externally between the diff preview read and the TOCTOU
    /// re-read. Returns different content on each call to `read_text_file`.
    struct ToctouClient {
        call_count: std::cell::Cell<usize>,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for ToctouClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "allow_once",
                )),
            ))
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            let n = self.call_count.get();
            self.call_count.set(n + 1);
            // First read (diff preview): original content.
            // Second read (TOCTOU guard): externally modified content.
            let content = if n == 0 {
                "original\n"
            } else {
                "modified by someone else\n"
            };
            Ok(acp::ReadTextFileResponse::new(content.to_owned()))
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            panic!("write_text_file must not be called when TOCTOU guard fires")
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn write_file_toctou_guard_aborts_when_file_changed() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(ToctouClient { call_count: std::cell::Cell::new(0) });
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) =
                    AcpPermissionGate::new(perm_conn.clone(), Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let (exec, handler) =
                    AcpFileExecutor::new(perm_conn, sid, false, true, test_cwd(), Some(gate));
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("toctou.txt")));
                params.insert("content".to_owned(), serde_json::json!("my new content\n"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(
                    matches!(err, ToolError::InvalidParams { ref message } if message.contains("file changed")),
                    "expected TOCTOU abort error, got: {err:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn write_new_file_with_no_old_content_succeeds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(NotFoundReadClient);
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) =
                    AcpPermissionGate::new(perm_conn.clone(), Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let (exec, handler) =
                    AcpFileExecutor::new(perm_conn, sid, false, true, test_cwd(), Some(gate));
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("new.txt")));
                params.insert("content".to_owned(), serde_json::json!("hello\n"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("write_file"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert!(result.summary.contains("wrote"));
                let diff = result.diff.unwrap();
                assert_eq!(diff.old_content, "");
                assert_eq!(diff.new_content, "hello\n");
            })
            .await;
    }
}
