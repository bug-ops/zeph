// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("dataset not found: {0}")]
    DatasetNotFound(String),

    #[error("dataset I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid dataset format: {0}")]
    InvalidFormat(String),

    #[error("channel error: {0}")]
    Channel(#[from] zeph_core::channel::ChannelError),

    #[error("{0}")]
    Other(String),
}
