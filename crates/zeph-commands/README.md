# zeph-commands

Slash command registry, handler trait, and channel sink abstraction for
[Zeph](https://github.com/bug-ops/zeph).

This crate provides the non-generic infrastructure for slash command dispatch. It has no
dependency on `zeph-core` — the agent implements the provided traits and wires them in at
startup.

## Modules

- `sink` — [`ChannelSink`] minimal async I/O trait; replaces the `C: Channel` generic in handlers
- `context` — [`CommandContext`] non-generic dispatch context with trait-object fields
- `traits` — sub-trait definitions for subsystem access (`AgentAccess`, `DebugAccess`, etc.)
- `handlers` — concrete handler implementations (session, debug, skill, mcp, plan, …)
- `commands` — static `COMMANDS` metadata table used by `/help`

## Design

`CommandRegistry<Ctx>` and `CommandHandler<Ctx>` are non-generic over the channel type.
Handlers receive a `&mut CommandContext` whose fields are trait objects, so a change in
`zeph-core`'s agent loop does not recompile this crate.

### Dispatch algorithm

`CommandRegistry::dispatch` performs a linear scan over registered handlers and picks the
**longest word-boundary match**, enabling subcommand resolution without ambiguity:

```
/plan confirm   →  handler "/plan confirm"   wins over "/plan"
/plan           →  handler "/plan"           (no "/plan confirm" match)
```

### Borrow splitting

When `CommandRegistry` is stored as an `Agent<C>` field, the dispatch site uses
`std::mem::take` to move the registry out temporarily, constructs a `CommandContext`, dispatches,
and restores the registry. This avoids borrow-checker conflicts with the channel field.

`NullSink` and `NullAgent` are zero-cost sentinels for dispatch blocks that do not need
channel I/O or agent-access commands respectively.

## Usage

### Register and dispatch commands

```rust,no_run
use zeph_commands::{CommandRegistry, CommandContext, NullSink, NullAgent};

// Build the registry once at agent startup.
let mut registry: CommandRegistry<CommandContext> = CommandRegistry::new();
// registry.register(MyHandler);

// At dispatch time, construct the context and call dispatch.
let mut sink = NullSink;
let mut agent = NullAgent;
let mut ctx = CommandContext::new(&mut sink, &mut agent);

// registry.dispatch(&mut ctx, "/help").await;
```

### Implement a custom handler

```rust,no_run
use std::future::Future;
use std::pin::Pin;
use zeph_commands::{CommandHandler, CommandOutput, CommandError, SlashCategory};

struct PingHandler;

impl<Ctx: Send> CommandHandler<Ctx> for PingHandler {
    fn name(&self) -> &'static str { "/ping" }
    fn description(&self) -> &'static str { "Reply with pong" }
    fn category(&self) -> SlashCategory { SlashCategory::Session }

    fn handle<'a>(
        &'a self,
        _ctx: &'a mut Ctx,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(CommandOutput::Message("pong".into())) })
    }
}
```

## Slash categories

Commands are grouped into categories for `/help` output:

| Category | Commands |
|---|---|
| `Session` | `/clear`, `/reset`, `/exit`, `/new`, … |
| `Configuration` | `/model`, `/provider`, `/guardrail`, … |
| `Memory` | `/memory`, `/graph`, `/compact`, `/guidelines`, … |
| `Skills` | `/skill`, `/skills`, `/feedback`, … |
| `Planning` | `/plan`, `/focus`, `/sidequest`, … |
| `Debugging` | `/debug-dump`, `/log`, `/lsp`, `/status`, … |
| `Integration` | `/mcp`, `/image`, `/agent`, … |
| `Advanced` | `/experiment`, `/policy`, `/scheduler`, … |

## License

MIT — see [LICENSE](../../LICENSE).
