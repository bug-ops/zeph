// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::rc::Rc;

use acp::Client as _;
use agent_client_protocol as acp;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use zeph_tools::{
    ToolCall, ToolError, ToolOutput,
    executor::deserialize_params,
    registry::{InvocationHint, ToolDef},
};

use crate::error::AcpError;

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
}

impl AcpFileExecutor {
    /// Create the executor and the `LocalSet`-side handler future.
    ///
    /// `can_read` / `can_write` gate which tool definitions are advertised.
    pub fn new<C>(
        conn: Rc<C>,
        session_id: acp::SessionId,
        can_read: bool,
        can_write: bool,
    ) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<FsRequest>();
        let handler = async move { run_fs_handler(conn, rx).await };
        (
            Self {
                session_id,
                request_tx: tx,
                can_read,
                can_write,
            },
            handler,
        )
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
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

fn validate_absolute_path(raw: &str) -> Result<PathBuf, ToolError> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(ToolError::SandboxViolation {
            path: raw.to_owned(),
        });
    }
    // Reject obvious traversal components even in absolute paths.
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
                description: "Read a file from the IDE workspace. Preferred over bash cat/head/tail for reading files. Returns structured content with line numbers. Supports optional line offset and limit.".into(),
                schema: schemars::schema_for!(ReadFileParams),
                invocation: InvocationHint::ToolCall,
            });
            defs.push(ToolDef {
                id: "list_directory".into(),
                description:
                    "List files and directories at the given path. Preferred over bash ls.".into(),
                schema: schemars::schema_for!(ListDirectoryParams),
                invocation: InvocationHint::ToolCall,
            });
            defs.push(ToolDef {
                id: "find_path".into(),
                description:
                    "Find files matching a glob pattern in a directory. Preferred over bash find."
                        .into(),
                schema: schemars::schema_for!(FindPathParams),
                invocation: InvocationHint::ToolCall,
            });
        }
        if self.can_write {
            defs.push(ToolDef {
                id: "write_file".into(),
                description:
                    "Write content to a file in the IDE workspace. Preferred over shell redirects."
                        .into(),
                schema: schemars::schema_for!(WriteFileParams),
                invocation: InvocationHint::ToolCall,
            });
        }
        defs
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        match call.tool_id.as_str() {
            "read_file" if self.can_read => {
                let params: ReadFileParams = deserialize_params(&call.params)?;
                let path = validate_absolute_path(&params.path)?;
                let content = self
                    .read(path, params.line, params.limit)
                    .await
                    .map_err(|e| ToolError::InvalidParams {
                        message: e.to_string(),
                    })?;
                let total_lines = content.lines().count();
                let start_line = params.line.unwrap_or(1);
                let raw_response = Some(serde_json::json!({
                    "type": "text",
                    "file": {
                        "filePath": &params.path,
                        "content": &content,
                        "numLines": total_lines,
                        "startLine": start_line,
                        "totalLines": total_lines
                    }
                }));
                Ok(Some(ToolOutput {
                    tool_name: "read_file".to_owned(),
                    summary: content,
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: Some(vec![params.path]),
                    raw_response,
                }))
            }
            "write_file" if self.can_write => {
                let params: WriteFileParams = deserialize_params(&call.params)?;
                let path = validate_absolute_path(&params.path)?;
                self.write(path, params.content)
                    .await
                    .map_err(|e| ToolError::InvalidParams {
                        message: e.to_string(),
                    })?;
                Ok(Some(ToolOutput {
                    tool_name: "write_file".to_owned(),
                    summary: format!("wrote {}", params.path),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: Some(vec![params.path]),
                    raw_response: None,
                }))
            }
            "list_directory" if self.can_read => {
                let params: ListDirectoryParams = deserialize_params(&call.params)?;
                Self::handle_list_directory(params)
            }
            "find_path" if self.can_read => {
                let params: FindPathParams = deserialize_params(&call.params)?;
                Self::handle_find_path(&params)
            }
            _ => Ok(None),
        }
    }
}

