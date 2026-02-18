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
