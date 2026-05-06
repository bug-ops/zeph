# Gonka AI Provider

[Gonka](https://gonka.ai) is a decentralized AI inference network built on a Cosmos-SDK chain that routes LLM requests to a peer-to-peer pool of GPU operators. Zeph supports two access paths.

## Path A: GonkaGate (Recommended for quick start)

GonkaGate is a hosted gateway to the Gonka network with USD-denominated billing — no token staking required.

**Setup:**

1. Sign up at <https://gonkagate.com/en/register> and create a `gp-...` API key.
2. Store the key in the Zeph vault:
   ```bash
   zeph vault set ZEPH_COMPATIBLE_GONKAGATE_API_KEY gp-...
   ```
3. Run `zeph init` and select **"Gonka (decentralized — via GonkaGate)"** when prompted for a provider.

**Resulting config:**

```toml
[[llm.providers]]
type = "compatible"
name = "gonkagate"
base_url = "https://api.gonkagate.com/v1"
model = "gpt-4o"
```

**Pricing:** USD-denominated. Top up at <https://gonkagate.com>.

## Path B: Native Gonka Network (Requires GNK staking)

The native path connects directly to Gonka inference nodes over a signed transport. Requests are authenticated with a secp256k1 key and GNK tokens are consumed per inference.

**Prerequisites:**

- Download and install `inferenced` CLI from <https://github.com/gonka-ai/gonka/releases>.
- Acquire GNK tokens and fund your address.

**Setup:**

1. Create a key with `inferenced`:
   ```bash
   inferenced keys add zeph
   ```
2. Store the signing key in the Zeph vault:
   ```bash
   zeph vault set ZEPH_GONKA_PRIVATE_KEY <your-hex-encoded-secp256k1-key>
   zeph vault set ZEPH_GONKA_ADDRESS <your-bech32-address>  # optional, for validation
   ```
3. Run `zeph init` and select **"Gonka (native — requires GNK staking)"**.

**Resulting config:**

```toml
[[llm.providers]]
type = "gonka"
name = "gonka-mainnet"
model = "gpt-4o"
gonka_chain_prefix = "gonka"

[[llm.providers.gonka_nodes]]
url = "https://node1.gonka.ai"
address = "gonka1..."

[[llm.providers.gonka_nodes]]
url = "https://node2.gonka.ai"
address = "gonka1..."

[[llm.providers.gonka_nodes]]
url = "https://node3.gonka.ai"
address = "gonka1..."
```

**Pricing:** GNK token consumption per inference.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| 401 / signature error | Invalid key format or address mismatch | Verify `ZEPH_GONKA_PRIVATE_KEY` is hex-encoded secp256k1; confirm address matches key |
| 401 with "clock skew" | System time out of sync | Sync your clock via NTP |
| "ZEPH_GONKA_PRIVATE_KEY not found in vault" | Key not stored | Run `zeph vault set ZEPH_GONKA_PRIVATE_KEY <key>` |
| "ZEPH_GONKA_ADDRESS does not match address derived from private key" | Address/key mismatch | Either unset `ZEPH_GONKA_ADDRESS` or correct it to match the key |
| `inferenced` not found | CLI not installed | Download from <https://github.com/gonka-ai/gonka/releases> |

## Migrating from GonkaGate to Native

Run `zeph migrate-config` — it will add advisory comments to your config pointing to the fields that need updating. Then:

1. Install `inferenced` and fund your GNK address.
2. Store `ZEPH_GONKA_PRIVATE_KEY` in the vault.
3. Update `[[llm.providers]]` in your config: change `type = "compatible"` to `type = "gonka"` and add `[[llm.providers.gonka_nodes]]` entries.
