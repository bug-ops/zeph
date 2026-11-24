# Let the Model Ask You Questions

The `ask_user` tool lets the model pause a turn and ask you one question when it is blocked on something only you know: a choice between valid options, a missing parameter, a scope decision. Your answer comes back as the tool result and the turn continues.

The tool is off by default.

## Enable it

```toml
[tools.ask_user]
enabled = true
timeout_secs = 300           # how long to wait for an answer, 1-3600
max_questions_per_turn = 3   # extra questions in one turn are refused
```

Or for one session: `zeph --ask-user`. `zeph --init` asks about it, and `zeph --migrate-config` adds the commented block to an existing config. The settings are read at startup and are not hot-reloaded.

## What you see

Every question starts with a fixed warning that the model cannot change: never share passwords, API keys or other secrets. Then comes the question and, for a choice, numbered options.

| Channel | How to answer |
|---------|---------------|
| CLI (terminal) | Type a number, an option text, or free text when allowed. |
| TUI | A dialog titled "assistant question". Enter submits, Esc cancels only the question. A closed multi-choice question is a checklist: Up/Down move, Space marks, Enter submits (with nothing marked you are asked again). |
| Telegram, Discord, Slack | Reply in the same chat. Numbers select options. |
| ACP (IDE) | A form shown by the IDE, when it supports elicitation. |

Reply `cancel` (or `/cancel`) to cancel. Sending a slash command such as `/status` also cancels the question, with the notice "Question cancelled - resend your command". A path like `/home/me/proj` is an answer, not a command. A multi-choice question takes numbers separated by commas.

## What the model gets back

- Your answer, with secrets redacted before it reaches history, memory, debug dumps, the audit log or the model.
- `declined` when no user can answer at the moment of the call (for example piped stdin or an IDE without elicitation). This returns immediately.
- `cancelled` when you cancel, and `timed out` after `timeout_secs`.

The model is offered the tool only on user turns of an interactive channel. It is hidden on scheduler, `/loop`, autonomous goal and webhook turns, where nobody could answer. Sub-agents never see it unless their definition sets `permissions.ask_user: allow`; then only foreground `/agent spawn` and `/agent resume` runs can ask, through the parent's channel, and each slash command gets its own question counter. In `zeph serve` the question arrives as an `ask_user` event and is answered with `POST /sessions/:id/answer`. `[tools.permissions]` deny rules for `ask_user` apply at listing and dispatch, as they do for `compress_context`.

On Discord and Slack only replies from the channel of the prompt count as an answer or confirmation. On ACP the question form works in new, loaded, forked and resumed sessions.

Option labels made only of digits are rejected, so a number is always a position.

See the specification in `specs/090-ask-user-model-callable-tool/spec.md`.
