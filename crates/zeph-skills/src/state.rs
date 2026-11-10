// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Typed skill execution state (spec 086).
//!
//! A skill may declare a small, fixed schema for its own bounded execution state in a `state:`
//! block of its SKILL.md frontmatter. While such a skill is active, the model records progress
//! by submitting patches to a built-in tool instead of re-reading the growing tool history.
//!
//! This module is the pure, synchronous layer: schema parsing ([`SkillStateSchema`]), patch
//! validation ([`StatePatch`], [`PatchError`]) and the live object ([`SkillExecutionState`]).
//! It has no dependency on `zeph-core`, so a future subagent adoption needs no redesign.
//!
//! # SKILL.md Format
//!
//! ```text
//! ---
//! name: my-skill
//! description: Does something useful.
//! state:
//!   phase:
//!     type: enum
//!     values: [listing, reporting]
//!     default: listing
//!   done:
//!     type: list
//!     items: { type: string, max_len: 200 }
//!     max_items: 50
//! ---
//! ```
//!
//! # Bounds
//!
//! Every value the model records is untrusted data. Schemas that omit a bound get
//! [`DEFAULT_MAX_LEN`] / [`DEFAULT_MAX_ITEMS`], field names must match
//! `[a-z_][a-z0-9_]{0,63}`, nesting is at most [`MAX_DEPTH`] levels, and the rendered state
//! never exceeds [`MAX_STATE_BYTES`].
//!
//! # Examples
//!
//! ```
//! use std::sync::Arc;
//! use serde_json::json;
//! use zeph_skills::state::{SkillExecutionState, parse_state};
//!
//! let yaml = "state:\n  phase:\n    type: enum\n    values: [a, b]\n    default: a\n";
//! let schema = parse_state(yaml).expect("valid schema");
//! let mut state = SkillExecutionState::new("demo", Arc::new(schema));
//!
//! let patch = state.prepare_patch(&json!({"phase": "b"})).expect("valid patch");
//! state.apply(patch);
//! assert_eq!(state.values()["phase"], json!("b"));
//!
//! assert!(state.prepare_patch(&json!({"phase": "zzz"})).is_err());
//! ```

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::extensions::{MAX_FRONTMATTER_BLOCK_BYTES, extract_block};

/// Maximum string length applied when a `string` field declares no `max_len`.
pub const DEFAULT_MAX_LEN: usize = 1024;
/// Maximum item count applied when a `list` field declares no `max_items`.
pub const DEFAULT_MAX_ITEMS: usize = 64;
/// Maximum nesting depth of a schema (top-level field = depth 1).
pub const MAX_DEPTH: usize = 4;
/// Upper bound on the rendered (escaped) size of the merged state; larger patches are rejected.
pub const MAX_STATE_BYTES: usize = 16 * 1024;
const MAX_FIELD_NAME_LEN: usize = 64;

/// Error produced while building a [`SkillStateSchema`] from a `state:` block.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The YAML block is not a valid schema document.
    #[error("invalid state block syntax: {0}")]
    Yaml(#[from] serde_norway::Error),
    /// The block declares no fields.
    #[error("state block declares no fields")]
    Empty,
    /// A field name does not match `[a-z_][a-z0-9_]{0,63}`.
    #[error("invalid field name `{0}`")]
    FieldName(String),
    /// Nesting exceeds [`MAX_DEPTH`].
    #[error("field `{0}` nests deeper than {MAX_DEPTH} levels")]
    TooDeep(String),
    /// A field declaration is inconsistent (wrong attribute for its type, bad bounds, ...).
    #[error("field `{path}`: {reason}")]
    Field {
        /// Dotted path of the offending field.
        path: String,
        /// Human-readable cause.
        reason: String,
    },
}

