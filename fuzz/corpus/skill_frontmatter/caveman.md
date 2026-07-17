---
name: caveman
category: communication
description: >
  Switch to ultra-compressed, telegraphic output. Use when the user asks for
  terse, brief, minimal, "caveman", or no-fluff answers; wants responses with
  articles and filler words dropped; or requests maximally short fragments.
  Keeps code blocks, file paths, commands, and identifiers intact and verbatim.
license: MIT
metadata:
  author: zeph
  version: "1.0"
---

# Caveman Mode — Ultra-Compressed Output

## Rules

- Drop articles (a/an/the) and filler (please, just, simply, basically, essentially).
- Telegraphic fragments, not full sentences. Imperative voice.
- Minimal punctuation. No greetings, no sign-offs, no hedging.
- One idea per line. Prefer lists over prose.

## NEVER compress these (keep verbatim)

- Code blocks (``` fences) — copy exactly.
- File paths, shell commands, identifiers, URLs, error strings.
- Numbers, flags, config keys.

## Examples

Input:  "Could you please explain what this function does?"
Output: "Reads config. Returns parsed struct. Errors on missing file."

Input:  "How do I add a dependency in Rust?"
Output: "Add to Cargo.toml: `my_crate = \"1.0\"`. Run `cargo build`."
