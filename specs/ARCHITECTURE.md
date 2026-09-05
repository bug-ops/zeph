---
aliases:
  - Specs Architecture
  - Dependency Map
  - Specs Dependency Graph
tags:
  - sdd
  - architecture
  - reference
created: 2026-04-10
status: reference
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
---

# Specs Architecture: Dependency Graph

> [!note] Graph coverage — 2026-09 update
> This dependency graph includes all specs 001–085. Coverage was extended per issue #6634
> to include all 85 registered specifications. Dependency edges were derived primarily from 
> each spec's front matter `related:` fields, with spot-checks against body-text cross-references 
> and "Sources" sections. The graph represents known dependencies as of 2026-09; future maintainers 
> should spot-check new specs' edges against their full source before publishing. For the most current details, see [[MOC-specs]].

## Dependency Graph (Mermaid)

```mermaid
graph TB
    001["001: System Invariants<br/>(contracts for everything)"]
    
    002["002: Agent Loop<br/>(turn lifecycle)"]
    003["003: LLM Providers"]
    022["022: Config Simplification<br/>(provider registry)"]
    023["023: Complexity Triage<br/>(routing)"]
    024["024: Multi-Model Design"]
    004["004: Memory<br/>(SQLite + Qdrant)"]
    012["012: Graph Memory<br/>(entity graph)"]
    005["005: Skills<br/>(SKILL.md, hot-reload)"]
    015["015: Self-Learning<br/>(feedback, Wilson score)"]
    009["009: Orchestration<br/>(DAG planning)"]
    
    007["007: Channels<br/>(I/O trait)"]
    011["011: TUI<br/>(ratatui dashboard)"]
    026["026: TUI Subagents<br/>(sidebar)"]
    030["030: TUI Autocomplete<br/>(slash commands)"]
    
    006["006: Tools<br/>(ToolExecutor)"]
    016["016: Output Filtering<br/>(security patterns)"]
    008["008: MCP<br/>(client + server)"]
    010["010: Security<br/>(vault, isolation)"]
    025["025: Classifiers<br/>(injection, PII)"]
    
    020["020: Config Loading<br/>(resolution order)"]
    029["029: Feature Flags<br/>(cargo features)"]
    031["031: Database<br/>(SQLite + PostgreSQL)"]
    018["018: Scheduler<br/>(cron tasks)"]
    028["028: Hooks<br/>(cwd, file events)"]
    017["017: Index<br/>(AST, semantic search)"]
    019["019: Gateway<br/>(webhooks)"]
    
    013["013: ACP<br/>(Agent Control)"]
    014["014: A2A<br/>(Agent-to-Agent)"]
    027["027: RuntimeLayer<br/>(middleware hooks)"]
    032["032: Handoff<br/>(skill exchange)"]
    033["033: Subagent Context<br/>(gap analysis)"]
    034["034: Benchmark<br/>(zeph-bench)"]
    
    %% Specs 035-085 (issue #6634)
    021["021: Context Crate<br/>(ContextBudget)"]
    035["035: Profiling<br/>(telemetry)"]
    036["036: Prometheus Metrics<br/>(export)"]
    037["037: Config Schema"]
    038["038: Vault"]
    039["039: Background Tasks"]
    040["040: Sanitizer"]
    041["041: Experiments"]
    042["042: Slash Commands"]
    043["043: Shared Primitives"]
    044["044: Subagent Lifecycle"]
    045["045: Interop Gaps"]
    046["046: MARCH Quality"]
    047["047: CLI Modes"]
    048["048: SLM Cost"]
    049["049: Agent Decomposition"]
    050["050: Security Governance"]
    051["051: Gonka Gateway"]
    052["052: Gonka Native"]
    053["053: Speculation Engine"]
    054["054: Agent Feedback"]
    055["055: Cocoon"]
    056["056: AutoSkill A1"]
    057["057: AutoSkill A2"]
    058["058: AutoSkill A3"]
    059["059: AutoSkill A4"]
    060["060: AutoSkill A5"]
    061["061: AutoSkill A6"]
    062["062: CAM"]
    063["063: Worktree"]
    064["064: Durable Execution"]
    065["065: Ephemeral Plugins"]
    066["066: Deep Link"]
    067["067: Knowledge Ingest"]
    068["068: Session Persistence"]
    069["069: Threat Model"]
    070["070: Runtime Thinking"]
    071["071: Router Thinking"]
    072["072: Multimodal MCP"]
    073["073: ORCH Ensemble"]
    074["074: HITL Interrupt"]
    075["075: Node Control"]
    076["076: CLI Flag Mismatch"]
    077["077: Safe Mode"]
    078["078: Agent Persistence"]
    079["079: Plugin Management"]
    080["080: Cross-Thread Store"]
    081["081: Transcript Integrity"]
    082["082: Usage Cost"]
    083["083: Write Consent"]
    084["084: Mention Picker"]
    085["085: Agent Identity"]
    
    %% Layer 0 → Layer 1 (core foundation)
    001 --> 002
    001 --> 003
    001 --> 004
    001 --> 005
    001 --> 006
    001 --> 007
    
    %% Layer 1: Agent Core
    002 --> 003
    002 --> 004
    002 --> 005
    002 --> 009
    003 --> 022
    003 --> 073
 023
    003 --> 024
    004 --> 012
    005 --> 015
    009 --> 023
    
    %% Layer 2: Channels & I/O
    002 --> 007
    007 --> 011
    011 --> 026
    011 --> 030
    
    %% Layer 3: Tools & Security
    002 --> 006
    006 --> 016
    006 --> 008
    008 --> 010
    006 --> 025
    010 --> 025
    
    %% Layer 4: Infrastructure
    002 --> 020
    002 --> 031
    002 --> 018
    002 --> 017
    002 --> 019
    020 --> 029
    020 --> 065
 028
    017 --> 004
    
    %% Bidirectional relationships (dotted)
    004 -.->|graph integration| 012
    011 -.->|metrics export| 036
    015 -.->|feedback signals| 025
    018 -.->|event triggers| 028
    020 -.->|feature resolution| 029
    022 -.->|routing logic| 023
    026 -.->|lifecycle| 027
    005 -.->|skill exchange| 032
    
    %% Cross-cutting protocols
    002 --> 013
    002 --> 014
    002 --> 027
    026 -.->|context| 033
    009 --> 032
    
    %% Specs 035-085 dependency edges (corrected direction: foundational --> dependent)
    001 --> 035
    001 --> 036
    001 --> 038
    001 --> 042
    001 --> 043
    001 --> 044
    001 --> 046
    001 --> 047
    001 --> 049
    001 --> 050
    001 --> 051
    001 --> 052
    001 --> 053
    001 --> 054
    001 --> 055
    001 --> 056
    001 --> 064
    001 --> 066
    001 --> 067
    001 --> 069
    001 --> 039
    001 --> 057
    001 --> 058
    001 --> 059
    001 --> 060
    001 --> 061
    001 --> 072
    001 --> 073
    001 --> 074
    001 --> 075
    001 --> 076
    001 --> 077
    001 --> 078
    001 --> 079
    001 --> 080
    001 --> 081
    001 --> 083
    
    002 --> 039
    002 --> 042
    002 --> 044
    002 --> 046
    002 --> 047
    002 --> 049
    002 --> 053
    002 --> 054
    002 --> 068
    002 --> 070
    002 --> 078
    
    003 --> 046
    003 --> 051
    003 --> 052
    003 --> 053
    003 --> 055
    003 --> 065
    003 --> 070
    003 --> 071
    003 --> 077
    
    004 --> 062
    004 --> 067
    004 --> 078
    004 --> 080
    004 --> 083
    
    005 --> 056
    005 --> 057
    005 --> 058
    005 --> 059
    005 --> 060
    005 --> 061
    005 --> 079
    005 --> 084
    
    006 --> 050
    006 --> 053
    
    007 --> 047
    007 --> 068
    
    008 --> 045
    008 --> 072
    008 --> 079
    
    009 --> 062
    009 --> 064
    009 --> 073
    009 --> 074
    009 --> 075
    009 --> 080
    
    010 --> 038
    010 --> 040
    010 --> 043
    010 --> 044
    010 --> 050
    010 --> 065
    010 --> 066
    010 --> 069
    010 --> 072
    010 --> 078
    010 --> 079
    010 --> 080
    010 --> 081
    
    011 --> 035
    011 --> 068
    011 --> 084
    
    012 --> 067
    
    013 --> 045
    013 --> 066
    013 --> 068
    013 --> 074
    
    014 --> 045
    
    015 --> 054
    015 --> 056
    015 --> 057
    015 --> 058
    015 --> 059
    015 --> 060
    015 --> 061
    
    016 --> 040
    
    017 --> 067
    
    018 --> 064
    
    019 --> 036
    
    020 --> 037
    020 --> 041
    
    021 --> 062
    021 --> 068
    
    022 --> 037
    022 --> 051
    022 --> 052
    022 --> 055
    
    023 --> 071
    024 --> 046
    024 --> 058
    024 --> 072
    024 --> 073
    
    025 --> 040
    
    026 --> 044
    
    027 --> 035
    027 --> 049
    027 --> 079
    
    028 --> 077
    029 --> 035
    029 --> 036
    029 --> 037
    029 --> 041
    029 --> 064
    
    030 --> 042
    030 --> 084
    
    031 --> 064
    031 --> 068
    031 --> 078
    031 --> 080
    031 --> 085
    
    033 --> 044
    
    035 --> 036

    036 --> 039
    
    038 --> 043
    038 --> 051
    038 --> 052
    038 --> 055
    038 --> 064
    038 --> 068
    038 --> 085
    039 --> 011
    039 --> 044
    039 --> 049
    039 --> 084
 049
    039 --> 064
    039 --> 073
    039 --> 075
    039 --> 080
    039 --> 083
    
    040 --> 067
    040 --> 072
    040 --> 080
    042 --> 044
    042 --> 070
    042 --> 047
 070
    042 --> 077
    043 --> 017
    043 --> 039
 062
    043 --> 066
    043 --> 068
    043 --> 062
    043 --> 077
    044 --> 047
    044 --> 063
    044 --> 064
    044 --> 084
    047 --> 076
    047 --> 077
    
    048 --> 082

    049 --> 042
    
    050 --> 069
    
    051 --> 052
    052 --> 055
    
    055 --> 069
    
    056 --> 057

    057 --> 061
    056 --> 067
    063 --> 064
    
    064 --> 074

    066 --> 037
    064 --> 068
    
    081 --> 064
    
    065 --> 070
    
    068 --> 037
    068 --> 072
    
    069 --> 072
    069 --> 074
    075 --> 080
    078 --> 056
    078 --> 064
    
    079 --> 065
    
    %% Styling by layer
    classDef layer0 fill:#ff6b6b,stroke:#c92a2a,color:#fff,font-weight:bold
    classDef layer1 fill:#4c6ef5,stroke:#364fc7,color:#fff
    classDef layer2 fill:#15aabf,stroke:#0b7285,color:#fff
    classDef layer3 fill:#a3e635,stroke:#5c940d,color:#000
    classDef layer4 fill:#ffa94d,stroke:#d9480f,color:#000
    classDef crosscutting fill:#e599f7,stroke:#9c36b5,color:#000
    classDef expanded fill:#b4a7d6,stroke:#6a4c93,color:#000
    
    class 001 layer0
    class 002,003,004,005,009,012,015,022,023,024 layer1
    class 007,011,026,030 layer2
    class 006,008,016,010,025 layer3
    class 020,029,031,018,028,017,019 layer4
    class 013,014,027,032,033,034 crosscutting
    class 021,035,036,037,038,039,040,041,042,043,044,045,046,047,048,049,050,051,052,053,054,055,056,057,058,059,060,061,062,063,064,065,066,067,068,069,070,071,072,073,074,075,076,077,078,079,080,081,082,083,084,085 expanded
```

