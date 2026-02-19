use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use candle_core::{Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::whisper::{self as m, Config};
use tokenizers::Tokenizer;

use crate::error::LlmError;
use crate::stt::{SpeechToText, Transcription};

#[derive(Clone)]
pub struct CandleWhisperProvider {
    model: Arc<Mutex<m::model::Whisper>>,
    config: Config,
    mel_filters: Vec<f32>,
    tokenizer: Arc<Tokenizer>,
    device: Device,
    language: String,
}

impl std::fmt::Debug for CandleWhisperProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleWhisperProvider")
            .field("device", &device_name(&self.device))
            .finish_non_exhaustive()
    }
}

fn device_name(d: &Device) -> &'static str {
    match d {
        Device::Cpu => "cpu",
        Device::Cuda(_) => "cuda",
        Device::Metal(_) => "metal",
    }
}

fn detect_device() -> Device {
    #[cfg(feature = "metal")]
    {
        if let Ok(d) = Device::new_metal(0) {
            return d;
        }
    }
    #[cfg(feature = "cuda")]
    {
        if let Ok(d) = Device::new_cuda(0) {
            return d;
        }
    }
    Device::Cpu
}

impl CandleWhisperProvider {
    /// Load a Whisper model from a HuggingFace repo.
    ///
    /// # Errors
    ///
    /// Returns `LlmError::ModelLoad` if downloading or loading fails.
    pub fn load(repo_id: &str, device: Option<Device>, language: &str) -> Result<Self, LlmError> {
        let device = device.unwrap_or_else(detect_device);
        tracing::info!(
            repo = repo_id,
            device = device_name(&device),
            "loading candle whisper model"
        );

        let api = hf_hub::api::sync::Api::new()
            .map_err(|e| LlmError::ModelLoad(format!("hf-hub init: {e}")))?;
        let repo = api.model(repo_id.to_string());

        let config_path = repo
            .get("config.json")
            .map_err(|e| LlmError::ModelLoad(format!("config.json: {e}")))?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| LlmError::ModelLoad(format!("tokenizer.json: {e}")))?;
        let weights_path = repo
            .get("model.safetensors")
            .map_err(|e| LlmError::ModelLoad(format!("model.safetensors: {e}")))?;

        let config: Config = serde_json::from_reader(std::io::BufReader::new(
            std::fs::File::open(&config_path)
                .map_err(|e| LlmError::ModelLoad(format!("open config: {e}")))?,
        ))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| LlmError::ModelLoad(format!("tokenizer: {e}")))?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)
                .map_err(|e| LlmError::ModelLoad(format!("weights: {e}")))?
        };

        let model = m::model::Whisper::load(&vb, config.clone())?;

        let mel_bytes = match config.num_mel_bins {
            80 => include_bytes!("melfilters.bytes").as_slice(),
            128 => include_bytes!("melfilters128.bytes").as_slice(),
            n => {
                return Err(LlmError::ModelLoad(format!(
                    "unsupported num_mel_bins: {n}"
                )));
            }
        };
        let mut mel_filters = vec![0f32; mel_bytes.len() / 4];
        for (i, chunk) in mel_bytes.chunks_exact(4).enumerate() {
            mel_filters[i] = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }

        tracing::info!("candle whisper model loaded");

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            config,
            mel_filters,
            tokenizer: Arc::new(tokenizer),
            device,
            language: language.to_string(),
        })
    }

    fn transcribe_sync(&self, audio: &[u8]) -> Result<Transcription, LlmError> {
        let pcm = decode_audio(audio)?;
        let mel = m::audio::pcm_to_mel(&self.config, &pcm, &self.mel_filters);
        let mel_len = mel.len();
        let n_mel = self.config.num_mel_bins;

        let mel = Tensor::from_vec(mel, (1, n_mel, mel_len / n_mel), &self.device)?;

        let sot = self
            .tokenizer
            .token_to_id(m::SOT_TOKEN)
            .ok_or_else(|| LlmError::TranscriptionFailed("missing SOT token".into()))?;
        let transcribe = self
            .tokenizer
            .token_to_id(m::TRANSCRIBE_TOKEN)
            .ok_or_else(|| LlmError::TranscriptionFailed("missing TRANSCRIBE token".into()))?;
        let no_timestamps = self
            .tokenizer
            .token_to_id(m::NO_TIMESTAMPS_TOKEN)
            .ok_or_else(|| LlmError::TranscriptionFailed("missing NO_TIMESTAMPS token".into()))?;
        let eot = self
            .tokenizer
            .token_to_id(m::EOT_TOKEN)
            .ok_or_else(|| LlmError::TranscriptionFailed("missing EOT token".into()))?;

        let lang_tag = if self.language == "auto" {
            "<|en|>".to_string()
        } else {
            format!("<|{}|>", self.language)
        };
        let language_token = self.tokenizer.token_to_id(&lang_tag).ok_or_else(|| {
            LlmError::TranscriptionFailed(format!(
                "language token {lang_tag} not found in tokenizer"
            ))
        })?;

        let mut model = self
            .model
            .lock()
            .map_err(|e| LlmError::TranscriptionFailed(format!("lock: {e}")))?;
        model.reset_kv_cache();

        let audio_features = model.encoder.forward(&mel, true)?;

        const MAX_DECODE_TOKENS: usize = 224;

        let mut tokens = vec![sot, language_token, transcribe, no_timestamps];

        for _ in 0..MAX_DECODE_TOKENS {
            let token_tensor = Tensor::new(tokens.as_slice(), &self.device)?.unsqueeze(0)?;
            let logits =
                model
                    .decoder
                    .forward(&token_tensor, &audio_features, tokens.len() == 4)?;

            let (_, seq_len, _) = logits.dims3()?;
            let next_logits = logits.i((0, seq_len - 1))?;
            let next_token = next_logits
                .argmax(candle_core::D::Minus1)?
                .to_scalar::<u32>()?;

            if next_token == eot {
                break;
            }
            tokens.push(next_token);
        }

        // Decode only generated tokens (skip prompt tokens)
        let generated = &tokens[4..];
        let text = self
            .tokenizer
            .decode(generated, true)
            .map_err(|e| LlmError::TranscriptionFailed(format!("decode: {e}")))?;

        Ok(Transcription {
            text: text.trim().to_string(),
            language: Some(
                if self.language == "auto" {
                    "en"
                } else {
                    &self.language
                }
                .into(),
            ),
            duration_secs: Some(pcm.len() as f32 / m::SAMPLE_RATE as f32),
        })
    }
}

