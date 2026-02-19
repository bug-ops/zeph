# Audio and Vision

Zeph supports audio transcription and image input across all channels.

## Audio Input

Pipeline: Audio attachment → STT provider → Transcribed text → Agent loop

### Configuration

Enable the `stt` feature flag:

```bash
cargo build --release --features stt
```

```toml
[llm.stt]
provider = "whisper"
model = "whisper-1"
```

The Whisper provider inherits the OpenAI API key from `[llm.openai]` or `ZEPH_OPENAI_API_KEY`. Environment variable overrides: `ZEPH_STT_PROVIDER`, `ZEPH_STT_MODEL`.

### Backends

| Backend | Provider | Feature | Description |
|---------|----------|---------|-------------|
| OpenAI Whisper API | `whisper` | `stt` | Cloud-based transcription |
| Local Whisper | `candle-whisper` | `candle` | Fully offline via candle |

### Local Whisper (Candle)

```bash
cargo build --release --features candle   # CPU
cargo build --release --features metal    # macOS Metal GPU
cargo build --release --features cuda     # NVIDIA GPU
```

```toml
[llm.stt]
provider = "candle-whisper"
model = "openai/whisper-tiny"
```

| Model | Parameters | Disk |
|-------|------------|------|
| `openai/whisper-tiny` | 39M | ~150 MB |
| `openai/whisper-base` | 74M | ~290 MB |
| `openai/whisper-small` | 244M | ~950 MB |

Models are downloaded from HuggingFace on first use. Device auto-detection: Metal → CUDA → CPU.

### Channel Support

- **Telegram**: voice notes and audio files downloaded automatically
- **Slack**: audio uploads detected, downloaded via `url_private_download` (25 MB limit, `.slack.com` host validation). Requires `files:read` OAuth scope
- **CLI/TUI**: no audio input mechanism

### Limits

- 5-minute audio duration guard (candle backend)
- 25 MB file size limit
- No streaming transcription — entire file processed in one pass
- One audio attachment per message

## Image Input

Pipeline: Image attachment → MessagePart::Image → LLM provider (base64) → Response

### Provider Support

| Provider | Vision | Notes |
|----------|--------|-------|
| Claude | Yes | Anthropic image content block |
| OpenAI | Yes | image_url data-URI |
| Ollama | Yes | Optional `vision_model` routing |
| Candle | No | Text-only |

### Ollama Vision Model

Route image requests to a dedicated model while keeping a smaller text model for regular queries:

```toml
[llm]
model = "mistral:7b"
vision_model = "llava:13b"
```

### Sending Images

- **CLI/TUI**: `/image /path/to/screenshot.png What is shown in this image?`
- **Telegram**: send a photo directly; the caption becomes the prompt

### Limits

- 20 MB maximum image size
- One image per message
- No image generation (input only)