/// Reason a single value failed validation against its [`FieldSpec`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    /// The JSON value has the wrong type.
    #[error("expected {expected}")]
    WrongType {
        /// The JSON type that was expected.
        expected: &'static str,
    },
    /// A string exceeds its `max_len`.
    #[error("string longer than {max} characters")]
    TooLong {
        /// Declared (or default) limit.
        max: usize,
    },
    /// A list exceeds its `max_items`.
    #[error("more than {max} items")]
    TooManyItems {
        /// Declared (or default) limit.
        max: usize,
    },
    /// An integer is outside `[min, max]`.
    #[error("integer outside {min}..={max}")]
    OutOfRange {
        /// Inclusive lower bound.
        min: i64,
        /// Inclusive upper bound.
        max: i64,
    },
    /// A string is not one of the enum values.
    #[error("not one of: {}", allowed.join(", "))]
    NotInEnum {
        /// The permitted values.
        allowed: Vec<String>,
    },
    /// An object lacks a declared field.
    #[error("missing field `{0}`")]
    MissingField(String),
    /// An object carries a field that is not declared.
    #[error("unknown field `{0}`")]
    UnknownField(String),
}

/// Reason a patch was rejected. A rejected patch never changes the state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// The patch is not a JSON object.
    #[error("patch must be a JSON object keyed by state field names")]
    NotObject,
    /// The patch is an empty object: nothing to record, and it must not absorb a batch.
    #[error("patch is empty; send at least one field")]
    Empty,
    /// The patch names a top-level field the schema does not declare.
    #[error("unknown state field `{0}`")]
    UnknownField(String),
    /// A supplied value violates its field declaration.
    #[error("field `{path}`: {reason}")]
    Invalid {
        /// Path of the offending value, e.g. `audited[2].rating`.
        path: String,
        /// What is wrong with it.
        reason: ValueError,
    },
    /// Merging would push the rendered state over [`MAX_STATE_BYTES`].
    #[error("state would be {bytes} bytes, exceeding the {max} byte limit")]
    StateTooLarge {
        /// Size the merged state would have.
        bytes: usize,
        /// The enforced limit.
        max: usize,
    },
}

/// The closed set of field kinds the `state:` grammar supports.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// UTF-8 text of at most `max_len` characters.
    String {
        /// Maximum character count.
        max_len: usize,
    },
    /// Signed integer within `[min, max]`.
    Integer {
        /// Inclusive lower bound.
        min: i64,
        /// Inclusive upper bound.
        max: i64,
    },
    /// One of a fixed, non-empty set of strings.
    Enum {
        /// Permitted values.
        values: Vec<String>,
    },
    /// Homogeneous list of at most `max_items` items.
    List {
        /// Spec every item must satisfy.
        items: Box<FieldSpec>,
        /// Maximum item count.
        max_items: usize,
    },
    /// Object with a fixed set of required fields.
    Object {
        /// Declared fields, all required.
        fields: BTreeMap<String, FieldSpec>,
    },
}

/// A field declaration: its kind plus an optional default.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSpec {
    kind: FieldKind,
    default: Option<Value>,
}

impl FieldSpec {
    /// The kind of value this field holds.
    #[must_use]
    pub fn kind(&self) -> &FieldKind {
        &self.kind
    }

    /// Value a reset (`null` patch) or initial state falls back to.
    ///
    /// The declared default, else an empty list for `list`, else `None`
    /// (the field stays unset).
    fn reset_value(&self) -> Option<Value> {
        if let Some(d) = &self.default {
            return Some(d.clone());
        }
        match self.kind {
            FieldKind::List { .. } => Some(Value::Array(vec![])),
            _ => None,
        }
    }

    fn validate(&self, value: &Value) -> Result<(), (String, ValueError)> {
        self.validate_at(value, String::new())
    }

