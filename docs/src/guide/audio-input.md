# Audio Input

Zeph supports speech-to-text transcription, allowing users to send voice messages that are automatically converted to text before entering the agent loop.

## Pipeline

```
Audio attachment → SpeechToText provider → Transcription text → Agent loop
```

When a `ChannelMessage` contains an audio `Attachment`, the agent calls `resolve_message_text()` which detects the attachment, sends it to the configured STT provider, and replaces the message body with the transcribed text. The rest of the agent loop processes it as a normal text message.

## Configuration

Enable the `stt` feature flag:

```bash
cargo build --release --features stt
```

Add the STT section to your config:

```toml
[llm.stt]
provider = "whisper"
model = "whisper-1"
```

The Whisper provider inherits the OpenAI API key from the `[llm.openai]` section (or `ZEPH_OPENAI_API_KEY` env var). No separate key is needed.

## Supported Backends

| Backend | Provider | Feature | Status |
|---------|----------|---------|--------|
| OpenAI Whisper API | `whisper` | `stt` | Available |
| Local Whisper (candle) | `candle-whisper` | `candle` | Available |

## Local Whisper (Candle)

The `candle-whisper` backend runs Whisper inference locally via [candle](https://github.com/huggingface/candle) — no network calls, fully offline after the initial model download.

### Requirements

Enable the `candle` feature flag:

```bash
cargo build --release --features candle   # CPU
cargo build --release --features metal    # macOS Metal GPU (implies candle)
cargo build --release --features cuda     # Linux NVIDIA GPU (implies candle)
```

### Configuration

```toml
[llm.stt]
provider = "candle-whisper"
model = "openai/whisper-tiny"
```

### Model Options

Models are downloaded from HuggingFace on first use and cached locally.

| Model | HuggingFace ID | Parameters | Disk |
|-------|---------------|------------|------|
| Tiny | `openai/whisper-tiny` | 39M | ~150 MB |
| Base | `openai/whisper-base` | 74M | ~290 MB |
| Small | `openai/whisper-small` | 244M | ~950 MB |

Smaller models are faster but less accurate. `whisper-tiny` is a good starting point for low-latency use cases.

### Device Auto-Detection

The backend automatically selects the best available compute device:

1. **Metal** — if `metal` feature is enabled and running on macOS
2. **CUDA** — if `cuda` feature is enabled and an NVIDIA GPU is available
3. **CPU** — fallback

### Audio Pipeline

Incoming audio is processed through: symphonia decode, rubato resample to 16 kHz mono, mel spectrogram extraction, then candle Whisper inference.

### Limitations

- **5-minute audio duration guard** — recordings longer than 5 minutes are rejected.
- **No streaming** — the entire file is decoded and transcribed in one pass.

## Telegram Voice Messages

The Telegram channel automatically detects voice and audio messages. When a user sends a voice note or audio file, the adapter downloads the file bytes via the Telegram Bot API and wraps them as an `Attachment` with `AttachmentKind::Audio`. The attachment then follows the standard transcription pipeline described above.

Download failures (network errors, expired file links) are logged at `warn` level and gracefully skipped — the message is delivered without an attachment rather than causing an error.

Bootstrap wiring is automatic: when `[llm.stt]` is present in the config and the `stt` feature is enabled, `main.rs` creates a `WhisperProvider` and injects it into the agent via `with_stt()`. No additional setup is needed beyond the configuration shown above.

## Slack Audio Files

The Slack channel automatically detects audio file uploads and voice messages in incoming events. When a message contains files with audio MIME types (`audio/*`) or `video/webm` (commonly used for voice recordings), the adapter downloads the file and wraps it as an `Attachment` with `AttachmentKind::Audio`. The attachment then follows the standard transcription pipeline.

Files are downloaded via `url_private_download` using Bearer token authentication with the bot token. For security, the adapter validates that the download URL host ends with `.slack.com` before making the request. Files exceeding 25 MB are skipped.

Download failures (network errors, host validation rejection, oversized files) are logged at `warn` level and gracefully skipped — the message is delivered without an attachment.

To enable Slack audio transcription, ensure both the `slack` and `stt` features are active and `[llm.stt]` is configured. Add the `files:read` OAuth scope to your Slack app so the bot can access uploaded files.

## Limitations

- **25 MB file size limit** — audio files exceeding this are rejected before upload.
- **No streaming transcription** — the entire file is sent and transcribed in one request.
- **No TTS** — text-to-speech output is not yet supported.
- **Batch only** — one audio attachment per message; additional attachments are ignored.
