pub mod builder;
pub mod builtin;
pub mod parallel;
pub mod step;

pub use builder::Pipeline;
pub use parallel::ParallelStep;
pub use step::Step;

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Llm(#[from] zeph_llm::LlmError),

    #[error(transparent)]
    Memory(#[from] zeph_memory::MemoryError),

    #[error("extraction failed: {0}")]
    Extract(String),

    #[error("{0}")]
    Custom(String),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::builtin::{ExtractStep, LlmStep, MapStep, RetrievalStep};
    use super::parallel::parallel;
    use super::*;
    use zeph_llm::mock::MockProvider;
    use zeph_memory::in_memory_store::InMemoryVectorStore;
    use zeph_memory::vector_store::{VectorPoint, VectorStore};

    struct AddSuffix {
        suffix: String,
    }

    impl Step for AddSuffix {
        type Input = String;
        type Output = String;

        async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
            Ok(format!("{input}{}", self.suffix))
        }
    }

    struct ParseLen;

    impl Step for ParseLen {
        type Input = String;
        type Output = usize;

        async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
            Ok(input.len())
        }
    }

    #[tokio::test]
    async fn single_step_pipeline() {
        let result = Pipeline::start(AddSuffix { suffix: "!".into() })
            .run("hello".into())
            .await
            .unwrap();
        assert_eq!(result, "hello!");
    }

    #[tokio::test]
    async fn chained_pipeline() {
        let result = Pipeline::start(AddSuffix {
            suffix: " world".into(),
        })
        .step(AddSuffix { suffix: "!".into() })
        .run("hello".into())
        .await
        .unwrap();
        assert_eq!(result, "hello world!");
    }

    #[tokio::test]
    async fn heterogeneous_chain() {
        let result = Pipeline::start(AddSuffix {
            suffix: "abc".into(),
        })
        .step(ParseLen)
        .run("".into())
        .await
        .unwrap();
        assert_eq!(result, 3);
    }

    #[tokio::test]
    async fn map_step() {
        let result = Pipeline::start(MapStep::new(|s: String| s.to_uppercase()))
            .run("hello".into())
            .await
            .unwrap();
        assert_eq!(result, "HELLO");
    }

    #[tokio::test]
    async fn parallel_step() {
        let step = parallel(
            AddSuffix {
                suffix: "_a".into(),
            },
            AddSuffix {
                suffix: "_b".into(),
            },
        );
        let result = Pipeline::start(step).run("x".into()).await.unwrap();
        assert_eq!(result, ("x_a".into(), "x_b".into()));
    }

    #[tokio::test]
    async fn error_propagation() {
        struct FailStep;

        impl Step for FailStep {
            type Input = String;
            type Output = String;

            async fn run(&self, _input: Self::Input) -> Result<Self::Output, PipelineError> {
                Err(PipelineError::Custom("boom".into()))
            }
        }

        let result = Pipeline::start(AddSuffix {
            suffix: "ok".into(),
        })
        .step(FailStep)
        .run("hi".into())
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("boom"));
    }

    #[tokio::test]
    async fn extract_step() {
        use super::builtin::ExtractStep;

        let result = Pipeline::start(MapStep::new(|_: ()| r#"{"a":1,"b":"two"}"#.to_owned()))
            .step(ExtractStep::<serde_json::Value>::new())
            .run(())
            .await
            .unwrap();
        assert_eq!(result["a"], 1);
        assert_eq!(result["b"], "two");
    }

    // --- LlmStep tests ---

    #[tokio::test]
    async fn llm_step_returns_response() {
        let provider = Arc::new(MockProvider::with_responses(vec!["answer".into()]));
        let result = Pipeline::start(LlmStep::new(provider))
            .run("question".into())
            .await
            .unwrap();
        assert_eq!(result, "answer");
    }

    #[tokio::test]
    async fn llm_step_with_system_prompt() {
        let provider = Arc::new(MockProvider::with_responses(vec!["ok".into()]));
        let result = Pipeline::start(LlmStep::new(provider).with_system_prompt("sys"))
            .run("input".into())
            .await
            .unwrap();
        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn llm_step_propagates_error() {
        let provider = Arc::new(MockProvider::failing());
        let result = Pipeline::start(LlmStep::new(provider))
            .run("input".into())
            .await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), PipelineError::Llm(_)),
            "expected PipelineError::Llm"
        );
    }

    // --- RetrievalStep tests ---

    #[tokio::test]
    async fn retrieval_step_returns_results() {
        let store = Arc::new(InMemoryVectorStore::new());
        store.ensure_collection("col", 3).await.unwrap();
        store
            .upsert(
                "col",
                vec![VectorPoint {
                    id: "p1".into(),
                    vector: vec![1.0, 0.0, 0.0],
                    payload: std::collections::HashMap::new(),
                }],
            )
            .await
            .unwrap();

        let mut provider = MockProvider::default();
        provider.supports_embeddings = true;
        provider.embedding = vec![1.0, 0.0, 0.0];
        let provider = Arc::new(provider);

        let step = RetrievalStep::new(store, provider, "col", 5);
        let results = Pipeline::start(step).run("query".into()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "p1");
    }

    #[tokio::test]
    async fn retrieval_step_embed_error_propagates() {
        let store = Arc::new(InMemoryVectorStore::new());
        store.ensure_collection("col", 3).await.unwrap();

        let provider = Arc::new(MockProvider::default());

        let step = RetrievalStep::new(store, provider, "col", 5);
        let result = Pipeline::start(step).run("query".into()).await;
        assert!(matches!(result.unwrap_err(), PipelineError::Llm(_)));
    }

    // --- ExtractStep failure tests ---

    #[tokio::test]
    async fn extract_step_invalid_json() {
        let result = Pipeline::start(MapStep::new(|_: ()| "not json".to_owned()))
            .step(ExtractStep::<serde_json::Value>::new())
            .run(())
            .await;
        assert!(matches!(result.unwrap_err(), PipelineError::Extract(_)));
    }

    #[tokio::test]
    async fn extract_step_type_mismatch() {
        #[derive(Debug, serde::Deserialize)]
        struct Strict {
            #[expect(dead_code)]
            required_field: Vec<u32>,
        }

        let result = Pipeline::start(MapStep::new(|_: ()| r#"{"a":1}"#.to_owned()))
            .step(ExtractStep::<Strict>::new())
            .run(())
            .await;
        assert!(matches!(result.unwrap_err(), PipelineError::Extract(_)));
    }

    // --- ParallelStep error tests ---

    #[tokio::test]
    async fn parallel_step_first_fails() {
        struct FailStep;
        impl Step for FailStep {
            type Input = String;
            type Output = String;
            async fn run(&self, _input: Self::Input) -> Result<Self::Output, PipelineError> {
                Err(PipelineError::Custom("fail_a".into()))
            }
        }

        let step = parallel(
            FailStep,
            AddSuffix {
                suffix: "_ok".into(),
            },
        );
        let result = Pipeline::start(step).run("x".into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn parallel_step_both_fail() {
        struct FailA;
        impl Step for FailA {
            type Input = String;
            type Output = String;
            async fn run(&self, _input: Self::Input) -> Result<Self::Output, PipelineError> {
                Err(PipelineError::Custom("fail_a".into()))
            }
        }
        struct FailB;
        impl Step for FailB {
            type Input = String;
            type Output = String;
            async fn run(&self, _input: Self::Input) -> Result<Self::Output, PipelineError> {
                Err(PipelineError::Custom("fail_b".into()))
            }
        }

        let step = parallel(FailA, FailB);
        let result = Pipeline::start(step).run("x".into()).await;
        assert!(result.is_err());
    }
}