    fn validate_at(&self, value: &Value, path: String) -> Result<(), (String, ValueError)> {
        let wrong = |expected| Err((path.clone(), ValueError::WrongType { expected }));
        match &self.kind {
            FieldKind::String { max_len } => {
                let Some(s) = value.as_str() else {
                    return wrong("string");
                };
                if s.chars().count() > *max_len {
                    return Err((path, ValueError::TooLong { max: *max_len }));
                }
            }
            FieldKind::Integer { min, max } => {
                let Some(n) = value.as_i64() else {
                    return wrong("integer");
                };
                if n < *min || n > *max {
                    return Err((
                        path,
                        ValueError::OutOfRange {
                            min: *min,
                            max: *max,
                        },
                    ));
                }
            }
            FieldKind::Enum { values } => {
                let Some(s) = value.as_str() else {
                    return wrong("string");
                };
                if !values.iter().any(|v| v == s) {
                    return Err((
                        path,
                        ValueError::NotInEnum {
                            allowed: values.clone(),
                        },
                    ));
                }
            }
            FieldKind::List { items, max_items } => {
                let Some(arr) = value.as_array() else {
                    return wrong("array");
                };
                if arr.len() > *max_items {
                    return Err((path, ValueError::TooManyItems { max: *max_items }));
                }
                for (i, item) in arr.iter().enumerate() {
                    items.validate_at(item, format!("{path}[{i}]"))?;
                }
            }
            FieldKind::Object { fields } => {
                let Some(obj) = value.as_object() else {
                    return wrong("object");
                };
                if let Some(extra) = obj.keys().find(|k| !fields.contains_key(*k)) {
                    return Err((path, ValueError::UnknownField(extra.clone())));
                }
                // ASSUMPTION: every declared object field is required (no partial objects).
                for (name, spec) in fields {
                    let child = format!("{path}.{name}");
                    let Some(v) = obj.get(name) else {
                        return Err((path, ValueError::MissingField(name.clone())));
                    };
                    spec.validate_at(v, child)?;
                }
            }
        }
        Ok(())
    }

    /// Whether `value` holds a non-empty free-text string (a `string` field, never an enum).
    fn has_free_text(&self, value: &Value) -> bool {
        match (&self.kind, value) {
            (FieldKind::String { .. }, Value::String(s)) => !s.is_empty(),
            (FieldKind::List { items, .. }, Value::Array(arr)) => {
                arr.iter().any(|v| items.has_free_text(v))
            }
            (FieldKind::Object { fields }, Value::Object(obj)) => obj
                .iter()
                .any(|(k, v)| fields.get(k).is_some_and(|f| f.has_free_text(v))),
            _ => false,
        }
    }

    fn json_schema(&self) -> Value {
        match &self.kind {
            FieldKind::String { max_len } => json!({"type": "string", "maxLength": max_len}),
            FieldKind::Integer { min, max } => {
                json!({"type": "integer", "minimum": min, "maximum": max})
            }
            FieldKind::Enum { values } => json!({"type": "string", "enum": values}),
            FieldKind::List { items, max_items } => {
                json!({"type": "array", "items": items.json_schema(), "maxItems": max_items})
            }
            FieldKind::Object { fields } => {
                let props: Map<String, Value> = fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.json_schema()))
                    .collect();
                let required: Vec<&String> = fields.keys().collect();
                json!({
                    "type": "object",
                    "properties": props,
                    "required": required,
                    "additionalProperties": false,
                })
            }
        }
    }
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}.{name}")
    }
}

/// A validated, non-empty set of state fields declared by a skill.
///
/// Built only by [`parse_state`] / [`SkillStateSchema::from_yaml_block`], so every instance has
/// valid field names, bounded nesting, consistent bounds and defaults that satisfy their own
/// field declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillStateSchema {
    fields: BTreeMap<String, FieldSpec>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawKind {
    String,
    Integer,
    Enum,
    List,
    Object,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    #[serde(rename = "type")]
    kind: RawKind,
    max_len: Option<usize>,
    min: Option<i64>,
    max: Option<i64>,
    values: Option<Vec<String>>,
    items: Option<Box<RawField>>,
    max_items: Option<usize>,
    fields: Option<BTreeMap<String, RawField>>,
    default: Option<Value>,
}

