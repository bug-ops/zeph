# References & Inspirations

Zeph is built on a foundation of research, engineering practice, and open protocol work from many authors.
This page collects the papers, blog posts, specifications, and tools that directly shaped its design.
Each entry is linked to the issue or feature where it was applied.

---

## Agent Architecture & Orchestration

**LLMCompiler: An LLM Compiler for Parallel Function Calling** (ICML 2024)\
Jin et al. — Identifies tool calls within a single LLM response that have no data dependencies and executes them in parallel. Demonstrated 3.7× latency improvement and 6× cost savings vs. sequential ReAct. Influenced Zeph's intra-turn parallel dispatch design ([#1646](https://github.com/bug-ops/zeph/issues/1646)).\
<https://arxiv.org/abs/2312.04511>

**RouteLLM: Learning to Route LLMs with Preference Data** (ICML 2024)\
Ong et al. — Framework for learning cost-quality routing between strong and weak models. Background for Zeph's model router and Thompson Sampling approach ([#1339](https://github.com/bug-ops/zeph/issues/1339)).\
<https://arxiv.org/abs/2406.18665>

**Unified LLM Routing + Cascading** (ICLR 2025)\
Try cheapest model first, escalate on quality threshold. Consistent 4% improvement over static routing. Influenced Zeph's cascade routing research ([#1339](https://github.com/bug-ops/zeph/issues/1339)).\
<https://openreview.net/forum?id=AAl89VNNy1>

**Context Engineering in Manus** (Lance Martin, Oct 2025)\
Practical breakdown of how the Manus agent handles context: soft compaction via observation masking, hard compaction via schema-based trajectory summarization, and just-in-time tool result retrieval. Directly influenced Zeph's soft/hard compaction stages, schema-based summarization, and `[tool output pruned; full content at {path}]` reference pattern ([#1738](https://github.com/bug-ops/zeph/issues/1738), [#1740](https://github.com/bug-ops/zeph/issues/1740)).\
<https://rlancemartin.github.io/2025/10/15/manus/>

---

## Memory & Knowledge Graphs

**A-MEM: Agentic Memory for LLM Agents** (NeurIPS 2025)\
Each memory write triggers a mini-agent action that generates structured attributes (keywords, tags) and dynamically links the note to related existing entries via embedding similarity. Memory organization is itself agentic rather than schema-driven. Influenced Zeph's write-time memory linking design ([#1694](https://github.com/bug-ops/zeph/issues/1694)).\
<https://arxiv.org/abs/2502.12110>

**Zep: A Temporal Knowledge Graph Architecture for Agent Memory** (Jan 2025)\
Introduces temporal edge validity (`valid_from` / `valid_until`) on knowledge graph edges. Expired facts are preserved for historical queries rather than deleted. Achieves 18.5% accuracy improvement on LongMemEval. Informed Zeph's graph memory temporal edge design and the Graphiti integration study ([#1693](https://github.com/bug-ops/zeph/issues/1693)).\
<https://arxiv.org/abs/2501.13956>

**Graphiti: Real-Time Knowledge Graphs for AI Agents** (Zep, 2025)\
Open-source implementation of temporal knowledge graphs for agents. Studied as a reference architecture for Zeph's `zeph-memory` graph storage layer.\
<https://github.com/getzep/graphiti>

**TA-Mem: Adaptive Retrieval Dispatch by Query Type** (Mar 2026)\
Shows that routing memory queries to different retrieval strategies by type (episodic vs. semantic) outperforms a fixed hybrid pipeline. Episodic queries ("what did I say yesterday?") benefit from FTS5 + timestamp lookup; semantic queries benefit from vector similarity. Directly implemented in Zeph's `HeuristicRouter` in `zeph-memory` ([#1629](https://github.com/bug-ops/zeph/issues/1629), [PR #1789](https://github.com/bug-ops/zeph/pull/1789)).\
<https://arxiv.org/abs/2603.09297>

**Episodic-to-Semantic Memory Promotion** (Jan 2025)\
Two papers on consolidating episodic memories into stable semantic facts via background clustering and LLM-driven merging. Influenced Zeph's memory tier design (episodic / working / semantic) ([#1608](https://github.com/bug-ops/zeph/issues/1608)).\
<https://arxiv.org/pdf/2501.11739> · <https://arxiv.org/abs/2512.13564>

**Temporal Versioning on Knowledge Graph Edges** (Apr 2025)\
Research on tracking fact evolution over time in agent knowledge graphs. Background for Zeph's planned temporal edge columns on the SQLite `edges` table ([#1341](https://github.com/bug-ops/zeph/issues/1341)).\
<https://arxiv.org/abs/2504.19413>

**MAGMA: Multi-Graph Agentic Memory Architecture** (Jan 2026)\
Represents each memory item across four orthogonal relation graphs (semantic, temporal, causal, entity) and frames retrieval as policy-guided graph traversal. Dual-stream write handles fast synchronous ingestion and async background consolidation. Outperforms A-MEM (0.58) and MemoryOS (0.55) on LoCoMo with 0.70. Research target for Zeph's multi-edge-type graph memory upgrade ([#1821](https://github.com/bug-ops/zeph/issues/1821)).\
<https://arxiv.org/abs/2601.03236>

