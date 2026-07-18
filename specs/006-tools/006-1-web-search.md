---
aliases:
  - Web Search Tool
  - WebSearchExecutor
  - SearchProvider
tags:
  - sdd
  - spec
  - tools
  - security
  - egress
created: 2026-07-18
status: draft
related:
  - "[[006-tools/spec]]"
  - "[[010-security/spec]]"
  - "[[010-3-authorization]]"
  - "[[010-5-egress-logging]]"
  - "[[040-sanitizer/spec]]"
  - "[[038-vault/spec]]"
---

# Spec: Native `web_search` Tool

> [!info]
> A native, query-based `web_search` tool for `zeph-tools` that lets the LLM issue
> a natural-language query and receive ranked `title`/`url`/`snippet` results,
> without requiring a pre-known URL. Mirrors `WebScrapeExecutor`'s cross-cutting
> machinery (SSRF validation, egress logging, audit, IPI filtering) but is
> query-in rather than URL-in, and never auto-fetches result URLs.

## Sources

### External
- **OWASP AI Agent Security Cheat Sheet** — egress monitoring and untrusted-content
  handling guidance: https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html

### Internal
| File | Contents |
|---|---|
| `crates/zeph-tools/src/scrape.rs` | `WebScrapeExecutor` — the pattern this spec mirrors: `validate_url`, `check_domain_policy`, `resolve_and_validate`, `build_client` (SSRF addr-pinning via `resolve_to_addrs`, lines 226-231), `EgressEvent` emission, `log_audit`, `apply_ipi_filter` |
| `crates/zeph-tools/src/executor.rs` | `ToolExecutor` trait, `ToolCall`, `ToolOutput`, `ToolError` (`Blocked { command }`, `Http { status, message }`, `Timeout { timeout_secs }`, `InvalidParams { message }`, `Execution`), `ClaimSource` (`#[non_exhaustive]`) |
| `crates/zeph-tools/src/registry.rs` | `ToolDef`, `InvocationHint::ToolCall` |
| `crates/zeph-core/src/agent/tool_execution/sanitize.rs` | `build_tool_output_source(tool_name: &str)` (line 85) — the sanitizer trust bridge; dispatches on **tool-name string**, not `ClaimSource` |
| `crates/zeph-config/src/tools.rs` | `ScrapeConfig`, `ToolsConfig` — config pattern to extend with `SearchConfig` |
| `010-5-egress-logging.md` | Mandatory `EgressEvent` contract this tool must satisfy |
| `010-3-authorization.md` | SSRF / domain-policy enforcement this tool must satisfy (with a documented allowlist exemption) |
| Architecture handoff `.local/handoff/2026-07-18T03-06-38-architect.md` | Full design rationale, alternatives considered, integration-point audit |
| Critic approval `.local/handoff/2026-07-18T03-16-14-critic.md` | Verification of the two security-critical fixes (S1, S2) against source |

---

## 1. Overview

### Problem Statement

Zeph has no first-class, query-based web search tool. The only web-facing tool
is `WebScrapeExecutor` (`web_scrape`/`fetch`), which requires a pre-known URL —
the tool description explicitly forbids the LLM from guessing or constructing
URLs. Comparable agents (Codex, Claude Code) ship a native `web_search` that
takes a natural-language query and returns ranked results, which the agent can
then choose to fetch. Without it, Zeph cannot answer open-ended, current-info
questions that have no obvious starting URL. GitHub issue #6358.

### Goal

- Add a `web_search` tool that takes a natural-language query and returns a
  ranked list of `{title, url, snippet}` results from an external search API.
- Reuse — do not duplicate — the existing SSRF, egress-logging, audit, and IPI
  (indirect prompt injection) machinery that `WebScrapeExecutor` already owns.
- Result URLs are **never** auto-fetched by this tool. Opening a result is a
  separate, explicit `fetch`/`web_scrape` call that re-applies full domain
  policy.
- Vault-only API key resolution (age vault, never env vars), runtime-gated
  (no cargo feature — see §3.6).

### Out of Scope (v1)

- Multiple simultaneous search backends active at once — v1 ships exactly one
  backend (Brave Search API) behind a `SearchBackend` enum designed so
  Tavily/SearXNG can be added as new variants later without touching the
  executor.
- LLM-based reranking, summarization, or query rewriting of results — v1
  returns raw ranked results only (see §6, Multi-Model Principle note).
