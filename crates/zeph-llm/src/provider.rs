use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

pub trait LlmProvider: Send + Sync {
    /// Send messages to the LLM and return the assistant response.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat(&self, messages: &[Message]) -> impl Future<Output = anyhow::Result<String>> + Send;

    fn name(&self) -> &'static str;
}
