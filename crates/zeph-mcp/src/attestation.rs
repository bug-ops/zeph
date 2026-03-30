// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use crate::manager::McpTrustLevel;
use crate::tool::McpTool;

/// Blake3 fingerprint of a tool definition (name + description + `input_schema`).
pub type ToolFingerprint = String;

/// Result of comparing actual server tools against operator-declared expectations.
#[derive(Debug, Clone)]
pub enum AttestationResult {
    /// All actual tools are in the operator-declared expected set.
    /// Fingerprints are stored for drift detection on subsequent refreshes.
    Verified {
        fingerprints: HashMap<String, ToolFingerprint>,
    },
    /// Server returned tools not declared in `expected_tools`.
    Unexpected {
        unexpected_tools: Vec<String>,
        fingerprints: HashMap<String, ToolFingerprint>,
    },
    /// No `expected_tools` declared — attestation skipped.
    Unconfigured,
}

/// Per-server trust boundary: isolates policy, effective tool list, and attestation state.
#[derive(Debug)]
pub struct ServerTrustBoundary {
    pub server_id: String,
    pub trust_level: McpTrustLevel,
    pub attestation: AttestationResult,
    /// Tool names visible after attestation filtering.
    pub effective_tools: Vec<String>,
    /// Fingerprints from the previous connection, used for drift detection on refresh.
    pub previous_fingerprints: Option<HashMap<String, ToolFingerprint>>,
}

/// Compute a blake3 fingerprint of a tool's name, description, and schema.
///
/// Uses the same field set as `registry::compute_hash` to keep fingerprints consistent.
#[must_use]
pub fn fingerprint_tool(tool: &McpTool) -> ToolFingerprint {
    let mut hasher = blake3::Hasher::new();
    hasher.update(tool.server_id.as_bytes());
    hasher.update(tool.name.as_bytes());
    hasher.update(tool.description.as_bytes());
    hasher.update(tool.input_schema.to_string().as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Compare actual tools from `list_tools()` against operator-declared `expected_tools`.
///
/// Behavior:
/// - If `expected_tools` is empty → `AttestationResult::Unconfigured`.
/// - If all tool names are in `expected_tools` → `AttestationResult::Verified`.
/// - If any tool name is outside `expected_tools` → `AttestationResult::Unexpected`.
///
/// When `previous_fingerprints` is provided, logs a warning for any tool whose
/// fingerprint has changed since the last connection (schema drift detection).
pub fn attest_tools<S: std::hash::BuildHasher>(
    tools: &[McpTool],
    expected_tools: &[String],
    previous_fingerprints: Option<&HashMap<String, ToolFingerprint, S>>,
) -> AttestationResult {
    if expected_tools.is_empty() {
        return AttestationResult::Unconfigured;
    }

    let fingerprints: HashMap<String, ToolFingerprint> = tools
        .iter()
        .map(|t| (t.name.clone(), fingerprint_tool(t)))
        .collect();

    // Detect schema drift against previous connection fingerprints.
    if let Some(prev) = previous_fingerprints {
        for (name, fp) in &fingerprints {
            if let Some(prev_fp) = prev.get(name)
                && prev_fp != fp
            {
                tracing::warn!(
                    tool = %name,
                    "MCP tool schema drift detected: fingerprint changed since last connection"
                );
            }
        }
    }

    let unexpected_tools: Vec<String> = tools
        .iter()
        .filter(|t| !expected_tools.iter().any(|e| e == &t.name))
        .map(|t| t.name.clone())
        .collect();

    if unexpected_tools.is_empty() {
        AttestationResult::Verified { fingerprints }
    } else {
        AttestationResult::Unexpected {
            unexpected_tools,
            fingerprints,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(server: &str, name: &str) -> McpTool {
        McpTool {
            server_id: server.into(),
            name: name.into(),
            description: "desc".into(),
            input_schema: serde_json::json!({}),
            security_meta: Default::default(),
        }
    }

    #[test]
    fn empty_expected_tools_returns_unconfigured() {
        let tools = vec![make_tool("srv", "read_file")];
        let result = attest_tools::<std::collections::hash_map::RandomState>(&tools, &[], None);
        assert!(matches!(result, AttestationResult::Unconfigured));
    }

    #[test]
    fn all_expected_tools_present_returns_verified() {
        let tools = vec![make_tool("srv", "read_file"), make_tool("srv", "list_dir")];
        let expected = vec!["read_file".to_owned(), "list_dir".to_owned()];
        let result =
            attest_tools::<std::collections::hash_map::RandomState>(&tools, &expected, None);
        assert!(matches!(result, AttestationResult::Verified { .. }));
    }

    #[test]
    fn subset_of_expected_tools_returns_verified() {
        // Server returns fewer tools than declared — that is fine.
        let tools = vec![make_tool("srv", "read_file")];
        let expected = vec!["read_file".to_owned(), "list_dir".to_owned()];
        let result =
            attest_tools::<std::collections::hash_map::RandomState>(&tools, &expected, None);
        assert!(matches!(result, AttestationResult::Verified { .. }));
    }

    #[test]
    fn unexpected_tool_returns_unexpected() {
        let tools = vec![make_tool("srv", "read_file"), make_tool("srv", "exec_cmd")];
        let expected = vec!["read_file".to_owned()];
        let result =
            attest_tools::<std::collections::hash_map::RandomState>(&tools, &expected, None);
        match result {
            AttestationResult::Unexpected {
                unexpected_tools, ..
            } => {
                assert_eq!(unexpected_tools, vec!["exec_cmd"]);
            }
            other => panic!("expected Unexpected, got {other:?}"),
        }
    }

    #[test]
    fn fingerprints_recorded_in_verified() {
        let tools = vec![make_tool("srv", "read_file")];
        let expected = vec!["read_file".to_owned()];
        let result =
            attest_tools::<std::collections::hash_map::RandomState>(&tools, &expected, None);
        match result {
            AttestationResult::Verified { fingerprints } => {
                assert!(fingerprints.contains_key("read_file"));
                assert!(!fingerprints["read_file"].is_empty());
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    #[test]
    fn schema_drift_detected_logs_warning() {
        // Build a tool, fingerprint it, then change its description → different fingerprint.
        let tool_v1 = make_tool("srv", "read_file");
        let fp_v1 = fingerprint_tool(&tool_v1);

        let mut tool_v2 = make_tool("srv", "read_file");
        tool_v2.description = "changed description".into();

        let mut prev = HashMap::new();
        prev.insert("read_file".to_owned(), fp_v1);

        let expected = vec!["read_file".to_owned()];
        // This should log a warning internally; we just verify it doesn't panic.
        let result = attest_tools(&[tool_v2], &expected, Some(&prev));
        assert!(matches!(result, AttestationResult::Verified { .. }));
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let tool = make_tool("srv", "read_file");
        let fp1 = fingerprint_tool(&tool);
        let fp2 = fingerprint_tool(&tool);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_differs_for_different_descriptions() {
        let mut t1 = make_tool("srv", "read_file");
        let mut t2 = make_tool("srv", "read_file");
        t1.description = "desc A".into();
        t2.description = "desc B".into();
        assert_ne!(fingerprint_tool(&t1), fingerprint_tool(&t2));
    }
}
