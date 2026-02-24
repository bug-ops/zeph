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
}
