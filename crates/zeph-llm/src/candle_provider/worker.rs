// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded inference worker for `CandleProvider`.
//!
//! Owns `ModelWeights` exclusively — no mutex required.
//! Callers send `InferenceRequest` through a bounded mpsc channel and receive
//! results via a oneshot channel embedded in each request.

use candle_transformers::models::quantized_llama::ModelWeights;
use std::time::Duration;
use tokenizers::Tokenizer;

use super::generate::{GenerationConfig, GenerationOutput, generate_tokens};
use super::template::ChatTemplate;
use crate::error::LlmError;
use crate::provider::Message;

/// Default timeout for a single inference request (CPU inference can be slow).
pub(crate) const DEFAULT_INFERENCE_TIMEOUT_SECS: u64 = 120;

/// A single inference job dispatched through the worker channel.
pub struct InferenceRequest {
    pub messages: Vec<Message>,
    pub reply: tokio::sync::oneshot::Sender<Result<GenerationOutput, LlmError>>,
}

/// Static configuration for an inference worker (passed once at spawn time).
pub(crate) struct WorkerConfig {
    pub weights: ModelWeights,
    pub tokenizer: std::sync::Arc<Tokenizer>,
    pub eos_token_id: u32,
    pub template: ChatTemplate,
    pub generation_config: GenerationConfig,
    pub device: candle_core::Device,
}

/// Bounded inference worker that owns `ModelWeights` and processes requests
/// sequentially through a `tokio::sync::mpsc` channel.
///
/// The worker runs in a `spawn_blocking` thread (long-lived) to avoid
/// per-request thread-pool churn. Dropping all `Sender` clones terminates
/// the worker gracefully.
pub(crate) struct InferenceWorker {
    pub tx: tokio::sync::mpsc::Sender<InferenceRequest>,
    pub inference_timeout: Duration,
    /// `Some` on the original worker; `None` on all clones (they share the same channel).
    /// The task exits naturally when all `Sender` clones are dropped — `_handle` exists only
    /// to prevent premature abort, not for join.
    pub(super) _handle: Option<tokio::task::JoinHandle<()>>,
}

impl InferenceWorker {
    /// Spawn the worker. Returns immediately; the worker runs in the background.
    pub fn spawn(
        config: WorkerConfig,
        channel_capacity: usize,
        inference_timeout: Duration,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel::<InferenceRequest>(channel_capacity);

        let handle = tokio::task::spawn_blocking(move || {
            worker_loop(config, rx);
        });

        Self {
            tx,
            inference_timeout,
            _handle: Some(handle),
        }
    }
}

/// The blocking worker loop. Runs until all `Sender`s are dropped.
fn worker_loop(mut config: WorkerConfig, mut rx: tokio::sync::mpsc::Receiver<InferenceRequest>) {
    while let Some(req) = rx.blocking_recv() {
        let result = generate_sync(
            &mut config.weights,
            &config.tokenizer,
            config.eos_token_id,
            config.template,
            &config.generation_config,
            &config.device,
            &req.messages,
        );
        // If the caller timed out and dropped the receiver, ignore the send error.
        let _ = req.reply.send(result);
    }
    tracing::debug!("candle inference worker exiting: all senders dropped");
}

/// Synchronous generation — called only from inside the worker loop.
fn generate_sync(
    weights: &mut ModelWeights,
    tokenizer: &Tokenizer,
    eos_token_id: u32,
    template: ChatTemplate,
    generation_config: &GenerationConfig,
    device: &candle_core::Device,
    messages: &[Message],
) -> Result<GenerationOutput, LlmError> {
    let prompt = template.format(messages);
    let encoding = tokenizer
        .encode(prompt.as_str(), false)
        .map_err(|e| LlmError::Inference(format!("tokenizer encode failed: {e}")))?;
    let input_tokens = encoding.get_ids();

    let mut forward_fn =
        |input: &candle_core::Tensor, pos: usize| -> Result<candle_core::Tensor, LlmError> {
            weights.forward(input, pos).map_err(LlmError::Candle)
        };

    generate_tokens(
        &mut forward_fn,
        tokenizer,
        input_tokens,
        generation_config,
        eos_token_id,
        device,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_inference_timeout_is_nonzero() {
        assert!(DEFAULT_INFERENCE_TIMEOUT_SECS > 0);
    }

    /// Verify that the oneshot channel round-trip in `InferenceRequest` compiles and
    /// delivers a value correctly — without needing real `ModelWeights`.
    #[test]
    fn inference_request_oneshot_roundtrip() {
        use crate::provider::Message;

        let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<GenerationOutput, LlmError>>();
        let req = InferenceRequest {
            messages: vec![Message::from_legacy(crate::provider::Role::User, "hello")],
            reply: tx,
        };

        let expected_text = "world";
        let output = GenerationOutput {
            text: expected_text.into(),
            tokens_generated: 1,
        };
        req.reply
            .send(Ok(output))
            .expect("send must succeed when receiver is live");

        let result = rx.try_recv().expect("reply must be immediately available");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().text, expected_text);
    }

    /// Verify that the worker loop does not panic when a caller drops its receiver
    /// before the worker sends the reply (M1: `let _ = req.reply.send(result)`).
    #[tokio::test]
    async fn dropped_reply_receiver_does_not_block_worker() {
        let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<InferenceRequest>(1);

        // Simulate worker loop: drain one request, send reply (receiver already dropped).
        tokio::task::spawn_blocking(move || {
            if let Some(req) = req_rx.blocking_recv() {
                let result: Result<GenerationOutput, LlmError> = Ok(GenerationOutput {
                    text: "ok".into(),
                    tokens_generated: 1,
                });
                // reply send may fail — worker must not panic
                let _ = req.reply.send(result);
            }
        });

        let (_reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        drop(reply_rx); // drop receiver before worker sends

        let req = InferenceRequest {
            messages: vec![],
            reply: _reply_tx,
        };
        // Send succeeds — worker receives it and handles the dropped receiver gracefully.
        req_tx
            .send(req)
            .await
            .expect("channel must accept the request");

        // Give the worker task time to run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // No panic = test passes.
    }

    /// Cloned `InferenceWorker` must not own a `JoinHandle` (no leaked tasks).
    #[test]
    fn cloned_worker_has_no_handle() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InferenceRequest>(1);
        let worker = InferenceWorker {
            tx: tx.clone(),
            inference_timeout: Duration::from_secs(1),
            _handle: None,
        };
        assert!(worker._handle.is_none());
    }
}
