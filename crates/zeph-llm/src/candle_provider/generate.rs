use candle_core::Tensor;

use crate::error::LlmError;
use candle_transformers::generation::LogitsProcessor;

#[derive(Debug, Clone)]
pub struct GenerationConfig {
    pub temperature: f64,
    pub top_p: Option<f64>,
    pub top_k: Option<usize>,
    pub max_tokens: usize,
    pub seed: u64,
    pub repeat_penalty: f32,
    pub repeat_last_n: usize,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: Some(0.9),
            top_k: None,
            max_tokens: 2048,
            seed: 42,
            repeat_penalty: 1.1,
            repeat_last_n: 64,
        }
    }
}

pub struct GenerationOutput {
    pub text: String,
    pub tokens_generated: usize,
}

/// Run token generation loop on a quantized llama model.
///
/// `forward_fn` abstracts over the specific model variant's forward pass.
///
/// # Errors
///
/// Returns an error if the forward pass or token decoding fails.
pub fn generate_tokens<F>(
    forward_fn: &mut F,
    tokenizer: &tokenizers::Tokenizer,
    input_tokens: &[u32],
    config: &GenerationConfig,
    eos_token_id: u32,
    device: &candle_core::Device,
) -> Result<GenerationOutput, LlmError>
where
    F: FnMut(&Tensor, usize) -> Result<Tensor, LlmError>,
{
    let mut logits_processor = LogitsProcessor::from_sampling(
        config.seed,
        candle_transformers::generation::Sampling::TopKThenTopP {
            k: config.top_k.unwrap_or(40),
            p: config.top_p.unwrap_or(0.9),
            temperature: config.temperature,
        },
    );

    let mut all_tokens: Vec<u32> = input_tokens.to_vec();
    let mut generated_tokens: Vec<u32> = Vec::with_capacity(config.max_tokens);

    // Process the prompt in one batch
    let input = Tensor::new(input_tokens, device)?;
    let logits = forward_fn(&input, 0)?;
    let logits = logits.squeeze(0)?.to_dtype(candle_core::DType::F32)?;

    // Get logits for the last position
    let seq_len = logits.dim(0)?;
    let last_logits = logits.get(seq_len - 1)?;
    let last_logits = apply_repeat_penalty(
        &last_logits,
        &all_tokens,
        config.repeat_penalty,
        config.repeat_last_n,
    )?;

    let mut next_token = logits_processor.sample(&last_logits)?;
    generated_tokens.push(next_token);
    all_tokens.push(next_token);

    if next_token == eos_token_id {
        let text = decode_tokens(tokenizer, &generated_tokens)?;
        return Ok(GenerationOutput {
            text,
            tokens_generated: generated_tokens.len(),
        });
    }

    // Autoregressive generation
    for i in 0..config.max_tokens.saturating_sub(1) {
        let input = Tensor::new(&[next_token], device)?;
        let pos = input_tokens.len() + i + 1;
        let logits = forward_fn(&input, pos)?;
        let logits = logits.squeeze(0)?.to_dtype(candle_core::DType::F32)?;

        let last_logits = if logits.dims().len() > 1 {
            let seq_len = logits.dim(0)?;
            logits.get(seq_len - 1)?
        } else {
            logits
        };

        let last_logits = apply_repeat_penalty(
            &last_logits,
            &all_tokens,
            config.repeat_penalty,
            config.repeat_last_n,
        )?;

        next_token = logits_processor.sample(&last_logits)?;
        generated_tokens.push(next_token);
        all_tokens.push(next_token);

        if next_token == eos_token_id {
            break;
        }
    }

    let text = decode_tokens(tokenizer, &generated_tokens)?;
    Ok(GenerationOutput {
        text,
        tokens_generated: generated_tokens.len(),
    })
}

fn apply_repeat_penalty(
    logits: &Tensor,
    tokens: &[u32],
    penalty: f32,
    last_n: usize,
) -> Result<Tensor, LlmError> {
    if (penalty - 1.0).abs() < f32::EPSILON {
        return Ok(logits.clone());
    }
    let start = tokens.len().saturating_sub(last_n);
    let recent = &tokens[start..];
    candle_transformers::utils::apply_repeat_penalty(logits, penalty, recent)
        .map_err(LlmError::Candle)
}

fn decode_tokens(tokenizer: &tokenizers::Tokenizer, tokens: &[u32]) -> Result<String, LlmError> {
    tokenizer
        .decode(tokens, true)
        .map_err(|e| LlmError::Inference(format!("tokenizer decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_generation_config() {
        let config = GenerationConfig::default();
        assert!((config.temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(config.max_tokens, 2048);
        assert_eq!(config.seed, 42);
        assert!((config.repeat_penalty - 1.1).abs() < f32::EPSILON);
        assert_eq!(config.repeat_last_n, 64);
    }

    #[test]
    fn repeat_penalty_no_op_when_one() {
        let logits = Tensor::new(&[1.0_f32, 2.0, 3.0], &candle_core::Device::Cpu).unwrap();
        let result = apply_repeat_penalty(&logits, &[0, 1], 1.0, 64).unwrap();
        let vals: Vec<f32> = result.to_vec1().unwrap();
        assert!((vals[0] - 1.0).abs() < f32::EPSILON);
        assert!((vals[1] - 2.0).abs() < f32::EPSILON);
        assert!((vals[2] - 3.0).abs() < f32::EPSILON);
    }
}
