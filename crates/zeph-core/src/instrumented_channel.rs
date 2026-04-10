// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Instrumented wrappers around tokio channels.
//!
//! When the `profiling` feature is disabled, all wrappers delegate
//! directly to the inner channel with zero overhead — no counters,
//! no timing, no additional allocations.
//!
//! When `profiling` is enabled, each send/receive updates an atomic
//! counter and periodically emits a `tracing::trace!` event with
//! channel-level metrics (sent count, backpressure latency).

#[cfg(feature = "profiling")]
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Instrumented wrapper around [`mpsc::Sender`].
///
/// Tracks message counts and backpressure when the `profiling` feature
/// is enabled. Every 16th send emits a `tracing::trace!` event with
/// the current queue depth and backpressure latency.
pub struct InstrumentedSender<T> {
    inner: mpsc::Sender<T>,
    name: &'static str,
    #[cfg(feature = "profiling")]
    sent: AtomicU64,
}

impl<T> InstrumentedSender<T> {
    /// Send a value, recording metrics when profiling is active.
    ///
    /// Behaves identically to [`mpsc::Sender::send`]; returns an error
    /// if the receiver has been dropped.
    ///
    /// # Errors
    ///
    /// Returns [`mpsc::error::SendError`] if the channel receiver has been dropped.
    pub async fn send(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        #[cfg(feature = "profiling")]
        let start = std::time::Instant::now();

        let result = self.inner.send(value).await;

        #[cfg(feature = "profiling")]
        {
            let count = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
            let latency_us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
            // Sample queue depth every 16th send to minimize overhead.
            if count.trailing_zeros() >= 4 {
                tracing::trace!(
                    channel = self.name,
                    sent = count,
                    queue_depth = self.inner.max_capacity() - self.inner.capacity(),
                    backpressure_latency_us = latency_us,
                    "channel.metrics"
                );
            }
        }

        result
    }

    /// Attempt to send without waiting.
    ///
    /// Returns immediately with an error if the channel is full or the
    /// receiver has been dropped. Does not update the sent counter.
    ///
    /// # Errors
    ///
    /// Returns [`mpsc::error::TrySendError`] if the channel is full or the receiver is gone.
    pub fn try_send(&self, value: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.inner.try_send(value)
    }

    /// Returns a reference to the inner sender.
    #[must_use]
    pub fn inner(&self) -> &mpsc::Sender<T> {
        &self.inner
    }

    /// Extracts the inner sender, discarding instrumentation.
    ///
    /// Use when passing the sender to code that does not accept the instrumented
    /// wrapper (e.g., a watcher that takes ownership of `mpsc::Sender<T>`).
    #[must_use]
    pub fn into_inner(self) -> mpsc::Sender<T> {
        self.inner
    }
}

impl<T> Clone for InstrumentedSender<T> {
    /// Clone the sender, resetting the `sent` counter to zero.
    ///
    /// Each clone maintains an independent per-clone sent count so that
    /// trace events from concurrent producers are not aggregated. When
    /// reading trace events, the `sent` field represents the number of
    /// messages dispatched by **this clone** since it was created, not
    /// the global channel throughput.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            name: self.name,
            #[cfg(feature = "profiling")]
            sent: AtomicU64::new(0),
        }
    }
}

/// Instrumented wrapper around [`mpsc::Receiver`].
///
/// Tracks received message count when the `profiling` feature is enabled.
pub struct InstrumentedReceiver<T> {
    inner: mpsc::Receiver<T>,
    #[allow(dead_code)]
    name: &'static str,
    #[cfg(feature = "profiling")]
    received: AtomicU64,
}

impl<T> InstrumentedReceiver<T> {
    /// Wrap an existing [`mpsc::Receiver`] in an instrumented receiver.
    ///
    /// Use this when the receiver is constructed outside the
    /// [`instrumented_channel`] constructor (e.g., from a pre-existing
    /// `WatcherBundle`).
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::mpsc;
    /// use zeph_core::instrumented_channel::InstrumentedReceiver;
    ///
    /// let (_, rx) = mpsc::channel::<u32>(4);
    /// let _instrumented = InstrumentedReceiver::from_receiver(rx, "my-channel");
    /// ```
    #[must_use]
    pub fn from_receiver(inner: mpsc::Receiver<T>, name: &'static str) -> Self {
        Self {
            inner,
            name,
            #[cfg(feature = "profiling")]
            received: AtomicU64::new(0),
        }
    }

