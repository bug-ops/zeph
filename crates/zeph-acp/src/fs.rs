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
use std::sync::Arc;

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

/// Bounded fs request channel capacity.
///
/// Each read/write tool call occupies one slot. 64 slots handle any realistic
/// burst of concurrent file operations; excess requests are backpressured via
/// the async send in each request method.
const FS_CHANNEL_CAPACITY: usize = 64;

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
        session_id: acp::schema::v1::SessionId,
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
        reply: oneshot::Sender<Result<String, AcpError>>,
    },
    Write {
        session_id: acp::schema::v1::SessionId,
        path: PathBuf,
        content: String,
        reply: oneshot::Sender<Result<(), AcpError>>,
    },
    ReadForDiff {
        session_id: acp::schema::v1::SessionId,
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
    session_id: acp::schema::v1::SessionId,
    request_tx: mpsc::Sender<FsRequest>,
    can_read: bool,
    can_write: bool,
    cwd: PathBuf,
    permission_gate: Option<AcpPermissionGate>,
}

impl AcpFileExecutor {
    /// Create the executor and its background handler future.
    ///
    /// `can_read` / `can_write` gate which tool definitions are advertised.
    /// `permission_gate` is used to request user confirmation before writing files.
    pub async fn new(
        conn: Arc<acp::ConnectionTo<acp::Client>>,
        session_id: acp::schema::v1::SessionId,
        can_read: bool,
        can_write: bool,
        cwd: PathBuf,
        permission_gate: Option<AcpPermissionGate>,
    ) -> (Self, impl std::future::Future<Output = ()> + Send + 'static) {
        let cwd = tokio::fs::canonicalize(&cwd).await.unwrap_or(cwd);
        let (tx, rx) = mpsc::channel::<FsRequest>(FS_CHANNEL_CAPACITY);
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
    fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
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
            .await
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
            .await
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
            .await
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
                let resolved = Self::resolve_path(&self.cwd, &path);
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
                self.handle_list_directory(params).await
            }
            "find_path" if self.can_read => {
                let params: FindPathParams = deserialize_params(&call.params)?;
                self.handle_find_path(params).await
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
        let resolved = Self::resolve_path(&self.cwd, &path);
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
            let diff = acp::schema::v1::Diff::new(resolved.clone(), params.content.clone())
                .old_text(old_content.clone());
            let fields = acp::schema::v1::ToolCallUpdateFields::new()
                .title("write_file".to_owned())
                .content(vec![acp::schema::v1::ToolCallContent::Diff(diff)])
                .raw_input(serde_json::json!({ "path": params.path }));
            let tool_call = acp::schema::v1::ToolCallUpdate::new("write_file".to_owned(), fields);
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

    /// Offload the synchronous directory walk to the blocking pool.
    ///
    /// `std::fs::read_dir` and the per-entry `symlink_metadata` calls are blocking
    /// syscalls; dispatching them through `spawn_blocking` keeps the Tokio executor
    /// thread free while a large workspace is enumerated.
    async fn handle_list_directory(
        &self,
        params: ListDirectoryParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || Self::list_directory_blocking(&cwd, params))
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: format!("list_directory task failed: {e}"),
            })?
    }

    fn list_directory_blocking(
        cwd: &Path,
        params: ListDirectoryParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let path = validate_path(&params.path)?;
        let dir = Self::resolve_path(cwd, &path);
        validate_within_sandbox(&dir, cwd)?;
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
            if meta.file_type().is_symlink() && validate_within_sandbox(&entry.path(), cwd).is_err()
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

    /// Offload the synchronous glob walk to the blocking pool.
    ///
    /// `glob::glob` walks the filesystem synchronously; dispatching it through
    /// `spawn_blocking` keeps the Tokio executor thread free for large workspaces.
    async fn handle_find_path(
        &self,
        params: FindPathParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || Self::find_path_blocking(&cwd, &params))
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: format!("find_path task failed: {e}"),
            })?
    }

    fn find_path_blocking(
        cwd: &Path,
        params: &FindPathParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        const MAX_RESULTS: usize = 1000;

        let path = validate_path(&params.path)?;
        let base = Self::resolve_path(cwd, &path);

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

        validate_within_sandbox(&base, cwd)?;

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
                if validate_within_sandbox(&p, cwd).is_err() {
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

async fn run_fs_handler(
    conn: Arc<acp::ConnectionTo<acp::Client>>,
    mut rx: mpsc::Receiver<FsRequest>,
) {
    while let Some(req) = rx.recv().await {
        match req {
            FsRequest::Read {
                session_id,
                path,
                line,
                limit,
                reply,
            } => {
                let req = acp::schema::v1::ReadTextFileRequest::new(session_id, path)
                    .line(line)
                    .limit(limit);
                let result = conn
                    .send_request(req)
                    .block_task()
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
                    .send_request(acp::schema::v1::WriteTextFileRequest::new(
                        session_id, path, content,
                    ))
                    .block_task()
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
                let req = acp::schema::v1::ReadTextFileRequest::new(session_id, path);
                let result = match conn.send_request(req).block_task().await {
                    Ok(r) => Ok(Some(r.content)),
                    Err(e) if e.code == acp::ErrorCode::ResourceNotFound => Ok(None),
                    Err(e) => Err(AcpError::ClientError(e.to_string())),
                };
                reply.send(result).ok();
            }
        }
    }
}