---

## Layer Breakdown

**Layer 0 (001)** — System Invariants: Foundational contracts all other specs depend on.

**Layer 1 (002-005, 009, 012, 015, 021-024)** — Agent Core: Core reasoning pipeline (agent loop, LLM providers, memory, skills, orchestration, context management).

**Layer 2 (007, 011, 026, 030, 035, 047, 068, 084)** — I/O & User Interaction: Channels, TUI, profiling, CLI modes, session persistence, mention picker.

**Layer 3 (006, 008, 016, 010, 025, 040, 050, 053, 072)** — Tool Execution & Safety: Tools, MCP, security, content sanitization, capability governance, ML classifiers, multimodal MCP.

**Layer 4 (017-020, 028, 029, 031, 037, 041, 064, 078)** — Infrastructure & Persistence: Config, database, scheduling, hooks, indexing, gateway, durable execution, agent persistence.

**Layer X (013, 014, 027, 032-034, 036, 038-039, 042-046, 048-052, 054-062, 063, 065-067, 069-077, 079-085)** — Protocols, Specialized, & Extended Subsystems: ACP, A2A, runtime layers, handoff, vault, background tasks, slash commands, shared primitives, subagent lifecycle, protocol gaps, MARCH quality, CLI, SLM metrics, decomposition, security governance, LLM integrations (Gonka, Cocoon), speculation, feedback, AutoSkill pipeline, context-adaptive memory, worktrees, deep links, knowledge ingest, threat model, runtime controls, orchestration ensemble/HITL/node control, config flags, safe mode, plugin management, cross-thread store, transcript integrity, usage tracking, memory consent, identity isolation.

