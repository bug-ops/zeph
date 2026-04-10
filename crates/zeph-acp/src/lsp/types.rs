// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lightweight LSP-compatible types for ACP extension methods.
//!
//! Positions use 1-based coordinates throughout (ACP/MCP convention).
//! The ACP client (IDE) is responsible for converting to 0-based LSP coordinates.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 1-based file position (line and character offset).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LspPosition {
    /// 1-based line number.
    pub line: u32,
    /// 1-based character offset.
    pub character: u32,
}

/// A contiguous range in a document defined by two 1-based positions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LspRange {
    /// Inclusive start of the range.
    pub start: LspPosition,
    /// Exclusive end of the range.
    pub end: LspPosition,
}

/// A location inside a resource (file URI + range).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspLocation {
    /// File URI, e.g. `file:///path/to/file.rs`.
    pub uri: String,
    pub range: LspRange,
}

/// Diagnostic severity values as defined by the LSP specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum LspDiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Info = 3,
    Hint = 4,
}

impl From<LspDiagnosticSeverity> for u8 {
    fn from(s: LspDiagnosticSeverity) -> u8 {
        s as u8
    }
}

impl TryFrom<u8> for LspDiagnosticSeverity {
    type Error = String;

    fn try_from(v: u8) -> Result<Self, String> {
        match v {
            1 => Ok(Self::Error),
            2 => Ok(Self::Warning),
            3 => Ok(Self::Info),
            4 => Ok(Self::Hint),
            _ => Err(format!("unknown diagnostic severity: {v}")),
        }
    }
}

impl std::fmt::Display for LspDiagnosticSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error => write!(f, "error"),
            Self::Warning => write!(f, "warning"),
            Self::Info => write!(f, "info"),
            Self::Hint => write!(f, "hint"),
        }
    }
}

/// A compiler or linter diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspDiagnostic {
    pub range: LspRange,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<LspDiagnosticSeverity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Diagnostic source, e.g. `"rust-analyzer"` or `"clippy"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
}

/// LSP `SymbolKind` values (matches the LSP specification integer encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum LspSymbolKind {
    File = 1,
    Module = 2,
    Namespace = 3,
    Package = 4,
    Class = 5,
    Method = 6,
    Property = 7,
    Field = 8,
    Constructor = 9,
    Enum = 10,
    Interface = 11,
    Function = 12,
    Variable = 13,
    Constant = 14,
    String = 15,
    Number = 16,
    Boolean = 17,
    Array = 18,
    Object = 19,
    Key = 20,
    Null = 21,
    EnumMember = 22,
    Struct = 23,
    Event = 24,
    Operator = 25,
    TypeParameter = 26,
}

impl From<LspSymbolKind> for u8 {
    fn from(k: LspSymbolKind) -> u8 {
        k as u8
    }
}

impl TryFrom<u8> for LspSymbolKind {
    type Error = String;

    fn try_from(v: u8) -> Result<Self, String> {
        match v {
            1 => Ok(Self::File),
            2 => Ok(Self::Module),
            3 => Ok(Self::Namespace),
            4 => Ok(Self::Package),
            5 => Ok(Self::Class),
            6 => Ok(Self::Method),
            7 => Ok(Self::Property),
            8 => Ok(Self::Field),
            9 => Ok(Self::Constructor),
            10 => Ok(Self::Enum),
            11 => Ok(Self::Interface),
            12 => Ok(Self::Function),
            13 => Ok(Self::Variable),
            14 => Ok(Self::Constant),
            15 => Ok(Self::String),
            16 => Ok(Self::Number),
            17 => Ok(Self::Boolean),
            18 => Ok(Self::Array),
            19 => Ok(Self::Object),
            20 => Ok(Self::Key),
            21 => Ok(Self::Null),
            22 => Ok(Self::EnumMember),
            23 => Ok(Self::Struct),
            24 => Ok(Self::Event),
            25 => Ok(Self::Operator),
            26 => Ok(Self::TypeParameter),
            _ => Err(format!("unknown symbol kind: {v}")),
        }
    }
}

/// Hierarchical document symbol returned by `lsp/documentSymbols`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspDocumentSymbol {
    /// Symbol name (e.g. function or struct name).
    pub name: String,
    /// Symbol kind classification.
    pub kind: LspSymbolKind,
    /// Range of the full symbol definition.
    pub range: LspRange,
    /// Range of the symbol's name token (used for highlights).
    pub selection_range: LspRange,
    /// Nested children (e.g. methods inside a struct).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<LspDocumentSymbol>,
}

/// Flat symbol information returned by `lsp/workspaceSymbol`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspSymbolInformation {
    /// Symbol name.
    pub name: String,
    /// Symbol kind classification.
    pub kind: LspSymbolKind,
    /// File and range where the symbol is defined.
    pub location: LspLocation,
}

/// A single text replacement in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspTextEdit {
    /// Range to replace (may be empty for pure insertions).
    pub range: LspRange,
    /// Replacement text.
    pub new_text: String,
}

/// A workspace-wide set of text edits (file URI → list of edits).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspWorkspaceEdit {
    /// Map of file URI to list of text edits to apply.
    pub changes: HashMap<String, Vec<LspTextEdit>>,
}

/// A code action (quick fix or refactor) returned by `lsp/codeActions`.
///
/// Actions without a workspace edit are filtered out on the agent side so that
/// only directly apply-able actions are exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspCodeAction {
    /// Human-readable title shown in the IDE UI.
    pub title: String,
    /// Action kind (e.g. `"quickfix"`, `"refactor.extract"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// When `true`, this is the preferred action for its associated diagnostic.
    #[serde(default)]
    pub is_preferred: bool,
    /// Diagnostics this action addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<LspDiagnostic>,
    /// Workspace edit to apply. `None` values are filtered out on the agent side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<LspWorkspaceEdit>,
}

