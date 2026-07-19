// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool ingestion pipeline: sanitization, attestation, allowlist filtering, and
//! trust-score penalty application.

use std::collections::HashMap;
use std::sync::Arc;

use crate::attestation::ToolFingerprint;
use crate::policy::check_data_flow;
use crate::sanitize::{SanitizeResult, sanitize_tools};
use crate::tool::{McpTool, ToolSecurityMeta, infer_security_meta};
use crate::trust_score::TrustScoreStore;

use super::{
    IngestConfig, MAX_INJECTION_PENALTIES_PER_REGISTRATION, McpTrustLevel, ServerTrust, StatusTx,
};

/// Apply trust score penalties for injection patterns detected during sanitization.
///
/// Calls `load_and_apply_delta()` in a loop capped at `MAX_INJECTION_PENALTIES_PER_REGISTRATION`
/// to bound the per-registration penalty even when many tools are flagged.
///
/// After applying penalties, loads the updated score and demotes the server's runtime
/// trust level when `recommended_trust_level()` is more restrictive than the current
/// level (as measured by `restriction_level()`). Auto-promotion never happens.
#[tracing::instrument(name = "mcp.manager.apply_injection_penalties", skip_all, fields(server_id = %server_id))]
pub(super) async fn apply_injection_penalties(
    trust_store: Option<&Arc<TrustScoreStore>>,
    server_id: &str,
    result: &SanitizeResult,
    server_trust: &ServerTrust,
) {
    if result.injection_count == 0 {
        return;
    }
    let Some(store) = trust_store else { return };

    let penalty_count = result
        .injection_count
        .min(MAX_INJECTION_PENALTIES_PER_REGISTRATION);
    for _ in 0..penalty_count {
        let _ = store
            .load_and_apply_delta(
                server_id,
                -crate::trust_score::ServerTrustScore::INJECTION_PENALTY,
                0,
                1,
            )
            .await;
    }

    // After penalties, check whether the updated score recommends a more restrictive
    // trust level and demote the server's runtime trust if so. Never auto-promote.
    if let Ok(Some(score)) = store.load(server_id).await {
        let recommended = score.recommended_trust_level();
        let mut guard = server_trust.write().await;
        if let Some(entry) = guard.get_mut(server_id) {
            let current = entry.0;
            if recommended.restriction_level() > current.restriction_level() {
                tracing::warn!(
                    server_id = server_id,
                    old_trust = ?current,
                    new_trust = ?recommended,
                    "demoting server trust level due to injection penalties"
                );
                entry.0 = recommended;
            }
        }
    }

    tracing::warn!(
        server_id = server_id,
        injection_count = result.injection_count,
        flagged_tools = ?result.flagged_tools,
        flagged_patterns = ?result.flagged_patterns,
        event_type = "registration_injection",
        "injection patterns detected in MCP tool definitions"
    );

    // Apply additional penalties for High-severity cross-tool references (cross-ref + injection).
    let high_cross_refs: usize = result
        .cross_references
        .iter()
        .filter(|r| r.severity == crate::sanitize::CrossRefSeverity::High)
        .count();
    for _ in 0..high_cross_refs.min(MAX_INJECTION_PENALTIES_PER_REGISTRATION) {
        let _ = store
            .load_and_apply_delta(
                server_id,
                -crate::trust_score::ServerTrustScore::INJECTION_PENALTY,
                0,
                1,
            )
            .await;
    }
}

/// Run the full tool-ingest pipeline: sanitize → assign security metadata →
/// enforce data-flow policy → attest → apply allowlist filtering.
///
/// # Security invariant
///
/// Sanitization always runs first, before any filtering or storage, to prevent
/// prompt-injection content from reaching the registry even for trusted servers.
pub(super) fn ingest_tools(
    mut tools: Vec<McpTool>,
    cfg: &IngestConfig<'_>,
) -> (
    Vec<McpTool>,
    SanitizeResult,
    Option<HashMap<String, ToolFingerprint>>,
) {
    // SECURITY INVARIANT: sanitize BEFORE any filtering or storage.
    let sanitize_result = sanitize_tools(&mut tools, cfg.server_id, cfg.max_description_bytes);
    assign_security_metadata(&mut tools, cfg.tool_metadata);
    filter_data_flow_violations(&mut tools, cfg.server_id, cfg.trust_level);
    let (tools, fingerprints) = apply_attestation(
        tools,
        cfg.server_id,
        cfg.trust_level,
        cfg.expected_tools,
        cfg.previous_fingerprints,
    );
    let filtered = apply_allowlist(
        tools,
        cfg.server_id,
        cfg.trust_level,
        cfg.allowlist,
        cfg.allow_untrusted_without_allowlist,
        cfg.status_tx,
    );
    (filtered, sanitize_result, fingerprints)
}

/// Assign per-tool security metadata from operator config or heuristic inference.
fn assign_security_metadata(
    tools: &mut [McpTool],
    tool_metadata: &HashMap<String, ToolSecurityMeta>,
) {
    for tool in tools.iter_mut() {
        tool.security_meta = tool_metadata
            .get(&tool.name)
            .cloned()
            .unwrap_or_else(|| infer_security_meta(&tool.name));
    }
}

/// Remove tools that violate data-flow sensitivity/trust constraints.
fn filter_data_flow_violations(
    tools: &mut Vec<McpTool>,
    server_id: &str,
    trust_level: McpTrustLevel,
) {
    tools.retain(|tool| match check_data_flow(tool, trust_level) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                server_id = server_id,
                tool_name = %tool.name,
                event_type = "data_flow_violation",
                "{e}"
            );
            false
        }
    });
}