---

## Context Management & Compression

**ACON: Optimizing Context Compression for Long-horizon LLM Agents** (ICLR 2026)\
Gradient-free failure-driven approach: when compressed context causes a task failure that full context avoids, an LLM updates the compression guidelines in natural language. Achieves 26–54% token reduction with up to 46% performance improvement. Directly implemented in Zeph as compression guideline injection into the compaction prompt ([#1647](https://github.com/bug-ops/zeph/issues/1647), [PR #1808](https://github.com/bug-ops/zeph/pull/1808)).\
<https://arxiv.org/abs/2510.00615>

**Effective Context Engineering for AI Agents** (Anthropic, 2025)\
Engineering guide covering just-in-time retrieval, lightweight identifiers as context references, and proactive vs. reactive context management. Co-inspired Zeph's tool output overflow and reference injection pattern ([#1740](https://github.com/bug-ops/zeph/issues/1740)).\
<https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents>

**Efficient Context Management for AI Agents** (JetBrains Research, Dec 2025)\
Production study finding that LLM summarization causes 13–15% trajectory elongation, while observation masking cuts costs >50% vs. unmanaged context and outperforms summarization on task completion. Motivated Zeph's `compaction_hard_count` / `turns_after_hard_compaction` metrics ([#1739](https://github.com/bug-ops/zeph/issues/1739)).\
<https://blog.jetbrains.com/research/2025/12/efficient-context-management/>

**Structured Anchored Summarization** (Factory.ai, 2025)\
Proposes typed summary schemas with mandatory sections (goal, decisions, open questions, next steps) to prevent LLM compressors from silently dropping critical facts. Influenced Zeph's structured compaction prompt design ([#1607](https://github.com/bug-ops/zeph/issues/1607)).\
<https://factory.ai/news/compressing-context>

**Evaluating Context Compression** (Factory.ai / ICLR 2025)\
Function-first metric: inject the summary as context, ask factual questions derived from the original turns, measure answer accuracy. Background for Zeph's compaction probe design ([#1609](https://github.com/bug-ops/zeph/issues/1609)).\
<https://factory.ai/news/evaluating-compression> · <https://arxiv.org/abs/2410.10347>

**Claude Context Management & Compaction API** (Anthropic, 2026)\
Reference for Zeph's integration with Claude's server-side `compact-2026-01-12` beta and prompt caching strategy ([#1626](https://github.com/bug-ops/zeph/issues/1626)).\
<https://platform.claude.com/docs/en/build-with-claude/context-management>

---

## Security & Safety

**OWASP AI Agent Security Cheat Sheet** (2026 edition)\
Comprehensive checklist of security controls for agentic systems. Used as a gap analysis baseline for Zeph's security hardening roadmap ([#1650](https://github.com/bug-ops/zeph/issues/1650)).\
<https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html>

**Prompt Injection Defenses** (Anthropic Research, 2025)\
Anthropic's technical overview of indirect prompt injection attack vectors and defense strategies (spotlighting, context sandboxing, dual-LLM pattern). Directly informed Zeph's `ContentSanitizer` and `QuarantinedSummarizer` design ([#1195](https://github.com/bug-ops/zeph/issues/1195)).\
<https://www.anthropic.com/research/prompt-injection-defenses>

**How Microsoft Defends Against Indirect Prompt Injection Attacks** (Microsoft MSRC, 2025)\
Engineering practices for isolation of untrusted content at system boundaries. Co-informed Zeph's `TrustLevel` / `ContentSource` model and source-specific sanitization boundaries ([#1195](https://github.com/bug-ops/zeph/issues/1195)).\
<https://www.microsoft.com/en-us/msrc/blog/2025/07/how-microsoft-defends-against-indirect-prompt-injection-attacks>

**Indirect Prompt Injection Attacks Survey** (arxiv, 2025)\
Survey of injection attack vectors across web scraping, tool results, and memory retrieval paths. Background for Zeph's multi-layer isolation design ([#1195](https://github.com/bug-ops/zeph/issues/1195)).\
<https://arxiv.org/html/2506.08837v1>

**Log-To-Leak: Prompt Injection via Model Context Protocol** (OpenReview, 2025)\
Demonstrates that malicious MCP servers can embed injection instructions in tool `description` fields that bypass content sanitization, since tool definitions are ingested as trusted system context. Motivated Zeph's MCP tool description sanitization at registration time ([#1691](https://github.com/bug-ops/zeph/issues/1691)).\
<https://openreview.net/forum?id=UVgbFuXPaO>

**Policy Compiler for Secure Agentic Systems** (Feb 2026)\
Argues that embedding authorization rules in LLM system prompts is insecure; proposes a declarative policy DSL compiled into a deterministic pre-execution enforcement layer independent of prompt content. Background for Zeph's `PolicyEnforcer` design and `PermissionPolicy` hardening ([#1695](https://github.com/bug-ops/zeph/issues/1695)).\
<https://arxiv.org/html/2602.16708v2>

**Llama Guard: LLM-based Input-Output Safeguard for Human-AI Conversations** (Meta AI, 2023)\
Binary safety classifier (SAFE / UNSAFE) trained on the MLCommons taxonomy. Inspired Zeph's `GuardrailFilter` classifier prompt design and strict prefix-matching output protocol ([#1651](https://github.com/bug-ops/zeph/issues/1651)).\
<https://arxiv.org/abs/2312.06674>

**Automated Adversarial Red-Teaming with DeepTeam** (2025)\
Framework for black-box red-teaming of agents via external endpoints. Background for Zeph's red-teaming playbook targeting the daemon A2A endpoint ([#1610](https://github.com/bug-ops/zeph/issues/1610)).\
<https://arxiv.org/abs/2503.16882> · <https://github.com/confident-ai/deepteam>

**AgentAssay: Behavioral Fingerprinting for LLM Agents** (2025)\
Evaluation framework for characterizing agent behavior under adversarial probing. Referenced in Zeph's Promptfoo integration research ([#1523](https://github.com/bug-ops/zeph/issues/1523)).\
<https://arxiv.org/html/2603.02601>

**Promptfoo: Automated Agent Red-Teaming** (open source)\
CLI tool for automated agent security testing with 50+ vulnerability classes. Evaluated as a black-box test harness against Zeph's ACP HTTP+SSE transport ([#1523](https://github.com/bug-ops/zeph/issues/1523)).\
<https://github.com/promptfoo/promptfoo> · <https://www.promptfoo.dev/docs/red-team/agents/>

---

## Protocols & Standards

**Agent-to-Agent (A2A) Protocol Specification**\
Google DeepMind open protocol for agent discovery and interoperability via JSON-RPC 2.0. Zeph implements both A2A client and server in `zeph-a2a`.\
<https://raw.githubusercontent.com/a2aproject/A2A/main/docs/specification.md>

**Model Context Protocol (MCP) Specification** (2025-11-25)\
Anthropic's open protocol for LLM tool and resource integration. Zeph's `zeph-mcp` crate implements the full MCP client with multi-server lifecycle and Qdrant-backed tool registry.\
<https://modelcontextprotocol.io/specification/2025-11-25.md>

**Agent Client Protocol (ACP)**\
IDE-native protocol for bidirectional agent ↔ editor communication. Zeph's `zeph-acp` crate supports stdio, HTTP+SSE, and WebSocket transports and works in Zed, Helix, and VS Code.\
<https://agentclientprotocol.com/get-started/introduction>

**ACP Rust SDK**\
Reference implementation used as the base for Zeph's ACP transport layer.\
<https://github.com/agentclientprotocol/rust-sdk>

**SKILL.md Specification** (agentskills.io)\
Portable skill format defining metadata, triggers, examples, and version metadata in a single Markdown file. Zeph's skill system is fully compatible with this format.\
<https://agentskills.io/specification.md>

---

## Instruction File Conventions

The `zeph.md` / `CLAUDE.md` / `AGENTS.md` pattern for project-scoped agent instructions was inspired by conventions established across the ecosystem:

| Tool | Convention file | Reference |
|------|----------------|-----------|
| Claude Code | `CLAUDE.md` | <https://code.claude.com/docs/en/memory> |
| OpenAI Codex | `AGENTS.md` | <https://developers.openai.com/codex/guides/agents-md/> |
| Gemini CLI | `GEMINI.md` | <https://geminicli.com/docs/cli/gemini-md/> |
| Cursor | `.cursor/rules` | <https://cursor.com/docs/context/rules> |
| Aider | `CONVENTIONS.md` | <https://aider.chat/docs/usage/conventions.html> |
| agents.md spec | `agents.md` | <https://agents.md/> |

Zeph unifies these under a single `zeph.md` that is always loaded, with provider-specific files loaded alongside it automatically ([#1122](https://github.com/bug-ops/zeph/issues/1122)).

---

## LLM Provider Documentation

**Google Gemini API** — Text generation, embeddings, function calling, and model catalog.\
Basis for Zeph's `GeminiProvider` implementation ([#1592](https://github.com/bug-ops/zeph/issues/1592)).\
<https://ai.google.dev/gemini-api/docs/text-generation>

**Anthropic Claude Prompt Caching** — Block-level caching with 5-minute TTL and automatic breakpoints.\
Directly implemented in `crates/zeph-llm/src/claude.rs` with stable/tools/volatile block splits.\
<https://platform.claude.com/docs/en/build-with-claude/prompt-caching>

**OpenAI Structured Outputs** — Strict JSON schema enforcement for function calling responses.\
Referenced when debugging graph memory extraction schema compatibility ([#1656](https://github.com/bug-ops/zeph/issues/1656)).\
<https://platform.openai.com/docs/guides/structured-outputs>

**Redis AI Agent Architecture** — Multi-tier caching patterns for LLM API cost reduction.\
Background for Zeph's semantic response caching research ([#1521](https://github.com/bug-ops/zeph/issues/1521)).\
<https://redis.io/blog/ai-agent-architecture/>

---

> This page is maintained alongside the codebase. When a new research issue is filed or a paper is implemented, the relevant entry should be added here.