impl AcpFileExecutor {
    fn handle_list_directory(params: ListDirectoryParams) -> Result<Option<ToolOutput>, ToolError> {
        let dir = validate_absolute_path(&params.path)?;
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
            tool_name: "list_directory".to_owned(),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: Some(vec![params.path]),
            raw_response: None,
        }))
    }

    fn handle_find_path(params: &FindPathParams) -> Result<Option<ToolOutput>, ToolError> {
        const MAX_RESULTS: usize = 1000;

        // path is required; defaulting to "." would bypass sandbox validation.
        let base_str = params
            .path
            .as_deref()
            .ok_or_else(|| ToolError::InvalidParams {
                message: "find_path: 'path' parameter is required and must be an absolute path"
                    .into(),
            })?;
        let _base = validate_absolute_path(base_str)?;

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

        let glob_str = format!("{base_str}/{}", params.pattern);
        let mut matches: Vec<String> = Vec::new();
        for entry in glob::glob(&glob_str).map_err(|e| ToolError::InvalidParams {
            message: format!("invalid glob pattern: {e}"),
        })? {
            if matches.len() >= MAX_RESULTS {
                break;
            }
            if let Ok(p) = entry {
                matches.push(p.display().to_string());
            }
        }

        let summary = matches.join("\n");
        Ok(Some(ToolOutput {
            tool_name: "find_path".to_owned(),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use zeph_tools::ToolExecutor as _;

    use super::*;

    fn test_path(name: &str) -> String {
        if cfg!(windows) {
            format!("C:\\tmp\\{name}")
        } else {
            format!("/tmp/{name}")
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
                let (exec, handler) = AcpFileExecutor::new(conn, sid, true, false);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("test.txt")));
                let call = ToolCall {
                    tool_id: "read_file".to_owned(),
                    params,
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
                let (exec, handler) = AcpFileExecutor::new(conn, sid, false, true);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("data"));
                let call = ToolCall {
                    tool_id: "write_file".to_owned(),
                    params,
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
                let (exec, handler) = AcpFileExecutor::new(conn, sid, true, true);
                tokio::task::spawn_local(handler);

                let call = ToolCall {
                    tool_id: "unknown".to_owned(),
                    params: serde_json::Map::new(),
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
        };
        let defs = exec_read_only.tool_definitions();
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
        assert!(ids.contains(&"read_file"));
        assert!(ids.contains(&"list_directory"));
        assert!(ids.contains(&"find_path"));
        assert!(!ids.contains(&"write_file"));
        assert!(defs[0].description.contains("cat/head/tail"));

        let exec_write_only = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: false,
            can_write: true,
        };
        let defs = exec_write_only.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "write_file");
        assert!(defs[0].description.contains("shell redirects"));
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
        };

        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: "list_directory".to_owned(),
            params,
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
        };

        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.rs"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: "find_path".to_owned(),
            params,
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
                let (exec, handler) = AcpFileExecutor::new(conn, sid, false, true);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("test.txt")));
                let call = ToolCall {
                    tool_id: "read_file".to_owned(),
                    params,
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
                let (exec, handler) = AcpFileExecutor::new(conn, sid, true, false);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!(test_path("out.txt")));
                params.insert("content".to_owned(), serde_json::json!("data"));
                let call = ToolCall {
                    tool_id: "write_file".to_owned(),
                    params,
                };
                let result = exec.execute_tool_call(&call).await.unwrap();
                assert!(result.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn list_directory_nonexistent_returns_error() {
        let (tx, _rx) = mpsc::unbounded_channel::<FsRequest>();
        let exec = AcpFileExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            can_read: true,
            can_write: false,
        };
        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::json!(test_path("nonexistent_dir_zeph")),
        );
        let call = ToolCall {
            tool_id: "list_directory".to_owned(),
            params,
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
        };
        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: "list_directory".to_owned(),
            params,
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
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.nomatch"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: "find_path".to_owned(),
            params,
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
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("[invalid"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: "find_path".to_owned(),
            params,
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
        };
        let mut params = serde_json::Map::new();
        params.insert("path".to_owned(), serde_json::json!(test_path("some_dir")));
        let call = ToolCall {
            tool_id: "list_directory".to_owned(),
            params,
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
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.rs"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: "find_path".to_owned(),
            params,
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
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("../../etc/passwd"));
        params.insert(
            "path".to_owned(),
            serde_json::json!(dir.path().to_str().unwrap()),
        );
        let call = ToolCall {
            tool_id: "find_path".to_owned(),
            params,
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
        };
        let mut params = serde_json::Map::new();
        params.insert("pattern".to_owned(), serde_json::json!("*.rs"));
        // no "path" key — should error, not default to "."
        let call = ToolCall {
            tool_id: "find_path".to_owned(),
            params,
        };
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn validate_absolute_path_rejects_relative() {
        let err = validate_absolute_path("relative/path.txt").unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation { .. }));
    }

    #[test]
    fn validate_absolute_path_rejects_traversal() {
        let traversal = if cfg!(windows) {
            "C:\\tmp\\..\\etc\\passwd"
        } else {
            "/tmp/../etc/passwd"
        };
        let err = validate_absolute_path(traversal).unwrap_err();
        assert!(matches!(err, ToolError::SandboxViolation { .. }));
    }

    #[test]
    fn validate_absolute_path_accepts_absolute() {
        let path = validate_absolute_path(&test_path("safe.txt")).unwrap();
        assert!(path.is_absolute());
    }

    #[tokio::test]
    async fn read_file_rejects_relative_path() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeClient {
                    content: "data".to_owned(),
                });
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpFileExecutor::new(conn, sid, true, false);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("path".to_owned(), serde_json::json!("relative/path.txt"));
                let call = ToolCall {
                    tool_id: "read_file".to_owned(),
                    params,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::SandboxViolation { .. }));
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
                let (exec, handler) = AcpFileExecutor::new(conn, sid, false, true);
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
                    tool_id: "write_file".to_owned(),
                    params,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::SandboxViolation { .. }));
            })
            .await;
    }
}
