use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::LlmError;
use crate::provider::{LlmProvider, Message, Role};

pub struct Extractor<'a, P: LlmProvider> {
    provider: &'a P,
    preamble: Option<String>,
}

impl<'a, P: LlmProvider> Extractor<'a, P> {
    pub fn new(provider: &'a P) -> Self {
        Self {
            provider,
            preamble: None,
        }
    }

    #[must_use]
    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = Some(preamble.into());
        self
    }

    /// # Errors
    ///
    /// Returns an error if the provider fails or the response cannot be parsed.
    pub async fn extract<T>(&self, input: &str) -> Result<T, LlmError>
    where
        T: DeserializeOwned + JsonSchema,
    {
        let mut messages = Vec::new();
        if let Some(ref preamble) = self.preamble {
            messages.push(Message::from_legacy(Role::System, preamble.clone()));
        }
        messages.push(Message::from_legacy(Role::User, input));
        self.provider.chat_typed::<T>(&messages).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatStream, LlmProvider, Message};

    struct StubProvider {
        response: String,
    }

    impl LlmProvider for StubProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
            Ok(self.response.clone())
        }

        async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
            let response = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(response))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
            Err(LlmError::EmbedUnsupported { provider: "stub" })
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
    struct TestOutput {
        value: String,
    }

    #[tokio::test]
    async fn extract_without_preamble() {
        let provider = StubProvider {
            response: r#"{"value": "result"}"#.into(),
        };
        let extractor = Extractor::new(&provider);
        let result: TestOutput = extractor.extract("test input").await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "result".into()
            }
        );
    }

    #[tokio::test]
    async fn extract_with_preamble() {
        let provider = StubProvider {
            response: r#"{"value": "with_preamble"}"#.into(),
        };
        let extractor = Extractor::new(&provider).with_preamble("Analyze this");
        let result: TestOutput = extractor.extract("test input").await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "with_preamble".into()
            }
        );
    }

    #[tokio::test]
    async fn extract_error_propagation() {
        struct FailProvider;

        impl LlmProvider for FailProvider {
            async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
                Err(LlmError::Unavailable)
            }

            async fn chat_stream(&self, _messages: &[Message]) -> Result<ChatStream, LlmError> {
                Err(LlmError::Unavailable)
            }

            fn supports_streaming(&self) -> bool {
                false
            }

            async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
                Err(LlmError::Unavailable)
            }

            fn supports_embeddings(&self) -> bool {
                false
            }

            fn name(&self) -> &'static str {
                "fail"
            }
        }

        let provider = FailProvider;
        let extractor = Extractor::new(&provider);
        let result = extractor.extract::<TestOutput>("test").await;
        assert!(matches!(result, Err(LlmError::Unavailable)));
    }
}
