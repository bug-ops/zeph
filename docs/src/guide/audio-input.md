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
| Local Whisper (candle) | — | — | Planned |

## Limitations

- **25 MB file size limit** — audio files exceeding this are rejected before upload.
- **No streaming transcription** — the entire file is sent and transcribed in one request.
- **No TTS** — text-to-speech output is not yet supported.
- **Batch only** — one audio attachment per message; additional attachments are ignored.