- Defending against rank/SEO poisoning (adversarial promotion of a malicious
  result to the top of legitimate rankings) — documented as an accepted v1
  limitation (§5).
- Auto-fetching or auto-summarizing result URLs — strictly out of scope; that
  is the job of `fetch`/`web_scrape`, invoked separately by the LLM.

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN the LLM issues a `web_search` tool call with a non-empty `query` AND `[tools.search].enabled = true` AND a backend resolves via `SearchBackend::from_config` THE SYSTEM SHALL issue exactly one outbound HTTPS request to the configured search endpoint and return ranked results. | must |
| FR-002 | WHEN `[tools.search].enabled = false` OR `SearchBackend::from_config` fails to resolve (e.g. no API key for a keyed backend) THE SYSTEM SHALL NOT advertise `web_search` in `tool_definitions()` — the LLM never sees the tool. | must |
| FR-003 | WHEN a `web_search` call completes (success, block, or error) THE SYSTEM SHALL classify the tool output's trust source identically to `web_scrape`/`fetch` — `ContentSourceKind::WebScrape` (`ExternalUntrusted` + quarantine) — via the **tool-name string** branch in `build_tool_output_source`, not via `ClaimSource`. | must |
| FR-004 | WHEN `web_search` returns results THE SYSTEM SHALL run the rendered result text (titles + snippets) through `IpiFilter::filter_async` before it reaches the LLM, exactly as `web_scrape` output does. | must |
| FR-005 | WHEN validating the search endpoint THE SYSTEM SHALL apply full SSRF validation (`validate_url` HTTPS-only + `resolve_and_validate` DNS/private-IP rejection) and `[tools.scrape].denied_domains`, but SHALL NOT apply `[tools.scrape].allowed_domains` to the fixed, operator-configured search endpoint. | must |
| FR-006 | WHEN the search endpoint's addresses are resolved via `resolve_and_validate` THE SYSTEM SHALL pin those exact resolved addresses into the backend's `reqwest::Client` via `resolve_to_addrs` (mirroring `scrape.rs`'s `build_client(host, addrs)`, lines 226-231) before issuing the request — closing the DNS-rebinding TOCTOU window between validation and connection. | must |
| FR-007 | WHEN `web_search` issues its outbound HTTP call (success, pre-response failure, or pre-flight block) THE SYSTEM SHALL emit exactly one `EgressEvent` per spec `010-5`, sharing the same `correlation_id` as the parent `AuditEntry`, with `tool = "web_search"` and `hop = 0`. | must |
| FR-008 | WHEN the backend returns HTTP 429 (rate limit / quota exhaustion) THE SYSTEM SHALL map it to `ToolError::Blocked` (permanent, `PolicyBlocked` category — not retried), distinct from genuine network transients which remain retryable via `is_tool_retryable("web_search") == true`. | must |
| FR-009 | WHEN `query` is empty or whitespace-only THE SYSTEM SHALL return `ToolError::InvalidParams` before issuing any HTTP call or `EgressEvent`. | must |
| FR-010 | WHEN the backend returns zero results for a well-formed query THE SYSTEM SHALL return a normal (non-error) `ToolOutput` summarizing "no results", mirroring `web_scrape`'s empty-selector behavior. | must |
| FR-011 | WHEN `limit` is absent, zero, or exceeds `[tools.search].max_results` THE SYSTEM SHALL clamp it to `[1, max_results]` (absent defaults to `max_results`). | must |
| FR-012 | WHEN a `web_search` `ToolOutput` is recorded THE SYSTEM SHALL set `claim_source = Some(ClaimSource::WebSearch)` for audit-log provenance only — this field is NOT read by the sanitizer trust bridge (see FR-003) and MUST NOT be relied upon as an enforcement mechanism. | must |
| FR-013 | WHEN the operator runs `--migrate-config` on a config predating this feature THE SYSTEM SHALL insert a commented-out `[tools.search]` block with defaults (`enabled = false`). | must |
| FR-014 | WHEN the operator runs `--init` THE SYSTEM SHALL offer a step to enable `web_search` and, if enabled, prompt for and store the API key in the age vault under the configured `api_key_vault_key` name. | should |

---

## 3. Architecture

### 3.1 Module layout

New `search/` module in `zeph-tools`:

```
crates/zeph-tools/src/search/
├── mod.rs       # WebSearchExecutor (ToolExecutor impl)
├── provider.rs  # SearchProvider trait, SearchResult, SearchError, SearchBackend enum
└── brave.rs     # BraveSearchProvider (v1 concrete backend)
```

Re-exported at `crates/zeph-tools/src/lib.rs`: `WebSearchExecutor`, `SearchProvider`,
`SearchResult`, `SearchError`, `SearchBackend`.

### 3.2 `SearchProvider` trait and backend dispatch

```rust
/// Contract for a query-based web search backend.
///
/// Implementors guarantee they issue at most one HTTPS call per `search()`
/// invocation, to the exact URL returned by `endpoint()`.
pub trait SearchProvider: Send + Sync {
    /// Issue a single search query and return ranked results.
    ///
    /// # Errors
    /// Returns [`SearchError`] on missing key, HTTP failure, timeout,
    /// SSRF/policy block, or response parse failure.
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<SearchResult>, SearchError>> + Send;

    /// The fixed, operator-configured endpoint this provider calls.
    /// Used by the executor to run SSRF/denylist validation and addr-pinning
    /// (see §3.4) BEFORE the request is issued.
    fn endpoint(&self) -> &url::Url;

    /// Stable provider name for audit/egress records (e.g. `"brave"`).
    fn name(&self) -> &'static str;
}

/// Concrete backend dispatch. Enum (not `Box<dyn ErasedSearchProvider>`) to
/// keep `WebSearchExecutor` non-generic and avoid erased-async-trait
/// boilerplate for a small, closed backend set. New backends are new variants.
pub enum SearchBackend {
    Brave(BraveSearchProvider),
}

impl SearchBackend {
    /// Resolve a backend from config + an optional pre-fetched API key.
    ///
    /// Returns `Err` for a keyed backend (e.g. Brave) with no key. A future
    /// keyless backend (e.g. SearXNG) MUST be constructible with
    /// `api_key: None` — do not gate on "key present"; gate on this
    /// function's success (see FR-002, §3.6).
    pub fn from_config(
        cfg: &SearchConfig,
        api_key: Option<zeph_common::Secret>,
    ) -> Result<Self, SearchError>;
}
```

```rust
/// One ranked search result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Search-backend error taxonomy — mapped to `ToolError` at the executor
/// boundary (§3.5); never surfaced raw to the LLM.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("no API key configured for backend {backend}")]
    MissingApiKey { backend: &'static str },
    #[error("HTTP error {status}: {message}")]
    Http { status: u16, message: String },
    #[error("request timed out")]
    Timeout,
    #[error("blocked: {reason}")]
    Blocked { reason: String },
    #[error("failed to parse response: {0}")]
    Parse(String),
    #[error("provider error: {0}")]
    Provider(String),
}
```

### 3.3 `WebSearchExecutor` (mirrors `WebScrapeExecutor`)

```rust
pub struct WebSearchExecutor {
    backend: SearchBackend,
    timeout: Duration,
    max_results: usize,
    /// From `[tools.scrape].denied_domains`. The scrape ALLOWlist is
    /// intentionally NOT consulted (see FR-005 / §4 exemption invariant).
    denied_domains: Vec<String>,
    audit_logger: Option<Arc<AuditLogger>>,
    egress_config: EgressConfig,
    egress_tx: Option<tokio::sync::mpsc::Sender<EgressEvent>>,
    egress_dropped: Arc<AtomicU64>,
    ipi_filter: IpiFilter,
}
```

- `WebSearchExecutor::new(&SearchConfig, api_key: Option<Secret>) -> Option<Self>` —
  returns `Some` only when `[tools.search].enabled == true` AND
  `SearchBackend::from_config(cfg, api_key)` succeeds; `None` otherwise (FR-002).
- `.with_audit(...)`, `.with_egress_config(...)`, `.with_egress_tx(...)` — same
  builder shape as `WebScrapeExecutor`.

`ToolExecutor` impl:

- `tool_definitions()` → one `ToolDef { id: "web_search", invocation:
  InvocationHint::ToolCall, schema: schema_for!(WebSearchParams), .. }` when
  constructed; the composite chain simply omits this executor's tool when
  `new()` returned `None` at wiring time (FR-002).
  - Tool description explicitly **inverts** `web_scrape`'s "only when a URL is
    known" guidance: it must state the LLM MAY call `web_search` with no prior
    URL for open-ended or current-info queries, that results are untrusted
    text (not to be treated as verified facts), and that opening a result URL
    requires a separate `fetch`/`web_scrape` call.
- `execute_tool_call()` for `"web_search"`:
  1. `deserialize_params::<WebSearchParams>` (`query: String`, `limit: Option<usize>`).
  2. Reject empty/whitespace `query` → `ToolError::InvalidParams` (FR-009), no HTTP call, no `EgressEvent`.
  3. Generate `correlation_id`.
  4. `validate_url(endpoint)` (HTTPS-only) + denylist check (`[tools.scrape].denied_domains`, allowlist skipped per FR-005) + `resolve_and_validate(endpoint)` (SSRF).
     - On block → `ToolError::Blocked`, emit `EgressEvent { blocked: true, block_reason, hop: 0, correlation_id }`.
  5. Pin resolved addrs into the backend's `reqwest::Client` via `resolve_to_addrs` (FR-006) — implemented as a `build_client(host, addrs)` helper mirroring `scrape.rs:226-231`.
  6. `backend.search(query, clamped_limit)` wrapped in `tokio::time::timeout(self.timeout, ...)` (Await Discipline).
  7. Emit `EgressEvent { hop: 0, status, duration_ms, response_bytes, correlation_id }` on completion (success or pre-response failure) (FR-007).
  8. Render results to text, run `apply_ipi_filter` (FR-004).
  9. `run_with_audit(...)` → `ToolOutput { tool_name: "web_search", summary: <rendered results>, claim_source: Some(ClaimSource::WebSearch), raw_response: Some(json results), .. }`.
- `execute()` (fenced-block path) → `Ok(None)` — structured tool-call only, no fenced-block invocation.
- `is_tool_retryable("web_search")` → `true` (idempotent GET; genuine transients retry, but `ToolError::Blocked` from FR-008/FR-005 is terminal by design, same as scrape's SSRF/denylist blocks).
- Uses the `tool_executor_no_inner_defaults!()` leaf defaults: `requires_confirmation = false`, `is_tool_speculatable = false` (conservative parity with `web_scrape` in v1). Confirmation/permission remains owned by the outer `TrustGate`/`PermissionPolicy` wrappers keyed on the `web_search` tool id — the leaf executor does not gate.

### 3.4 SSRF addr-pinning (mandatory — see §4 INVARIANT-2)

The search endpoint's resolved addresses MUST be threaded into the backend's
`reqwest::Client`, not merely validated then discarded:

```rust
fn build_client(host: &str, addrs: &[SocketAddr], timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, addrs)
        .build()
        .unwrap_or_default()
}
```

This is the exact pattern `scrape.rs::build_client` (lines 226-231) already
uses. Without it, a DNS-rebinding attacker could pass `resolve_and_validate`
against a benign IP and then have the actual `reqwest` connection resolve the
hostname again independently, landing on a private/internal address — a
TOCTOU gap. Pinning closes it.

### 3.5 Error handling

| `SearchError` | Maps to | Category | Notes |
|---|---|---|---|
| `MissingApiKey` | `ToolError::InvalidParams` | InvalidParameters | Should not occur at request time — the executor is only constructed with a resolved key/backend (FR-002); defensive mapping only. |
| `Http { status: 429, .. }` | `ToolError::Blocked` | PolicyBlocked (terminal, not retried) | FR-008. Distinguishes quota exhaustion from transient network errors. |
| `Http { status, message }` (other) | `ToolError::Http { status, message }` | per `classify_http_status` | Standard HTTP error taxonomy. |
| `Timeout` | `ToolError::Timeout { timeout_secs }` | Timeout | Every external `.await` wrapped in `tokio::time::timeout` per Await Discipline. |
| `Blocked { reason }` (SSRF/denylist) | `ToolError::Blocked { command: reason }` | PolicyBlocked (terminal) | Same terminal semantics as scrape's SSRF/denylist blocks. |
| `Parse` | `ToolError::Execution` | per IO/parse classification | No partial results surfaced on malformed JSON. |
| `Provider` | `ToolError::Execution` | per IO/parse classification | Catch-all for backend-specific failures. |

No lock is held across any `.await`. No `tokio::spawn` — this is a leaf,
synchronous-request-path tool. The IPI regex scan runs via
`IpiFilter::filter_async` (already `spawn_blocking`-backed), keeping the
executor non-blocking per the CLAUDE.md non-blocking contract.

### 3.6 Runtime gate (no cargo feature)

`web_search` compiles unconditionally — `reqwest`/`serde_json` are already
workspace dependencies and this adds no new heavy deps. It is advertised to
the LLM only when **both**:

1. `[tools.search].enabled == true`, AND
2. `SearchBackend::from_config(cfg, api_key)` succeeds.

Gating on `from_config().is_ok()` rather than "API key present" is
deliberate: it keeps a future keyless backend (e.g. self-hosted SearXNG)
viable without executor changes — such a backend would construct
successfully with `api_key: None`.

### 3.7 Config schema

```toml
[tools.search]
enabled = false                    # runtime gate — false means the tool is never advertised to the LLM
backend = "brave"                  # SearchBackend variant selector
api_key_vault_key = "ZEPH_WEB_SEARCH_API_KEY"   # age-vault key name; NEVER an env var (see §4)
endpoint = "https://api.search.brave.com/res/v1/web/search"  # override for self-host/proxy/alt backend
max_results = 10                   # cap on returned results
timeout = 15                       # seconds
# denied_domains is inherited from [tools.scrape]; the scrape ALLOWlist is
# intentionally NOT applied to this fixed, operator-configured endpoint.
```

```rust
// crates/zeph-config/src/tools.rs
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_search_backend")]
    pub backend: String,
    #[serde(default = "default_search_vault_key")]
    pub api_key_vault_key: String,
    #[serde(default = "default_search_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_search_max_results")]
    pub max_results: usize,
    #[serde(default = "default_search_timeout")]
    pub timeout: u64,
}
// impl Default for SearchConfig; add `#[serde(default)] pub search: SearchConfig`
// to ToolsConfig and its Default impl.
```

Vault key resolution mirrors the existing `skills.registry.auth_vault_key`
pattern (`crates/zeph-core/src/config.rs`, `resolve_secrets_masked`): resolved
once at startup from the age vault, never from an environment variable, and
registered via `register_masked_secret` so it never appears unredacted in
debug dumps or logs.

---

## 4. Key Invariants

### Always (without asking)

- **INVARIANT-1 (trust classification, S1).** `web_search` output is classified
  identically to `web_scrape`/`fetch` — `ContentSourceKind::WebScrape`
  (`ExternalUntrusted` + quarantine) — by adding `|| tool_name == "web_search"`
  to the tool-name branch in `build_tool_output_source`
  (`crates/zeph-core/src/agent/tool_execution/sanitize.rs:88`). This bridge
  dispatches on the **tool-name string**, not on `ClaimSource`.
  `ClaimSource::WebSearch` is audit-provenance only and MUST NOT be treated as
  a trust mechanism — it is not read anywhere in the sanitizer trust-decision
  path. Skipping this fix silently drops `web_search` into the `else` branch
  (`ContentSourceKind::ToolResult`, no quarantine) — strictly weaker than
  `web_scrape` for equivalent attacker-controllable content.
- **INVARIANT-2 (SSRF addr-pinning, S2).** The search endpoint's resolved
  addresses from `resolve_and_validate` are threaded into the backend's
  `reqwest::Client` via `resolve_to_addrs` (§3.4) before the request is
  issued. Validating and then discarding the resolved address (using plain
  hostname-based connection) is insufficient and leaves a DNS-rebinding TOCTOU
  window open.
- Full SSRF validation (`validate_url` HTTPS-only + `resolve_and_validate`
  DNS/private-IP rejection) and `[tools.scrape].denied_domains` apply to the
  search endpoint on every call, with no exceptions.
- Result URLs returned by `web_search` are text only. They are never
  auto-fetched by this tool. Opening one requires a distinct, explicit
  `fetch`/`web_scrape` tool call, which re-applies the full domain policy
  (allowlist included) against that specific URL.
- Every `web_search` HTTP call emits exactly one `EgressEvent` per spec
  `010-5`, sharing the parent `AuditEntry`'s `correlation_id`, with
  `tool = "web_search"`.
- The API key is resolved exclusively from the age vault via
  `api_key_vault_key`. It is never read from an environment variable, never
  logged unredacted, and always registered with `register_masked_secret`.
- Rendered result text (titles + snippets) passes through `IpiFilter::filter_async`
  before reaching the LLM — snippet/title text originates from arbitrary
  indexed pages and is attacker-controllable even though the search API
  endpoint itself is trusted infrastructure.
- `tool_definitions()` returns no `web_search` entry when the tool is
  disabled or the backend fails to construct — the LLM never sees an
  unusable tool.

### Ask First

- Adding a second concurrently-active search backend (multi-backend fan-out
  or fallback) — the v1 `SearchBackend` enum assumes exactly one active
  backend at a time; concurrent dispatch changes the egress/audit
  cardinality assumptions in `010-5`.
- Making `web_search` speculatable (`is_tool_speculatable = true`) — v1 ships
  `false` for conservative parity with `web_scrape`; revisit only with an
  explicit review of the speculative-execution trust implications for
  externally-sourced content.
- Relaxing the allowlist exemption (§3.7) to apply `[tools.scrape].allowed_domains`
  to the search endpoint, or conversely exempting it from `denied_domains` —
  either changes the security posture documented in INVARIANT-2's sibling
  exemption note and requires a spec update, not a code-only change.

### Never

- Never rely on `ClaimSource::WebSearch` (or any `ClaimSource` variant) as the
  sanitizer trust-classification mechanism — the bridge reads the tool-name
  string only (INVARIANT-1).
- Never auto-fetch, auto-summarize, or auto-open a `web_search` result URL
  from within the search tool itself.
- Never resolve the search API key from an environment variable or a config
  file literal — vault only.
- Never issue the backend's HTTP request before both SSRF validation
  (`resolve_and_validate`) and addr-pinning (`resolve_to_addrs`) have
  completed for the endpoint.
- Never treat HTTP 429 as a transient, retryable error — it is `ToolError::Blocked`
  (terminal) to avoid hammering an exhausted quota.
- Never skip `EgressEvent` emission for `web_search`, including on pre-flight
  blocks and pre-response failures.

---

## 5. Known Limitation (v1)

**Rank/SEO poisoning is not defended against by content filtering alone.**
`IpiFilter` catches prompt-injection payloads *inside* result snippet/title
text, but it does not evaluate the *trustworthiness of ranking* — an
adversary who successfully SEO-poisons a query to place a malicious page at
rank 1 is not caught by this tool. Mitigations (result-source reputation
scoring, cross-checking against multiple backends) are out of scope for v1
and are noted here as a documented, accepted gap rather than a silent one.

---

## 6. Multi-Model Design Principle Compliance

v1 has no LLM-backed step (no rerank, no summarization, no query rewrite) —
raw ranked results are the deliverable. Per CLAUDE.md's multi-model
principle, a subsystem calling an LLM must expose a `*_provider` field; since
v1 makes no such call, **no `*_provider` field is added** to `SearchConfig`
(avoids a dead/YAGNI config field). The extension point is documented here,
not built: a future `rerank_provider: ProviderName` field would resolve via
the existing provider-registry pattern (`resolve_named_provider`), analogous
to `memory.compression.compaction_provider`.

---

## 7. Edge Cases and Error Handling

| Case | Behavior |
|---|---|
| Empty/whitespace `query` | `ToolError::InvalidParams`, no HTTP call, no `EgressEvent` (FR-009). |
| Zero results for a well-formed query | Normal `ToolOutput { summary: "No results for query: <q>", .. }` — not an error (FR-010). |
| `limit` absent | Defaults to `[tools.search].max_results`. |
| `limit` is `0` or exceeds `max_results` | Clamped to `[1, max_results]` (FR-011). |
| Endpoint DNS/TLS/connect failure | `ToolError::Execution`; `EgressEvent { status: None, blocked: false }`. |
| HTTP 429 (backend quota) | `ToolError::Blocked`, not retried (FR-008). |
| Malformed backend JSON | `SearchError::Parse` → `ToolError::Execution`; no partial results surfaced. |
| Endpoint fails SSRF/denylist validation | `ToolError::Blocked`; `EgressEvent { blocked: true, block_reason }` emitted before returning. |
| Backend disabled or unconstructible | Tool absent from `tool_definitions()` — no error path reachable by the LLM (FR-002). |

---

## 8. Testing Requirements

Mandatory `.local/testing/playbooks/web-search.md` scenarios (new file,
integration point):

1. Query with results — verify ranked `title`/`url`/`snippet` list, one
   `EgressEvent` emitted, `correlation_id` shared with the `AuditEntry`.
2. No API key / disabled config — `web_search` absent from the tool list
   presented to the LLM; no error surfaced mid-conversation.
3. `[tools.scrape].denied_domains` containing the search endpoint's host —
   call blocked, `EgressEvent { blocked: true, block_reason: "blocklist" }`.
4. Endpoint SSRF block (private-IP override in test config) — call blocked,
   `EgressEvent { blocked: true, block_reason: "ssrf" }`.
5. Result snippet containing an IPI payload — verify `IpiFilter` flags/redacts
   it before the LLM sees it, and the tool output is quarantined per
   INVARIANT-1 (verify via the sanitizer's `ContentSourceKind::WebScrape`
   classification, not `ClaimSource`).
6. HTTP 429 from the backend — `ToolError::Blocked`, not retried; verify
   `is_tool_retryable` distinguishes this from a genuine timeout/connection
   reset, which does retry.
7. Empty query — `InvalidParams`, zero `EgressEvent`s emitted.
8. Zero-result query — normal `ToolOutput`, not an error.
9. Cross-mode consistency (CLI/TUI) — identical `web_search` behavior and
   TUI spinner (`Searching web…`) per the TUI Rules in CLAUDE.md.

Unit tests (mandatory, in `crates/zeph-tools/src/search/`):
- `SearchBackend::from_config` — `Ok` with valid Brave key, `Err(MissingApiKey)`
  without one.
- `WebSearchExecutor::new` — `None` when disabled or backend construction
  fails; `Some` otherwise.
- Query clamping: absent/zero/over-limit `limit` values.
- 429 → `ToolError::Blocked` mapping (not `ToolError::Http`).
- Denylist-only enforcement — allowlist is provably not consulted for the
  search endpoint (regression guard for the FR-005 exemption).
- Addr-pinning: `build_client` receives the exact addrs from
  `resolve_and_validate` (mirrors the equivalent `scrape.rs` test at line
  ~1663).

Coverage row (integration point): `.local/testing/coverage-status.md` — add
`Web search tool`, `SearchProvider/Brave backend`, `web_search egress` rows,
status `Untested`.

---

## 9. Integration Points

Full verified file:line integration-point table (config, executor, sanitizer
bridge, wiring, vault, CLI, TUI, wizard, migration, playbook, coverage,
CHANGELOG) is maintained in the architecture handoff
`.local/handoff/2026-07-18T03-06-38-architect.md` (§"Integration Points",
items 1-15) — carried forward here as the developer's implementation
checklist, not duplicated verbatim to avoid drift between the two documents.
The two non-negotiable items from that table are elevated to this spec's Key
Invariants (§4, INVARIANT-1 and INVARIANT-2) because they are
security-critical and MUST NOT regress independently of the rest of the
integration-point list.

Required integration surface (per CLAUDE.md "Development Rules"):

1. `config.toml` — `[tools.search]` section (§3.7).
2. CLI — `zeph search <query> [--limit N]` subcommand running one `web_search`
   call and printing results.
3. TUI — `/search <query>` slash command with a `Searching web…` spinner
   status per the TUI Rules.
4. `--init` wizard — step prompting enable + API key, stored to vault.
5. `--migrate-config` — insert commented `[tools.search]` defaults when absent.
6. Live-testing playbook — `.local/testing/playbooks/web-search.md` (§8).
7. Coverage row — `.local/testing/coverage-status.md` (§8).
8. `CHANGELOG.md` `[Unreleased]` — feature entry noting the new
   `ClaimSource::WebSearch` audit-only variant (pre-v1, no deprecation
   window required).

---

## 10. Related Specifications

- `[[006-tools/spec]]` — parent tool-execution contract (`ToolExecutor`,
  `CompositeExecutor`, `ClaimSource`).
- `[[010-3-authorization]]` — SSRF / domain-policy enforcement this tool
  extends with a documented allowlist exemption for its own fixed endpoint.
- `[[010-5-egress-logging]]` — mandatory `EgressEvent` contract this tool
  satisfies (§3.3 step 7, §4 invariant).
- `[[040-sanitizer/spec]]` — IPI filter / content sanitizer this tool routes
  through (§3.3 step 8, INVARIANT-1).
- `[[038-vault/spec]]` — age-vault-only API key resolution contract.
- `[[024-multi-model-design/spec]]` — Multi-Model Principle compliance note (§6).