fn is_valid_field_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_lowercase());
    first_ok
        && name.len() <= MAX_FIELD_NAME_LEN
        && chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn field_err(path: &str, reason: impl Into<String>) -> SchemaError {
    SchemaError::Field {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

impl RawField {
    fn unexpected_attrs(&self) -> Vec<&'static str> {
        let (len, range, vals, items, max_items, fields) = match self.kind {
            RawKind::String => (true, false, false, false, false, false),
            RawKind::Integer => (false, true, false, false, false, false),
            RawKind::Enum => (false, false, true, false, false, false),
            RawKind::List => (false, false, false, true, true, false),
            RawKind::Object => (false, false, false, false, false, true),
        };
        let mut bad = Vec::new();
        if !len && self.max_len.is_some() {
            bad.push("max_len");
        }
        if !range && (self.min.is_some() || self.max.is_some()) {
            bad.push("min/max");
        }
        if !vals && self.values.is_some() {
            bad.push("values");
        }
        if !items && self.items.is_some() {
            bad.push("items");
        }
        if !max_items && self.max_items.is_some() {
            bad.push("max_items");
        }
        if !fields && self.fields.is_some() {
            bad.push("fields");
        }
        bad
    }

    fn build(self, path: &str, depth: usize) -> Result<FieldSpec, SchemaError> {
        if depth > MAX_DEPTH {
            return Err(SchemaError::TooDeep(path.to_owned()));
        }
        if let Some(bad) = self.unexpected_attrs().first() {
            return Err(field_err(path, format!("attribute `{bad}` not valid here")));
        }
        let kind = match self.kind {
            RawKind::String => FieldKind::String {
                max_len: self.max_len.unwrap_or(DEFAULT_MAX_LEN),
            },
            RawKind::Integer => {
                let (min, max) = (self.min.unwrap_or(i64::MIN), self.max.unwrap_or(i64::MAX));
                if min > max {
                    return Err(field_err(path, "min is greater than max"));
                }
                FieldKind::Integer { min, max }
            }
            RawKind::Enum => {
                let values = self.values.unwrap_or_default();
                if values.is_empty() {
                    return Err(field_err(path, "enum requires a non-empty `values` list"));
                }
                FieldKind::Enum { values }
            }
            RawKind::List => {
                let items = self
                    .items
                    .ok_or_else(|| field_err(path, "list requires `items`"))?;
                FieldKind::List {
                    items: Box::new(items.build(&format!("{path}[]"), depth + 1)?),
                    max_items: self.max_items.unwrap_or(DEFAULT_MAX_ITEMS),
                }
            }
            RawKind::Object => FieldKind::Object {
                fields: build_fields(
                    self.fields
                        .ok_or_else(|| field_err(path, "object requires `fields`"))?,
                    path,
                    depth + 1,
                )?,
            },
        };
        let spec = FieldSpec {
            kind,
            default: self.default,
        };
        if let Some(d) = &spec.default {
            if spec.has_free_text(d) {
                return Err(field_err(
                    path,
                    "default must not carry free text (it would render as model-recorded)",
                ));
            }
            spec.validate(d)
                .map_err(|(_, e)| field_err(path, format!("default is invalid: {e}")))?;
        }
        Ok(spec)
    }
}

fn build_fields(
    raw: BTreeMap<String, RawField>,
    parent: &str,
    depth: usize,
) -> Result<BTreeMap<String, FieldSpec>, SchemaError> {
    if raw.is_empty() {
        return Err(SchemaError::Empty);
    }
    raw.into_iter()
        .map(|(name, field)| {
            if !is_valid_field_name(&name) {
                return Err(SchemaError::FieldName(name));
            }
            let path = join_path(parent, &name);
            let spec = field.build(&path, depth)?;
            Ok((name, spec))
        })
        .collect()
}

impl SkillStateSchema {
    /// Parse the (de-indented) content of a `state:` block.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError`] when the YAML is malformed or the declaration violates the
    /// grammar's invariants (empty block, bad names, excessive nesting, inconsistent bounds,
    /// invalid defaults).
    pub fn from_yaml_block(block: &str) -> Result<Self, SchemaError> {
        let _span = tracing::info_span!("skills.state.parse").entered();
        let raw: BTreeMap<String, RawField> = serde_norway::from_str(block)?;
        Ok(Self {
            fields: build_fields(raw, "", 1)?,
        })
    }

    /// Declared top-level fields in name order.
    pub fn fields(&self) -> impl Iterator<Item = (&str, &FieldSpec)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// JSON Schema of the `skill_state_patch` tool input: an object with one optional property
    /// per declared field and no additional properties.
    #[must_use]
    pub fn input_schema(&self) -> Value {
        let props: Map<String, Value> = self
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), v.json_schema()))
            .collect();
        json!({"type": "object", "properties": props, "additionalProperties": false})
    }

    fn initial_values(&self) -> BTreeMap<String, Value> {
        self.fields
            .iter()
            .filter_map(|(k, v)| v.reset_value().map(|r| (k.clone(), r)))
            .collect()
    }
}