---

## Dependency Summary Table

| Layer | Specs | Purpose | Key Contracts |
|-------|-------|---------|---|
| **0** | 001 | Contracts & invariants | System-wide rules |
| **1** | 002, 003, 004, 005, 009, 012, 015, 022, 023, 024, 021 | Agent loop & reasoning | LLM, memory, skills, orchestration, context |
| **2** | 007, 011, 026, 030, 035, 047, 068, 084 | I/O, telemetry & user interaction | Channel trait, TUI widgets, profiling, CLI modes |
| **3** | 006, 008, 016, 010, 025, 040, 050, 053, 072 | Tool execution & safety | ToolExecutor trait, security gates, sanitizer, classifiers |
| **4** | 020, 029, 031, 018, 028, 017, 019, 037, 041, 064, 078 | Infrastructure & persistence | Config, persistence, hooks, durable execution |
| **X** | 013, 014, 027, 032, 033, 034, 036, 038, 039, 042, 043, 044, 045, 046, 048, 049, 051, 052, 054, 055, 056-061, 062, 063, 065, 066, 067, 069, 070, 071, 073, 074, 075, 076, 077, 079, 080, 081, 082, 083, 085 | Protocols, security, orchestration & specialized subsystems | ACP, A2A, handoff, vault, skills, orchestration, memory, LLM integrations |

