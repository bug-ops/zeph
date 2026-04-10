---
aliases:
  - Output Filtering
  - FilterPipeline
tags:
  - sdd
  - spec
  - tools
  - security
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[006-tools/spec]]"
  - "[[010-security/spec]]"
---

# Spec: Output Filtering

> [!info]
> FilterPipeline, CommandMatcher, SecurityPatterns; prevents sensitive data leaks in tool output.

## Sources

### External
- **OWASP AI Agent Security Cheat Sheet** (2026) — secret/credential redaction requirements: https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html
- **Log-To-Leak: Prompt Injection via MCP** (2025) — tool output as injection vector, motivates SecurityPatterns: https://openreview.net/forum?id=UVgbFuXPaO

### Internal
| File | Contents |
|---|---|
| `crates/zeph-tools/src/filter/mod.rs` | `OutputFilterRegistry`, `FilterPipeline`, `CommandMatcher`, `FilterMetrics` |
| `crates/zeph-tools/src/filter/security.rs` | `SecurityPatterns`, 17 `LazyLock<Regex>` |
| `crates/zeph-tools/src/filter/declarative.rs` | Per-filter TOML config structs |

---

`crates/zeph-tools/src/filter/` — composable filters applied to tool output before context injection.

## OutputFilterRegistry

```
OutputFilterRegistry
├── filters: Vec<(CommandMatcher, Box<dyn OutputFilter>)>
├── security: SecurityPatterns (17 LazyLock<Regex>, compiled at init)
└── metrics: Mutex<FilterMetrics>
```

- Security patterns compiled at registry creation — **not per-command** (perf guarantee)
- Security patterns applied **after** filter pipeline — findings are never lost by early filtering
- Metrics checked every 50 commands, logged at DEBUG level

## CommandMatcher

```rust
enum CommandMatcher {
    Exact(String),            // exact tool name match
    Prefix(String),           // tool name starts with prefix
    Regex(Box<Regex>),        // pattern match on tool name
    Custom(fn(&str) -> bool), // tests only
}
```

**`matches(command)` logic** — checks both:
1. Direct command as-is
2. **Extracted last command** from compound expressions:
   - Split by `&&` or `;`, take rightmost segment
   - Strip trailing pipes and `2>` redirections
   - Examples: `cd /path && cargo test 2>&1 | tail` → matches against `cargo test`

## FilterPipeline

```
FilterPipeline<'a>: stateless, no order dependency
  stage_1 → stage_2 → stage_3
           output passes forward
```

- Each stage produces `FilterResult { output, confidence: FilterConfidence, kept_lines }`
- `FilterConfidence`: `Full > Partial > Fallback` (monotonically worse: Fallback > Partial > Full)
- **Confidence aggregation**: `worse_confidence()` across all stages — monotonic, never improves
- `kept_lines` overwritten by last non-empty stage result
- Final stats: `raw_chars` from original input, `filtered_chars` from final output

## OutputFilter Trait

```rust
trait OutputFilter {
    fn filter(&self, command: &str, raw_output: &str, exit_code: i32) -> FilterResult;
}
```

## Registry Apply Logic

1. `enabled = false` → return `None` immediately
2. No matching filter → return `None` (no filtering, raw output passes)
3. Single matching filter → direct call, no pipeline overhead
4. Multiple matching filters → compose into `FilterPipeline`
5. Append security patterns to final output (unconditional if `security.enabled`)
6. Record metrics

## FilterMetrics

```
FilterMetrics {
    total_commands: u64,
    filtered_commands: u64,  // incremented only if filtered_chars < raw_chars
    skipped: u64,
    raw_chars_total: u64,
    filtered_chars_total: u64,
    confidence_counts: [u64; 3],  // [0]=Full, [1]=Partial, [2]=Fallback
    avg_reduction_pct: f32,
}
```

## SecurityPatterns

17 `LazyLock<Regex>` across 6 categories:

| Category | Examples |
|---|---|
| API keys | `sk-[A-Za-z0-9]{32,}`, `AKIA[0-9A-Z]{16}` |
| Auth tokens | Bearer, JWT |
| Private keys | PEM blocks, SSH headers |
| Passwords | Common password field patterns |
| Connection strings | DB URIs with credentials |
| Cloud metadata | AWS/GCP/Azure metadata responses |

Matched content replaced with `[REDACTED:{category}]`.

## Key Invariants

- Security patterns compiled at init — never recompile per-command
- Security append runs **after** pipeline — cannot be reordered before pipeline stages
- `FilterConfidence::worse_confidence()` is monotonic — never improves through pipeline
- Raw output passes unchanged if no matching filters (`None` return)
- `confidence_counts` array is always `[u64; 3]` indexed `Full=0, Partial=1, Fallback=2`
- `filtered_commands` counter incremented **only if** `filtered_chars < raw_chars`
- `extract_last_command()` logic (split by `&&`/`;`, strip pipes/redirections) must not change — changing it breaks compound command matching
