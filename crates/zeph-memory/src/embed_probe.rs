// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared embedding-dimension probe helper.
//!
//! Several ingest and memory subsystems need to learn an embedding provider's output
//! dimension before creating a Qdrant collection: they call the provider once with a
//! throwaway probe string and use the resulting vector's length as the collection's
//! vector size. [`probe_vector_size`] centralizes that sequence (including an optional
//! timeout) so callers stop re-implementing it inline.

use std::future::Future;
use std::time::Duration;

/// Error surfaced by [`probe_vector_size`].
#[derive(Debug, thiserror::Error)]
pub enum ProbeError<E> {
    /// The probe embedding call exceeded `timeout`.
    #[error("embedding probe timed out after {0:?}")]
    Timeout(Duration),
    /// The probe embedding call itself returned an error.
    #[error("{0}")]
    Embed(E),
}

/// Learn an embedding provider's output dimension.
///
/// Awaits `embed_fut` — the caller's already-invoked embed call for a throwaway probe string —
/// and returns the length of the resulting vector. When `timeout` is `Some`, the await is
/// bounded by it; pass `None` to await unconditionally.
///
/// Taking the future itself (rather than a `Fn(&str) -> Fut` closure) sidesteps the higher-rank
/// lifetime that a borrowing `embed(&self, text: &str)` call would otherwise require, and lets
/// each caller keep its own probe string literal.
///
/// This function only performs the probe — pair the returned size with the caller's own
/// `ensure_collection` (or `ensure_collection_with_quantization`) call, since collection setup
/// differs across call sites (plain vs. quantized, fixed vs. caller-supplied collection name).
///
/// # Errors
///
/// Returns [`ProbeError::Timeout`] if `timeout` elapses, or [`ProbeError::Embed`] if the probe
/// call fails.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use zeph_memory::embed_probe::probe_vector_size;
///
/// # async fn probe() -> Result<(), Box<dyn std::error::Error>> {
/// let embed_fut = async { Ok::<_, std::convert::Infallible>(vec![0.0_f32; 384]) };
/// let size = probe_vector_size(embed_fut, Some(Duration::from_secs(15))).await?;
/// assert_eq!(size, 384);
/// # Ok(())
/// # }
/// ```
#[tracing::instrument(
    name = "memory.embed_probe.probe_vector_size",
    skip_all,
    err,
    level = "debug"
)]
pub async fn probe_vector_size<Fut, E>(
    embed_fut: Fut,
    timeout: Option<Duration>,
) -> Result<u64, ProbeError<E>>
where
    Fut: Future<Output = Result<Vec<f32>, E>>,
    E: std::fmt::Display,
{
    let vector = match timeout {
        Some(d) => tokio::time::timeout(d, embed_fut)
            .await
            .map_err(|_| ProbeError::Timeout(d))?
            .map_err(ProbeError::Embed)?,
        None => embed_fut.await.map_err(ProbeError::Embed)?,
    };
    // Safe: a Vec<f32> with 4B+ elements is impossible in practice on any 64-bit platform.
    Ok(vector.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    #[tokio::test]
    async fn probe_vector_size_returns_length() {
        let size = probe_vector_size(async { Ok::<_, Infallible>(vec![0.0_f32; 128]) }, None)
            .await
            .unwrap();
        assert_eq!(size, 128);
    }

    #[tokio::test]
    async fn probe_vector_size_propagates_embed_error() {
        let result = probe_vector_size(async { Err::<Vec<f32>, _>("boom") }, None).await;
        assert!(matches!(result, Err(ProbeError::Embed("boom"))));
    }

    #[tokio::test]
    async fn probe_vector_size_times_out() {
        let result = probe_vector_size(
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<_, Infallible>(vec![0.0_f32; 4])
            },
            Some(Duration::from_millis(1)),
        )
        .await;
        assert!(matches!(result, Err(ProbeError::Timeout(_))));
    }

    #[tokio::test]
    async fn probe_vector_size_no_timeout_awaits_unconditionally() {
        let size = probe_vector_size(
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok::<_, Infallible>(vec![0.0_f32; 7])
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(size, 7);
    }
}
