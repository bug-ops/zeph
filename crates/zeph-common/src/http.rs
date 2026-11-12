// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Size-capped reads of [`reqwest::Response`] bodies.
//!
//! A remote endpoint is untrusted: without a cap, a hostile or compromised server can stream
//! gigabytes inside the request timeout. [`read_capped`] and [`read_prefix`] abort as soon as the
//! declared length or the bytes received so far cross the [`BodyLimit`], so memory use is bounded
//! by the limit and not by what the peer chooses to send.
//!
//! Requires the `http` feature.
//!
//! # Examples
//!
//! ```
//! use zeph_common::http::BodyLimit;
//!
//! let limit = BodyLimit::new(4096).expect("non-zero");
//! assert_eq!(limit.get(), 4096);
//! assert!(BodyLimit::new(0).is_none());
//! ```

use std::num::NonZeroUsize;

/// Maximum number of bytes accepted for one response body.
///
/// Zero is unrepresentable: a limit that rejects every body is a configuration bug, not a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimit(NonZeroUsize);

impl BodyLimit {
    /// Builds a limit from a byte count, or `None` when it is zero.
    #[must_use]
    pub const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// Wraps an already validated non-zero byte count.
    #[must_use]
    pub const fn from_non_zero(bytes: NonZeroUsize) -> Self {
        Self(bytes)
    }

    /// The limit in bytes.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

/// Failure of a capped body read.
#[derive(Debug, thiserror::Error)]
pub enum BodyReadError {
    /// The declared `Content-Length` or the received bytes exceeded the limit.
    #[error("response body exceeded the {limit}-byte limit")]
    TooLarge {
        /// The cap that was exceeded, in bytes.
        limit: usize,
    },
    /// The transport failed while reading the body.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
}

/// Reads the whole body of `resp`, failing as soon as it exceeds `limit`.
///
/// A declared `Content-Length` above the limit is rejected before any body byte is read;
/// otherwise chunks are accumulated and the read aborts at the first chunk that crosses the limit.
///
/// # Errors
///
/// [`BodyReadError::TooLarge`] when the limit is exceeded, [`BodyReadError::Transport`] on
/// transport errors.
#[tracing::instrument(name = "common.http.read_capped", skip_all, fields(limit = limit.get()), err)]
pub async fn read_capped(
    mut resp: reqwest::Response,
    limit: BodyLimit,
) -> Result<Vec<u8>, BodyReadError> {
    let max = limit.get();
    let declared = resp.content_length();
    if declared.is_some_and(|len| usize::try_from(len).map_or(true, |len| len > max)) {
        return Err(BodyReadError::TooLarge { limit: max });
    }
    let capacity = declared.map_or(0, |len| usize::try_from(len).map_or(0, |len| len.min(max)));
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(BodyReadError::TooLarge { limit: max });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Reads at most `limit` bytes of `resp` for diagnostics; never fails.
///
/// Transport errors and an over-long body both just end the read, so the result is the best-effort
/// prefix received so far.
#[tracing::instrument(name = "common.http.read_prefix", skip_all, fields(limit = limit.get()))]
pub async fn read_prefix(mut resp: reqwest::Response, limit: BodyLimit) -> Vec<u8> {
    let max = limit.get();
    let mut body = Vec::new();
    while body.len() < max {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(max - body.len());
                body.extend_from_slice(&chunk[..take]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    const LIMIT: usize = 1024;
    const CHUNKED_HEAD: &str = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

    fn serve(head: &'static str, chunks: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            sock.write_all(head.as_bytes()).unwrap();
            for chunk in chunks {
                if sock.write_all(&chunk).is_err() {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        format!("http://{addr}/")
    }

    fn chunked(sizes: &[usize]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for &n in sizes {
            out.push(format!("{n:x}\r\n").into_bytes());
            out.push(vec![b'a'; n]);
            out.push(b"\r\n".to_vec());
        }
        out.push(b"0\r\n\r\n".to_vec());
        out
    }

    fn limit() -> BodyLimit {
        BodyLimit::new(LIMIT).unwrap()
    }

    #[test]
    fn zero_limit_is_unrepresentable() {
        assert!(BodyLimit::new(0).is_none());
    }

    #[tokio::test]
    async fn chunked_over_limit_is_rejected_mid_stream() {
        let url = serve(CHUNKED_HEAD, chunked(&[600, 600, 600]));
        let resp = reqwest::get(&url).await.unwrap();
        let err = read_capped(resp, limit()).await.unwrap_err();
        assert_matches!(err, BodyReadError::TooLarge { limit: LIMIT });
    }

    #[tokio::test]
    async fn content_length_over_limit_is_rejected_before_reading_body() {
        // Declares 1 MiB but sends no body byte: only the header check can reject it.
        let url = serve("HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n", vec![]);
        let resp = reqwest::get(&url).await.unwrap();
        let err = read_capped(resp, limit()).await.unwrap_err();
        assert_matches!(err, BodyReadError::TooLarge { limit: LIMIT });
    }

    #[tokio::test]
    async fn body_at_limit_is_accepted() {
        let url = serve(CHUNKED_HEAD, chunked(&[LIMIT / 2, LIMIT / 2]));
        let resp = reqwest::get(&url).await.unwrap();
        let body = read_capped(resp, limit()).await.unwrap();
        assert_eq!(body.len(), LIMIT);
    }

    #[tokio::test]
    async fn prefix_is_truncated_to_limit() {
        let url = serve(CHUNKED_HEAD, chunked(&[600, 600]));
        let resp = reqwest::get(&url).await.unwrap();
        let body = read_prefix(resp, limit()).await;
        assert_eq!(body.len(), LIMIT);
    }
}
