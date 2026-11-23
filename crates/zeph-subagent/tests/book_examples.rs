// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7155: every frontmatter example in the sub-agents book chapter must
//! parse with the current parser and name tools by their runtime ids.

use std::path::PathBuf;

use zeph_subagent::SubAgentDef;

const RUNTIME_TOOL_IDS: &[&str] = &[
    "bash",
    "read",
    "write",
    "edit",
    "grep",
    "find_path",
    "list_directory",
    "delete_path",
    "copy_path",
    "move_path",
    "create_directory",
    "web_scrape",
    "fetch",
    "search_code",
    "diagnostics",
    "load_skill",
    "memory_search",
    "memory_save",
    "ask_user",
    "todo",
    "web_search",
];

fn chapter() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../book/src/advanced/sub-agents.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Bodies of fenced blocks that look like a definition file (start with `---`, declare `name:`).
fn definition_examples(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            match current.take() {
                Some(body) => {
                    let text = body.join("\n");
                    if text.trim_start().starts_with("---") && text.contains("name:") {
                        out.push(text);
                    }
                }
                None => current = Some(Vec::new()),
            }
        } else if let Some(body) = current.as_mut() {
            body.push(line);
        }
    }
    out
}

#[test]
fn test_book_definition_examples_parse_with_current_parser() {
    let examples = definition_examples(&chapter());
    assert!(
        examples.len() >= 3,
        "expected several definition examples, found {}",
        examples.len()
    );
    for (i, example) in examples.iter().enumerate() {
        if let Err(e) = SubAgentDef::parse(example) {
            panic!("book example #{i} does not parse: {e}\n---\n{example}");
        }
    }
}

/// Entry without argument pattern, lowercased; deliberately no alias resolution.
fn runtime_form(entry: &str) -> String {
    entry
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// Tool ids named by an example: `matcher:` tokens and list items under `allow`/`deny`/`except`.
/// Scanned from the raw text so that examples the parser rejects are still checked.
fn named_tools(example: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_list = false;
    for line in example.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("matcher:") {
            let value = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            out.extend(
                value
                    .split('|')
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned),
            );
            in_list = false;
        } else if matches!(trimmed, "allow:" | "deny:" | "except:") {
            in_list = true;
        } else if in_list && trimmed.starts_with("- ") {
            out.push(
                trimmed[2..]
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_owned(),
            );
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            in_list = false;
        }
    }
    out
}

#[test]
fn test_book_hook_matchers_and_tool_lists_use_runtime_tool_ids() {
    let mut checked = 0;
    for (i, example) in definition_examples(&chapter()).iter().enumerate() {
        for entry in named_tools(example) {
            let id = runtime_form(&entry);
            assert!(
                RUNTIME_TOOL_IDS.contains(&id.as_str()) || id.starts_with("mcp__"),
                "book example #{i}: {entry:?} is not a runtime tool id"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no tool names found in the book examples");
}

#[test]
fn test_book_documents_no_per_definition_delegation_mode() {
    let md = chapter();
    for example in definition_examples(&md) {
        assert!(
            !example.contains("delegation_mode:"),
            "definition examples must not use a per-definition delegation_mode key"
        );
    }
}