/// Compare tools against operator-declared expectations and filter unexpected
/// tools from Untrusted/Sandboxed servers.
///
/// `previous_fingerprints` — when `Some`, carries the tool fingerprints computed on this
/// server's previous connection so [`attest_tools`] can detect schema drift (a tool
/// description/schema that silently changed between sessions, aka an "MCP rug-pull").
///
/// Returns the (possibly filtered) tools plus the fingerprints computed for *this*
/// connection, which the caller must cache for the next reconnect/refresh. `None` when
/// attestation is unconfigured (no `expected_tools` declared for this server) — fingerprints
/// are only computed as part of attestation, so drift detection requires `expected_tools`.
fn apply_attestation(
    tools: Vec<McpTool>,
    server_id: &str,
    trust_level: McpTrustLevel,
    expected_tools: &[String],
    previous_fingerprints: Option<&HashMap<String, ToolFingerprint>>,
) -> (Vec<McpTool>, Option<HashMap<String, ToolFingerprint>>) {
    use crate::attestation::{AttestationResult, attest_tools};

    let attestation = attest_tools::<std::collections::hash_map::RandomState>(
        &tools,
        expected_tools,
        previous_fingerprints,
    );
    match attestation {
        AttestationResult::Unconfigured => (tools, None),
        AttestationResult::Verified { fingerprints } => {
            tracing::debug!(server_id, "attestation: all tools in expected set");
            (tools, Some(fingerprints))
        }
        AttestationResult::Unexpected {
            unexpected_tools,
            fingerprints,
        } => {
            let unexpected_names = unexpected_tools.join(", ");
            let tools = match trust_level {
                McpTrustLevel::Trusted => {
                    tracing::warn!(
                        server_id,
                        unexpected = %unexpected_names,
                        "attestation: unexpected tools from Trusted server"
                    );
                    tools
                }
                McpTrustLevel::Untrusted | McpTrustLevel::Sandboxed => {
                    tracing::warn!(
                        server_id,
                        unexpected = %unexpected_names,
                        "attestation: filtering unexpected tools from Untrusted/Sandboxed server"
                    );
                    tools
                        .into_iter()
                        .filter(|t| expected_tools.iter().any(|e| e == &t.name))
                        .collect()
                }
                _ => tools,
            };
            (tools, Some(fingerprints))
        }
    }
}

/// Enforce the trust-level allowlist policy and return the permitted tool set.
///
/// # Security invariant
///
/// `Untrusted` servers fail **closed** (zero tools) when no `tool_allowlist` is declared,
/// unless `allow_untrusted_without_allowlist` is explicitly `true` — see
/// [`ServerEntry::allow_untrusted_without_allowlist`](super::ServerEntry::allow_untrusted_without_allowlist).
/// Only `Trusted` exposes all tools unconditionally. Any trust level not explicitly matched
/// here (including future `#[non_exhaustive]` variants) also fails closed, so a new variant
/// is secure-by-default rather than silently exposing every tool.
fn apply_allowlist(
    tools: Vec<McpTool>,
    server_id: &str,
    trust_level: McpTrustLevel,
    allowlist: Option<&[String]>,
    allow_untrusted_without_allowlist: bool,
    status_tx: Option<&StatusTx>,
) -> Vec<McpTool> {
    match trust_level {
        McpTrustLevel::Untrusted => match allowlist {
            None if allow_untrusted_without_allowlist => {
                let msg = format!(
                    "MCP server '{}' is untrusted with no tool_allowlist — all {} tools exposed \
                     because allow_untrusted_without_allowlist=true; consider adding an \
                     explicit allowlist",
                    server_id,
                    tools.len()
                );
                tracing::warn!(server_id, tool_count = tools.len(), "{msg}");
                if let Some(tx) = status_tx {
                    let _ = tx.send(msg);
                }
                tools
            }
            None => {
                let msg = format!(
                    "MCP server '{server_id}' is untrusted with no tool_allowlist — 0 tools \
                     exposed (fail-closed); declare a tool_allowlist or set \
                     allow_untrusted_without_allowlist=true to expose tools"
                );
                tracing::warn!(server_id, "{msg}");
                if let Some(tx) = status_tx {
                    let _ = tx.send(msg);
                }
                Vec::new()
            }
            Some([]) => {
                tracing::warn!(
                    server_id,
                    "untrusted MCP server has empty tool_allowlist — \
                     no tools exposed (fail-closed)"
                );
                Vec::new()
            }
            Some(list) => {
                let filtered: Vec<McpTool> = tools
                    .into_iter()
                    .filter(|t| list.iter().any(|a| a == &t.name))
                    .collect();
                tracing::info!(
                    server_id,
                    total = filtered.len(),
                    "untrusted server: filtered tools by allowlist"
                );
                filtered
            }
        },
        McpTrustLevel::Sandboxed => {
            let list = allowlist.unwrap_or(&[]);
            if list.is_empty() {
                tracing::warn!(
                    server_id,
                    "sandboxed MCP server has empty tool_allowlist — \
                     no tools exposed (fail-closed)"
                );
                Vec::new()
            } else {
                let filtered: Vec<McpTool> = tools
                    .into_iter()
                    .filter(|t| list.iter().any(|a| a == &t.name))
                    .collect();
                tracing::info!(
                    server_id,
                    total = filtered.len(),
                    "sandboxed server: filtered tools by allowlist"
                );
                filtered
            }
        }
        McpTrustLevel::Trusted => tools,
        _ => {
            tracing::warn!(
                server_id,
                trust_level = ?trust_level,
                "unrecognized MCP trust level — no tools exposed (fail-closed)"
            );
            Vec::new()
        }
    }
}
