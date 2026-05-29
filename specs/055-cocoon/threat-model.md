---
aliases:
  - Cocoon Threat Model
  - Cocoon Security Analysis
  - arXiv:2605.03213 Mapping
tags:
  - sdd
  - spec
  - security
  - tee
  - threat-model
created: 2026-05-29
status: draft
related:
  - "[[055-cocoon/spec]]"
  - "[[001-system-invariants/spec]]"
  - "[[constitution]]"
---

# Threat Model: Cocoon Confidential Compute Integration

> [!info]
> This document maps the threat model from arXiv:2605.03213 ("Confidential
> Computing for AI Agents") to the Zeph/Cocoon integration. It identifies which
> security goals are met, which have gaps, and what mitigations are available
> or required.
>
> Issue: [#3692](https://github.com/bug-ops/zeph/issues/3692)
> Parent spec: [[055-cocoon/spec]] — Section 15 (Security and Trust Model)

---

## 1. Six-Layer Agent Stack Mapping

arXiv:2605.03213 defines a six-layer agent stack for confidential AI systems.
The table below maps each layer to Zeph and Cocoon components.

| Layer | Paper Definition | Zeph Component | TEE Protected? |
|-------|-----------------|----------------|----------------|
| **L1 — User Interface** | Input/output channel where the human interacts | CLI, TUI, Telegram/Discord channels (`zeph-channels`) | No |
| **L2 — Agent Runtime** | Orchestration, tool dispatch, memory read/write | `zeph-core` agent loop, `zeph-orchestration`, `zeph-agent-*` crates | No |
| **L3 — Memory & Retrieval** | Vector store, graph memory, episodic memory | Qdrant + SQLite (`zeph-memory`, `zeph-db`) | No |
| **L4 — Tool Execution** | Shell, web, MCP tools | `zeph-tools`, `zeph-mcp` | No |
| **L5 — Inference Gateway** | LLM provider interface, routing, prompt assembly | `zeph-llm`, `CocoonProvider`, `CocoonClient` | No (client side) |
| **L6 — Compute Backend** | GPU/CPU inference inside TEE | Cocoon Proxy (TDX) + Worker (TDX + H100 CC) | **Yes** |

**Observation:** Only Layer 6 is TEE-protected in the current architecture.
Layers L1–L5 run in the Zeph process on the operator's host without any TEE
enclave. This is the fundamental trust boundary constraint documented in
Section 15.1 of the parent spec.

---

## 2. Nine Security Goals Assessment

arXiv:2605.03213 defines nine security goals for confidential AI agents.

| # | Goal | Status | Notes |
|---|------|--------|-------|
| SG-1 | **Confidentiality of user input** | Partial | Input is plaintext in Zeph (L1–L5); TEE protection starts at L6 entry point only |
| SG-2 | **Confidentiality of model weights** | Met | Model weights live inside Cocoon Worker TEE; never exposed to Zeph or the proxy host OS |
| SG-3 | **Integrity of inference computation** | Met | TDX attestation proves worker runs expected code; RA-TLS chain enforces this |
| SG-4 | **Confidentiality of agent memory** | Not met | Qdrant and SQLite run outside TEE; embeddings and retrieved context cross trust boundary in plaintext |
| SG-5 | **Integrity of tool execution** | Not met | `zeph-tools` and `zeph-mcp` run outside TEE; tool results are plaintext in L4 |
| SG-6 | **Auditability of agent actions** | Partial | `zeph-agent-feedback`, tool audit log, tracing spans provide auditability; none is TEE-sealed |
| SG-7 | **Compound attestation (end-to-end)** | Not met | Zeph cannot verify the full Zeph → sidecar → proxy → worker attestation chain; sidecar is trusted implicitly |
| SG-8 | **Side-channel resistance** | Partial | TDX provides L6 hardware isolation; `ton_balance` in TUI is an application-level side-channel; no mitigations for timing or cache side-channels at L1–L5 |
| SG-9 | **Secure multi-agent coordination** | Partial | Subagents (`zeph-subagent`) each independently connect to the sidecar; no shared TEE session or cross-agent attestation |

---

## 3. Three Open Challenges

arXiv:2605.03213 identifies three open challenges for confidential AI agent
systems. Below is their applicability to Zeph/Cocoon.

### Challenge 1: Compound Attestation

**Paper definition:** Verifying that the entire agent pipeline — from user
input through every layer to the inference backend — operates within a
continuous TEE boundary.

**Zeph/Cocoon gap:** Zeph trusts the sidecar via localhost without any
cryptographic attestation. The sidecar attests to the proxy, the proxy attests
to the worker, but this chain is opaque to Zeph. A malicious or compromised
sidecar binary could present valid RA-TLS certificates while routing inference
through non-TEE infrastructure.

**Current mitigation:** `cocoon doctor` checks `proxy_connected = true` and
`worker_count > 0` from `/stats`. These are informational signals from the
sidecar itself — not independently verifiable attestation evidence.

**Gap severity:** High for operators with strong confidentiality requirements;
acceptable for operators who trust their own host environment.

**Recommended action:** File a P2 issue to investigate whether the Cocoon
sidecar exposes attestation evidence (TDX quote, proxy certificate chain) via
an API endpoint. If so, implement a `cocoon attestation verify` command that
fetches and validates this evidence. This is blocked on upstream Cocoon
protocol support.

**Status of issue #3692:** Documented as a known limitation. A dedicated P2
follow-up issue should be filed if the sidecar gains attestation evidence
exposure in a future Cocoon release.

### Challenge 2: TEE-Backed RAG Isolation

**Paper definition:** Ensuring that retrieval-augmented generation (RAG)
memory stores operate inside the TEE, preventing retrieved context from
leaking to untrusted infrastructure.

**Zeph/Cocoon gap:** Zeph's RAG pipeline — Qdrant vector search, SQLite
episodic memory, code index (`zeph-index`) — runs entirely outside the TEE.
This is by architectural design: Cocoon is an inference provider, not a RAG
provider. Retrieved context is assembled in plaintext at L3 (memory layer)
before crossing into L6 (inference backend).

**Partial mitigation (future):** E2E payload encryption (#3677, Section 16.2)
would protect context in transit from Zeph to the Cocoon worker, but would not
protect data at rest in Qdrant or during context assembly.

**Full mitigation:** Would require a TEE-backed vector store (e.g., running
Qdrant inside a TDX enclave) — not feasible without a significant architecture
change. This is a known architectural limitation, not a bug.

### Challenge 3: GPU-TEE Overhead

**Paper definition:** The performance cost of hardware-enforced GPU-TEE
(e.g., NVIDIA H100 Confidential Computing) for inference workloads.

**Zeph/Cocoon context:** NVIDIA H100 Confidential Computing adds 10–30%
latency overhead for inference compared to non-confidential GPU execution
(per arXiv:2605.03213 benchmarks). This overhead occurs entirely within the
Cocoon worker and is transparent to Zeph — `CocoonProvider` does not observe
it differently from normal sidecar latency.

**Operator guidance:** Operators selecting Cocoon for confidentiality-sensitive
workloads should expect higher per-request latency than equivalent non-TEE
providers. This is a documented trade-off, not a Zeph issue.

**Zeph mitigation (informational):** `cocoon_pricing` allows manual per-1K-token
pricing calibrated for actual TEE costs. Future work could add a latency
histogram specific to Cocoon in the metrics subsystem to make this overhead
visible to operators.

---

## 4. Threat Table

| Threat | Current Mitigation | Gap | Recommended Action |
|--------|-------------------|-----|-------------------|
| **Sidecar binary substitution** — attacker replaces the Cocoon C++ binary on the operator's host | Operator is responsible for binary integrity; out of scope for Zeph | No binary hash verification, no TEE attestation of the sidecar itself | Document as operator responsibility; consider adding a `zeph cocoon verify-binary` command once Cocoon publishes signed release hashes |
| **Compromised proxy (RA-TLS termination attack)** — proxy terminates RA-TLS and inspects/modifies prompts without TEE | RA-TLS attestation verified by sidecar before connecting | Zeph cannot verify this verification occurred | Compound attestation (Challenge 1 above); partial mitigation via E2E encryption (#3677) |
| **Localhost interception** — process on same host intercepts Zeph ↔ sidecar HTTP | Localhost-only URL validation; OS network isolation | plaintext localhost segment | Document as known limitation; E2E encryption (#3677) is the only mitigation |
| **Memory exfiltration from Qdrant** — attacker with host access reads Qdrant data | Qdrant access control (API key if configured); network firewall | Qdrant not TEE-protected | Document as known limitation; full mitigation requires TEE-backed vector store |
| **Side-channel via `ton_balance` TUI display** — shared-screen observer infers usage volume | None | `ton_balance` visible to anyone with TUI access | Recommend opt-in or redactable balance display (see Section 15.2 of parent spec) |
| **Credential exfiltration via LLM prompt injection** — malicious tool output or web content triggers prompt that leaks `ZEPH_COCOON_ACCESS_HASH` | `zeph-sanitizer` exfiltration guard, PII filtering | Sanitizer is heuristic, not TEE-sealed | Rely on sanitizer; enforce `ZEPH_COCOON_ACCESS_HASH` stays in vault (never in context) |
| **Sidecar crash loop under `cocoon_managed = true`** (deferred feature) | Feature deferred; not implemented | If implemented without circuit breaker, Zeph could hang retrying indefinitely | Acceptance criteria for #3676 implementation require exponential backoff + circuit breaker |
| **Stale attestation** — sidecar reconnects to a different proxy after initial health check passes | `proxy_connected` checked once at `cocoon doctor` | No continuous re-attestation | Document; `cocoon doctor` is a point-in-time check, not continuous monitoring |

---

## 5. Filed Issues and Follow-Up Recommendations

| Issue | Severity | Status | Description |
|-------|----------|--------|-------------|
| #3692 | P3/research | Open | arXiv:2605.03213 threat model analysis — resolved by this document |
| #3676 | P3 | Open | Sidecar lifecycle management — DEFERRED (see Section 16.1 of parent spec) |
| #3677 | P3 | Open | E2E payload encryption — DEFERRED (see Section 16.2 of parent spec) |

**Recommended follow-up issues:**

1. **Compound attestation investigation (P2)**: File if the Cocoon sidecar
   gains an attestation evidence endpoint in a future release. The issue should
   request: fetch TDX quote + proxy certificate chain via sidecar API; validate
   quote signature; surface result in `cocoon doctor` output. Do not file until
   upstream capability is confirmed.

2. **`ton_balance` opt-in display (P3)**: File a UX improvement issue to make
   the balance display in the TUI sidebar opt-in. This is a low-priority privacy
   improvement for shared-access scenarios. The existing TUI integration points
   (Section 9 of parent spec) make this a small UI change.

---

## 6. References

- arXiv:2605.03213 — "Confidential Computing for AI Agents" (primary source)
- [Intel TDX overview](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-trust-domain-extensions.html)
- [NVIDIA H100 Confidential Computing](https://www.nvidia.com/en-us/data-center/solutions/confidential-computing/)
- [[055-cocoon/spec]] — Parent Cocoon integration spec (implementation contract)
- [[038-vault/spec]] — Age vault backend (ZEPH_COCOON_ACCESS_HASH storage)
- [[001-system-invariants/spec]] — Cross-cutting invariants
