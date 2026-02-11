# Local Inference (Candle)

Run HuggingFace models locally via [candle](https://github.com/huggingface/candle) without external API dependencies. Supports GGUF quantized models with Metal/CUDA acceleration.

```bash
cargo build --release --features candle,metal  # macOS with Metal GPU
```

## Configuration

```toml
[llm]
provider = "candle"

[llm.candle]
source = "huggingface"
repo_id = "TheBloke/Mistral-7B-Instruct-v0.2-GGUF"
filename = "mistral-7b-instruct-v0.2.Q4_K_M.gguf"
template = "mistral"              # llama3, chatml, mistral, phi3, raw
embedding_repo = "sentence-transformers/all-MiniLM-L6-v2"  # optional BERT embeddings

[llm.candle.generation]
temperature = 0.7
top_p = 0.9
top_k = 40
max_tokens = 2048
repeat_penalty = 1.1
```

## Chat Templates

| Template | Models |
|----------|--------|
| `llama3` | Llama 3, Llama 3.1 |
| `chatml` | Qwen, Yi, OpenHermes |
| `mistral` | Mistral, Mixtral |
| `phi3` | Phi-3 |
| `raw` | No template (raw completion) |

## Device Auto-Detection

- **macOS** — Metal GPU (requires `--features metal`)
- **Linux with NVIDIA** — CUDA (requires `--features cuda`)
- **Fallback** — CPU
