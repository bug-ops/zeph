// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared JSON Schema traversal utilities for LLM provider normalization.
//!
//! Each provider normalizes tool parameter schemas differently (`OpenAI` strict mode,
//! Gemini API constraints). This module provides the shared recursive walk so that
//! provider-specific visitors only need to implement their transformation logic.

/// Visitor for JSON Schema nodes.
///
/// `visit` is called for each node in the schema tree. The visitor may mutate
/// or replace the node entirely (e.g., to unwrap `anyOf` Option patterns).
///
/// Return `true` to recurse into child nodes after visiting, `false` to stop.
pub(crate) trait SchemaVisitor {
    fn visit(&mut self, schema: &mut serde_json::Value) -> bool;
}

/// Walk a JSON Schema tree, calling `visitor.visit` for each node.
///
/// The visitor is called before recursion (pre-order). If `visit` returns `false`,
/// recursion into that node's children is skipped.
///
/// Recurses into `properties`, `items`, `anyOf`, `oneOf`, and `allOf`.
/// The `depth` parameter guards against infinite recursion from circular references.
pub(crate) fn walk_schema(
    schema: &mut serde_json::Value,
    visitor: &mut dyn SchemaVisitor,
    depth: u8,
) {
    if depth == 0 {
        return;
    }
    if !visitor.visit(schema) {
        return;
    }

    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    let prop_keys: Vec<String> = obj
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();

    for key in prop_keys {
        if let Some(serde_json::Value::Object(props)) = obj.get_mut("properties")
            && let Some(child) = props.get_mut(&key)
        {
            walk_schema(child, visitor, depth - 1);
        }
    }

    if let Some(items) = obj.get_mut("items") {
        walk_schema(items, visitor, depth - 1);
    }

    for keyword in &["anyOf", "oneOf", "allOf"] {
        if let Some(serde_json::Value::Array(variants)) = obj.get_mut(*keyword) {
            for v in variants.iter_mut() {
                walk_schema(v, visitor, depth - 1);
            }
        }
    }
}
