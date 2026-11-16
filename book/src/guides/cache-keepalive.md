# Keep the Prompt Cache Warm

Claude's prompt cache expires after about five minutes. If you pause longer, the next turn pays for writing the whole prefix again. Cache keepalive replays the last request as a cheap ping while you are idle so the cache stays warm.

It is off by default and costs tokens: enable it only when you often pause between turns.

## Enable it

```toml
[llm.cache_keepalive]
enabled = true
keepalive_provider = ""      # [[llm.providers]] name; empty = the provider serving your requests
max_consecutive_pings = 6    # pings in a row without real activity
```

Or for one session: `zeph --cache-keepalive`. Toggle at runtime with `/keepalive on`, `/keepalive off` and `/keepalive status`. The settings are read at startup.

## How it works

- A ping is sent `ttl - 30 s` after the last real request, replaying the messages and tools of that request. It writes nothing to your history or provider state.
- The request is captured after every successful response, including tool-use ones, so a turn that waits on a long tool run or an unanswered `ask_user` question stays warm too.
- Any real request resets the timer and cancels a ping in flight.
- Before each ping a cost gate compares its worst-case cost (including the thinking budget) with the cache write it avoids, and respects the daily budget.
- The TUI shows `Warming prompt cache...` while a ping is in flight; `/cost`, `/keepalive status` and the status bar show the ping count and cost.

## When it stops

| State | Meaning | Cleared by |
|-------|---------|------------|
| cap reached | `max_consecutive_pings` pings without activity | your next message |
| budget exhausted | the cost gate refused a ping | your next message |
| no cache hit | a ping read no cache, so it is not helping | `/keepalive on` or a provider switch |
| prefix rewritten | a ping wrote more than 512 cache tokens, so the request differed | `/keepalive on` or a provider switch |
| provider mismatch | `keepalive_provider` names another backend | `/keepalive on` or a provider switch |
| off | you ran `/keepalive off` | `/keepalive on` |

**Thinking budget:** with extended thinking enabled the worst-case ping includes the whole thinking budget as output, which usually costs more than the cache write it avoids. The gate then refuses the first ping of each idle period and `/keepalive status` shows `paused`. Keepalive is rarely useful with a thinking budget; use it without one or with a small one.

Only providers with a time-limited prompt cache (Claude) ping; others report that keepalive is unsupported. See `specs/091-prompt-cache-keepalive-pings/spec.md`.