---

## Bidirectional Links (Peer Dependencies)

Specs that reference each other (not purely hierarchical):

```
004 ↔ 012     Memory ↔ Graph (graph is integrated with memory)
022 ↔ 023     Provider Registry ↔ Complexity Triage (routing)
009 ↔ 023     Orchestration ↔ Complexity Triage (DAG routing)
026 ↔ 027     TUI Subagents ↔ RuntimeLayer (lifecycle hooks)
020 ↔ 029     Config Loading ↔ Feature Flags (resolution)
010 ↔ 025     Security ↔ ML Classifiers (security signals)
015 ↔ 025     Self-Learning ↔ ML Classifiers (feedback)
005 ↔ 032     Skills ↔ Handoff Protocol (skill exchange)
017 ↔ 004     Code Index ↔ Memory (context injection)
018 ↔ 028     Scheduler ↔ Hooks (event triggers)
```

---

## How to Read This Map

### For Understanding Architecture

1. **Start at Layer 0** — read [[001-system-invariants/spec]] to understand non-negotiable contracts
2. **Layer 1** is the **agent heart** — how reasoning, memory, and skills work together
3. **Layer 2** is **user interaction** — how input reaches the agent and output leaves
4. **Layer 3** is **execution safety** — tools, security gates, and permission models
5. **Layer 4** is **infrastructure** — persistence, scheduling, indexing, configuration
6. **Layer X** is **integration glue** — protocols, multi-agent handoff, performance testing

### For Planning Features

- **New reasoning feature?** → Modify [[002-agent-loop/spec|Layer 1]]
- **New input channel?** → Add to [[007-channels/spec|Layer 2]]
- **New security gate?** → Add to [[010-security/spec|Layer 3]]
- **New persistence backend?** → Modify [[031-database-abstraction/spec|Layer 4]]
- **Multi-agent coordination?** → Extend [[032-handoff-skill-system/spec|Layer X]]

### For Debugging

Trace the dependency chain backward from the failing component:
- **TUI widget broken?** → Check [[007-channels/spec|Channel trait]]
- **Tool not executing?** → Check [[006-tools/spec|ToolExecutor trait]]
- **Memory not persisting?** → Check [[031-database-abstraction/spec|Database]] and [[020-config-loading/spec|Config]]
- **Subagent spawning fails?** → Check [[033-subagent-context-propagation/spec|Context propagation]]

### For Onboarding

Read in this order:
1. [[001-system-invariants/spec]] — establish mental model of contracts
2. [[002-agent-loop/spec]] — understand main control flow
3. Your domain layer (1–4) — drill into the subsystem you're working on
4. Related specs via the dependency graph — understand integration points

---

## Legend

```
┌─────┐
│ NNN │ = Spec ID and title
└─────┘

    │
    ▼     = Depends on (reads/calls)

    ↔     = Bidirectional dependency (peer relationship)

    ┌─┐
    │─├─┬─ = Fan-out (multiple specs depend on this one)
    └─┘
```

---

## See Also

- [[MOC-specs]] — complete specs index with descriptions
- [[constitution]] — project-wide non-negotiable principles
- [[TEMPLATE.md]] — template for creating new specs
