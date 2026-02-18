use std::marker::PhantomData;
use std::sync::Arc;

use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_memory::vector_store::{ScoredVectorPoint, VectorStore};

use super::PipelineError;
use super::step::Step;

pub struct LlmStep<P> {
    provider: Arc<P>,
    system_prompt: Option<String>,
}

impl<P> LlmStep<P> {
    #[must_use]
    pub fn new(provider: Arc<P>) -> Self {
        Self {
            provider,
            system_prompt: None,
        }
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }
}

impl<P: LlmProvider> Step for LlmStep<P> {
    type Input = String;
    type Output = String;

    async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
        let mut messages = Vec::new();
        if let Some(sys) = &self.system_prompt {
            messages.push(Message::from_legacy(Role::System, sys.clone()));
        }
        messages.push(Message::from_legacy(Role::User, input));
        self.provider
            .chat(&messages)
            .await
            .map_err(PipelineError::Llm)
    }
}

pub struct RetrievalStep<P, V> {
    store: Arc<V>,
    provider: Arc<P>,
    collection: String,
    limit: u64,
}

impl<P, V> RetrievalStep<P, V> {
    #[must_use]
    pub fn new(store: Arc<V>, provider: Arc<P>, collection: impl Into<String>, limit: u64) -> Self {
        Self {
            store,
            provider,
            collection: collection.into(),
            limit,
        }
    }
}

impl<P: LlmProvider, V: VectorStore> Step for RetrievalStep<P, V> {
    type Input = String;
    type Output = Vec<ScoredVectorPoint>;

    async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
        let embedding = self
            .provider
            .embed(&input)
            .await
            .map_err(PipelineError::Llm)?;
        self.store
            .search(&self.collection, embedding, self.limit, None)
            .await
            .map_err(|e| PipelineError::Memory(e.into()))
    }
}

pub struct ExtractStep<T> {
    _marker: PhantomData<T>,
}

impl<T> ExtractStep<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<T> Default for ExtractStep<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: serde::de::DeserializeOwned + Send + Sync> Step for ExtractStep<T> {
    type Input = String;
    type Output = T;

    async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
        serde_json::from_str(&input).map_err(|e| PipelineError::Extract(e.to_string()))
    }
}

pub struct MapStep<F, In, Out> {
    f: F,
    _marker: PhantomData<fn(In) -> Out>,
}

impl<F, In, Out> MapStep<F, In, Out> {
    #[must_use]
    pub fn new(f: F) -> Self {
        Self {
            f,
            _marker: PhantomData,
        }
    }
}

impl<F, In, Out> Step for MapStep<F, In, Out>
where
    F: Fn(In) -> Out + Send + Sync,
    In: Send,
    Out: Send,
{
    type Input = In;
    type Output = Out;

    async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
        Ok((self.f)(input))
    }
}
