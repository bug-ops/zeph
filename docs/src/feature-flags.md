# Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `a2a` | Enabled | [A2A protocol](https://github.com/a2aproject/A2A) client and server for agent-to-agent communication |
| `openai` | Enabled | OpenAI-compatible provider (GPT, Together, Groq, Fireworks, etc.) |
| `mcp` | Enabled | MCP client for external tool servers via stdio/HTTP transport |
| `candle` | Enabled | Local HuggingFace model inference via [candle](https://github.com/huggingface/candle) (GGUF quantized models) |
| `orchestrator` | Enabled | Multi-model routing with task-based classification and fallback chains |
| `self-learning` | Enabled | Skill evolution via failure detection, self-reflection, and LLM-generated improvements |
| `vault-age` | Enabled | Age-encrypted vault backend for file-based secret storage ([age](https://age-encryption.org/)) |
| `metal` | Disabled | Metal GPU acceleration for candle on macOS (implies `candle`) |
| `cuda` | Disabled | CUDA GPU acceleration for candle on Linux (implies `candle`) |

## Build Examples

```bash
cargo build --release                                     # all default features
cargo build --release --features metal                    # macOS with Metal GPU
cargo build --release --features cuda                     # Linux with NVIDIA GPU
cargo build --release --no-default-features               # minimal binary
```