/// Extract and parse the `state:` block of a raw SKILL.md frontmatter string.
///
/// Returns `None` when the block is absent, exceeds the 8 KiB frontmatter block cap, or fails
/// to parse (a warning is logged). Never fails skill loading.
#[must_use]
pub fn parse_state(yaml_str: &str) -> Option<SkillStateSchema> {
    let block = extract_block(yaml_str, "state")?;
    if block.len() > MAX_FRONTMATTER_BLOCK_BYTES {
        tracing::warn!("'state:' block exceeds 8 KiB, skipping");
        return None;
    }
    match SkillStateSchema::from_yaml_block(&block) {
        Ok(schema) => Some(schema),
        Err(e) => {
            tracing::warn!("failed to parse 'state:' block in SKILL.md frontmatter: {e}");
            None
        }
    }
}

/// Like [`parse_state`] but reports why a present block is invalid.
///
/// # Errors
///
/// Returns [`SchemaError`] when the block exists but is oversized or fails to parse.
pub fn parse_state_strict(yaml_str: &str) -> Result<Option<SkillStateSchema>, SchemaError> {
    let Some(block) = extract_block(yaml_str, "state") else {
        return Ok(None);
    };
    if block.len() > MAX_FRONTMATTER_BLOCK_BYTES {
        return Err(SchemaError::Field {
            path: "state".to_owned(),
            reason: "block exceeds 8 KiB".to_owned(),
        });
    }
    SkillStateSchema::from_yaml_block(&block).map(Some)
}

/// One validated change to a single top-level state field.
#[derive(Debug, Clone, PartialEq)]
enum PatchValue {
    Set(Value),
    Reset,
}

/// A schema-validated patch, obtainable only through [`SkillExecutionState::prepare_patch`].
///
/// Applying it cannot fail, so an invalid patch can never be partially merged.
#[derive(Debug, Clone, PartialEq)]
pub struct StatePatch(BTreeMap<String, PatchValue>);

impl StatePatch {
    fn validate(schema: &SkillStateSchema, raw: &Value) -> Result<Self, PatchError> {
        let _span = tracing::info_span!("skills.state.validate").entered();
        let obj = raw.as_object().ok_or(PatchError::NotObject)?;
        if obj.is_empty() {
            return Err(PatchError::Empty);
        }
        let mut changes = BTreeMap::new();
        for (key, value) in obj {
            let spec = schema
                .fields
                .get(key)
                .ok_or_else(|| PatchError::UnknownField(key.clone()))?;
            let change = if value.is_null() {
                PatchValue::Reset
            } else {
                spec.validate(value)
                    .map_err(|(sub, reason)| PatchError::Invalid {
                        path: format!("{key}{sub}"),
                        reason,
                    })?;
                PatchValue::Set(value.clone())
            };
            changes.insert(key.clone(), change);
        }
        Ok(Self(changes))
    }
}

