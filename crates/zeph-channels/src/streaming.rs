// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared streaming buffer and helpers used by all channel adapters that support
//! edit-in-place streaming (Telegram, Discord, Slack).
//!
//! Each adapter holds one [`StreamingBuffer`] instance. Chunks are pushed via
//! [`push`][StreamingBuffer::push]; the adapter checks [`should_flush`][StreamingBuffer::should_flush]
//! to decide whether to issue an API edit, then calls either [`take`][StreamingBuffer::take]
//! (drain) or [`mark_flushed`][StreamingBuffer::mark_flushed] (Telegram: read without drain).
//!
//! The [`StreamingSend`] trait extracts the common `send_chunk` / `flush_chunks`
//! logic that is otherwise duplicated across Discord, Slack, and Telegram.

use std::time::{Duration, Instant};

use zeph_core::channel::ChannelError;

/// Accumulates streaming LLM chunks and throttles edit-in-place updates.
///
/// Each channel adapter holds one instance. Chunks are pushed via [`Self::push`];
/// the adapter checks [`Self::should_flush`] to decide whether to issue an API edit.
/// Adapters that drain the buffer on flush call [`Self::take`]; adapters that read
/// without draining (e.g. Telegram, which clones `accumulated`) call
/// [`Self::mark_flushed`] to record the edit timestamp without clearing text.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use zeph_channels::streaming::StreamingBuffer;
///
/// let mut buf = StreamingBuffer::new(Duration::from_secs(2));
/// buf.push("hello ");
/// buf.push("world");
/// assert!(buf.should_flush()); // first chunk, no prior edit
/// let text = buf.take();
/// assert_eq!(text, "hello world");
/// assert!(buf.is_empty());
/// ```
pub struct StreamingBuffer {
    accumulated: String,
    last_edit: Option<Instant>,
    throttle: Duration,
}

impl StreamingBuffer {
    /// Create a new buffer with the given edit throttle interval.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let buf = StreamingBuffer::new(Duration::from_millis(1500));
    /// assert!(buf.is_empty());
    /// ```
    #[must_use]
    pub fn new(throttle: Duration) -> Self {
        Self {
            accumulated: String::new(),
            last_edit: None,
            throttle,
        }
    }

    /// Append a chunk to the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let mut buf = StreamingBuffer::new(Duration::from_secs(2));
    /// buf.push("hello");
    /// buf.push(" world");
    /// assert_eq!(buf.text(), "hello world");
    /// ```
    pub fn push(&mut self, chunk: &str) {
        self.accumulated.push_str(chunk);
    }

    /// Whether enough time has elapsed since the last flush to issue an edit.
    ///
    /// Returns `true` on the first call when no edit has been issued yet.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let buf = StreamingBuffer::new(Duration::from_secs(2));
    /// assert!(buf.should_flush()); // no prior edit
    /// ```
    #[must_use]
    pub fn should_flush(&self) -> bool {
        self.last_edit
            .is_none_or(|last| last.elapsed() > self.throttle)
    }

    /// Drain the accumulated text and reset `last_edit` to now.
    ///
    /// Returns an empty string when nothing has been accumulated.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let mut buf = StreamingBuffer::new(Duration::from_secs(2));
    /// buf.push("data");
    /// let text = buf.take();
    /// assert_eq!(text, "data");
    /// assert!(buf.is_empty());
    /// ```
    pub fn take(&mut self) -> String {
        self.last_edit = Some(Instant::now());
        std::mem::take(&mut self.accumulated)
    }

    /// Reset all state: clear accumulated text and forget the last edit timestamp.
    ///
    /// Call this at the start of a new agent turn.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let mut buf = StreamingBuffer::new(Duration::from_secs(2));
    /// buf.push("stale");
    /// buf.reset();
    /// assert!(buf.is_empty());
    /// assert!(buf.should_flush());
    /// ```
    pub fn reset(&mut self) {
        self.accumulated.clear();
        self.last_edit = None;
    }

    /// Mark that an edit was just sent without draining the buffer.
    ///
    /// Used by adapters (e.g. Telegram) that clone `accumulated` rather than
    /// drain it — they send `self.buffer.text().to_owned()` and then call this
    /// to record the edit timestamp.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let mut buf = StreamingBuffer::new(Duration::from_millis(100));
    /// buf.push("data");
    /// buf.mark_flushed();
    /// // text is still present
    /// assert_eq!(buf.text(), "data");
    /// // but throttle is now active
    /// assert!(!buf.should_flush());
    /// ```
    pub fn mark_flushed(&mut self) {
        self.last_edit = Some(Instant::now());
    }

    /// Read the accumulated text without draining.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let mut buf = StreamingBuffer::new(Duration::from_secs(2));
    /// buf.push("hello");
    /// assert_eq!(buf.text(), "hello");
    /// assert!(!buf.is_empty());
    /// ```
    #[must_use]
    pub fn text(&self) -> &str {
        &self.accumulated
    }

