# Local Inference (Candle)

Run HuggingFace GGUF models locally via [candle](https://github.com/huggingface/candle) without external API dependencies. Metal and CUDA GPU acceleration are supported.

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
chat_template = "mistral"          # llama3, chatml, mistral, phi3, raw
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

## Candle-Backed Classifiers

When built with the `classifiers` feature, Zeph uses Candle to run DeBERTa-based models directly for injection detection and PII detection — no external API calls required.

### Injection Detection (`CandleClassifier`)

`CandleClassifier` runs `protectai/deberta-v3-small-prompt-injection-v2` (sequence classification) to detect prompt injection attempts in incoming messages. When the model scores above `injection_threshold`, the message is flagged and existing injection-handling logic applies.

Long inputs are split into overlapping chunks (448 tokens each, 64-token overlap). The final score is the maximum across all chunks.

### PII Detection (`CandlePiiClassifier`)

`CandlePiiClassifier` runs `iiiorg/piiranha-v1-detect-personal-information` (NER token classification) to detect personal information in messages. Detected spans are merged with the existing regex-based PII filter — the union of both result sets is used.

Per-token confidence below `pii_threshold` is treated as O (no entity). Entity types include: `GIVENNAME`, `EMAIL`, `PHONE`, `DRIVERLICENSE`, `PASSPORT`, `IBAN`, and others as defined by the model.

### Configuration

```toml
[classifiers]
enabled = true                                            # Master switch (default: false)
timeout_ms = 5000                                        # Per-inference timeout in ms (default: 5000)
injection_model = "protectai/deberta-v3-small-prompt-injection-v2"
injection_threshold = 0.8                                # Minimum score to classify as injection (default: 0.8)
# injection_model_sha256 = "abc123..."                   # Optional: verify model file integrity at load
pii_enabled = true                                       # Enable NER PII detection (default: false)
pii_model = "iiiorg/piiranha-v1-detect-personal-information"
pii_threshold = 0.75                                     # Minimum per-token confidence (default: 0.75)
# pii_model_sha256 = "def456..."                         # Optional: verify model file integrity at load
```

**SHA-256 verification:** Set `injection_model_sha256` or `pii_model_sha256` to the hex digest of the model's safetensors file. Zeph verifies the file before loading and aborts startup on mismatch. Use this in security-sensitive deployments to detect corruption or tampering.

**Timeout fallback:** When an inference call exceeds `timeout_ms`, Zeph falls back to the existing regex-based detection. Classifiers never block the agent — degraded mode is always available.

**Model download:** Models are downloaded from HuggingFace on first use and cached locally. Subsequent startups load from cache. Set `injection_model` / `pii_model` to a custom HuggingFace repo ID to use alternative models with the same DeBERTa architecture.