/// Compact JSON of `values` with every tag-opening lookalike, `>` and `&` written as `\uXXXX`.
///
/// The size cap is enforced on this rendered form, which is what the model sees.
fn render_values(values: &BTreeMap<String, Value>) -> String {
    let json = serde_json::to_string(values).unwrap_or_else(|_| "{}".to_owned());
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c == '>' || c == '&' || zeph_common::delimiter::is_bracket(c) {
            let _ = write!(out, "\\u{:04x}", c as u32);
        } else {
            out.push(c);
        }
    }
    out
}

/// The live, bounded state of the active state-declaring skill. In-memory only.
#[derive(Debug, Clone)]
pub struct SkillExecutionState {
    skill: String,
    schema: Arc<SkillStateSchema>,
    values: BTreeMap<String, Value>,
}

impl SkillExecutionState {
    /// Create a state holding each field's default (or empty list).
    #[must_use]
    pub fn new(skill: impl Into<String>, schema: Arc<SkillStateSchema>) -> Self {
        let values = schema.initial_values();
        Self {
            skill: skill.into(),
            schema,
            values,
        }
    }

    /// Name of the skill that owns this state.
    #[must_use]
    pub fn skill(&self) -> &str {
        &self.skill
    }

    /// The schema used for both the tool definition and patch validation.
    #[must_use]
    pub fn schema(&self) -> &Arc<SkillStateSchema> {
        &self.schema
    }

    /// Current field values in name order.
    #[must_use]
    pub fn values(&self) -> &BTreeMap<String, Value> {
        &self.values
    }

    fn merged(&self, patch: &StatePatch) -> BTreeMap<String, Value> {
        let mut out = self.values.clone();
        for (key, change) in &patch.0 {
            let next = match change {
                PatchValue::Set(v) => Some(v.clone()),
                PatchValue::Reset => self.schema.fields.get(key).and_then(FieldSpec::reset_value),
            };
            match next {
                Some(v) => out.insert(key.clone(), v),
                None => out.remove(key),
            };
        }
        out
    }

    /// Validate `raw` against the schema and the size cap without changing the state.
    ///
    /// A shallow per-key merge is assumed: supplied list/object values replace the old ones,
    /// and `null` resets a key to its default (or empty collection, or unsets it).
    ///
    /// # Errors
    ///
    /// Returns [`PatchError`] on any violation; the whole patch is rejected.
    pub fn prepare_patch(&self, raw: &Value) -> Result<StatePatch, PatchError> {
        let patch = StatePatch::validate(&self.schema, raw)?;
        let bytes = render_values(&self.merged(&patch)).len();
        if bytes > MAX_STATE_BYTES {
            return Err(PatchError::StateTooLarge {
                bytes,
                max: MAX_STATE_BYTES,
            });
        }
        Ok(patch)
    }

    /// Merge a validated patch into the state.
    pub fn apply(&mut self, patch: StatePatch) {
        let _span = tracing::info_span!("skills.state.merge").entered();
        for (key, change) in patch.0 {
            let next = match change {
                PatchValue::Set(v) => Some(v),
                PatchValue::Reset => self
                    .schema
                    .fields
                    .get(&key)
                    .and_then(FieldSpec::reset_value),
            };
            match next {
                Some(v) => self.values.insert(key, v),
                None => self.values.remove(&key),
            };
        }
    }

    /// Compact JSON of the values with `<`, `>` and `&` written as `\uXXXX` escapes.
    ///
    /// The result is still valid JSON, and no model-supplied value can contain a literal
    /// delimiter tag.
    #[must_use]
    pub fn render_json(&self) -> String {
        render_values(&self.values)
    }