impl SpeechToText for CandleWhisperProvider {
    fn transcribe(
        &self,
        audio: &[u8],
        _filename: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Transcription, LlmError>> + Send + '_>> {
        let audio = audio.to_vec();
        Box::pin(async move {
            let provider = self.clone();
            tokio::task::spawn_blocking(move || provider.transcribe_sync(&audio))
                .await
                .map_err(|e| LlmError::TranscriptionFailed(e.to_string()))?
        })
    }
}

fn decode_audio(bytes: &[u8]) -> Result<Vec<f32>, LlmError> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| LlmError::TranscriptionFailed(format!("probe: {e}")))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| LlmError::TranscriptionFailed("no audio track".into()))?;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| LlmError::TranscriptionFailed("unknown sample rate".into()))?;
    let channels = track.codec_params.channels.map_or(1, |c| c.count());
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| LlmError::TranscriptionFailed(format!("decoder: {e}")))?;

    let mut pcm = Vec::new();

    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                tracing::trace!("skipping packet decode error: {e}");
                continue;
            }
        };
        let spec = *decoded.spec();
        let mut sample_buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let samples = sample_buf.samples();

        if channels > 1 {
            for chunk in samples.chunks(channels) {
                let avg = chunk.iter().sum::<f32>() / channels as f32;
                pcm.push(avg);
            }
        } else {
            pcm.extend_from_slice(samples);
        }
    }

    if pcm.is_empty() {
        return Err(LlmError::TranscriptionFailed("no audio decoded".into()));
    }

    // Guard against pathological inputs: max 5 minutes at the source sample rate
    let max_samples = 5 * 60 * sample_rate as usize;
    if pcm.len() > max_samples {
        return Err(LlmError::TranscriptionFailed(format!(
            "audio too long: {} samples exceeds {max_samples} limit (5 min)",
            pcm.len()
        )));
    }

    // Resample to 16kHz if needed
    if sample_rate != m::SAMPLE_RATE as u32 {
        pcm = resample(&pcm, sample_rate, m::SAMPLE_RATE as u32)?;
    }

    Ok(pcm)
}

fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Result<Vec<f32>, LlmError> {
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
    };

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };

    let ratio = f64::from(to_rate) / f64::from(from_rate);
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, input.len(), 1)
        .map_err(|e| LlmError::TranscriptionFailed(format!("resampler init: {e}")))?;

    let output = resampler
        .process(&[input], None)
        .map_err(|e| LlmError::TranscriptionFailed(format!("resample: {e}")))?;

    Ok(output.into_iter().next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_detection_returns_cpu_by_default() {
        let d = detect_device();
        // On CI without GPU, should be CPU
        assert!(matches!(
            d,
            Device::Cpu | Device::Metal(_) | Device::Cuda(_)
        ));
    }

    #[test]
    fn debug_format() {
        let d = detect_device();
        let name = device_name(&d);
        assert!(!name.is_empty());
    }

    #[test]
    fn decode_audio_rejects_invalid_bytes() {
        let result = decode_audio(&[0, 1, 2, 3, 4]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("probe"), "expected probe error, got: {err}");
    }

    #[test]
    fn decode_audio_rejects_empty_input() {
        let result = decode_audio(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn resample_zeros_preserves_silence() {
        let input = vec![0.0_f32; 1000];
        let output = resample(&input, 44100, 16000).unwrap();
        assert!(!output.is_empty());
        for &s in &output {
            assert!(s.abs() < 1e-6, "expected silence, got {s}");
        }
    }

    #[test]
    fn resample_changes_length() {
        let input = vec![0.5_f32; 44100];
        let output = resample(&input, 44100, 16000).unwrap();
        let expected_len = (44100.0 * 16000.0 / 44100.0) as usize;
        let tolerance = expected_len / 10;
        assert!(
            output.len().abs_diff(expected_len) < tolerance,
            "expected ~{expected_len} samples, got {}",
            output.len()
        );
    }
}
