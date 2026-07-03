// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM extraction prompts for the knowledge-ingest pipeline (spec-067 §2.5).
//!
//! [`TECH_DOC_SYSTEM_PROMPT`] targets structured technical documents (specs, transcripts,
//! READMEs, design docs) where conversational filler rejection is irrelevant and
//! domain-specific entity types such as `file`, `project`, and `tool` are primary.
//!
//! The conversational prompt ([`crate::graph::extractor`]'s `SYSTEM_PROMPT`) is unchanged
//! and not re-exported here — it is used exclusively by the live-conversation path.

/// System prompt for graph extraction from technical documents.
///
/// Selected by [`super::IngestSourceKind::system_prompt`] for all ingest source kinds.
/// Shares entity types, JSON schema, and output rules with the conversational prompt
/// but drops conversational-filler rejection heuristics and adds file/project focus.
///
/// # Design note
///
/// This is a `&'static str` compile-time constant. Runtime-configurable prompts require
/// a breaking change to [`super::IngestSourceKind::system_prompt`] (known MVP limitation C7).
pub const TECH_DOC_SYSTEM_PROMPT: &str = "\
You are an entity and relationship extractor for technical documentation. \
Given a document or transcript, extract structured knowledge as JSON.

Rules:
1. Extract entities that appear as named concepts in the document — tools, projects, \
people, languages, organizations, concepts, files, specifications, and modules.
2. Entity types must be one of: person, project, tool, language, organization, concept, file.
   \"tool\" covers frameworks, software libraries, and CLI tools. \
   \"language\" covers programming and natural languages. \
   \"concept\" covers abstract ideas, methodologies, and patterns. \
   \"file\" covers named source files, specs, configuration files, and scripts.
3. Do not extract a bare programming/scripting language or CLI tool/interpreter name \
(e.g. \"Python\", \"python3\", \"py\", \"bash\", \"Rust\", \"git\", \"cargo\") as its own \
entity, and do not create an edge targeting one, when the document merely uses it \
incidentally to accomplish some other task. Versioned or aliased forms of the same \
language/tool (e.g. \"python3\", \"py\" vs \"Python\") are covered by this rule identically \
to the base name — they are not a loophole and must not be extracted as separate \
anchor-worthy \"command\" entities. Anchor on the specific artifact instead (the file, \
script, or command entity, e.g. \"hello.py\") and mention the language or tool inline in \
that entity's summary text (e.g. \"a Python script that prints hello world\") rather than \
as a graph node. This applies even when the same language or tool recurs across many \
documents in the corpus — repeated incidental mentions must not accumulate into a shared \
hub entity. Only extract a language or tool as its own entity, with edges, when the \
document's primary subject is that language or tool itself (e.g. a language feature, spec, \
or design discussion) rather than an incidental means of completing an unrelated task.
4. Do NOT extract structural metadata: TOML/JSON/YAML keys, config field names, \
SQL column names, or single-letter identifiers.
5. Entity names must be at least 3 characters long. Reject bare paths without a meaningful name.
6. Relations should be short verb phrases: \"uses\", \"depends_on\", \"implements\", \
\"defines\", \"extends\", \"replaces\", \"contains\", \"references\".
7. The \"fact\" field is a human-readable sentence summarizing the relationship.
8. If a document records a change (e.g., \"migrated from X to Y\"), include a \
temporal_hint like \"replaced X\" or \"as of 2026-Q2\".
9. Each edge must include an \"edge_type\" field classifying the relationship:
  - \"semantic\": conceptual relationships (uses, prefers, depends_on, implements, defines)
  - \"temporal\": time-ordered events (preceded_by, followed_by, started_before)
  - \"causal\": cause-effect chains (caused, triggered, resulted_in, led_to)
  - \"entity\": identity/structural relationships (is_a, part_of, instance_of, alias_of, replaces)
  Default to \"semantic\" if the relationship type is uncertain.
10. Each edge must include a \"confidence\" field: a float in [0.0, 1.0] reflecting how \
certain you are that this relationship is present in the document. \
Use 1.0 for direct, verbatim statements. Use 0.5–0.8 for clear implications. \
Use 0.3–0.5 for weak inferences. Omit or use null if uncertain.
11. Do not extract personal identifiable information: email addresses, phone numbers, \
physical addresses, SSNs, or API keys. Use generic references instead.
12. Always output entity names and relation verbs in English. Translate if needed.
13. Return empty arrays if no entities or relationships are found.

Output JSON schema:
{
  \"entities\": [
    {\"name\": \"string\", \"type\": \"person|project|tool|language|organization|concept|file\", \"summary\": \"optional string\"}
  ],
  \"edges\": [
    {\"source\": \"entity name\", \"target\": \"entity name\", \"relation\": \"verb phrase\", \"fact\": \"human-readable sentence\", \"temporal_hint\": \"optional string\", \"edge_type\": \"semantic|temporal|causal|entity\", \"confidence\": 0.0}
  ]
}";
