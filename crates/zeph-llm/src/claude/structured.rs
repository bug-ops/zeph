// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSON Schema normalization for Claude structured outputs (`output_config.format`).
//!
//! The API accepts a restricted JSON Schema dialect: every object must set
//! `additionalProperties: false`, numeric/string/array bound keywords are rejected, only a fixed
//! set of `format`s is allowed, and recursive `$ref` graphs are unsupported.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::schema::{SchemaVisitor, walk_schema};

const MAX_DEPTH: u8 = 32;

const UNSUPPORTED_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "examples",
    "default",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "uniqueItems",
    "pattern",
];

const SUPPORTED_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

const DEFINITION_KEYS: &[&str] = &["$defs", "definitions"];

struct ClaudeStructuredVisitor;

impl SchemaVisitor for ClaudeStructuredVisitor {
    fn visit(&mut self, schema: &mut Value) -> bool {
        let Some(obj) = schema.as_object_mut() else {
            return true;
        };
        for key in UNSUPPORTED_KEYWORDS {
            obj.remove(*key);
        }
        if obj
            .get("format")
            .and_then(Value::as_str)
            .is_some_and(|f| !SUPPORTED_FORMATS.contains(&f))
        {
            obj.remove("format");
        }
        let is_object = obj.get("type").and_then(Value::as_str) == Some("object")
            || obj.contains_key("properties");
        if is_object {
            obj.insert("additionalProperties".to_owned(), Value::Bool(false));
        }
        true
    }
}

/// Normalize `schema` for Claude structured outputs.
///
/// Returns `None` when the schema cannot be expressed in the supported dialect (a recursive
/// `$ref` graph), in which case the caller falls back to the tool-use path.
pub(super) fn claude_structured_schema(schema: &Value) -> Option<Value> {
    if has_recursive_ref(schema) || has_map_schema(schema) {
        return None;
    }
    let mut normalized = schema.clone();
    walk_schema(&mut normalized, &mut ClaudeStructuredVisitor, MAX_DEPTH);
    for key in DEFINITION_KEYS {
        if let Some(Value::Object(defs)) = normalized.get_mut(*key) {
            for def in defs.values_mut() {
                walk_schema(def, &mut ClaudeStructuredVisitor, MAX_DEPTH);
            }
        }
    }
    Some(normalized)
}

/// `true` when any node declares `additionalProperties` other than `false` (a map type), which
/// the structured dialect cannot express: it would be forced to an empty object.
fn has_map_schema(node: &Value) -> bool {
    match node {
        Value::Object(map) => {
            map.get("additionalProperties")
                .is_some_and(|v| *v != Value::Bool(false))
                || map.values().any(has_map_schema)
        }
        Value::Array(items) => items.iter().any(has_map_schema),
        _ => false,
    }
}

fn collect_refs(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                out.push(r.clone());
            }
            for value in map.values() {
                collect_refs(value, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

/// `true` when following `$ref`s from the root (or between definitions) can reach a cycle.
fn has_recursive_ref(schema: &Value) -> bool {
    let mut defs: HashMap<String, Vec<String>> = HashMap::new();
    for key in DEFINITION_KEYS {
        if let Some(Value::Object(map)) = schema.get(*key) {
            for (name, def) in map {
                let mut refs = Vec::new();
                collect_refs(def, &mut refs);
                defs.insert(format!("#/{key}/{name}"), refs);
            }
        }
    }
    let mut root_refs = Vec::new();
    collect_refs(schema, &mut root_refs);
    if root_refs.iter().any(|r| r == "#") {
        return true;
    }
    let mut done: HashSet<&str> = HashSet::new();
    defs.keys()
        .any(|name| reaches_cycle(name, &defs, &mut Vec::new(), &mut done))
}

fn reaches_cycle<'a>(
    node: &'a str,
    defs: &'a HashMap<String, Vec<String>>,
    path: &mut Vec<&'a str>,
    done: &mut HashSet<&'a str>,
) -> bool {
    if path.contains(&node) {
        return true;
    }
    if done.contains(node) {
        return false;
    }
    let Some(children) = defs.get(node) else {
        return false;
    };
    path.push(node);
    let cyclic = children
        .iter()
        .any(|child| reaches_cycle(child, defs, path, done));
    path.pop();
    done.insert(node);
    cyclic
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn objects_get_additional_properties_false_and_bounds_are_stripped() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "n": {"type": "integer", "minimum": 0, "maximum": 9, "default": 1},
                "s": {"type": "string", "minLength": 1, "format": "weird"},
                "d": {"type": "string", "format": "date-time"},
                "list": {"type": "array", "items": {"type": "object", "properties": {}}, "maxItems": 3}
            }
        });
        let out = claude_structured_schema(&schema).unwrap();
        assert_eq!(out["additionalProperties"], false);
        assert!(out.get("$schema").is_none());
        assert!(out["properties"]["n"].get("minimum").is_none());
        assert!(out["properties"]["n"].get("default").is_none());
        assert!(out["properties"]["s"].get("minLength").is_none());
        assert!(out["properties"]["s"].get("format").is_none());
        assert_eq!(out["properties"]["d"]["format"], "date-time");
        assert!(out["properties"]["list"].get("maxItems").is_none());
        assert_eq!(
            out["properties"]["list"]["items"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn definitions_are_normalized() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/$defs/A"}},
            "$defs": {"A": {"type": "object", "properties": {"x": {"type": "integer", "minimum": 1}}}}
        });
        let out = claude_structured_schema(&schema).unwrap();
        assert_eq!(out["$defs"]["A"]["additionalProperties"], false);
        assert!(
            out["$defs"]["A"]["properties"]["x"]
                .get("minimum")
                .is_none()
        );
    }

    #[test]
    fn recursive_ref_is_rejected() {
        let schema = json!({
            "type": "object",
            "properties": {"child": {"$ref": "#/$defs/Node"}},
            "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}}
        });
        assert!(claude_structured_schema(&schema).is_none());
    }

    #[test]
    fn mutually_recursive_refs_are_rejected() {
        let schema = json!({
            "$ref": "#/$defs/A",
            "$defs": {
                "A": {"properties": {"b": {"$ref": "#/$defs/B"}}},
                "B": {"properties": {"a": {"$ref": "#/$defs/A"}}}
            }
        });
        assert!(claude_structured_schema(&schema).is_none());
    }

    #[test]
    fn map_schema_is_rejected() {
        let schema = json!({
            "type": "object",
            "properties": {"params": {"type": "object", "additionalProperties": {"type": "string"}}}
        });
        assert!(claude_structured_schema(&schema).is_none());
        let strict = json!({"type": "object", "additionalProperties": false, "properties": {}});
        assert!(claude_structured_schema(&strict).is_some());
    }

    #[test]
    fn shared_non_recursive_ref_is_accepted() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/$defs/X"}, "b": {"$ref": "#/$defs/X"}},
            "$defs": {"X": {"type": "object", "properties": {}}}
        });
        assert!(claude_structured_schema(&schema).is_some());
    }
}
