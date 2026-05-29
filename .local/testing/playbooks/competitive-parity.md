# Competitive Parity Monitoring

Living document tracking Zeph's feature and protocol parity against reference agents.
Update after every parity scan. See `.claude/rules/continuous-improvement.md` for the full protocol.

---

## Reference Agents — Last Checked

| Agent | Stack | Key features to watch | Last checked | Version |
|---|---|---|---|---|
| Codex CLI (OpenAI) | Rust | OS sandbox (Seatbelt/Landlock), plugins, session replay/fork, subagents, exec mode, marketplace, thread-store, /goal workflow, multi-env per-turn, remote-control headless server, plugin sharing/hooks, Bedrock AWS-login auth, voice sessions (WebRTC v2), live config reload, Vim modal editing, PreToolUse hook context, thread pagination (unloaded/summary/full), Codex-Spark model (128k ctx), configurable OTEL trace metadata, built-in MCPs as first-class runtime servers | **2026-05-17** (CI-825) | **v0.131.0-alpha.22** (latest stable: none, latest alpha: rust-v0.131.0-alpha.22, unchanged); #4160 tracks |
| OpenCode | Rust | Tool system, context mgmt, ACP HTTP, memory | 2026-03-30 | latest |
| ZeroClaw | Rust | 14+ channels, SQLite-only vector, Landlock sandbox, hardware (RPi/USB) | 2026-03-30 | 0.1.7 |
| IronClaw | Rust + WASM | Dynamic WASM tool gen, pgvector, heartbeat/routines, capability sandbox | 2026-03-30 | latest |
| OpenCrust | Rust | 16MB binary, sqlite-vec, DNA personality, MCP stdio | 2026-03-30 | latest |
| Goose (AAIF/Linux Foundation, formerly Block) | Rust | Extension system, multi-provider routing, ACP stdio, goose serve, Gemini OAuth, Copilot ACP provider, egress logging inspector, goose doctor, GOOSE_SHOW_FULL_OUTPUT, ACP as primary interface (Phase 1 complete, Phase 2 TS TUI beta, Phase 3 Tauri in-progress), Gemma 4 local model support, configurable fast_model, hooks stable in config.toml, agents CRUD via ACP, projects as backend sources, ACP streamable HTTP spec compliance, auto-updating plugins, recursive nested skill discovery, skills platform extension manifest, restricted secrets file permissions (chmod 600), goose-tui binary, mergeable configs, reverted client-side autocompaction, built-in skills exposed via ACP, non-vulkan Linux arm64 build (ubuntu 22.04), tab-expandable tool calls, improved message rendering, independent text mode, goose review subcommand, open-plugins generalization, hooks+/goal+review+recursive skills+extension manifest+oauth token refresh+deep links+atomic/routstr/futmix/omlx/saladcloud providers | **2026-05-30** (CI-926 writer scan) | **v1.35.0** (May 22, 2026); #3917/#4023/#4059 track |
| Claude Code | TypeScript/Node | Tool approval UX, hooks, slash commands, context mgmt, plugin options, MCP auto-retry (3x startup retry), project purge, SSH OAuth, /goal autonomous multi-turn + supervisor verifier, Agent View fleet dashboard, plugin dependency enforcement (disable-chain), projected context cost per-turn/invocation, worktree.bgIsolation none, worktree.baseRef (fresh|head), --plugin-url session-scoped plugin, Ctrl+R all-project history, bg session model/effort persistence, iTerm2 clipboard access auto-config, /bg preserves --mcp-config/--settings/--add-dir/--plugin-dir across respawn, claude agents --add-dir/--settings/--mcp-config per-session, worktree cleanup no rm-rf fallback, stop hook block cap (8 blocks, CLAUDE_CODE_STOP_HOOK_BLOCK_CAP), PowerShell -ExecutionPolicy Bypass default, corrupt .credentials.json fix, right-click paste fix on WSL/Windows Terminal | **2026-05-17** (CI-825) | **v2.1.143** (unchanged); #3903/#3918/#3995/#4004 track |
| OpenHands | Python | Sandboxed execution, multi-agent delegation, event stream, benchmarks | **2026-04-17** | **SDK v1 / v1.0.0** |
| Aider | Python | Repo-map, multi-file edits, git integration, architect/editor split | **2026-05-03** | v0.88+ (May 2026) |
| Zed assistant | Rust | ACP protocol (first-class target), elicitation, IDE UX | 2026-03-30 | v0.221+ |
| OpenClaw | TypeScript/Node | 150K★, 20+ channels, ClawHub skill registry, Canvas/A2UI, Device Node | 2026-03-30 | latest |
| Claude Code | TypeScript/Node | Tool approval UX, hooks, slash commands, context mgmt | 2026-03-30 | latest |
| Aider | Python | Repo-map, multi-file edits, git integration, architect/editor split | 2026-03-30 | — |
| Cursor / Windsurf | Electron + TS | Shadow workspace, speculative edits, inline diff, context window | 2026-03-30 | — |
| Continue.dev | TypeScript | Context providers, slash command arch, ACP adoption | 2026-03-30 | — |
| Cline / RooCode | TypeScript | Tool execution UX, permission flows, diff presentation, native subagents | **2026-04-08** | **v3.58** |
| OpenHands | Python | Sandboxed execution, multi-agent delegation, event stream, benchmarks | 2026-03-30 | — |
| SWE-agent | Python | Benchmark-driven ACI | 2026-03-30 | — |