    /// The `<skill_state>` section exactly as it is shown to the model.
    ///
    /// The values are data the model itself recorded and carry `source="model"`.
    #[must_use]
    pub fn render_xml(&self) -> String {
        format!(
            "<skill_state source=\"model\" skill=\"{}\">\n{}\n</skill_state>",
            self.skill,
            self.render_json()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PILOT: &str = "\
state:
  phase:
    type: enum
    values: [listing, auditing]
    default: listing
  pending:
    type: list
    items: { type: string }
  audited:
    type: list
    items:
      type: object
      fields:
        skill:  { type: string }
        rating: { type: integer, min: 1, max: 10 }
    default: []
  note:
    type: string
    max_len: 5
";

    fn state() -> SkillExecutionState {
        SkillExecutionState::new("demo", Arc::new(parse_state(PILOT).expect("schema")))
    }

    #[test]
    fn absent_block_is_none() {
        assert!(parse_state("name: x\ndescription: y\n").is_none());
    }

    #[test]
    fn initial_values_use_defaults_and_empty_collections() {
        let s = state();
        assert_eq!(s.values()["phase"], json!("listing"));
        assert_eq!(s.values()["pending"], json!([]));
        assert!(!s.values().contains_key("note"));
    }

    #[test]
    fn oversized_block_is_none() {
        let big = format!("state:\n  a:\n    type: string\n  # {}\n", "x".repeat(9000));
        assert!(parse_state(&big).is_none());
    }

    #[test]
    fn schema_rejects_invalid_declarations() {
        for bad in [
            "a:\n  type: enum\n  values: []\n",
            "a:\n  type: integer\n  min: 5\n  max: 1\n",
            "a:\n  type: string\n  values: [x]\n",
            "A:\n  type: string\n",
            "a:\n  type: string\n  default: 3\n",
            "a:\n  type: list\n",
            "a:\n  type: object\n  fields: {}\n",
            "a:\n  type: wat\n",
        ] {
            assert!(SkillStateSchema::from_yaml_block(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn schema_rejects_excess_nesting() {
        let deep = "a:\n type: list\n items:\n  type: list\n  items:\n   type: list\n   items:\n    type: list\n    items:\n     type: string\n";
        assert!(matches!(
            SkillStateSchema::from_yaml_block(deep),
            Err(SchemaError::TooDeep(_))
        ));
    }

    #[test]
    fn valid_patch_merges() {
        let mut s = state();
        let p = s
            .prepare_patch(&json!({"phase": "auditing", "audited": [{"skill": "a", "rating": 3}]}))
            .expect("valid");
        s.apply(p);
        assert_eq!(s.values()["phase"], json!("auditing"));
        assert_eq!(s.values()["audited"][0]["rating"], json!(3));
    }

    #[test]
    fn invalid_patches_are_rejected_without_change() {
        let s = state();
        let before = s.values().clone();
        for bad in [
            json!([1]),
            json!({}),
            json!({"nope": 1}),
            json!({"phase": "zzz"}),
            json!({"note": "toolong"}),
            json!({"audited": [{"skill": "a", "rating": 11}]}),
            json!({"audited": [{"skill": "a"}]}),
            json!({"audited": [{"skill": "a", "rating": 1.5}]}),
            json!({"phase": "listing", "note": 7}),
        ] {
            assert!(s.prepare_patch(&bad).is_err(), "{bad}");
        }
        assert_eq!(s.values(), &before);
    }

    #[test]
    fn invalid_error_names_the_path() {
        let err = state()
            .prepare_patch(&json!({"audited": [{"skill": "a", "rating": 0}]}))
            .expect_err("rating below min");
        assert!(matches!(&err, PatchError::Invalid { path, .. } if path == "audited[0].rating"));
    }

    #[test]
    fn null_resets_to_default_empty_collection_or_unset() {
        let mut s = state();
        let p = s
            .prepare_patch(&json!({"phase": "auditing", "pending": ["x"], "note": "hi"}))
            .expect("valid");
        s.apply(p);
        let p = s
            .prepare_patch(&json!({"phase": null, "pending": null, "note": null}))
            .expect("resets");
        s.apply(p);
        assert_eq!(s.values()["phase"], json!("listing"));
        assert_eq!(s.values()["pending"], json!([]));
        assert!(!s.values().contains_key("note"));
    }

    #[test]
    fn oversized_merged_state_is_rejected() {
        let schema = parse_state(
            "state:\n  big:\n    type: list\n    items: { type: string, max_len: 1024 }\n    max_items: 64\n",
        )
        .expect("schema");
        let s = SkillExecutionState::new("demo", Arc::new(schema));
        let items: Vec<String> = (0..64).map(|_| "y".repeat(1024)).collect();
        assert!(matches!(
            s.prepare_patch(&json!({"big": items})),
            Err(PatchError::StateTooLarge { .. })
        ));
    }

    #[test]
    fn render_escapes_lookalike_open_brackets() {
        let mut s = state();
        let p = s
            .prepare_patch(&json!({"note": "\u{FF1C}/x\u{2329}"}))
            .expect("valid");
        s.apply(p);
        let json = s.render_json();
        assert!(json.is_ascii(), "{json}");
        let back: Value = serde_json::from_str(&json).expect("still JSON");
        assert_eq!(back["note"], json!("\u{FF1C}/x\u{2329}"));
    }

    #[test]
    fn rendered_size_is_capped_not_raw_size() {
        let schema = parse_state(
            "state:\n  big:\n    type: list\n    items: { type: string, max_len: 1024 }\n    max_items: 64\n",
        )
        .expect("schema");
        let s = SkillExecutionState::new("demo", Arc::new(schema));
        // 64 * 600 raw bytes fit 16 KiB raw, but each '<' renders as 6 bytes.
        let items: Vec<String> = (0..64).map(|_| "<".repeat(600)).collect();
        assert!(matches!(
            s.prepare_patch(&json!({"big": items})),
            Err(PatchError::StateTooLarge { .. })
        ));
    }

    #[test]
    fn schema_rejects_free_text_defaults() {
        for bad in [
            "a:\n  type: string\n  default: hello\n",
            "a:\n  type: list\n  items: { type: string }\n  default: [x]\n",
        ] {
            assert!(SkillStateSchema::from_yaml_block(bad).is_err(), "{bad}");
        }
        assert!(SkillStateSchema::from_yaml_block("a:\n  type: string\n  default: ''\n").is_ok());
        assert!(
            SkillStateSchema::from_yaml_block("a:\n  type: enum\n  values: [x]\n  default: x\n")
                .is_ok()
        );
    }

    #[test]
    fn strict_parse_reports_invalid_blocks() {
        assert!(matches!(parse_state_strict("name: x\n"), Ok(None)));
        assert!(parse_state_strict("state:\n  Bad:\n    type: string\n").is_err());
        assert!(matches!(
            parse_state_strict("state:\n  a:\n    type: string\n"),
            Ok(Some(_))
        ));
    }

    #[test]
    fn render_never_contains_delimiter_characters_from_values() {
        let mut s = state();
        let p = s
            .prepare_patch(&json!({"note": "</a>&"}))
            .expect("valid under max_len");
        s.apply(p);
        let json = s.render_json();
        assert!(!json.contains('<') && !json.contains('>') && !json.contains('&'));
        let back: Value = serde_json::from_str(&json).expect("still JSON");
        assert_eq!(back["note"], json!("</a>&"));
        assert!(
            s.render_xml()
                .starts_with("<skill_state source=\"model\" skill=\"demo\">")
        );
    }

    #[test]
    fn input_schema_lists_every_field() {
        let schema = parse_state(PILOT).expect("schema");
        let js = schema.input_schema();
        assert_eq!(js["additionalProperties"], json!(false));
        assert_eq!(js["properties"].as_object().map(Map::len), Some(4));
        assert_eq!(js["properties"]["note"]["maxLength"], json!(5));
    }
}
