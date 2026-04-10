// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Errors produced by the benchmark harness.
///
/// All fallible public functions in `zeph-bench` return `Result<T, BenchError>`.
///
/// # Examples
///
/// ```
/// use zeph_bench::BenchError;
///
/// fn example() -> Result<(), BenchError> {
///     Err(BenchError::DatasetNotFound("tau-bench".into()))
/// }
///
/// assert!(example().is_err());
/// ```
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// A named dataset was requested but is not registered in [`crate::DatasetRegistry`].
    #[error("dataset not found: {0}")]
    DatasetNotFound(String),

    /// An I/O error occurred while reading or writing a dataset or results file.
    #[error("dataset I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The dataset file could not be parsed (wrong schema, corrupt JSON/JSONL, etc.).
    ///
    /// The inner `String` carries a human-readable description that includes the line
    /// number for JSONL formats.
    #[error("invalid dataset format: {0}")]
    InvalidFormat(String),

    /// An error propagated from the [`zeph_core::channel::Channel`] implementation.
    #[error("channel error: {0}")]
    Channel(#[from] zeph_core::channel::ChannelError),

    /// A catch-all variant for errors that do not fit the above categories.
    #[error("{0}")]
    Other(String),
}