---

## Known Gaps

| Feature | Agent(s) with it | Research backing | Zeph status | Issue | Priority |
|---|---|---|---|---|---|
| `elicitation` (interactive permission prompts) | Zed, Claude Code | ACP spec v0.11.3 | Not implemented | #2411 | P2 |
| `logout` capability in ACP | Zed, Claude Code | ACP spec v0.11.3 | Not implemented | #2411 | P2 |
| NES / `additional_directories` in ACP (unstable) | Zed | ACP spec v0.11.4 | Not implemented; no client ships it yet | #2411 | P4 |
| ACP `fs/` + `terminal/` agent→client APIs | Zed | ACP protocol (Zed IDE integration) | Not implemented as agent-side calls | — | P3 |
| Tool call streaming: `pending` status at tool start | Zed, OpenCode | ACP session/update spec | Unverified; may only emit on completion | — | P3 |
| `session/list` stabilized (no unstable guard) | Zed, OpenCode, Goose | ACP spec v0.11.1 | Guarded by `unstable_session_list` | #2411 | P2 |
| `session/close` handler + capability advertisement | Zed, OpenCode, Goose | ACP spec v0.11.2 | **FIXED PR #2429** — `sessionCapabilities.close: {}` advertised, handler implemented ✅ | #2421 | — |
| Discovery `protocol_version` = integer 1 (not string "0.9") | Zed, OpenCode, Goose | ACP wire protocol | **FIXED PR #2423** — uses `ProtocolVersion::LATEST` ✅ | #2412 | — |
| Non-empty `authMethods` in `initialize` response | Zed, Claude Code | ACP spec v0.11.0 + Registry CI gate | **FIXED PR #2431** — `authMethods: [{"id":"zeph","name":"Zeph"}]` ✅ | #2422 | — |
| `agent-client-protocol` crate at 0.10.3 | Zed, OpenCode | — | **FIXED PR #2423** ✅ | #2411 | — |
| `agent-client-protocol-schema` crate at 0.11.4 | Zed, OpenCode | — | At 0.11.3 (schema 0.11.4/NES deferred P4) | #2411 | P4 |
| ACP Registry listing (`agent.json` + PR) | Zed, Claude Code, OpenCode | Zed v0.221+ preferred | `/agent.json` endpoint added PR #2431 ✅; Registry PR submission pending (manual step) | #2422 | P3 |
| Repo-map for context (AST-level file summary) | Aider, Claude Code | Aider repo-map design | Partial via zeph-index | — | P3 |
| Architect / editor mode split (planning vs execution) | Aider, Cursor | Aider architect mode | Orchestration exists, no explicit edit mode | — | P3 |
| Sandboxed tool execution (Docker/container) | OpenHands, Cursor, IronClaw | OpenHands sandbox | Shell tool has confirm_patterns, no container isolation | — | P3 |
| Event stream architecture (full audit trail) | OpenHands, Claude Code | OpenHands event stream | Tool audit exists; no full event stream replay | — | P3 |
| Transactional filesystem rollback in shell executor | None yet | arXiv:2512.12806 | Not implemented | #2414 | P2 |
| WASM dynamic tool generation at runtime | IronClaw | IronClaw WASM arch | Not implemented (static SKILL.md) | #2418 | P3 |
| Hardware channel (RPi, USB peripherals) | ZeroClaw, OpenClaw | ZeroClaw Device Node | Not implemented | — | P4 |
| Pure-SQLite vector search (no Qdrant dependency) | ZeroClaw, OpenCrust | ZeroClaw/OpenCrust arch | Qdrant required (or sqlite fallback for embeddings only) | — | P3 |
| Heartbeat / proactive background execution | IronClaw, OpenClaw | IronClaw routines | zeph-scheduler covers periodic tasks, no heartbeat semantics | — | P3 |
| ClawHub-style public skill registry | OpenClaw | OpenClaw ecosystem | Not implemented | — | P4 |
| MCP tool trust/confidentiality metadata (per-tool) | — | arXiv:2601.08012 | Per-server trust only, no per-tool metadata | #2420 | P2 |
| Formal 4-property security model (Task/Action/Source/Data) | — | arXiv:2603.19469 | ExfiltrationGuard + ContentSanitizer (partial) | #2417 | P2 |
| BaRP cost-weight dial for bandit routing | — | arXiv:2510.07429 | LinUCB accuracy-only | #2415 | P2 |
| RL-based memory admission control | — | arXiv:2603.04549 (ICLR oral) | Static threshold (A-MAC, 0.30) | #2416 | P2 |
| Formal belief revision for graph memory (AGM-compliant edge versioning) | — | arXiv:2603.17244 (Kumiho) | MAGMA temporal edge versioning — varying relation strings defeat conflict detection | #2441 | P2 |
| Dopamine-gated MAGMA evolution (RPE routing skips O(N²) for low-surprise turns) | — | arXiv:2603.14597 (D-MEM) | Full graph extraction on every memory_save | #2442 | P2 |
| Memory-augmented routing signal (memory hit confidence → smaller model) | — | arXiv:2603.23013 | LinUCB uses complexity only, not memory hit confidence | #2443 | P2 |
| MCP Roots protocol (roots/list + roots/list_changed) | Goose v1.28.0 | MCP spec | MCP client sends no roots | #2445 | P2 |
| Adversarial policy agent (pre-execution LLM tool validation) | Goose v1.28.0 | — | ContentSanitizer covers injection; no policy LLM reviewer | #2447 | P2 |
| Constant-time auth token comparison (ACP HTTP) | Goose v1.28.0 | — | Standard string equality in ACP HTTP auth | #2448 | P2 |
| Subprocess credential scrubbing in ShellExecutor | Claude Code v2.1.x | — | Full env inherited by shell child procs | #2449 | P2 |
| MCP tool description cap (2KB) | Claude Code v2.1.x | — | Descriptions passed as-is to LLM | #2450 | P3 |
| /new command (fresh conversation, preserve session state) | OpenHands v1.6.0 | — | No equivalent; only /compact exists | #2451 | P3 |
| MCP Server Cards (/.well-known/mcp/server-card.json) | Claude Code, OpenCode (SEP-1649) | MCP 2026 roadmap priority | No /.well-known/ endpoint in zeph-mcp HTTP server | #3225 | P3 |
| Cost-sensitive store routing (select memory store per query type) | — | arXiv:2603.15658 | All stores queried uniformly per turn | #2444 | P3 |
| Session recap / away-summary when returning to conversation | Claude Code v2.1.95+ | — | No equivalent; /compact exists but no resume summary | #3064 | P3 |
| Native read-only subagents (parallel info gathering) | Cline v3.58, Claude Code | — | No equivalent; orchestrator is sequential | #2789 | P4 |
| `--bare` / `--json` CLI mode for scripted/CI usage | Claude Code v2.1.81, Cline CLI 2.0 | — | Full agent stack always initialized | #2790 | P4 |
| Plugin packaging system (skills+MCP+apps bundles) | Codex CLI v0.117+ | — | SKILL.md + MCP config exist separately, no unified packaging | #2806 | P4 |
| Session persistence with resume/fork | Codex CLI v0.118+, OpenHands | — | SQLite message history, no session resume/fork | #2807 | P4 |
| OS-level sandbox (Seatbelt/Landlock/seccomp) | Codex CLI, ZeroClaw | — | Implemented via #3068: Seatbelt (macOS) + bwrap/Landlock (Linux) | #2808 | Implemented |
| Persistent background service mode | Goose v1.30.0 | — | No \`serve\` subcommand; zeph-scheduler covers periodic tasks only | #3074 | P4 |
| Path-based inter-agent addressing (multi-agent v2) | Codex CLI v0.118+ | — | DAG planner exists; no named agent namespace or structured inter-agent envelopes | #3072 | P4 |
| Parallel multi-agent code review with verifier (explorer+critic) | Claude Code v2.1.100+ | — | No dedicated parallel review orchestration mode or verifier composition | #3073 | P4 |
| Runtime thinking-budget slash command (/think-tokens, /reasoning-effort) | Aider v0.87+ (April 2026) | Aider release history | Thinking config static (from provider config); no session-scoped override command | #3098 | P4 |
| Cloud-side plan editing (Ultraplan) with web review before local execution | Claude Code v2.1 (April 2026) | code.claude.com | No equivalent; /plan works locally only; no cloud handoff for plan review | #3093 (extends) | P4 |
| Reactive environment hooks (CwdChanged, FileChanged, PermissionDenied) | Claude Code v2.1.115 | — | Fixed event type set; no environment-reactive events | #3292 | P3 |
| Hooks invoke MCP tools directly (type:mcp_tool) | Claude Code v2.1.116 | — | Hook executor supports shell only | #3293 | P3 |
| Per-domain network egress deny-list in sandbox | Claude Code v2.1.114, Codex CLI (April 2026) | — | Sandbox allows/denies network globally; no per-domain control | #3294 | P3 |
| Plugin initialPrompt + monitors manifest keys | Claude Code v2.1.116 | — | SKILL.md has no auto-submit prompt or monitor declaration | #3295 | P4 |
| Multi-strategy graph retrieval (A*, WaterCircles, beam, hybrid) for memory | — | arXiv:2506.17001 (April 2026) | MAGMA uses SYNAPSE only; no per-query strategy selection | #3296 | P3 |
| Auto-compaction context window UX (token progress gauge) | Goose v1.32.0 | — | Spinner only; no token fill gauge or compaction progress | #3314 | P3 |
| PostToolUse hook duration_ms timing field | Claude Code v2.1.119 | — | Hook env vars carry no tool execution timing | #3316 | P3 |
| Parallel MCP server connections on startup | Claude Code v2.1.117 | — | Sequential server init in lifecycle.rs | #3315 | P3 |
| MCP server startup auto-retry (3× exponential backoff) | Claude Code v2.1.122 | — | Single connect attempt; transient errors mark server unavailable for session | #3568 | P3 |
| Persisted /goal workflow with pause/resume/clear lifecycle | Codex CLI (May 2026) | — | No goal tracking; orchestrator DAG exists but no user-named objective with lifecycle | #3567 | P3 |
| RL-learned dynamic capability governance (Aethelgard) | — | arXiv:2604.11839 | All tools exposed every turn; static trust levels, no learned minimum viable set | #3563 | P3 |
| Vault-broker credential delegation (CapSeal/SUDP) — agents propose ops, never receive secrets | — | arXiv:2604.16762 + arXiv:2604.24920 | Vault resolves secrets at startup; agent process holds raw credential strings | #3569 | P3 |
| Runtime trajectory-stateful tool mediation (SafeAgent) | — | arXiv:2604.17562 | ContentSanitizer/PolicyGate are per-turn; no cumulative risk scoring across trajectory | #3570 | P3 |
| Visual trajectory encoding for long-horizon memory (OCR-Memory) | — | arXiv:2604.26622 | All memory stores are text-based; lossy summarization on compact | #3571 | P3 |
| Per-turn multi-environment selection in orchestrator | Codex CLI v0.124.0 | — | ShellExecutor uses single global session CWD; no per-turn execution context | #3572 | P3 |
| Project state lifecycle management (project purge command) | Claude Code v2.1.126 | — | No single command to purge all project-scoped state (DB + embeddings + traces) | #3573 | P4 |
| PostToolUse hook replace tool output (hookSpecificOutput.updatedToolOutput) | Claude Code v2.1.127+ | — | Hook executor reads exit code/stdout only; no output replacement or duration_ms exposure | #3798 | P3 |
| Headless app-server with thread pagination (unloaded/summary/full) + remote-control | Codex CLI v0.130.0 | — | zeph-gateway handles webhooks but no persistent session store or pagination API | #3800 | P3 |
| Async episodic→semantic consolidation daemon with cognitive weight feedback | — | arXiv:2605.03675 (MemTier) | MemFlow (PR #3791) routes retrieval but no async promotion or cognitive weight signal | #3799 | P2 |
| Multimodal EM-Graph memory nodes (OCR+vision-to-text Scrapbook Pages) | — | arXiv:2605.03804 (ScrapMem) | EM-Graph implemented (text-only); no multimodal ingestion pipeline | #3801 | P3 |
| MCP OAuth token refresh race condition (serialized refresh guard) | Claude Code v2.1.136 | — | Potential race in zeph-mcp OAuth token refresh — not verified | #3699 | P2 |
| Vim modal editing in TUI composer (/vim command, Normal/Insert modes) | Codex CLI v0.129 | — | No modal input mode in ratatui composer | #3697 | P4 |
| PreToolUse hook receives full structured tool context (name + args JSON) | Codex CLI v0.129 | — | Hook env vars carry no structured tool call metadata | #3698 | P3 |
| Thread pagination (unloaded/summary/full view modes) for long sessions | Codex CLI v0.130 | — | No session thread pagination; /compact exists but no view modes | — | P4 |
| Compound attestation for multi-hop TEE chains (Omega trustlets model) | — | arXiv:2605.03213 §VIII-A | No attestation verification across A2A/MCP delegation hops in Cocoon path | #3700 | P3 |
| MAGE shadow memory trajectory-aware long-horizon threat defense | — | arXiv:2605.03228 | ContentSanitizer/PolicyGate are per-turn only; no trajectory-accumulated risk | #3695 | P3 |
| BeliefMem probabilistic multi-hypothesis memory (Noisy-OR updates) | — | arXiv:2605.05583 | MAGMA commits to single edge per observation; no uncertainty preservation | #3696 | P3 |
| STALE implicit conflict detection (outdated memory without explicit negation) | — | arXiv:2605.06527 | APEX-MEM conflict resolution triggers on explicit predicate re-assertion only | #3702 | P3 |
| MemTier tiered memory with 5-signal weighted retrieval + async promotion daemon | — | arXiv:2605.03675 | SQLite+Qdrant split exists; no async consolidation daemon or PPO retrieval adaptation | #3703 | P3 |
| MemReranker reasoning-aware reranking (0.6B/4B, temporal+causal queries) | — | arXiv:2605.06132 | SYNAPSE spreading activation; no reranking stage after initial retrieval | #3704 | P4 |
| /goal autonomous multi-turn completion with supervisor verifier session | Claude Code v2.1.139 | — | /plan interactive mode only; no fire-and-forget goal loop or verifier session | #3883 | P3 |
| Agent fleet view (claude agents): unified dashboard for all sessions by status | Claude Code v2.1.139 | — | TUI shows current session only; no cross-session fleet view | #3884 | P4 |
| Hooks stable in config.toml + ACP streamable HTTP spec compliance | Goose v1.34.0 | — | Hooks not declarable in config.toml; MCP tool events not observed (#3698); ACP compliance unverified | #3885 | P3 |
| AgentTrust: shell deobfuscation + RiskChain multi-step attack detection | — | arXiv:2605.04785 | ContentSanitizer lacks deobfuscation and multi-step RiskChain scoring | #3887 | P2 |
| Uno-Orchestra: RL-learned joint routing+decomposition under unified objective | — | arXiv:2605.05007 | LinUCB routes per-query; DAG decomposes statically; not jointly optimized | #3891 | P3 |
| OS-model security: capability rings + namespace isolation per tool invocation | — | arXiv:2605.14932 | Trust levels are coarse; tools share shell session; no per-operation ACL | #3894 | P3 |
| SafeHarbor: self-evolving hierarchical guardrail with dynamic rule injection | — | arXiv:2605.05704 | GuardrailFilter uses static system prompt; no memory-backed evolution | #3897 | P3 |
| Mnemonic Sovereignty: 9 governance primitives (write auth, tamper-evidence, query privacy, cross-agent propagation, deletability audit) | — | arXiv:2604.16548 | No write authorization, no tamper-evidence, no forget audit trail | #3898 | P3 |
| MAGE shadow memory: cross-turn safety accumulation for long-horizon threat detection | — | arXiv:2605.03228 | CausalIpi is per-batch; no turn-spanning safety memory | #3899 | P2 |
| Memory Experience stage: proactive exploration + cross-trajectory abstraction | — | arXiv:2605.06716 | ExperienceStore at Stage 2 (Reflection); no Stage 3 | #3900 | P4 |
| LASM 7-layer defense: Tool Execution + Governance + Multi-Agent Coordination layers | — | arXiv:2604.23338 | Upper layers under-defended; no Agent Bill of Materials | #3901 | P3 |
| Goose v1.34: agent CRUD via ACP, projects as backend sources, auto-updating plugins | Goose v1.34.0 | — | ACP server IDE-only; no agent lifecycle CRUD; no auto-update policy | #3902 | P3 |
| Claude Code plugin dependency enforcement + projected context cost + background session persistence | Claude Code v2.1.143 | — | No dependency graph in zeph-plugins; no pre-turn cost projection | #3903 | P3 |
| Codex CLI configurable TUI keymaps + named permission profiles + reasoning token tracking | Codex CLI May 2026 | — | Hardcoded keymaps; no named profiles; reasoning tokens not tracked separately | #3904 | P4 |
| MAGE cross-turn shadow memory in sanitizer (implementation gap) | — | arXiv:2605.03228 | CausalIpi per-batch only; no ShadowMemory struct, no turn-spanning trajectory | #3912 | P2 |
| MATRA attack surface threat model (asset-based attack trees) | — | arXiv:2605.10763 | No formal threat model document for Zeph | #3913 | P3 |
| Authorization propagation: delegation chain + temporal revocation in subagents | — | arXiv:2605.05440 | SubagentGrant has no valid_until, no revocation propagation, no delegation_chain audit | #3915 | P3 |
| OS-level capability scope declaration in SKILL.md | IronClaw (WASM manifest) | arXiv:2605.14932 | Skills declare no capability scope; any skill can invoke any tool | #3916 | P3 |
| Successor Representation Spectrum — topology diagnostics for multi-agent DAGs | — | arXiv:2605.11453 | No pre-inference topology risk diagnostic in PlanVerifier | #3919 | P4 |
| **Recursive nested skill discovery** | Goose v1.35.0 | — | Uses flat `read_dir`; no recursive subdirectory walking for nested skills | **#4682** (NEW) | **P3** |
| **Skills platform extension manifest** | Goose v1.35.0 | — | SKILL.md has no extension manifest; skills have no platform-level integration metadata | **#4683** (NEW) | **P3** |
| **Egress logging inspector** | Goose v1.35.0 | — | Tool audit exists; network egress from tools not attributed to source skill | **#4684** (NEW) | **P3** |
| **Restricted secrets file permissions (chmod 600)** | Goose v1.35.0 | — | **VALIDATED** — zeph-vault enforces 0o600 on vault-key.txt and secrets.age ✅ | — | — |
| **Auto-updating plugins** | Goose v1.35.0 | — | **VALIDATED** — zeph-plugins has `auto_update` field + `check_auto_updates()` ✅ | — | — |
| **Goose-tui separate binary** | Goose v1.35.0 | — | `--tui` feature flag only; no separate binary for independent TUI packaging | **#4685** (NEW) | **P3** |
| **Mergeable configs** | Goose v1.35.0 | — | `zeph-config` uses last-write-wins; no merge semantics for layered configs | **#4023** (DUPLICATE) | **P3** |
| **Revert client-side autocompaction** | Goose v1.35.0 | — | **VALIDATED** — zeph-context has compaction guards (#3999 CI-799); prevents same regression class ✅ | — | — |
| **Goose v1.35.0 OAuth token refresh** | Goose v1.35.0 | — | MCP OAuth token refresh exists in zeph-mcp; potential race already tracked #3699 P2 | — | — |
| **Deep link / fresh session URL scheme** | Goose v1.35.0 | — | No `zeph://new-session` or equivalent deep link scheme | **#4687** (NEW) | **P4** |

