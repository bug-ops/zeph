// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("failed to bind {0}: {1}")]
    Bind(String, std::io::Error),
    #[error("server error: {0}")]
    Server(String),
}
