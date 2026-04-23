// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use thiserror::Error;

/// Step in the ACP handshake sequence at which a failure occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeStep {
    /// The `initialize` request round-trip failed.
    Initialize,
    /// The `session/new` request round-trip failed.
    NewSession,
}

impl std::fmt::Display for HandshakeStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initialize => f.write_str("initialize"),
            Self::NewSession => f.write_str("session/new"),
        }
    }
}

/// Errors returned by the ACP sub-agent client.
#[derive(Debug, Error)]
pub enum AcpClientError {
    /// The command string was empty or could not be shell-split.
    #[error("invalid command config: {0}")]
    InvalidConfig(String),

    /// The subprocess failed to spawn.
    #[error("failed to spawn subprocess: {0}")]
    Spawn(#[source] std::io::Error),

    /// The ACP handshake failed at the named step.
    #[error("handshake failed at {step}: {source}")]
    Handshake {
        /// Which handshake step failed.
        step: HandshakeStep,
        /// The underlying protocol error.
        #[source]
        source: agent_client_protocol::Error,
    },

    /// The prompt or notification could not be sent to the sub-agent.
    #[error("failed to send to sub-agent: {0}")]
    SendFailed(#[source] agent_client_protocol::Error),

    /// A second command was sent while the driver was already servicing a read.
    ///
    /// The caller should wait for the in-flight operation to complete before retrying.
    #[error("driver is busy servicing another read operation")]
    DriverBusy,

    /// The driver task exited unexpectedly before the operation could complete.
    ///
    /// This usually means the subprocess crashed or the transport was closed.
    #[error("driver task exited unexpectedly")]
    DriverDied,

    /// The operation timed out.
    #[error("operation timed out")]
    Timeout,

    /// The session was closed by a call to [`super::SubagentHandle::close`] or via
    /// a [`super::SubagentCommand::Close`] command.
    #[error("session is closed")]
    Closed,

    /// A cancel was requested and the sub-agent acknowledged it by returning a
    /// `StopReason::Cancelled` update.
    #[error("operation cancelled")]
    Cancelled,

    /// Underlying SDK/protocol error not covered by the variants above.
    #[error("ACP SDK error: {0}")]
    Sdk(#[source] agent_client_protocol::Error),
}
