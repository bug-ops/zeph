# Pipeline API

The pipeline module provides a composable, type-safe way to chain processing steps into linear or parallel workflows. Each step transforms typed input into typed output, and the compiler enforces that adjacent steps have compatible types.

## Step Trait

Every pipeline unit implements the `Step` trait:

```rust
pub trait Step: Send + Sync {
    type Input: Send;
    type Output: Send;

    fn run(
        &self,
        input: Self::Input,
    ) -> impl Future<Output = Result<Self::Output, PipelineError>> + Send;
}
```

Steps are async, fallible, and composable. The associated types ensure that chaining a step whose `Input` does not match the previous step's `Output` is a compile-time error.

## Building a Pipeline

`Pipeline::start()` accepts the first step. Additional steps are appended with `.step()`. Call `.run(input)` to execute:

```rust
let result = Pipeline::start(LlmStep::new(provider.clone()))
    .step(ExtractStep::<MyStruct>::new())
    .run("Generate JSON for ...".into())
    .await?;
```

The builder uses a recursive `Chain<Prev, Current>` type internally, so the full pipeline is monomorphized at compile time with zero dynamic dispatch.

## ParallelStep

`parallel(a, b)` creates a step that runs two branches concurrently via `tokio::join!`. Both branches receive a clone of the input and produce a tuple `(A::Output, B::Output)`:

```rust
let step = parallel(
    LlmStep::new(provider.clone()).with_system_prompt("Summarize"),
    LlmStep::new(provider.clone()).with_system_prompt("Extract keywords"),
);
let (summary, keywords) = Pipeline::start(step)
    .run(document)
    .await?;
```

The input type must implement `Clone`. If either branch fails, the error propagates immediately.

## Built-in Steps

### LlmStep

Sends input as a user message to an `LlmProvider` and returns the response string.

```rust
LlmStep::new(provider)
    .with_system_prompt("You are a translator.")
```

- **Input**: `String`
- **Output**: `String`

### RetrievalStep

Embeds the input query via the provider, then searches a `VectorStore` collection.

```rust
RetrievalStep::new(store, provider, "documents", 10)
```

- **Input**: `String`
- **Output**: `Vec<ScoredVectorPoint>`

### ExtractStep

Deserializes a JSON string into any `DeserializeOwned` type.

```rust
ExtractStep::<MyStruct>::new()
```

- **Input**: `String`
- **Output**: `T` (any `serde::de::DeserializeOwned + Send + Sync`)

### MapStep

Wraps a synchronous closure as a step.

```rust
MapStep::new(|s: String| s.to_uppercase())
```

- **Input**: closure input type
- **Output**: closure return type

## Error Handling

All steps return `Result<_, PipelineError>`. The enum variants:

| Variant | Source |
|---------|--------|
| `Llm` | Propagated from `LlmProvider` calls |
| `Memory` | Propagated from `VectorStore` operations |
| `Extract` | JSON deserialization failure |
| `Custom` | Arbitrary error string for custom steps |

Errors short-circuit the chain: if any step fails, subsequent steps are skipped and the error is returned to the caller.

## Example: RAG Pipeline

A retrieve-then-generate pipeline combining several built-in steps:

```rust
use std::sync::Arc;
use zeph_core::pipeline::{Pipeline, Step, ParallelStep};
use zeph_core::pipeline::builtin::{LlmStep, RetrievalStep, MapStep};

let retrieve = RetrievalStep::new(store, embedder, "knowledge", 5);
let format = MapStep::new(|results: Vec<ScoredVectorPoint>| {
    results.iter().map(|r| r.id.clone()).collect::<Vec<_>>().join("\n")
});
let answer = LlmStep::new(provider).with_system_prompt("Answer using the context below.");

let result = Pipeline::start(retrieve)
    .step(format)
    .step(answer)
    .run("What is the pipeline API?".into())
    .await?;
```
