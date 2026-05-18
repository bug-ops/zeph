// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared streaming buffer used by all channel adapters that support
//! edit-in-place streaming (Telegram, Discord, Slack).
//!
//! Each adapter holds one [`StreamingBuffer`] instance. Chunks are pushed via
//! [`push`][StreamingBuffer::push]; the adapter checks [`should_flush`][StreamingBuffer::should_flush]
//! to decide whether to issue an API edit, then calls either [`take`][StreamingBuffer::take]
//! (drain) or [`mark_flushed`][StreamingBuffer::mark_flushed] (Telegram: read without drain).

use std::time::{Duration, Instant};

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
