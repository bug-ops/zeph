# Vision (Image Input)

Zeph supports image input across all channels. Images are sent to the LLM as inline content parts alongside the text prompt, enabling visual reasoning tasks such as screenshot analysis, diagram interpretation, and document extraction.

## Pipeline

```
Image attachment → MessagePart::Image → LLM provider (base64) → Response
```

When a `ChannelMessage` contains an image `Attachment`, the agent converts it to a `MessagePart::Image` (raw bytes + MIME type). The active LLM provider encodes the image into its native API format and sends it as part of the chat request.

## Provider Support

Not all providers support vision. The `LlmProvider::supports_vision()` method indicates capability at runtime.

| Provider | Vision | Format |
|----------|--------|--------|
| Claude | Yes | `AnthropicContentBlock::Image` (base64 source) |
| OpenAI | Yes | Array content with `image_url` data-URI |
| Ollama | Yes | `with_images()` API; optional `vision_model` routing |
| Candle | No | Text-only |

### Ollama Vision Model Routing

Ollama can route image requests to a dedicated vision model (e.g., `llava`, `bakllava`) while keeping a smaller text model for regular queries. Set the `vision_model` field:

```toml
[llm]
provider = "ollama"
model = "mistral:7b"
vision_model = "llava:13b"
```

When `vision_model` is set and the message contains an image, Ollama uses the vision model for that request. When unset, images are sent to the default model (which must support vision).

## Sending Images

### CLI and TUI

Use the `/image` slash command followed by a file path:

```
/image /path/to/screenshot.png What is shown in this image?
```

The path can be absolute or relative to the working directory. Supported formats: JPEG, PNG, GIF, WebP.

### Telegram

Send a photo directly in the chat. The Telegram channel downloads the image via the Bot API (using the largest available photo size) and delivers it as an `Attachment` with `AttachmentKind::Image`. The text caption, if present, is used as the accompanying prompt.

A pre-download size guard rejects images exceeding 20 MB before the download begins.

## Configuration

```toml
[llm]
vision_model = "llava:13b"  # Ollama only: dedicated model for image requests
```

| Variable | Description | Default |
|----------|-------------|---------|
| `ZEPH_LLM_VISION_MODEL` | Vision model name for Ollama | (none) |

The `zeph init` wizard includes a prompt for the vision model when configuring the Ollama provider.

## Limits

- **20 MB maximum image size** -- images exceeding this limit are rejected.
- **Path traversal protection** -- the `/image` command validates file paths to prevent directory traversal attacks.
- **One image per message** -- additional image attachments in the same message are ignored.
- **No image generation** -- only image input (vision) is supported; image output is not.
