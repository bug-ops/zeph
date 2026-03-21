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
Represents each memory item across four orthogonal relation graphs (semantic, temporal, causal, entity) and frames retrieval as policy-guided graph traversal. Dual-stream write handles fast synchronous ingestion and async background consolidation. Outperforms A-MEM (0.58) and MemoryOS (0.55) on LoCoMo with 0.70. Implemented in Zeph as MAGMA typed edges with five `EdgeType` variants (Semantic, Temporal, Causal, CoOccurrence, Hierarchical) and `bfs_typed()` traversal ([#1821](https://github.com/bug-ops/zeph/issues/1821), [PR #2077](https://github.com/bug-ops/zeph/pull/2077)).\
<https://arxiv.org/abs/2601.03236>

**SYNAPSE: Episodic-Semantic Memory via Spreading Activation** (Jan 2026)\
Models agent memory as a dynamic graph where retrieval activates a seed node and propagation spreads through edges with decay factor λ^depth. Lateral inhibition suppresses already-activated neighbors to prevent echo-chamber retrieval. Triple Hybrid Retrieval fuses vector similarity, spreading activation, and BM25 keyword match. Implemented in Zeph's `graph::activation` module with configurable decay (λ=0.85), max hops (3), edge-type filtering, and 500ms timeout ([#1888](https://github.com/bug-ops/zeph/issues/1888), [PR #2080](https://github.com/bug-ops/zeph/pull/2080)).\
<https://arxiv.org/abs/2601.02744>

**MemOS: A Memory OS for AI Systems** (EMNLP 2025 oral)\
Cross-attention memory retrieval with importance weighting. Assigns explicit importance scores at write time combining recency, reference frequency, and content salience. Implemented in Zeph as write-time importance scoring with weighted markers (50%), density (30%), and role (20%) blended into hybrid recall score ([#2021](https://github.com/bug-ops/zeph/issues/2021), [PR #2062](https://github.com/bug-ops/zeph/pull/2062)).\
<https://arxiv.org/abs/2507.03724>

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
Proposes typed summary schemas with mandatory sections (goal, decisions, open questions, next steps) to prevent LLM compressors from silently dropping critical facts. Implemented in Zeph as `AnchoredSummary` with 5-section schema (session intent, files modified, decisions, open questions, next steps) and fallback-to-prose guarantee ([#1607](https://github.com/bug-ops/zeph/issues/1607), [PR #2037](https://github.com/bug-ops/zeph/pull/2037)).\
<https://factory.ai/news/compressing-context>

**Evaluating Context Compression** (Factory.ai / ICLR 2025)\
Function-first metric: inject the summary as context, ask factual questions derived from the original turns, measure answer accuracy. Implemented in Zeph as compaction probe validation with Q&A pipeline, three-tier verdict (Pass/SoftFail/HardFail), and `--init` wizard step ([#1609](https://github.com/bug-ops/zeph/issues/1609), [PR #2047](https://github.com/bug-ops/zeph/pull/2047)).\
<https://factory.ai/news/evaluating-compression> · <https://arxiv.org/abs/2410.10347>

**HiAgent: Hierarchical Working Memory for Long-Horizon Agent Tasks** (ACL 2025)\
Tracks current subgoal and compresses only information no longer relevant to it, achieving 2× success rate improvement and 3.8× step reduction on long-horizon benchmarks. Implemented in Zeph as subgoal-aware compaction with `SubgoalRegistry`, three eviction tiers (Active/Completed/Outdated), and two-phase fire-and-forget subgoal refresh ([#2022](https://github.com/bug-ops/zeph/issues/2022), [PR #2061](https://github.com/bug-ops/zeph/pull/2061)).\
<https://aclanthology.org/2025.acl-long.1575.pdf>

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

## Tool Intelligence

**Think-Augmented Function Calling (TAFC)** (arXiv, Jan 2026)\
Adds an optional `think` parameter to tool schemas, allowing the model to reason about parameter values before committing. Average win rate of 69.6% vs 18.2% for standard function calling on ToolBench. Implemented in Zeph with `_tafc_think` field injection for complex schemas (complexity > τ), strip-before-execution guarantee, and configurable threshold ([#1861](https://github.com/bug-ops/zeph/issues/1861), [PR #2038](https://github.com/bug-ops/zeph/pull/2038)).\
<https://arxiv.org/abs/2601.18282>

**Less is More: Better Reasoning with Fewer Tools** (arXiv, Nov 2024)\
Demonstrates that filtering which tool schemas are included in the prompt per-turn significantly improves function-calling accuracy. Implemented in Zeph as dynamic tool schema filtering with embedding-based relevance scoring, always-on tool list, and dependency graph gating ([#2020](https://github.com/bug-ops/zeph/issues/2020), [PR #2026](https://github.com/bug-ops/zeph/pull/2026)).\
<https://arxiv.org/abs/2411.15399>

**Speculative Tool Calls** (arXiv, Dec 2025)\
Analyzes redundant tool executions within agent sessions and proposes caching strategies. Implemented in Zeph as per-session tool result cache with TTL expiration, deny list for side-effecting tools, and lazy eviction ([#2027](https://github.com/bug-ops/zeph/issues/2027), [PR #2027](https://github.com/bug-ops/zeph/pull/2027)).\
<https://arxiv.org/abs/2512.15834>

---

## Orchestration

**Agentic Plan Caching (APC)** (arXiv, Jun 2025)\
Extracts structured plan templates from completed executions and stores them indexed by goal embedding. On similar requests, adapts the cached template rather than replanning from scratch. Reduces planning cost by 50% and latency by 27%. Implemented in Zeph's `LlmPlanner` with similarity lookup, lightweight adaptation call, and two-phase eviction (TTL + LRU) ([#1856](https://github.com/bug-ops/zeph/issues/1856), [PR #2068](https://github.com/bug-ops/zeph/pull/2068)).\
<https://arxiv.org/abs/2506.14852>

**MAST: Why Do Multi-Agent LLM Systems Fail?** (UC Berkeley, Mar 2025)\
Analysis of 1,642 execution traces finding coordination breakdowns account for 36.9% of all failures. Identifies 14 failure modes across system design, inter-agent misalignment, and task verification. Informed Zeph's handoff hardening research; initial implementation (PRs #2076, #2078) was reverted (#2082) for redesign ([#2023](https://github.com/bug-ops/zeph/issues/2023)).\
<https://arxiv.org/abs/2503.13657>

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
Informed Zeph's semantic response caching with embedding similarity matching, dual-mode lookup (exact key + cosine similarity), and model-change invalidation ([#1521](https://github.com/bug-ops/zeph/issues/1521), [PR #2029](https://github.com/bug-ops/zeph/pull/2029)).\
<https://redis.io/blog/ai-agent-architecture/>

---

> This page is maintained alongside the codebase. When a new research issue is filed or a paper is implemented, the relevant entry should be added here.
