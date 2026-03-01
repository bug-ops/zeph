// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("IDE returned error: {0}")]
    ClientError(String),

    #[error("capability not available: {0}")]
    CapabilityUnavailable(String),

    #[error("channel closed")]
    ChannelClosed,

    #[error("terminal command timed out; partial output: {output}")]
    TerminalTimeout { output: String },

    #[error("stdin payload too large: {size} bytes (max 65536)")]
    StdinTooLarge { size: usize },

    #[error("broken pipe: terminal stdin closed")]
    BrokenPipe,
}
