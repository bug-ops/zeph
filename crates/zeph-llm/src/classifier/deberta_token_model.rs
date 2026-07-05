// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared bias-correct DeBERTa-v2 token-classification model.
//!
//! Used by both `CandlePiiClassifier` (trait `PiiDetector`, in `candle_pii.rs`) and
//! `CandleNerClassifier` (trait `ClassifierBackend`, in `ner.rs`) — the two NER-style
//! classifiers differ in download orchestration, chunking, and span decoding, but both need the
//! same backbone-plus-head forward pass, so that single model implementation lives here to avoid
//! the two call sites drifting out of sync (see the bias-tensor rationale below, which is exactly
//! how they drifted before this module existed).

use candle_core::{Module, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::debertav2::{Config as DebertaConfig, DebertaV2Model};

/// DeBERTa-v2 backbone plus a token-classification head, loaded with a bias term.
///
/// `candle_transformers::models::debertav2::DebertaV2NERModel` cannot be used directly:
/// it builds its classifier layer with `candle_nn::linear_no_bias`, which silently skips
/// the `classifier.bias` tensor present in real NER checkpoints (confirmed against
/// `iiiorg/piiranha-v1-detect-personal-information`'s `model.safetensors`). Dropping a
/// trained bias term corrupts every token's logits, so this type re-implements the same
/// head using `candle_nn::linear` (with bias) instead to stay faithful to the checkpoint.
pub(crate) struct DebertaV2TokenClassifier {
    deberta: DebertaV2Model,
    dropout: candle_nn::Dropout,
    classifier: candle_nn::Linear,
}

impl DebertaV2TokenClassifier {
    /// Load the DeBERTa-v2 backbone under `vb` and a bias-including linear classifier
    /// head at `vb`'s root `classifier.*` keys, mirroring the checkpoint layout that
    /// `DebertaV2NERModel::load` targets internally.
    ///
    /// Trade-off versus the upstream `linear_no_bias`-based loader: a checkpoint whose
    /// classifier head has no `classifier.bias` tensor now fails to load
    /// (`candle_core::Error` from `candle_nn::linear`'s `vb.get_with_hints(.., "bias", ..)`)
    /// instead of loading silently without it. This is intentional — fail loud on an
    /// unsupported checkpoint shape rather than silently producing degraded predictions —
    /// but it does narrow the set of model repo IDs this classifier can load.
    pub(crate) fn load(
        vb: &VarBuilder,
        config: &DebertaConfig,
        id2label_len: usize,
    ) -> candle_core::Result<Self> {
        let deberta = DebertaV2Model::load(vb.clone(), config)?;
        // Dropout probability precision loss from f64 -> f32 is immaterial here.
        #[allow(clippy::cast_possible_truncation)]
        let dropout = candle_nn::Dropout::new(config.hidden_dropout_prob as f32);
        let classifier =
            candle_nn::linear(config.hidden_size, id2label_len, vb.root().pp("classifier"))?;
        Ok(Self {
            deberta,
            dropout,
            classifier,
        })
    }

    /// Run the backbone followed by the classification head, returning per-token logits
    /// of shape `[batch, seq_len, num_labels]`.
    pub(crate) fn forward(
        &self,
        input_ids: &Tensor,
        token_type_ids: Option<Tensor>,
        attention_mask: Option<Tensor>,
    ) -> candle_core::Result<Tensor> {
        let output = self
            .deberta
            .forward(input_ids, token_type_ids, attention_mask)?;
        let output = self.dropout.forward(&output, false)?;
        self.classifier.forward(&output)
    }
}
