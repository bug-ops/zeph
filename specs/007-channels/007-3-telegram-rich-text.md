---
aliases:
  - Telegram Rich Text Formatting
  - MarkdownV2 Rendering
  - Bot API 10.1/10.2 Formatting
tags:
  - sdd
  - spec
  - channels
  - telegram
  - bot-api-10
created: 2026-07-20
status: implemented
related:
  - "[[007-channels/spec]]"
  - "[[007-channels/007-1-telegram-guest-mode]]"
  - "[[007-channels/007-2-telegram-bot-to-bot]]"
  - "[[001-system-invariants/spec]]"
  - "[[020-config-loading/spec]]"
---

# Spec: Telegram Rich-Text Formatting (Bot API 10.1/10.2)

> [!info]
> Telegram Bot API 10.1/10.2 add obscenely-rich-text formatting (underline, spoiler,
> expandable blockquote, custom emoji, `sendRichMessage` structured blocks). Zeph already
> renders CommonMark to MarkdownV2 via a single `TelegramRenderer`, but that renderer has a
> latent multi-line-blockquote bug, and the guest-mode reply path bypasses it entirely,
> sending raw unconverted Markdown with `parse_mode="HTML"`. This spec fixes both defects
> as Phase 1 MVP, defines the phased roadmap for the new formatting surface (Phase 2:
> spoiler/underline/custom-emoji via markup), and defers structured `sendRichMessage`
> blocks to a separate epic (Phase 3). Closes #6541.
>
> **Phase 1 MVP landed** in commit `fdd887fbf` (#6604): guest-mode escaping fix, per-line
> multi-line/nested blockquote flattening, `expandable_blockquote_min_lines` config
> (wired into `--init`/`--migrate-config`), all matching this spec's FR-001..009. Phase 2/3
> (§8) remain roadmap-only — not implemented.

## Sources

### External

- [Telegram blog — obscenely rich text formatting for bots](https://telegram.org/blog/watch-apps-and-more#obscenely-rich-text-formatting-for-bots)
- [Bot API — Formatting options](https://core.telegram.org/bots/api#formatting-options)
- [Bot API — `sendRichMessage`](https://core.telegram.org/bots/api#sendrichmessage)

### Internal

| File | Contents |
|---|---|
| `crates/zeph-channels/src/telegram.rs` | `TelegramChannel::send` (regular path, `markdown_to_telegram` + `ParseMode::MarkdownV2`) and `flush_chunks` (guest path, bug site: `telegram.rs:1289-1307`) |
| `crates/zeph-channels/src/markdown.rs` | `markdown_to_telegram`, `TelegramRenderer` — `pulldown-cmark` event walker, MarkdownV2 escaping, chunking; bug site: blockquote handling at `markdown.rs:199` |
| `crates/zeph-channels/src/telegram_api_ext.rs` | `TelegramApiClient`, `answer_guest_query(query_id, text, parse_mode)`, shared `post()` helper, `REQUEST_TIMEOUT` |
| `crates/zeph-config/src/telegram.rs` | `TelegramConfig` — target for the new `expandable_blockquote_min_lines` field (Phase 1) and `custom_emoji` map (Phase 2) |

---

## 1. Overview

### Problem Statement

Two independent defects exist in Telegram outbound text formatting:

1. **Guest-mode escaping bug.** `TelegramChannel::flush_chunks` (`telegram.rs:1289-1307`) sends
   `full_text.trim()` — the raw, unconverted Markdown accumulated from the LLM — via
   `answer_guest_query(&query_id, &text, Some("HTML"))`. `parse_mode` is set to `"HTML"` but the
   text never passes through any Markdown→HTML conversion or HTML escaping (no HTML escaper
   exists anywhere in the crate). Any literal `<`, `>`, `&`, or Markdown syntax character in the
   LLM's response either breaks Telegram's HTML parser (message rejected) or renders literal
   markup to the guest user. The regular `send` path (`telegram.rs:1202-1213`) does this
   correctly: `markdown_to_telegram()` + `ParseMode::MarkdownV2`.
2. **Multi-line blockquote bug.** `TelegramRenderer` emits a single leading `>` at the start of a
   `BlockQuote` event (`markdown.rs:199`) and a trailing newline at the end — it does not prefix
   every line of a multi-line quote. In MarkdownV2, each quoted line requires its own leading `>`;
   without it, only the first line of a multi-line blockquote is actually quoted by Telegram's
   client, and the remaining lines render as plain text outside the quote block.

Independently, Telegram Bot API 10.1/10.2 introduce a broader formatting surface Zeph does not
yet use: `underline`, `spoiler`, `expandable_blockquote`, `custom_emoji` entities, and a fully
structured `sendRichMessage` method with `InputRichBlock*` types (headings, dividers, math,
collapsible sections, tables, media blocks, ephemeral messages). Adopting these expands what
Zeph's Telegram responses can express, but most of them (underline, spoiler, custom emoji) have
no corresponding construct in the CommonMark the LLM produces — they need a source convention
before they are reachable at all.

### Goal

Phase 1 (MVP, this spec's primary deliverable): guest-mode responses are formatted through the
same `markdown_to_telegram` renderer as regular messages, and multi-line blockquotes render
correctly — with an opt-in expandable form for long quotes. Phase 2/3 (documented here as a
roadmap, not committed in this PR): additive markup-level support for spoiler/underline/custom
emoji, and a separate epic for `sendRichMessage` structured blocks.

### Out of Scope

- **A second formatting path (HTML).** Zeph has exactly one outbound text renderer
  (`markdown_to_telegram` → MarkdownV2). This spec fixes guest mode by routing it through that
  renderer, not by adding a Markdown→HTML converter or HTML escaper.
- **Inbound rich-text parsing.** `Message.rich_message` / `RichBlock*` on *incoming* Telegram
  messages are not parsed by Zeph in any phase; inbound messages are consumed as plain text
  as today.
- **Ephemeral-message methods** (`editEphemeralMessage*`, `deleteEphemeralMessage`,
  `receiver_user`, `ephemeral_message_id`) — no Zeph use case yet; deferred indefinitely.
- **Structured `sendRichMessage` blocks** (Phase 3) — separate epic, not delivered by this spec
  or its Phase 1 PR. See §6.
- **Multi-Model Design Principle** — not applicable; this feature does not call an LLM.
- **teloxide upgrade** — not required for Phase 1 or Phase 2 (inline entity kinds already exist
  in teloxide-core 0.13; see §5). Only relevant if/when `sendRichMessage` (Phase 3) is
  implemented via native types instead of the raw-HTTP extension layer.

---

## 2. User Stories

### US-001: Guest-mode response renders correctly

AS A user who @mentions Zeph in a group chat (guest mode, Bot API 10.0),
I WANT my response to render with proper formatting instead of raw Markdown syntax or a
rejected message,
SO THAT the guest-mode experience matches the quality of regular chat responses.

**Acceptance criteria:**

```
GIVEN telegram.guest_mode = true and an authorized guest query
WHEN the LLM response contains Markdown constructs (bold, code, links, blockquotes)
THEN the response delivered via answerGuestQuery renders with the same formatting fidelity
     as a regular (non-guest) message, using ParseMode::MarkdownV2
```

### US-002: Multi-line quotes render fully quoted

AS A user reading a Zeph response that quotes multiple lines,
I WANT every line of the quote to render inside Telegram's blockquote UI,
SO THAT I can visually distinguish quoted content from the rest of the response.

**Acceptance criteria:**

```
GIVEN a CommonMark blockquote spanning N > 1 lines in the LLM output
WHEN TelegramRenderer converts it to MarkdownV2
THEN every one of the N lines is prefixed with `>` in the emitted text
AND Telegram's client renders all N lines as part of a single blockquote
```

### US-003: Long quotes are collapsible

AS A user receiving a long quoted passage,
I WANT it to render as an expandable (collapsed-by-default) blockquote when it exceeds a
configurable length,
SO THAT long quotes don't dominate the chat view.

**Acceptance criteria:**

```
GIVEN telegram.expandable_blockquote_min_lines = 10 (default)
WHEN a blockquote has 10 or more lines
THEN TelegramRenderer emits the expandable form (`**>` prefix on the first line, `||` suffix
     on the last line, per-line `>` prefix on all lines)
AND a blockquote with fewer lines renders as a regular (non-expandable) blockquote
```

### US-004: Feature is opt-out safe

AS AN operator who has not changed the default configuration,
I WANT existing formatting behavior to remain unchanged apart from the two bug fixes,
SO THAT this feature does not silently alter previously-correct output.

**Acceptance criteria:**

```
GIVEN default config (expandable_blockquote_min_lines = 10)
WHEN existing constructs (bold, italic, strikethrough, code, links, lists, single-line
     blockquotes) are rendered
THEN their output is byte-for-byte identical to pre-feature output
```

---

## 3. Functional Requirements

### Phase 1 (MVP — this spec's committed scope)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN `TelegramChannel::flush_chunks` sends a guest-mode response THE SYSTEM SHALL format the accumulated text via `markdown_to_telegram()` before sending | must |
| FR-002 | WHEN sending a guest-mode response THE SYSTEM SHALL call `answer_guest_query` with `parse_mode = "MarkdownV2"`, never `"HTML"` | must |
| FR-003 | WHEN the formatted guest-mode text is empty THE SYSTEM SHALL skip sending, mirroring the empty-check already present on the regular `send` path | must |
| FR-004 | WHEN the formatted guest-mode text exceeds `MAX_MESSAGE_LEN` THE SYSTEM SHALL apply the same truncation-warning behavior as the regular `send` path | must |
| FR-005 | WHEN `TelegramRenderer` emits a blockquote spanning multiple lines THE SYSTEM SHALL prefix every line with `>` | must |
| FR-006 | WHEN a blockquote's line count is `>= expandable_blockquote_min_lines` AND `expandable_blockquote_min_lines > 0` THE SYSTEM SHALL render the expandable form (`**>` … `||`) | must |
| FR-007 | WHEN `expandable_blockquote_min_lines = 0` THE SYSTEM SHALL never render the expandable form, regardless of quote length | must |
| FR-008 | WHEN `telegram.expandable_blockquote_min_lines` is absent from config THE SYSTEM SHALL default to `10` | must |
| FR-009 | THE SYSTEM SHALL apply the multi-line blockquote fix (FR-005) identically on the guest-mode and regular paths, since both share `markdown_to_telegram` | must |

### Phase 2 (roadmap — not committed in the Phase 1 PR; tracked as follow-up)

| ID | Requirement | Notes |
|----|-------------|-------|
| FR-010 | WHEN a config-mapped custom-emoji glyph appears in LLM output AND `telegram.custom_emoji` contains a mapping for it THE SYSTEM SHALL emit `![glyph](tg://emoji?id=<id>)` | Requires `[telegram.custom_emoji]` map; source signal is a config-driven glyph lookup, not a CommonMark construct |
| FR-011 | THE SYSTEM SHOULD support an LLM-facing passthrough syntax for spoiler text that `TelegramRenderer` maps to `\|\|text\|\|` | Requires an agreed source convention (see §5) — no CommonMark equivalent exists; convention TBD in the Phase 2 spec update |
| FR-012 | THE SYSTEM MAY support an equivalent passthrough syntax for underline (`__text__`) | Same source-convention dependency as FR-011; lower priority than spoiler |

> [!question]
> FR-010/011/012 are not specified to Given/When/Then acceptance-criteria depth here because
> their source signal (the passthrough syntax, the emoji-glyph vocabulary) is an open design
> question, not an implementation detail. Resolve the source convention in a follow-up spec
> update before starting Phase 2 implementation — do not treat this table as ready-to-implement.

### Phase 3 (separate epic — explicitly not part of this spec)

`sendRichMessage` / `InputRichBlock*` structured blocks. See §6.

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Correctness | Guest-mode and regular-message formatting MUST produce identical output for the same input Markdown (parity requirement; verified by a shared-fixture test run through both call sites) |
| NFR-002 | Reliability | The multi-line blockquote fix MUST NOT alter the escaping behavior of any other `pulldown-cmark` event type (bold, italic, code, links, lists) — regression-tested against the existing `markdown.rs` `#[cfg(test)]` suite |
| NFR-003 | Compatibility | Default config (`expandable_blockquote_min_lines = 10`) MUST preserve existing rendering for quotes shorter than 10 lines, and MUST correctly quote (non-expandable) all lines of quotes that were previously only partially quoted due to the bug |
| NFR-004 | Observability | No new logging is required for Phase 1 (both fixes are deterministic, non-network-dependent transforms); existing guest-mode HTTP failure logging in `answer_guest_query` is unaffected |

---

## 5. Design Decision: Source Signal, Not Emit Format

The primary design question for this feature is not "emit `MessageEntity` or emit MarkdownV2
markup" — it is **what source signal indicates that a span should carry a given formatting
kind**. The LLM produces CommonMark; `pulldown-cmark` yields events for bold, italic,
strikethrough, code, links, blockquotes, and lists, all already rendered by `TelegramRenderer`.
CommonMark has **no construct** for underline, spoiler, collapsible quote, or custom emoji.

Consequences for scoping:

- A formatting feature is MVP-reachable only if it is **derivable from an existing CommonMark
  construct**. The expandable blockquote qualifies: a multi-line `BlockQuote` event already
  exists; the expandable form is a length-triggered rendering choice on top of it, not a new
  source signal (§3, FR-006).
- A feature with **no CommonMark source** (underline, spoiler, custom emoji) requires an agreed
  source convention — either an LLM-facing passthrough syntax or a config-driven mapping — before
  it can be built, **regardless of whether the emit format is markup or structured entities**.
  This is why FR-010/011/012 are listed as roadmap items with an open question, not committed
  Phase 1 requirements.

### Emit-Format Decision: Stay on MarkdownV2 Markup

**Decision:** keep the single MarkdownV2 text-string renderer (`markdown_to_telegram` +
`ParseMode::MarkdownV2`) for Phase 1 and Phase 2. Defer structured `MessageEntity` emission.

**Rationale:**

- MarkdownV2 markup already expresses every Phase 1/2-targeted inline feature: spoiler
  `||text||`, underline `__text__`, expandable blockquote `**>line1\n>line2||`, custom emoji
  `![👍](tg://emoji?id=<id>)`.
- It is the existing, tested architecture — a full `#[cfg(test)]` suite in `markdown.rs` already
  covers escaping and chunking. Escaping discipline is already solved there.
- Structured entities carry a real correctness cost (UTF-16 offset arithmetic, see below) and
  are only warranted when escaping precision must exceed what markup guarantees. Neither Phase
  1 nor Phase 2 requires that.

> [!note] Verified ground truth (2026-07-20)
> teloxide-core 0.13 already models the Bot API 10.1/10.2 inline entities —
> `MessageEntityKind::{Underline, Spoiler, Blockquote, ExpandableBlockquote, CustomEmoji}` and
> `SendMessage.entities: Vec<MessageEntity>` are present in the vendored source
> (`teloxide-core-0.13.0/src/types/message_entity.rs:258-268`,
> `payloads/send_message.rs:32`). Entity-based inline rich text needs **no teloxide upgrade** —
> the decision to stay on markup is architectural (§5), not a library-availability constraint.
> `sendRichMessage` / `InputRichBlock*`, by contrast, are absent from teloxide-core 0.13 (zero
> grep hits) — that gap is real and drives the Phase 3 routing decision in §6.

> [!important] Future-conditional invariant — applies only if/when structured entities are
> ever adopted (not required by this spec's Phase 1 or Phase 2 scope, recorded here so it is
> not rediscovered the hard way later)
> - Entity `offset`/`length` fields MUST be counted in **UTF-16 code units** — Telegram's entity
>   index unit — never bytes, never Unicode scalar values. A byte- or char-offset computation
>   silently corrupts every entity boundary past the first non-ASCII or non-BMP character.
> - `entities` and `parse_mode` are mutually exclusive on a single `sendMessage` call — Telegram
>   uses `entities` *instead of* `parse_mode`. A message MUST NOT set both fields.

---

## 6. Guest-Mode Escaping Fix (Phase 1)

**Target change** at `crates/zeph-channels/src/telegram.rs:1289-1307`:

| Before | After |
|---|---|
| `let text = full_text.trim().to_owned();` | `let formatted = markdown_to_telegram(full_text.trim());` |
| `answer_guest_query(&query_id, &text, Some("HTML"))` | `answer_guest_query(&query_id, &formatted, Some("MarkdownV2"))` |

Additional parity requirements (FR-003, FR-004):

- The `MAX_MESSAGE_LEN` truncation-warning check MUST operate on `formatted`, not the raw
  accumulated text — matching the regular `send` path.
- Skip sending when `formatted.is_empty()` — matching the regular `send` path's empty-output
  skip.
- `answer_guest_query`'s signature is unchanged; it already accepts an arbitrary `parse_mode`
  string argument.

> [!danger]
> NEVER add a Markdown→HTML converter or an HTML escaper to fix this bug. That would create a
> second formatting path, violating the project's single-renderer architecture (constitution
> §VII, DRY). The correct fix is **unification** onto the one existing renderer, not a parallel
> implementation.

This fix conceptually belongs to the guest-mode feature area — [[007-channels/007-1-telegram-guest-mode]]
owns the `answerGuestQuery` response-routing contract (see its §6 "Response Routing" and §13
NEVER list, which currently documents the *old*, buggy `ParseMode::Html` behavior and MUST be
updated alongside this fix — see §13 of this spec). The code change lives in the same file/module
touched by the blockquote fix below, so both land in a single Phase 1 PR.

---

## 7. Multi-Line Blockquote Fix (Phase 1)

**Bug** at `crates/zeph-channels/src/markdown.rs:199`: `TelegramRenderer` emits a single `>` at
`BlockQuote` start and a trailing newline at end — it does not prefix each line. For a
multi-line quote, only the first line is actually quoted in the resulting MarkdownV2; subsequent
lines render as plain text.

**Fix approach:** track blockquote state in the renderer's event walker and prefix every text
line emitted while inside a `BlockQuote` event with `>`. Since expandable-blockquote support
(FR-006) requires per-line `>` prefixing anyway, both are fixed together in the same change:

```
on BlockQuote start:
    buffer the quote's lines (don't emit yet)
on BlockQuote end:
    line_count = buffered lines' count
    if line_count >= expandable_blockquote_min_lines and expandable_blockquote_min_lines > 0:
        emit "**>" + line[0]
        for line in line[1..]: emit ">" + line
        emit "||" appended to the last line
    else:
        for line in lines: emit ">" + line
```

> [!warning]
> The buffering approach above is illustrative, not prescriptive — the implementing PR should
> follow whatever incremental-emission pattern `TelegramRenderer`'s existing event walker uses
> for other multi-event constructs (e.g. lists), as long as the per-line `>` prefix invariant
> (FR-005) and the length-triggered expandable form (FR-006/FR-007) hold.

**As implemented (#6604):** nested `BlockQuote` events are flattened to a single `>`-per-line
level via a bounded mark stack, `MAX_BLOCKQUOTE_NESTING_DEPTH = 512` (same pattern as
`MAX_CHUNK_DEPTH`, #6595) — input nesting past the cap does not grow tracked-mark memory
unboundedly; only the outermost mark within the cap is recorded, and flattening still applies
beyond it.

---

## 8. Phase 2 / Phase 3 Roadmap (Not Committed in This Spec's PR)

### Phase 2 — additive markup-level features (follow-up PR)

| Feature | Source signal | Status |
|---|---|---|
| Spoiler (`\|\|text\|\|`) | LLM-facing passthrough syntax — convention TBD | Roadmap (FR-011) |
| Custom emoji (`![glyph](tg://emoji?id=<id>)`) | `[telegram.custom_emoji]` config map, glyph/name → `custom_emoji_id` | Roadmap (FR-010) |
| Underline (`__text__`) | Same passthrough-convention dependency as spoiler | Roadmap, lower priority (FR-012) |

### Phase 3 — structured rich messages (`sendRichMessage`) — separate epic

**Decision:** separate epic, not part of this spec's PR. Prefer native teloxide types when
available; fall back to the `telegram_api_ext.rs` raw-HTTP layer only if the timeline demands it
sooner.

Covers `InputRichBlock*`: paragraphs, section headings, preformatted blocks, dividers, math,
lists, block/pull quotations, collage, slideshow, table, details/collapsible sections, media
blocks (Bot API 10.2 `InputRichMessageMedia`), thinking blocks; the raised 32768-character
message limit; and the ephemeral-message method family (explicitly out of scope, §1).

Two implementation routes, in priority order:

1. **Preferred** — wait for a teloxide release covering Bot API 10.1/10.2 and use native types.
   This aligns with the existing issue #3732 (migrate `TelegramApiExt` to native teloxide Bot
   API types) and avoids hand-maintaining DTOs that a future teloxide release would supersede.
2. **If needed sooner** — add `send_rich_message` + `InputRichBlock*` request DTOs to
   `telegram_api_ext.rs`, following the exact raw-HTTP pattern already proven for guest mode
   (spec 007-1), bot-to-bot (spec 007-2), and reaction moderation (`spec.md` §"Telegram Reaction
   Moderation Tools"): all calls go through the shared `post()` helper, the bot token is never
   logged, and the client uses the existing 30-second `REQUEST_TIMEOUT`.

> [!danger] Phase 3 boundary (advance notice — not enforced by Phase 1 code, recorded so the
> eventual Phase 3 spec inherits it)
> - NEVER add Bot API 10.1/10.2 raw methods anywhere but `telegram_api_ext.rs`, until native
>   teloxide types exist for them.
> - NEVER bypass the shared `post()` helper for a new raw method.
> - NEVER hard-fail a send because a rich block is unsupported by the current teloxide/ext-layer
>   version — fall back to MarkdownV2 rendering of an equivalent plain-text representation.

---

## 9. Configuration Schema

### Phase 1

```toml
[telegram]
# Blockquotes with this many lines or more render as an expandable (collapsed-by-default)
# blockquote (Bot API 10.1 `expandable_blockquote`). 0 disables the expandable form entirely
# — all blockquotes render as regular (always-expanded) quotes regardless of length.
# Default: 10.
expandable_blockquote_min_lines = 10
```

Required integration points per project convention (mandatory for any new config field):

1. TOML section above — `[telegram]`.
2. `TelegramConfig` field (`crates/zeph-config/src/telegram.rs`), `u32`, default `10`.
3. `--init` wizard entry describing the field and its default.
4. `--migrate-config` migration step so existing configs gain the field with the documented
   default without manual editing.

### Phase 2 (roadmap — not part of the Phase 1 config surface)

```toml
[telegram.custom_emoji]
# Maps a glyph or logical name to a Telegram custom_emoji_id. Empty map (default)
# disables custom-emoji substitution entirely.
# Example: "👍" = "5368324170671202286"
```

---

## 10. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Guest-mode response contains characters that are special in both Markdown and HTML (e.g. `<`, `&`, `*`) | Escaped correctly by `markdown_to_telegram`'s existing MarkdownV2 escaping — no HTML-specific handling needed since HTML is no longer used on this path |
| Guest-mode response is empty after `.trim()` and formatting | Skip sending (FR-003); no empty `answerGuestQuery` call |
| Guest-mode response exceeds `MAX_MESSAGE_LEN` after formatting | Same truncation-warning behavior as the regular `send` path (FR-004); guest mode has no `editMessageText` fallback, so truncation (not chunking) applies, consistent with the one-shot `answerGuestQuery` constraint documented in [[007-channels/007-1-telegram-guest-mode]] §6 |
| Nested blockquote (blockquote containing another blockquote) | Corrected during review (2026-07-20): Telegram MarkdownV2 has no nested-blockquote grammar — a blockquote is exactly one leading `>` per line, and a second `>` on the same line is an unescaped reserved character that Telegram's parser rejects outright (`400 Bad Request`, dropping the whole message). `pulldown-cmark` emits nested `BlockQuote` events; the fix MUST flatten them to a single `>`-per-line level (never accumulate `>>`) — verify against a nested-quote unit test fixture before merge. This supersedes this row's original "cumulative `>` depth" guidance, which was never live-verified and is disproven by Telegram's own formatting-options documentation |
| Blockquote line count exactly equals `expandable_blockquote_min_lines` | Renders as expandable (`>=` comparison per FR-006, not `>`) |
| `expandable_blockquote_min_lines = 0` with a 50-line blockquote | Renders as a regular (fully expanded) blockquote — never expandable (FR-007) |
| A blockquote line itself contains MarkdownV2 special characters | Escaped exactly as it would be outside a blockquote — the per-line `>` prefix is prepended to the already-escaped line content, not interleaved with escaping logic |

---

## 11. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Guest-mode responses are formatted via `markdown_to_telegram` for 100% of sends | 100% (unit test enforced) |
| SC-002 | `answer_guest_query` is never called with `parse_mode = "HTML"` after this fix | 0 occurrences (unit test enforced) |
| SC-003 | Multi-line blockquotes (2–9 lines) render with `>` on every line | 100% (unit test fixture, regular and expandable-disabled config) |
| SC-004 | Blockquotes with `>= expandable_blockquote_min_lines` lines render the expandable form; shorter ones do not | 100% (unit test, boundary case at exactly `min_lines`) |
| SC-005 | Existing `markdown.rs` `#[cfg(test)]` suite passes unchanged (no regression to bold/italic/code/link/list rendering) | 100% pass |
| SC-006 | Live test: guest-mode @mention response with bold/code/blockquote content renders correctly in a real Telegram client | Pass |
| SC-007 | Live test: a 12-line blockquote response renders as collapsed/expandable in a real Telegram client (default config) | Pass |

---

## 12. Key Invariants

- Exactly one outbound Telegram text-formatting path exists: `markdown_to_telegram` +
  `ParseMode::MarkdownV2`. Guest-mode, regular streaming, and any future formatting path MUST
  share it — no channel-specific or mode-specific renderer variants.
- Guest-mode responses MUST be formatted through the shared renderer before being sent via
  `answer_guest_query` — raw, unconverted LLM text MUST NOT reach `answer_guest_query`.
- Renderer-emitted formatting markers (`*`, `_`, `~`, `` ` ``, `>`, `\|\|`, etc.) MUST remain
  unescaped where they are structural; all user/LLM text content MUST be escaped as regular
  MarkdownV2 text per existing `markdown.rs` escaping rules.
- Multi-line blockquotes MUST prefix every line with `>` — a single leading `>` on only the
  first line is the bug this spec fixes, not acceptable output.
- New formatting (expandable blockquote, and any Phase 2 additions) MUST be strictly additive —
  existing constructs (bold, italic, strikethrough, code, links, lists, short blockquotes) MUST
  render byte-for-byte unchanged.
- `expandable_blockquote_min_lines = 0` MUST fully disable the expandable form — this is the
  documented escape hatch for operators who want classic always-expanded blockquotes.
- Bot API 10.1/10.2 raw HTTP methods (Phase 3, if/when implemented via the ext-layer route) MUST
  live only in `telegram_api_ext.rs`, dispatched through the shared `post()` helper, until native
  teloxide types exist — consistent with the existing `TelegramApiClient` design invariants in
  `spec.md`.
- *(Future-conditional, applies only if structured `MessageEntity` emission is ever adopted)*
  Entity offsets/lengths MUST be UTF-16 code units, never bytes or Unicode scalar values.

## 13. NEVER

- NEVER send raw, unconverted LLM text to `answer_guest_query` with any `parse_mode` set — this
  is the exact defect this spec fixes; any regression reintroducing it is a P0.
- NEVER introduce a Markdown→HTML converter or HTML escaper as a "fix" for the guest-mode bug —
  the fix is unification onto the single existing MarkdownV2 renderer, not a second path.
- NEVER set both `entities` and `parse_mode` on a single `sendMessage`/`answerGuestQuery` call
  (future-conditional — applies only if entities are ever emitted).
- NEVER compute `MessageEntity` offsets in bytes or Unicode scalar values (future-conditional,
  same scope as above).
- NEVER emit a `tg://emoji?id=` link for an unresolved custom-emoji glyph — Phase 2 custom-emoji
  substitution MUST fall back to the literal glyph when no config mapping exists, not emit a
  broken link.
- NEVER hard-fail a Telegram send because a Phase 3 rich block is unsupported by the current
  teloxide/ext-layer version — fall back to an equivalent MarkdownV2 rendering.
- NEVER add raw Bot API 10.1/10.2 HTTP calls outside `telegram_api_ext.rs`'s `post()` helper.

---

## 14. Agent Boundaries

### Always (without asking)

- Run `cargo nextest` (`markdown.rs` and `telegram.rs` test targets) after any change to
  `TelegramRenderer` or `flush_chunks`
- Follow the existing `pulldown-cmark` event-walker pattern already used in `markdown.rs` for
  other block-level constructs (e.g. lists) when implementing the blockquote fix
- Add `///` doc comments to any new public function, type, or config field
- Update `[[007-channels/007-1-telegram-guest-mode]]` §6/§13 to reflect the corrected
  `ParseMode::MarkdownV2` behavior once the guest-mode fix lands (that sub-spec currently
  documents the pre-fix `ParseMode::Html` call as the intended design and must not be left
  stale)

### Ask First

- Adding the Phase 2 `[telegram.custom_emoji]` config surface or any spoiler/underline
  passthrough syntax — the source convention is an open question (§8) requiring a spec update
  before implementation
- Starting Phase 3 (`sendRichMessage`) implementation — confirm whether to wait for native
  teloxide types or proceed via the ext-layer route first

### Never

- Modify `TelegramApiClient`'s `post()` helper or `REQUEST_TIMEOUT` as part of this Phase 1 fix
  — out of scope
- Introduce a second Markdown-to-anything converter anywhere in `zeph-channels`
- Change `IncomingMessage` / `ChannelMessage` structs as part of this fix — Phase 1 touches only
  the renderer and the guest-mode send call site

---

## 15. Acceptance Criteria (Issue #6541)

- [ ] Guest-mode `flush_chunks` formats text via `markdown_to_telegram()` before sending
- [ ] `answer_guest_query` called with `parse_mode = "MarkdownV2"`, never `"HTML"`
- [ ] Guest-mode empty-after-formatting output is skipped (parity with `send`)
- [ ] Guest-mode `MAX_MESSAGE_LEN` truncation warning operates on formatted text (parity with `send`)
- [ ] `TelegramRenderer` prefixes every line of a multi-line blockquote with `>`
- [ ] `expandable_blockquote_min_lines` config field added to `TelegramConfig` with default `10`
- [ ] `--init` wizard and `--migrate-config` updated for the new config field
- [ ] Expandable blockquote form (`**>` … `\|\|`) emitted when line count ≥ threshold and threshold > 0
- [ ] `expandable_blockquote_min_lines = 0` disables the expandable form unconditionally
- [ ] Existing `markdown.rs` test suite passes unchanged (no regression)
- [ ] New unit tests: guest-mode formatting parity, multi-line blockquote (2–9 lines), boundary at exactly `min_lines`, `min_lines = 0` disables expandable, nested blockquote
- [ ] [[007-channels/007-1-telegram-guest-mode]] updated to reflect `ParseMode::MarkdownV2` (was `Html`)
- [ ] Live test: guest-mode @mention response renders correctly (pending live session)
- [ ] Live test: 12-line blockquote renders expandable with default config (pending live session)
- [ ] Playbook updated: `.local/testing/playbooks/telegram.md`
- [ ] Coverage-status updated

---

## 16. References

- Issue #6541 — this feature
- [Telegram blog — obscenely rich text formatting for bots](https://telegram.org/blog/watch-apps-and-more#obscenely-rich-text-formatting-for-bots)
- [Bot API — Formatting options](https://core.telegram.org/bots/api#formatting-options)
- [Bot API — `sendRichMessage`](https://core.telegram.org/bots/api#sendrichmessage)
- Issue #3732 — migrate `TelegramApiExt` to native teloxide Bot API types (Phase 3 preferred route)
- [[007-channels/spec]] — channel trait, AnyChannel, streaming protocol, `TelegramApiClient` design invariants
- [[007-channels/007-1-telegram-guest-mode]] — guest-mode response routing; §6/§13 require an
  update once this spec's guest-mode fix lands
- [[007-channels/007-2-telegram-bot-to-bot]] — sibling Bot API 10.0 sub-spec (structure reference)
- [[001-system-invariants/spec]] — cross-cutting architectural invariants
- [[020-config-loading/spec]] — config resolution order, defaults, `--migrate-config` mechanics