    /// Receive a value, recording metrics when profiling is active.
    ///
    /// Returns `None` when all senders have been dropped and the channel is empty.
    pub async fn recv(&mut self) -> Option<T> {
        let result = self.inner.recv().await;
        #[cfg(feature = "profiling")]
        if result.is_some() {
            self.received.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Returns a mutable reference to the inner receiver.
    pub fn inner_mut(&mut self) -> &mut mpsc::Receiver<T> {
        &mut self.inner
    }

    /// Extracts the inner receiver, discarding instrumentation.
    ///
    /// Use when passing the receiver to code that does not accept
    /// the instrumented wrapper (e.g., agent builder methods).
    #[must_use]
    pub fn into_inner(self) -> mpsc::Receiver<T> {
        self.inner
    }
}

/// Instrumented wrapper around [`mpsc::UnboundedSender`].
///
/// For unbounded channels, only the sent message count is tracked —
/// there is no backpressure or queue depth because unbounded channels
/// have no capacity limit.
pub struct InstrumentedUnboundedSender<T> {
    inner: mpsc::UnboundedSender<T>,
    name: &'static str,
    #[cfg(feature = "profiling")]
    sent: AtomicU64,
}

impl<T> InstrumentedUnboundedSender<T> {
    /// Wrap an existing [`mpsc::UnboundedSender`] in an instrumented sender.
    ///
    /// Use this when the sender is constructed outside the
    /// [`instrumented_unbounded_channel`] constructor (e.g., obtained from
    /// an external channel factory).
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::mpsc;
    /// use zeph_core::instrumented_channel::InstrumentedUnboundedSender;
    ///
    /// let (tx, _rx) = mpsc::unbounded_channel::<String>();
    /// let _instrumented = InstrumentedUnboundedSender::from_sender(tx, "status");
    /// ```
    #[must_use]
    pub fn from_sender(inner: mpsc::UnboundedSender<T>, name: &'static str) -> Self {
        Self {
            inner,
            name,
            #[cfg(feature = "profiling")]
            sent: AtomicU64::new(0),
        }
    }

    /// Send a value, recording sent count when profiling is active.
    ///
    /// Emits a `tracing::trace!` event every 64th send with the cumulative
    /// sent count. No queue depth or backpressure is tracked for unbounded channels.
    ///
    /// # Errors
    ///
    /// Returns [`mpsc::error::SendError`] if the receiver has been dropped.
    pub fn send(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        let result = self.inner.send(value);
        #[cfg(feature = "profiling")]
        {
            let count = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
            if count.trailing_zeros() >= 6 {
                tracing::trace!(channel = self.name, sent = count, "channel.metrics");
            }
        }
        result
    }

    /// Returns a reference to the inner sender.
    #[must_use]
    pub fn inner(&self) -> &mpsc::UnboundedSender<T> {
        &self.inner
    }
}

impl<T> Clone for InstrumentedUnboundedSender<T> {
    /// Clone the sender, resetting the `sent` counter to zero.
    ///
    /// Each clone maintains an independent per-clone sent count. See
    /// [`InstrumentedSender`]'s `Clone` impl for the rationale.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            name: self.name,
            #[cfg(feature = "profiling")]
            sent: AtomicU64::new(0),
        }
    }
}

/// Create an instrumented bounded mpsc channel pair.
///
/// The `name` is used in tracing events to identify the channel.
/// Use a short, static `&'static str` such as `"skill_reload"` or
/// `"config_reload"` — it is embedded in every trace event.
///
/// # Examples
///
/// ```
/// use zeph_core::instrumented_channel::instrumented_channel;
///
/// let (tx, mut rx) = instrumented_channel::<u32>(32, "my-channel");
/// // tx.send(42).await?;
/// // let _ = rx.recv().await;
/// ```
#[must_use]
pub fn instrumented_channel<T>(
    buffer: usize,
    name: &'static str,
) -> (InstrumentedSender<T>, InstrumentedReceiver<T>) {
    let (tx, rx) = mpsc::channel(buffer);
    (
        InstrumentedSender {
            inner: tx,
            name,
            #[cfg(feature = "profiling")]
            sent: AtomicU64::new(0),
        },
        InstrumentedReceiver {
            inner: rx,
            name,
            #[cfg(feature = "profiling")]
            received: AtomicU64::new(0),
        },
    )
}

