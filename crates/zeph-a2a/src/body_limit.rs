// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Size caps for response bodies read from remote A2A peers.
//!
//! A peer is untrusted: without a cap, a hostile or compromised peer can stream gigabytes
//! inside the request timeout. Every body read by [`crate::A2aClient`] and
//! [`crate::AgentRegistry`] goes through [`zeph_common::http::read_capped`],
//! [`zeph_common::http::read_prefix`] or [`cap_event_size`], using the limits defined here.

use std::future::ready;

use futures::StreamExt;
use futures_core::Stream;
use zeph_common::http::{BodyLimit, BodyReadError};

use crate::error::A2aError;

/// Agent card served at `/.well-known/agent.json`: 256 KiB.
pub const AGENT_CARD: BodyLimit = BodyLimit::from_const(256 * 1024);
/// JSON-RPC response body: 16 MiB.
pub const RPC_RESPONSE: BodyLimit = BodyLimit::from_const(16 * 1024 * 1024);
/// One server-sent event, from its first byte to the terminating blank line: 4 MiB.
pub const SSE_EVENT: BodyLimit = BodyLimit::from_const(4 * 1024 * 1024);
/// Non-2xx error body kept for diagnostics: 4 KiB.
pub const ERROR_BODY: BodyLimit = BodyLimit::from_const(4 * 1024);

impl From<BodyReadError> for A2aError {
    fn from(e: BodyReadError) -> Self {
        match e {
            BodyReadError::TooLarge { limit } => Self::BodyTooLarge { limit },
            BodyReadError::Transport(e) => Self::Http(e),
        }
    }
}

/// Tracks the size of the SSE event currently being received, across arbitrary chunk splits.
///
/// An event ends at a blank line; LF, CRLF and CR are all line terminators.
#[derive(Debug)]
struct EventSizeTracker {
    limit: usize,
    since_boundary: usize,
    at_line_start: bool,
    pending_cr: bool,
}

impl EventSizeTracker {
    fn new(limit: BodyLimit) -> Self {
        Self {
            limit: limit.get(),
            since_boundary: 0,
            at_line_start: true,
            pending_cr: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<(), A2aError> {
        for &byte in chunk {
            match byte {
                b'\n' if self.pending_cr => {
                    self.pending_cr = false;
                    self.since_boundary += 1;
                }
                b'\n' | b'\r' => {
                    self.pending_cr = byte == b'\r';
                    if self.at_line_start {
                        self.since_boundary = 0;
                    } else {
                        self.at_line_start = true;
                        self.since_boundary += 1;
                    }
                }
                _ => {
                    self.pending_cr = false;
                    self.at_line_start = false;
                    self.since_boundary += 1;
                }
            }
            if self.since_boundary > self.limit {
                return Err(A2aError::BodyTooLarge { limit: self.limit });
            }
        }
        Ok(())
    }
}

/// Wraps a byte stream so that no single SSE event may exceed `limit` bytes.
///
/// Must sit before `.eventsource()`, which buffers an event until its blank line with no bound of
/// its own. On overflow the stream yields one [`A2aError::BodyTooLarge`] and then ends.
///
/// # Examples
///
/// ```
/// use futures::StreamExt;
/// use zeph_a2a::A2aError;
/// use zeph_a2a::body_limit::{ERROR_BODY, cap_event_size};
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let chunks = tokio_stream::iter(vec![Ok::<_, A2aError>(vec![b'x'; 5000])]);
/// let mut capped = Box::pin(cap_event_size(chunks, ERROR_BODY));
/// assert!(capped.next().await.unwrap().is_err());
/// assert!(capped.next().await.is_none());
/// # }
/// ```
pub fn cap_event_size<S, B, E>(
    stream: S,
    limit: BodyLimit,
) -> impl Stream<Item = Result<B, A2aError>>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    A2aError: From<E>,
{
    let state = (EventSizeTracker::new(limit), false);
    stream.scan(state, |(tracker, failed), item| {
        if *failed {
            return ready(None);
        }
        let out = match item {
            Ok(bytes) => tracker.feed(bytes.as_ref()).map(|()| bytes),
            Err(e) => Err(A2aError::from(e)),
        };
        *failed = out.is_err();
        ready(Some(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(limit: usize) -> EventSizeTracker {
        EventSizeTracker::new(BodyLimit::new(limit).unwrap())
    }

    #[test]
    fn many_small_events_pass_when_each_is_under_the_limit() {
        let mut t = tracker(16);
        for _ in 0..100 {
            t.feed(b"data: hello\n\n").unwrap();
        }
    }

    #[test]
    fn crlf_and_cr_terminators_reset_the_event() {
        for sep in [&b"\r\n\r\n"[..], b"\r\r", b"\n\r\n", b"\n\n"] {
            let mut t = tracker(16);
            for _ in 0..20 {
                t.feed(b"data: hello").unwrap();
                t.feed(sep).unwrap();
            }
        }
    }

    #[test]
    fn crlf_split_across_chunks_is_one_terminator() {
        let mut t = tracker(16);
        for _ in 0..20 {
            t.feed(b"data: hi\r").unwrap();
            t.feed(b"\n\r").unwrap();
            t.feed(b"\n").unwrap();
        }
    }

    #[test]
    fn unterminated_event_over_limit_is_rejected() {
        let mut t = tracker(16);
        let mut result = Ok(());
        for _ in 0..10 {
            result = t.feed(b"data: aaaa");
            if result.is_err() {
                break;
            }
        }
        assert!(matches!(result, Err(A2aError::BodyTooLarge { limit: 16 })));
    }

    #[test]
    fn multi_line_event_counts_all_lines() {
        let mut t = tracker(20);
        t.feed(b"data: aaaaa\n").unwrap();
        assert!(t.feed(b"data: bbbbbbbbbb\n").is_err());
    }

    #[tokio::test]
    async fn capped_stream_yields_error_once_then_ends() {
        let chunks = vec![
            Ok::<_, A2aError>(b"data: ok\n\n".to_vec()),
            Ok(b"data: ".to_vec()),
            Ok(vec![b'x'; 64]),
            Ok(b"never seen".to_vec()),
        ];
        let mut capped = Box::pin(cap_event_size(
            tokio_stream::iter(chunks),
            BodyLimit::new(16).unwrap(),
        ));
        assert!(capped.next().await.unwrap().is_ok());
        assert!(capped.next().await.unwrap().is_ok());
        assert!(matches!(
            capped.next().await.unwrap(),
            Err(A2aError::BodyTooLarge { .. })
        ));
        assert!(capped.next().await.is_none());
    }
}
