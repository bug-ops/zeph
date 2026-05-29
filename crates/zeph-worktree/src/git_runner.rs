// SPDX-License-Identifier: MIT
//! [`GitRunner`] trait and [`DefaultGitRunner`] production implementation.
//!
//! The trait is the primary testability seam in `zeph-worktree`.  Unit tests
//! inject a `FakeGitRunner` (defined in the test module) that verifies argument
//! hygiene without touching the file system.  Production code uses
//! [`DefaultGitRunner`], which delegates to [`tokio::process::Command`] with a
//! default timeout of 30 seconds.

use std::{path::Path, process::Output, time::Duration};

use crate::error::WorktreeError;

/// Runs `git` sub-commands on behalf of [`WorktreeManager`][crate::WorktreeManager].
///
/// The `cwd` parameter is an explicit directory argument, making it safe to use
/// from contexts where the process working directory has already been mutated by
/// a `CwdRestoreGuard` (in `zeph-subagent`).
///
/// Implementors must be `Send + Sync` so they can be shared across async tasks.
pub trait GitRunner: Send + Sync {
    /// Run `git` with the given `args` from the directory `cwd`.
    ///
    /// Returns the raw [`Output`] so callers can inspect both stdout and stderr.
    ///
    /// # Errors
    ///
    /// Returns [`WorktreeError`] on I/O failure or timeout.  Exit-code checking
    /// is the caller's responsibility.
    fn run(
        &self,
        args: &[&str],
        cwd: &Path,
    ) -> impl std::future::Future<Output = Result<Output, WorktreeError>> + Send;
}

/// Production [`GitRunner`] that invokes the system `git` binary.
///
/// A 30-second timeout is applied to every invocation to prevent indefinite
/// hangs caused by credential prompts, slow networks, or stalled lock files.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_worktree::git_runner::{DefaultGitRunner, GitRunner};
///
/// # async fn example() -> Result<(), zeph_worktree::WorktreeError> {
/// let runner = DefaultGitRunner::default();
/// let out = runner.run(&["--version"], Path::new("/tmp")).await?;
/// assert!(out.status.success());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default, Clone)]
pub struct DefaultGitRunner {
    timeout: Duration,
}

impl DefaultGitRunner {
    /// Creates a runner with the default 30-second command timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }

    /// Creates a runner with a custom command timeout.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl GitRunner for DefaultGitRunner {
    async fn run(&self, args: &[&str], cwd: &Path) -> Result<Output, WorktreeError> {
        let timeout = self.timeout;
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args).current_dir(cwd);
        // Capture both streams so callers can inspect them without noise on the terminal.
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let run_fut = async move { cmd.output().await.map_err(WorktreeError::Io) };

        tokio::time::timeout(timeout, run_fut)
            .await
            .map_err(|_| WorktreeError::GitCommand {
                op: args.first().copied().unwrap_or("git").to_string(),
                stderr: format!("timed out after {}s", timeout.as_secs()),
            })?
    }
}

/// A scripted fake [`GitRunner`] for unit tests.
///
/// Each call pops the next entry from the response queue.  If the queue is empty
/// the call panics, which surfaces as a test failure with a clear backtrace.
///
/// The `calls` field records every `(args, cwd)` pair passed to [`run`][Self::run]
/// for post-test assertion (e.g. that `--` was always present).
#[cfg(test)]
pub struct FakeGitRunner {
    /// Queued responses, front = next response.
    responses: std::sync::Mutex<std::collections::VecDeque<FakeResponse>>,
    /// All calls recorded in order.
    pub calls: std::sync::Mutex<Vec<(Vec<String>, std::path::PathBuf)>>,
}

#[cfg(test)]
pub struct FakeResponse {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

#[cfg(test)]
impl FakeGitRunner {
    /// Creates a new `FakeGitRunner` with an empty response queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Enqueues a successful response with the given stdout bytes.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn push_ok(&self, stdout: impl Into<Vec<u8>>) {
        self.responses.lock().unwrap().push_back(FakeResponse {
            stdout: stdout.into(),
            stderr: vec![],
            exit_code: 0,
        });
    }

    /// Enqueues a failing response with the given stderr bytes.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn push_err(&self, stderr: impl Into<Vec<u8>>) {
        self.responses.lock().unwrap().push_back(FakeResponse {
            stdout: vec![],
            stderr: stderr.into(),
            exit_code: 1,
        });
    }
}

#[cfg(test)]
impl Default for FakeGitRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Blanket implementation so `Arc<FakeGitRunner>` can be passed as a runner in tests.
#[cfg(test)]
impl GitRunner for std::sync::Arc<FakeGitRunner> {
    async fn run(&self, args: &[&str], cwd: &Path) -> Result<Output, WorktreeError> {
        (**self).run(args, cwd).await
    }
}

#[cfg(test)]
impl GitRunner for FakeGitRunner {
    async fn run(&self, args: &[&str], cwd: &Path) -> Result<Output, WorktreeError> {
        // Record the call for post-test assertions.
        self.calls.lock().unwrap().push((
            args.iter().map(ToString::to_string).collect(),
            cwd.to_path_buf(),
        ));

        let response =
            self.responses.lock().unwrap().pop_front().expect(
                "FakeGitRunner: no more scripted responses (add more with push_ok/push_err)",
            );

        let exit_status = if response.exit_code == 0 {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                std::process::ExitStatus::from_raw(0)
            }
            #[cfg(not(unix))]
            {
                // On non-unix platforms we build a real process to get an ExitStatus.
                std::process::Command::new("true")
                    .status()
                    .unwrap_or_else(|_| {
                        std::process::Command::new("cmd")
                            .args(["/c", "exit", "0"])
                            .status()
                            .unwrap()
                    })
            }
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                // Shift by 8 to set the exit code in the wait-status.
                std::process::ExitStatus::from_raw(response.exit_code << 8)
            }
            #[cfg(not(unix))]
            {
                std::process::Command::new("cmd")
                    .args(["/c", "exit", "1"])
                    .status()
                    .unwrap()
            }
        };

        Ok(Output {
            status: exit_status,
            stdout: response.stdout,
            stderr: response.stderr,
        })
    }
}