/// Create an instrumented unbounded mpsc channel pair.
///
/// Returns an [`InstrumentedUnboundedSender`] and a raw [`mpsc::UnboundedReceiver`].
/// The receiver is not wrapped because unbounded channels have no meaningful
/// receive-side metrics beyond what the sender already tracks.
///
/// # Examples
///
/// ```
/// use zeph_core::instrumented_channel::instrumented_unbounded_channel;
///
/// let (tx, mut rx) = instrumented_unbounded_channel::<String>("status");
/// // tx.send("hello".to_string())?;
/// // let _ = rx.recv().await;
/// ```
#[must_use]
pub fn instrumented_unbounded_channel<T>(
    name: &'static str,
) -> (InstrumentedUnboundedSender<T>, mpsc::UnboundedReceiver<T>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        InstrumentedUnboundedSender {
            inner: tx,
            name,
            #[cfg(feature = "profiling")]
            sent: AtomicU64::new(0),
        },
        rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sending through a bounded channel increments the `sent` counter.
    #[cfg(feature = "profiling")]
    #[tokio::test]
    async fn bounded_send_increments_counter() {
        let (tx, mut rx) = instrumented_channel::<u32>(8, "test-bounded");
        assert_eq!(tx.sent.load(Ordering::Relaxed), 0);

        tx.send(1).await.expect("send succeeds");
        assert_eq!(tx.sent.load(Ordering::Relaxed), 1);

        tx.send(2).await.expect("send succeeds");
        assert_eq!(tx.sent.load(Ordering::Relaxed), 2);

        // drain to avoid log noise from dropped channel
        drop(rx.recv().await);
        drop(rx.recv().await);
    }

    /// Cloning an `InstrumentedSender` resets the clone's counter to 0.
    #[cfg(feature = "profiling")]
    #[tokio::test]
    async fn clone_resets_counter_to_zero() {
        let (tx, mut rx) = instrumented_channel::<u32>(8, "test-clone");
        tx.send(1).await.expect("send succeeds");
        tx.send(2).await.expect("send succeeds");
        assert_eq!(tx.sent.load(Ordering::Relaxed), 2);

        let tx2 = tx.clone();
        assert_eq!(tx2.sent.load(Ordering::Relaxed), 0, "clone starts at 0");

        // Original counter is unaffected by the clone.
        assert_eq!(tx.sent.load(Ordering::Relaxed), 2);

        drop(rx.recv().await);
        drop(rx.recv().await);
    }

    /// Sending through an unbounded channel increments the `sent` counter.
    #[cfg(feature = "profiling")]
    #[test]
    fn unbounded_send_increments_counter() {
        let (tx, _rx) = instrumented_unbounded_channel::<u32>("test-unbounded");
        assert_eq!(tx.sent.load(Ordering::Relaxed), 0);

        tx.send(1).expect("send succeeds");
        assert_eq!(tx.sent.load(Ordering::Relaxed), 1);
    }

    /// Cloning an `InstrumentedUnboundedSender` resets the clone's counter.
    #[cfg(feature = "profiling")]
    #[test]
    fn unbounded_clone_resets_counter() {
        let (tx, _rx) = instrumented_unbounded_channel::<u32>("test-unbounded-clone");
        tx.send(1).expect("send succeeds");
        assert_eq!(tx.sent.load(Ordering::Relaxed), 1);

        let tx2 = tx.clone();
        assert_eq!(tx2.sent.load(Ordering::Relaxed), 0);
    }

    /// `from_receiver` wraps an existing receiver without panicking.
    #[tokio::test]
    async fn from_receiver_wraps_existing() {
        let (tx_raw, rx_raw) = tokio::sync::mpsc::channel::<u32>(4);
        let mut wrapped = InstrumentedReceiver::from_receiver(rx_raw, "wrap-test");
        tx_raw.send(42).await.expect("send succeeds");
        let val = wrapped.recv().await.expect("recv succeeds");
        assert_eq!(val, 42);
    }

    /// `from_sender` wraps an existing unbounded sender without panicking.
    #[test]
    fn from_sender_wraps_existing() {
        let (tx_raw, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let wrapped = InstrumentedUnboundedSender::from_sender(tx_raw, "wrap-unbounded");
        wrapped.send(99).expect("send succeeds");
        let val = rx.try_recv().expect("value available");
        assert_eq!(val, 99);
    }
}