/// Hover result returned by `lsp/hover`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspHoverResult {
    /// Markdown-formatted hover content (type signature, documentation).
    pub contents: String,
    /// Optional range that the hover applies to (used by IDEs to highlight the hovered token).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<LspRange>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_position_round_trip() {
        let pos = LspPosition {
            line: 1,
            character: 5,
        };
        let json = serde_json::to_string(&pos).unwrap();
        let decoded: LspPosition = serde_json::from_str(&json).unwrap();
        assert_eq!(pos, decoded);
    }

    #[test]
    fn lsp_range_round_trip() {
        let range = LspRange {
            start: LspPosition {
                line: 1,
                character: 0,
            },
            end: LspPosition {
                line: 3,
                character: 10,
            },
        };
        let json = serde_json::to_string(&range).unwrap();
        let decoded: LspRange = serde_json::from_str(&json).unwrap();
        assert_eq!(range, decoded);
    }

    #[test]
    fn lsp_diagnostic_severity_round_trip() {
        for sev in [
            LspDiagnosticSeverity::Error,
            LspDiagnosticSeverity::Warning,
            LspDiagnosticSeverity::Info,
            LspDiagnosticSeverity::Hint,
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            let decoded: LspDiagnosticSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(sev, decoded);
        }
    }

    #[test]
    fn lsp_diagnostic_severity_invalid_value() {
        let result: Result<LspDiagnosticSeverity, _> = serde_json::from_str("0");
        assert!(result.is_err());
        let result: Result<LspDiagnosticSeverity, _> = serde_json::from_str("5");
        assert!(result.is_err());
    }

    #[test]
    fn lsp_symbol_kind_round_trip() {
        for kind in [
            LspSymbolKind::Function,
            LspSymbolKind::Struct,
            LspSymbolKind::TypeParameter,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let decoded: LspSymbolKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, decoded);
        }
    }

    #[test]
    fn lsp_symbol_kind_invalid() {
        let result: Result<LspSymbolKind, _> = serde_json::from_str("0");
        assert!(result.is_err());
        let result: Result<LspSymbolKind, _> = serde_json::from_str("27");
        assert!(result.is_err());
    }

    #[test]
    fn lsp_diagnostic_round_trip() {
        let diag = LspDiagnostic {
            range: LspRange {
                start: LspPosition {
                    line: 10,
                    character: 0,
                },
                end: LspPosition {
                    line: 10,
                    character: 20,
                },
            },
            severity: Some(LspDiagnosticSeverity::Error),
            code: Some("E0001".to_owned()),
            source: Some("rust-analyzer".to_owned()),
            message: "type mismatch".to_owned(),
        };
        let json = serde_json::to_string(&diag).unwrap();
        let decoded: LspDiagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.message, "type mismatch");
        assert_eq!(decoded.severity, Some(LspDiagnosticSeverity::Error));
    }

    #[test]
    fn lsp_code_action_without_edit_serializes() {
        let action = LspCodeAction {
            title: "Remove unused import".to_owned(),
            kind: Some("quickfix".to_owned()),
            is_preferred: true,
            diagnostics: vec![],
            edit: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(!json.contains("\"edit\""));
    }

    #[test]
    fn lsp_workspace_edit_round_trip() {
        let mut changes = HashMap::new();
        changes.insert(
            "file:///src/main.rs".to_owned(),
            vec![LspTextEdit {
                range: LspRange {
                    start: LspPosition {
                        line: 1,
                        character: 0,
                    },
                    end: LspPosition {
                        line: 1,
                        character: 5,
                    },
                },
                new_text: "hello".to_owned(),
            }],
        );
        let edit = LspWorkspaceEdit { changes };
        let json = serde_json::to_string(&edit).unwrap();
        let decoded: LspWorkspaceEdit = serde_json::from_str(&json).unwrap();
        assert!(decoded.changes.contains_key("file:///src/main.rs"));
    }

    #[test]
    fn lsp_hover_result_round_trip() {
        let hover = LspHoverResult {
            contents: "**fn** foo() -> i32".to_owned(),
            range: Some(LspRange {
                start: LspPosition {
                    line: 5,
                    character: 4,
                },
                end: LspPosition {
                    line: 5,
                    character: 7,
                },
            }),
        };
        let json = serde_json::to_string(&hover).unwrap();
        let decoded: LspHoverResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.contents, hover.contents);
    }

    #[test]
    fn lsp_document_symbol_round_trip() {
        let sym = LspDocumentSymbol {
            name: "my_fn".to_owned(),
            kind: LspSymbolKind::Function,
            range: LspRange {
                start: LspPosition {
                    line: 1,
                    character: 0,
                },
                end: LspPosition {
                    line: 5,
                    character: 1,
                },
            },
            selection_range: LspRange {
                start: LspPosition {
                    line: 1,
                    character: 3,
                },
                end: LspPosition {
                    line: 1,
                    character: 8,
                },
            },
            children: vec![],
        };
        let json = serde_json::to_string(&sym).unwrap();
        let decoded: LspDocumentSymbol = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, "my_fn");
        // children should be omitted when empty
        assert!(!json.contains("\"children\""));
    }
}