---

## Zeph Differentiators (not present in any monitored agent)

| Feature | Status |
|---|---|
| SYNAPSE spreading activation recall | Unique to Zeph |
| MAGMA multi-graph memory with typed edges | Unique to Zeph |
| LinUCB bandit + Thompson multi-armed routing | Unique to Zeph |
| Native ACP protocol (not subprocess proxy) | Unique among Rust agents |
| ratatui TUI with real-time metrics panel | Unique to Zeph |
| SKILL.md self-learning with feedback loop | Unique to Zeph |

---

## Parity Scan Log

### 2026-05-30 — CI-926 writer scan (Goose v1.34.0–v1.35.0 assessment)

**Scope**: Comprehensive Goose v1.34.0–v1.35.0 feature assessment against Zeph. Focus: skill discovery, extension manifests, egress logging, file permissions, plugins, TUI binary, config merging, autocompaction revert, OAuth, deep links.

**Reference table updates**:

| Agent | Previous | Current | New notable features | Zeph gap/action |
|---|---|---|---|---|
| Goose (AAIF) | v1.34.1 | **v1.35.0** (May 22, 2026) | v1.35.0: /goal command, goose review subcommand, TUI diff viewer, ACP slash commands, unified thinking effort, open-plugins generalization, subagent instruction summoning, OAuth token refresh, deep links (goose://new-session). v1.34.1: non-Vulkan Linux build. v1.34.0: hooks stable in config.toml, agent CRUD via ACP, projects as backend sources, ACP streamable HTTP, auto-updating plugins, recursive nested skill discovery, skills extension manifest, restricted secrets (chmod 600), goose-tui binary, mergeable configs, reverted autocompaction | Filed #4682–#4687 (new gaps); validated 3 features as already covered |

**Feature assessment (9 features assessed)**:

| Feature | Goose v1.34–v1.35 | Zeph status | Gap class | Issue |
|---|---|---|---|---|
| Recursive nested skill discovery | `*.../sub/SKILL.md` supported | Flat `read_dir` only; no recursion | **P3 parity gap** | #4682 |
| Skills platform extension manifest | Metadata beyond SKILL.md; UI/keybinding integration | SKILL.md only; no manifest | **P3 parity gap** | #4683 |
| Egress logging inspector | Tool execution logged with source skill attribution | Tool audit exists; no skill attribution | **P3 parity gap** | #4684 |
| Restricted secrets file permissions (chmod 600) | Enforced at creation/validation | **zeph-vault validates 0o600** ✅ | Covered | — |
| Auto-updating plugins | `auto_update: true` in plugin.toml; checked at startup | **zeph-plugins has `check_auto_updates()`** ✅ | Covered | — |
| Separate goose-tui binary | Independent binary distribution | `--tui` flag + feature only | **P3 parity gap** | #4685 |
| Mergeable configs (layered semantics) | User→project→env compose; not last-write-wins | Last-write-wins in zeph-config | **P3 parity gap** | #4023 (DUPLICATE) |
| Revert client-side autocompaction | Goose removed unconditional autocompaction due to quality regressions | **zeph-context has compaction guards (#3999)** ✅ | Covered | — |
| OAuth token refresh + deep links (v1.35.0) | OAuth refresh prevents re-auth per session; `goose://new-session` deep links | MCP OAuth refresh exists (#3699); no deep links | Partial; #3699 tracks OAuth race | #4687 (deep links) |

**Files read for assessment**:

- `/Users/rabax/Dev/zeph/crates/zeph-skills/src/registry.rs` — skill discovery uses `read_dir` (line 132–137), not recursive
- `/Users/rabax/Dev/zeph/crates/zeph-vault/src/lib.rs` — validates 0o600 (line 183–184) ✅
- `/Users/rabax/Dev/zeph/crates/zeph-plugins/src/manifest.rs` — `auto_update` field + `check_auto_updates()` ✅
- `/Users/rabax/Dev/zeph/crates/zeph-plugins/src/manager.rs` — `AutoUpdateResult` enum ✅
- `/Users/rabax/Dev/zeph/crates/zeph-config/src/loader.rs` — config loading (single file, no merge semantics)
- `/Users/rabax/Dev/zeph/Cargo.toml` — no separate `[[bin]]` for zeph-tui; only feature flag

**Issues filed**:

| Issue | Priority | Category | Summary |
|---|---|---|---|
| #4682 | P3 | parity(skills) | Recursive nested skill discovery — walk subdirectories for SKILL.md |
| #4683 | P3 | parity(skills) | Skills platform extension manifest — metadata for UI/keybinding integration |
| #4684 | P3 | parity(logging) | Egress logging inspector — attribute tool network access to source skill |
| #4685 | P3 | parity(ux) | goose-tui separate binary — independent TUI distribution alongside main agent |
| #4023 | P3 | parity(config) | Mergeable config layering — user→project→env composition semantics (DUPLICATE) |
| #4687 | P4 | parity(ux) | Deep link scheme (zeph://new-session) for fresh session initiation |

**Features validated as already covered**:

1. **Restricted secrets file permissions (chmod 600)** — zeph-vault enforces 0o600 on `vault-key.txt` and `secrets.age` (assert at lines 183–184)
2. **Auto-updating plugins** — zeph-plugins has `auto_update: bool` field in manifest + `PluginManager::check_auto_updates()` method
3. **Revert client-side autocompaction** — zeph-context compaction guards implemented in CI-799 (#3999) prevent unconditional autocompaction

**Dependency audit**:
- No new RUSTSEC advisories affecting Zeph's tree
- `metrics 0.24.6` (upgraded from yanked 0.24.5 in prior cycle) — no issues
- `qdrant-client` still blocked on rustls-pemfile (RUSTSEC-2025-0134 advisory, #3772, #2347)

**Next scan trigger**: Goose v1.36.0, Claude Code v2.2.x, or Codex CLI v0.131+ stable.

---

### 2026-05-17 — CI-822 researcher scan

**Scope**: Full scan — all 3 agents unchanged from CI-821. Dependency audit: RUSTSEC-2025-0134 only (no new advisories). arXiv focus: agent safety/tool-use policies, memory retrieval, multi-agent coordination.

**Reference table updates**:

| Agent | Previous | Current | New notable features | Zeph gap/action |
|---|---|---|---|---|
| Goose (AAIF) | v1.34.1 | **v1.34.1** (unchanged) | — | No new parity gaps |
| Claude Code | v2.1.143 | **v2.1.143** (unchanged) | — | No new parity gaps |
| Codex CLI | v0.131.0-alpha.22 | **v0.131.0-alpha.22** (unchanged) | — | No new issues |

**New arXiv issues filed**:

| Issue | Source | Priority | Title |
|---|---|---|---|
| #4176 | arXiv:2603.20449 | P4 | research(tools): solver-aided tool-call policy enforcement — SMT pre-condition checking for zeph-tools |
| #4177 | arXiv:2603.18272 | P4 | research(memory): retrieval-augmented trajectory memory — reuse prior agent episodes in zeph-memory |
| #4178 | arXiv:2604.17612 | P4 | research(subagent): MSC-based deadlock-free coordination spec for zeph-subagent |

---

### 2026-05-16 — CI-800 researcher scan

**Scope**: Full scan — Goose v1.34.1 (no new features vs v1.34.0), Claude Code v2.1.143 (unchanged), Codex CLI stable unchanged. Dependency audit: RUSTSEC-2025-0134 + yanked metrics 0.24.5 remain only known advisories. arXiv focus: scheduler security (2605.02812 RTW-A temporal re-entry), agent reliability (2602.16666), SGH scheduler framework (2604.11378).

**Reference table updates**:

| Agent | Previous | Current | New notable features | Zeph gap/action |
|---|---|---|---|---|
| Goose (AAIF) | v1.34.0 | **v1.34.1** (May 15, 2026) | Build fix only (non-Vulkan Linux ubuntu 22.04) | No new parity gaps vs CI-799/CI-787 |
| Claude Code | v2.1.143 | **v2.1.143** (unchanged) | — | No new parity gaps |
| Codex CLI | v0.131.0-alpha.22 | **v0.131.0-alpha.22** (unchanged) | — | No new issues |

**New arXiv issues filed**:

| Issue | Source | Priority | Title |
|---|---|---|---|
| #4026 | arXiv:2605.02812 | P2 | security(scheduler): temporal re-entry defense for persistent scheduled task state |
| #4029 | arXiv:2604.11378 | P3 | research(orchestration): scheduler-theoretic execution framework with strict escalation protocols |

---

### 2026-05-16 — CI-787 researcher scan

**Scope**: Full scan — Goose v1.34.0 new release, Claude Code v2.1.138→v2.1.143 delta, Codex CLI v0.131-alpha, dependency advisories, new arXiv security and orchestration papers (May 6–14, 2026).

**Reference table updates**:

| Agent | Previous | Current | New notable features | Zeph gap/action |
|---|---|---|---|---|
| Goose (AAIF) | v1.33.1 | **v1.34.0** (May 13, 2026) | Hooks stable (inline config.toml), agents CRUD via ACP, projects as backend sources, ACP streamable HTTP spec compliance, auto-updating plugins, provider-first onboarding, consecutive tool call summarization | Filed #3885 (hooks+ACP P3) |
| Claude Code | v2.1.138 | **v2.1.143** (May 15, 2026) | v2.1.139: /goal autonomous multi-turn + Agent View fleet dashboard, hook args field, /scroll-speed; v2.1.140-143: rapid fix/regression cycle (Unhandled case [object Object]); plugin dependency enforcement; worktree.bgIsolation none; projected context cost | Filed #3883 (/goal P3), #3884 (agent view P4) |
| Codex CLI | v0.130.0 | **v0.131.0-alpha.22** (May 15, pre-release) | Alpha only; no new stable features confirmed | No new issues |

**New research papers filed**:

| Issue | Priority | Category | Source |
|---|---|---|---|
| #3887 | P2 | research(security) | AgentTrust — runtime interception with shell deobfuscation + RiskChain multi-step attack detection (arXiv:2605.04785) |
| #3891 | P3 | research(orchestration) | Uno-Orchestra — RL-learned joint routing+decomposition, 77% pass@1, 10× cost reduction (arXiv:2605.05007) |
| #3894 | P3 | research(security) | OS-model agent security — capability rings + namespace isolation per tool invocation (arXiv:2605.14932) |

**New parity gaps filed**:

| Issue | Priority | Category | Summary |
|---|---|---|---|
| #3883 | P3 | parity(orchestration) | Claude Code /goal — autonomous multi-turn with supervisor verifier session |
| #3884 | P4 | parity(tui) | Claude Code agent view — cross-session fleet dashboard |
| #3885 | P3 | parity(hooks) | Goose v1.34.0 hooks stable in config.toml + ACP streamable HTTP compliance |

**Dependency status**:
- RUSTSEC-2025-0134 (rustls-pemfile via qdrant-client → tonic) — **unchanged**, still only advisory, blocked upstream (#3772)
- `metrics 0.24.5` **yanked** — filed #3895 P3; fix: `cargo update -p metrics`
- No new RUSTSEC security advisories affecting Zeph's dependency tree

**Next scan trigger**: Codex CLI v0.131.0 stable, Goose v1.35.0, or Claude Code v2.2.x.

---

[Previous scan history preserved above; see full file for complete CI cycle logs dating back to 2026-03-30]