    /// Whether the buffer is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let buf = StreamingBuffer::new(Duration::from_secs(2));
    /// assert!(buf.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.accumulated.is_empty()
    }

    /// Current accumulated length in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_channels::streaming::StreamingBuffer;
    ///
    /// let mut buf = StreamingBuffer::new(Duration::from_secs(2));
    /// buf.push("hi");
    /// assert_eq!(buf.len(), 2);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.accumulated.len()
    }
}

/// Shared streaming send/flush logic for edit-in-place channel adapters.
///
/// Implementing this trait eliminates the duplicated `send_chunk` / `flush_chunks`
/// bodies that would otherwise appear verbatim in Discord, Slack, and Telegram.
///
/// Implementors must provide:
/// - [`send_or_edit`](StreamingSend::send_or_edit) — the adapter-specific API call.
/// - [`streaming_buffer`](StreamingSend::streaming_buffer) /
///   [`streaming_buffer_mut`](StreamingSend::streaming_buffer_mut) — access to the shared buffer.
/// - [`has_pending_message`](StreamingSend::has_pending_message) — whether a sent message can be
///   edited in place.
/// - [`clear_pending_message`](StreamingSend::clear_pending_message) — reset the stored message
///   ID / timestamp.
///
/// Default methods [`streaming_send_chunk`](StreamingSend::streaming_send_chunk) and
/// [`streaming_flush_chunks`](StreamingSend::streaming_flush_chunks) encode the shared
/// accumulate-and-throttle pattern.
#[allow(async_fn_in_trait)]
pub trait StreamingSend {
    /// Issue one API call: send a new message or edit the last one in place.
    async fn send_or_edit(&mut self) -> Result<(), ChannelError>;

    /// Shared read access to the streaming buffer.
    fn streaming_buffer(&self) -> &StreamingBuffer;

    /// Exclusive access to the streaming buffer.
    fn streaming_buffer_mut(&mut self) -> &mut StreamingBuffer;

    /// Returns `true` when a message has been sent and can be edited in place.
    fn has_pending_message(&self) -> bool;

    /// Clear the stored message ID / timestamp so the next send creates a new message.
    fn clear_pending_message(&mut self);

    /// Accumulate `chunk` and call [`send_or_edit`] when the throttle window has elapsed.
    ///
    /// [`send_or_edit`]: StreamingSend::send_or_edit
    async fn streaming_send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.streaming_buffer_mut().push(chunk);
        if self.streaming_buffer().should_flush() {
            self.send_or_edit().await?;
        }
        Ok(())
    }

    /// Finalise the stream: perform one last [`send_or_edit`] when there is
    /// outstanding text or an editable message, then clear all streaming state.
    ///
    /// [`send_or_edit`]: StreamingSend::send_or_edit
    async fn streaming_flush_chunks(&mut self) -> Result<(), ChannelError> {
        if self.has_pending_message() || !self.streaming_buffer().is_empty() {
            self.send_or_edit().await?;
        }
        self.streaming_buffer_mut().reset();
        self.clear_pending_message();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn new_is_empty_and_ready_to_flush() {
        let buf = StreamingBuffer::new(Duration::from_secs(2));
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert!(buf.should_flush());
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn push_accumulates() {
        let mut buf = StreamingBuffer::new(Duration::from_secs(2));
        buf.push("hello ");
        buf.push("world");
        assert_eq!(buf.text(), "hello world");
        assert_eq!(buf.len(), 11);
        assert!(!buf.is_empty());
    }

    #[test]
    fn take_drains_and_returns_text() {
        let mut buf = StreamingBuffer::new(Duration::from_secs(2));
        buf.push("data");
        let text = buf.take();
        assert_eq!(text, "data");
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn take_activates_throttle() {
        let mut buf = StreamingBuffer::new(Duration::from_mins(1));
        buf.push("x");
        buf.take();
        // Throttle is active immediately after take()
        assert!(!buf.should_flush());
    }

    #[test]
    fn reset_clears_all_state() {
        let mut buf = StreamingBuffer::new(Duration::from_mins(1));
        buf.push("something");
        buf.take(); // activates throttle
        buf.push("more");
        buf.reset();
        assert!(buf.is_empty());
        assert!(buf.should_flush()); // last_edit cleared
    }

    #[test]
    fn mark_flushed_records_timestamp_without_draining() {
        let mut buf = StreamingBuffer::new(Duration::from_mins(1));
        buf.push("data");
        buf.mark_flushed();
        // Text still present
        assert_eq!(buf.text(), "data");
        // Throttle active
        assert!(!buf.should_flush());
    }

    #[test]
    fn should_flush_false_within_throttle() {
        let mut buf = StreamingBuffer::new(Duration::from_mins(1));
        buf.push("x");
        buf.mark_flushed();
        assert!(!buf.should_flush());
    }

    #[test]
    fn should_flush_true_after_throttle_elapsed() {
        let mut buf = StreamingBuffer::new(Duration::from_millis(1));
        buf.push("x");
        buf.mark_flushed();
        // Sleep slightly more than the 1 ms throttle
        std::thread::sleep(Duration::from_millis(5));
        assert!(buf.should_flush());
    }

    #[test]
    fn take_empty_buffer_returns_empty_string() {
        let mut buf = StreamingBuffer::new(Duration::from_secs(2));
        assert_eq!(buf.take(), "");
    }
}
