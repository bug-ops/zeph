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
    use super::builtin::MapStep;
    use super::parallel::parallel;
    use super::*;

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
}
