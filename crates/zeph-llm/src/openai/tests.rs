// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::provider::ImageData;
use crate::provider::MessageMetadata;
use tokio_stream::StreamExt;

fn test_provider() -> OpenAiProvider {
    OpenAiProvider::new(
        "sk-test-key".into(),
        "https://api.openai.com/v1".into(),
        "gpt-5.2".into(),
        4096,
        Some("text-embedding-3-small".into()),
        None,
    )
}

#[test]
fn context_window_gpt4o() {
    let p = OpenAiProvider::new(
        "k".into(),
        "https://api.openai.com/v1".into(),
        "gpt-4o".into(),
        1024,
        None,
        None,
    );
    assert_eq!(p.context_window(), Some(128_000));
}

#[test]
fn context_window_gpt5() {
    assert_eq!(test_provider().context_window(), Some(1_000_000));
}

#[test]
fn context_window_unknown() {
    let p = OpenAiProvider::new(
        "k".into(),
        "https://api.openai.com/v1".into(),
        "custom-model".into(),
        1024,
        None,
        None,
    );
    assert!(p.context_window().is_none());
}

fn test_provider_no_embed() -> OpenAiProvider {
    OpenAiProvider::new(
        "sk-test-key".into(),
        "https://api.openai.com/v1".into(),
        "gpt-5.2".into(),
        4096,
        None,
        None,
    )
}

#[test]
fn new_stores_fields() {
    let p = test_provider();
    assert_eq!(p.api_key, "sk-test-key");
    assert_eq!(p.base_url, "https://api.openai.com/v1");
    assert_eq!(p.model, "gpt-5.2");
    assert_eq!(p.max_tokens, 4096);
    assert_eq!(p.embedding_model.as_deref(), Some("text-embedding-3-small"));
    assert!(p.reasoning_effort.is_none());
}

#[test]
fn new_with_reasoning_effort() {
    let p = OpenAiProvider::new(
        "key".into(),
        "https://api.openai.com/v1".into(),
        "gpt-5.2".into(),
        4096,
        None,
        Some("high".into()),
    );
    assert_eq!(p.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn clone_preserves_fields() {
    let p = test_provider();
    let c = p.clone();
    assert_eq!(c.api_key, p.api_key);
    assert_eq!(c.base_url, p.base_url);
    assert_eq!(c.model, p.model);
    assert_eq!(c.max_tokens, p.max_tokens);
    assert_eq!(c.embedding_model, p.embedding_model);
}

#[test]
fn debug_redacts_api_key() {
    let p = test_provider();
    let debug = format!("{p:?}");
    assert!(!debug.contains("sk-test-key"));
    assert!(debug.contains("<redacted>"));
    assert!(debug.contains("gpt-5.2"));
    assert!(debug.contains("api.openai.com"));
}

#[test]
fn supports_streaming_returns_true() {
    assert!(test_provider().supports_streaming());
}

#[test]
fn supports_embeddings_with_model() {
    assert!(test_provider().supports_embeddings());
}

#[test]
fn supports_embeddings_without_model() {
    assert!(!test_provider_no_embed().supports_embeddings());
}

#[test]
fn name_returns_openai() {
    assert_eq!(test_provider().name(), "openai");
}

#[test]
fn chat_request_serialization() {
    let msgs = [ApiMessage {
        role: "user",
        content: "hello",
    }];
    let body = ChatRequest {
        model: "gpt-5.2",
        messages: &msgs,
        completion_tokens: CompletionTokens::for_model("gpt-5.2", 1024),
        stream: false,
        reasoning: None,
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"model\":\"gpt-5.2\""));
    assert!(json.contains("\"max_completion_tokens\":1024"));
    assert!(!json.contains("\"max_tokens\":1024"));
    assert!(json.contains("\"role\":\"user\""));
    assert!(!json.contains("\"stream\""));
    assert!(!json.contains("\"reasoning\""));
}

#[test]
fn chat_request_serialization_non_gpt5_uses_max_tokens() {
    let msgs = [ApiMessage {
        role: "user",
        content: "hello",
    }];
    let body = ChatRequest {
        model: "gpt-4o",
        messages: &msgs,
        completion_tokens: CompletionTokens::for_model("gpt-4o", 256),
        stream: false,
        reasoning: None,
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"max_tokens\":256"));
    assert!(!json.contains("\"max_completion_tokens\""));
}

#[test]
fn chat_request_with_stream_flag() {
    let msgs = [];
    let body = ChatRequest {
        model: "gpt-5.2",
        messages: &msgs,
        completion_tokens: CompletionTokens::for_model("gpt-5.2", 100),
        stream: true,
        reasoning: None,
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"stream\":true"));
}

#[test]
fn chat_request_with_reasoning_effort() {
    let msgs = [];
    let body = ChatRequest {
        model: "gpt-5.2",
        messages: &msgs,
        completion_tokens: CompletionTokens::for_model("gpt-5.2", 100),
        stream: false,
        reasoning: Some(Reasoning { effort: "medium" }),
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"reasoning\":{\"effort\":\"medium\"}"));
}

#[test]
fn vision_chat_request_serialization_uses_gpt5_completion_tokens() {
    let body = VisionChatRequest {
        model: "gpt-5-mini",
        messages: vec![VisionApiMessage {
            role: "user".to_owned(),
            content: vec![
                OpenAiContentPart::Text {
                    text: "describe".to_owned(),
                },
                OpenAiContentPart::ImageUrl {
                    image_url: ImageUrlDetail {
                        url: "data:image/png;base64,abc".to_owned(),
                    },
                },
            ],
        }],
        completion_tokens: CompletionTokens::for_model("gpt-5-mini", 55),
        stream: false,
        reasoning: None,
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"max_completion_tokens\":55"));
    assert!(!json.contains("\"max_tokens\":55"));
}

#[test]
fn typed_chat_request_serialization_uses_gpt5_completion_tokens() {
    let msgs = [ApiMessage {
        role: "user",
        content: "hello",
    }];
    let body = TypedChatRequest {
        model: "gpt-5-mini",
        messages: &msgs,
        completion_tokens: CompletionTokens::for_model("gpt-5-mini", 88),
        response_format: ResponseFormat {
            r#type: "json_schema",
            json_schema: JsonSchemaFormat {
                name: "result",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}}
                }),
                strict: true,
            },
        },
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"max_completion_tokens\":88"));
    assert!(!json.contains("\"max_tokens\":88"));
}

#[test]
fn tool_chat_request_serialization_uses_gpt5_completion_tokens() {
    let msgs = [StructuredApiMessage {
        role: "user".to_owned(),
        content: Some("hello".to_owned()),
        tool_calls: None,
        tool_call_id: None,
    }];
    let tools = [OpenAiTool {
        r#type: "function",
        function: OpenAiFunction {
            name: "echo",
            description: "Echo input",
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}}
            })),
        },
    }];
    let body = ToolChatRequest {
        model: "gpt-5-mini",
        messages: &msgs,
        completion_tokens: CompletionTokens::for_model("gpt-5-mini", 77),
        tools: &tools,
        reasoning: None,
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"max_completion_tokens\":77"));
    assert!(!json.contains("\"max_tokens\":77"));
}

#[test]
fn parse_chat_response() {
    let json = r#"{"choices":[{"message":{"content":"Hello!"}}]}"#;
    let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].message.content, "Hello!");
}

#[test]
fn parse_embedding_response() {
    let json = r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#;
    let resp: EmbeddingResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].embedding, vec![0.1, 0.2, 0.3]);
}

#[test]
fn embedding_request_serialization() {
    let body = EmbeddingRequest {
        input: "hello world",
        model: "text-embedding-3-small",
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"input\":\"hello world\""));
    assert!(json.contains("\"model\":\"text-embedding-3-small\""));
}

#[test]
fn convert_messages_maps_roles() {
    let messages = vec![
        Message {
            role: Role::System,
            content: "system prompt".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: "user msg".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "assistant reply".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let api_msgs = convert_messages(&messages);
    assert_eq!(api_msgs.len(), 3);
    assert_eq!(api_msgs[0].role, "system");
    assert_eq!(api_msgs[0].content, "system prompt");
    assert_eq!(api_msgs[1].role, "user");
    assert_eq!(api_msgs[2].role, "assistant");
}

#[tokio::test]
async fn chat_unreachable_endpoint_errors() {
    let p = OpenAiProvider::new(
        "key".into(),
        "http://127.0.0.1:1".into(),
        "model".into(),
        100,
        None,
        None,
    );
    let messages = vec![Message {
        role: Role::User,
        content: "test".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    assert!(p.chat(&messages).await.is_err());
}

#[tokio::test]
async fn stream_unreachable_endpoint_errors() {
    let p = OpenAiProvider::new(
        "key".into(),
        "http://127.0.0.1:1".into(),
        "model".into(),
        100,
        None,
        None,
    );
    let messages = vec![Message {
        role: Role::User,
        content: "test".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    assert!(p.chat_stream(&messages).await.is_err());
}

#[tokio::test]
async fn embed_unreachable_endpoint_errors() {
    let p = OpenAiProvider::new(
        "key".into(),
        "http://127.0.0.1:1".into(),
        "model".into(),
        100,
        Some("embed-model".into()),
        None,
    );
    assert!(p.embed("test").await.is_err());
}

#[tokio::test]
async fn embed_without_model_returns_error() {
    let p = test_provider_no_embed();
    let result = p.embed("test").await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("embedding not supported")
    );
}

#[test]
fn base_url_strips_trailing_slash() {
    let p = OpenAiProvider::new(
        "key".into(),
        "https://api.openai.com/v1/".into(),
        "m".into(),
        100,
        None,
        None,
    );
    assert_eq!(p.base_url, "https://api.openai.com/v1");
}

#[test]
fn convert_messages_empty() {
    let msgs = convert_messages(&[]);
    assert!(msgs.is_empty());
}

#[test]
fn api_message_serializes() {
    let msg = ApiMessage {
        role: "user",
        content: "hello",
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"role\":\"user\""));
    assert!(json.contains("\"content\":\"hello\""));
}

#[test]
fn chat_response_empty_choices() {
    let json = r#"{"choices":[]}"#;
    let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
    assert!(resp.choices.is_empty());
}

#[test]
fn embedding_response_empty_data() {
    let json = r#"{"data":[]}"#;
    let resp: EmbeddingResponse = serde_json::from_str(json).unwrap();
    assert!(resp.data.is_empty());
}

#[tokio::test]
#[ignore = "requires ZEPH_OPENAI_API_KEY env var"]
async fn integration_openai_chat() {
    let api_key = std::env::var("ZEPH_OPENAI_API_KEY").expect("ZEPH_OPENAI_API_KEY must be set");
    let provider = OpenAiProvider::new(
        api_key,
        "https://api.openai.com/v1".into(),
        "gpt-5.2".into(),
        256,
        None,
        None,
    );

    let messages = vec![Message {
        role: Role::User,
        content: "Reply with exactly: pong".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];

    let response = provider.chat(&messages).await.unwrap();
    assert!(response.to_lowercase().contains("pong"));
}

#[tokio::test]
#[ignore = "requires ZEPH_OPENAI_API_KEY env var"]
async fn integration_openai_chat_stream() {
    let api_key = std::env::var("ZEPH_OPENAI_API_KEY").expect("ZEPH_OPENAI_API_KEY must be set");
    let provider = OpenAiProvider::new(
        api_key,
        "https://api.openai.com/v1".into(),
        "gpt-5.2".into(),
        256,
        None,
        None,
    );

    let messages = vec![Message {
        role: Role::User,
        content: "Reply with exactly: pong".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];

    let mut stream = provider.chat_stream(&messages).await.unwrap();
    let mut full_response = String::new();

    while let Some(result) = stream.next().await {
        if let crate::StreamChunk::Content(text) = result.unwrap() {
            full_response.push_str(&text);
        }
    }

    assert!(!full_response.is_empty());
    assert!(full_response.to_lowercase().contains("pong"));
}

#[test]
fn context_window_gpt35() {
    let p = OpenAiProvider::new(
        "k".into(),
        "https://api.openai.com/v1".into(),
        "gpt-3.5-turbo".into(),
        1024,
        None,
        None,
    );
    assert_eq!(p.context_window(), Some(16_385));
}

#[test]
fn context_window_gpt4_turbo() {
    let p = OpenAiProvider::new(
        "k".into(),
        "https://api.openai.com/v1".into(),
        "gpt-4-turbo".into(),
        1024,
        None,
        None,
    );
    assert_eq!(p.context_window(), Some(128_000));
}

#[tokio::test]
#[ignore = "requires ZEPH_OPENAI_API_KEY env var"]
async fn integration_openai_embed() {
    let api_key = std::env::var("ZEPH_OPENAI_API_KEY").expect("ZEPH_OPENAI_API_KEY must be set");
    let provider = OpenAiProvider::new(
        api_key,
        "https://api.openai.com/v1".into(),
        "gpt-5.2".into(),
        256,
        Some("text-embedding-3-small".into()),
        None,
    );

    let embedding = provider.embed("Hello world").await.unwrap();
    assert!(!embedding.is_empty());
}

#[test]
fn supports_tool_use_returns_true() {
    assert!(test_provider().supports_tool_use());
}

#[test]
fn openai_tool_serialization() {
    let tool = OpenAiTool {
        r#type: "function",
        function: OpenAiFunction {
            name: "bash",
            description: "Execute a shell command",
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            })),
        },
    };
    let json = serde_json::to_string(&tool).unwrap();
    assert!(json.contains("\"type\":\"function\""));
    assert!(json.contains("\"name\":\"bash\""));
    assert!(json.contains("\"parameters\""));
}

#[test]
fn prepare_tool_params_empty_object_returns_none() {
    let empty = serde_json::json!({"type": "object", "properties": {}});
    assert!(prepare_tool_params(&empty).is_none());
}

#[test]
fn prepare_tool_params_no_properties_key_returns_none() {
    let empty = serde_json::json!({"type": "object"});
    assert!(prepare_tool_params(&empty).is_none());
}

#[test]
fn prepare_tool_params_non_empty_normalizes_strict() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"name": {"type": "string"}}
    });
    let result = prepare_tool_params(&schema).expect("non-empty should return Some");
    assert_eq!(result["additionalProperties"], false);
    assert!(result["required"].as_array().is_some());
}

#[test]
fn openai_tool_empty_params_omitted_in_serialization() {
    let tool = OpenAiTool {
        r#type: "function",
        function: OpenAiFunction {
            name: "list_tasks",
            description: "List all tasks",
            parameters: None,
        },
    };
    let json = serde_json::to_string(&tool).unwrap();
    assert!(!json.contains("parameters"));
}

#[test]
fn parse_tool_chat_response_with_tool_calls() {
    let json = r#"{
            "choices": [{
                "message": {
                    "content": "I'll run that",
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                }
            }]
        }"#;
    let resp: ToolChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.choices.len(), 1);
    let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0].id, "call_123");
    assert_eq!(tc[0].function.name, "bash");
}

#[test]
fn parse_tool_chat_response_text_only() {
    let json = r#"{"choices":[{"message":{"content":"Hello!"}}]}"#;
    let resp: ToolChatResponse = serde_json::from_str(json).unwrap();
    assert!(resp.choices[0].message.tool_calls.is_none());
}

#[test]
fn parse_tool_chat_response_with_null_content_and_tool_calls() {
    let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                }
            }]
        }"#;
    let resp: ToolChatResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.choices[0].message.content, "");
    let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0].function.name, "bash");
}

#[test]
fn convert_messages_structured_with_tool_parts() {
    let messages = vec![
        Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::Text {
                    text: "Running command".into(),
                },
                MessagePart::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
        ),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file1.rs".into(),
                is_error: false,
            }],
        ),
    ];
    let result = convert_messages_structured(&messages);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].role, "assistant");
    assert!(result[0].tool_calls.is_some());
    assert_eq!(result[1].role, "tool");
    assert_eq!(result[1].tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn convert_messages_structured_plain_messages() {
    let messages = vec![Message::from_legacy(Role::User, "hello")];
    let result = convert_messages_structured(&messages);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, "user");
    assert_eq!(result[0].content.as_deref(), Some("hello"));
    assert!(result[0].tool_calls.is_none());
}

#[test]
fn convert_messages_structured_assistant_tool_only_content_is_none() {
    // When assistant message has tool_calls but no text, content must be None (not "")
    // OpenAI API rejects "content": "" combined with "tool_calls" with HTTP 400
    let messages = vec![Message::from_parts(
        Role::Assistant,
        vec![MessagePart::ToolUse {
            id: "call_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }],
    )];
    let result = convert_messages_structured(&messages);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, "assistant");
    assert!(
        result[0].content.is_none(),
        "content must be None (not \"\") for tool-only assistant messages"
    );
    assert!(result[0].tool_calls.is_some());
}

#[test]
fn parse_usage_with_cached_tokens() {
    let json = r#"{
            "prompt_tokens": 2006,
            "completion_tokens": 300,
            "prompt_tokens_details": {
                "cached_tokens": 1920
            }
        }"#;
    let usage: OpenAiUsage = serde_json::from_str(json).unwrap();
    assert_eq!(usage.prompt_tokens, 2006);
    assert_eq!(usage.completion_tokens, 300);
    assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 1920);
}

#[test]
fn parse_usage_without_cached_tokens() {
    let json = r#"{"prompt_tokens": 100, "completion_tokens": 50}"#;
    let usage: OpenAiUsage = serde_json::from_str(json).unwrap();
    assert!(usage.prompt_tokens_details.is_none());
}

#[test]
fn parse_chat_response_with_usage() {
    let json = r#"{
            "choices": [{"message": {"content": "Hello!"}}],
            "usage": {
                "prompt_tokens": 500,
                "completion_tokens": 100,
                "prompt_tokens_details": {"cached_tokens": 400}
            }
        }"#;
    let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
    let usage = resp.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 500);
    assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 400);
}

#[test]
fn parse_chat_response_without_usage() {
    let json = r#"{"choices":[{"message":{"content":"Hi"}}]}"#;
    let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
    assert!(resp.usage.is_none());
}

#[test]
fn last_cache_usage_initially_none() {
    let p = test_provider();
    assert!(p.last_cache_usage().is_none());
}

#[test]
fn last_usage_initially_none() {
    let p = test_provider();
    assert!(p.last_usage().is_none());
}

#[test]
fn store_cache_usage_stores_token_counts() {
    let p = test_provider();
    let usage = OpenAiUsage {
        prompt_tokens: 1000,
        completion_tokens: 200,
        prompt_tokens_details: None,
    };
    p.store_cache_usage(&usage);
    let (prompt, completion) = p.last_usage().unwrap();
    assert_eq!(prompt, 1000);
    assert_eq!(completion, 200);
}

#[test]
fn clone_resets_last_usage() {
    let p = test_provider();
    let usage = OpenAiUsage {
        prompt_tokens: 500,
        completion_tokens: 100,
        prompt_tokens_details: None,
    };
    p.store_cache_usage(&usage);
    assert!(p.last_usage().is_some());
    let cloned = p.clone();
    assert!(cloned.last_usage().is_none());
}

#[test]
fn store_and_retrieve_cache_usage() {
    let p = test_provider();
    let usage = OpenAiUsage {
        prompt_tokens: 1000,
        completion_tokens: 200,
        prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 800 }),
    };
    p.store_cache_usage(&usage);
    let (creation, read) = p.last_cache_usage().unwrap();
    assert_eq!(creation, 0);
    assert_eq!(read, 800);
}

#[test]
fn store_cache_usage_zero_cached_tokens_not_stored() {
    let p = test_provider();
    let usage = OpenAiUsage {
        prompt_tokens: 100,
        completion_tokens: 50,
        prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 0 }),
    };
    p.store_cache_usage(&usage);
    assert!(p.last_cache_usage().is_none());
}

#[test]
fn clone_resets_last_cache() {
    let p = test_provider();
    let usage = OpenAiUsage {
        prompt_tokens: 500,
        completion_tokens: 100,
        prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 400 }),
    };
    p.store_cache_usage(&usage);
    assert!(p.last_cache_usage().is_some());
    let cloned = p.clone();
    assert!(cloned.last_cache_usage().is_none());
}

#[test]
fn has_image_parts_detects_image() {
    let msg_with_image = Message::from_parts(
        Role::User,
        vec![
            MessagePart::Text {
                text: "look".into(),
            },
            MessagePart::Image(Box::new(ImageData {
                data: vec![1, 2, 3],
                mime_type: "image/png".into(),
            })),
        ],
    );
    let msg_text_only = Message::from_legacy(Role::User, "plain");
    assert!(has_image_parts(&[msg_with_image]));
    assert!(!has_image_parts(&[msg_text_only]));
    assert!(!has_image_parts(&[]));
}

#[test]
fn convert_messages_vision_produces_data_uri() {
    let data = vec![0xFFu8, 0xD8, 0xFF]; // JPEG magic bytes
    let msg = Message::from_parts(
        Role::User,
        vec![
            MessagePart::Text {
                text: "describe this".into(),
            },
            MessagePart::Image(Box::new(ImageData {
                data: data.clone(),
                mime_type: "image/jpeg".into(),
            })),
        ],
    );
    let converted = convert_messages_vision(&[msg]);
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].role, "user");
    // Should have text part + image_url part
    assert_eq!(converted[0].content.len(), 2);
    match &converted[0].content[0] {
        OpenAiContentPart::Text { text } => assert_eq!(text, "describe this"),
        OpenAiContentPart::ImageUrl { .. } => panic!("expected Text part first"),
    }
    match &converted[0].content[1] {
        OpenAiContentPart::ImageUrl { image_url } => {
            use base64::{Engine, engine::general_purpose::STANDARD};
            let expected = format!("data:image/jpeg;base64,{}", STANDARD.encode(&data));
            assert_eq!(image_url.url, expected);
        }
        OpenAiContentPart::Text { .. } => panic!("expected ImageUrl part second"),
    }
}

#[test]
fn convert_messages_vision_text_only_message() {
    let msg = Message::from_legacy(Role::System, "system prompt");
    let converted = convert_messages_vision(&[msg]);
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].role, "system");
    assert_eq!(converted[0].content.len(), 1);
    match &converted[0].content[0] {
        OpenAiContentPart::Text { text } => assert_eq!(text, "system prompt"),
        OpenAiContentPart::ImageUrl { .. } => panic!("expected Text part"),
    }
}

#[test]
fn convert_messages_vision_image_only_no_text_part() {
    let msg = Message::from_parts(
        Role::User,
        vec![MessagePart::Image(Box::new(ImageData {
            data: vec![1],
            mime_type: "image/png".into(),
        }))],
    );
    let converted = convert_messages_vision(&[msg]);
    // No text parts collected → only image_url
    assert_eq!(converted[0].content.len(), 1);
    assert!(matches!(
        &converted[0].content[0],
        OpenAiContentPart::ImageUrl { .. }
    ));
}

#[test]
fn cache_slug_openai() {
    let p = OpenAiProvider::new(
        "k".into(),
        "https://api.openai.com/v1".into(),
        "gpt-4o".into(),
        1024,
        None,
        None,
    );
    assert_eq!(p.cache_slug(), "api_openai_com");
}

#[test]
fn cache_slug_custom_host() {
    let p = OpenAiProvider::new(
        "k".into(),
        "https://my-llm.example.com/v1".into(),
        "model".into(),
        1024,
        None,
        None,
    );
    assert_eq!(p.cache_slug(), "my_llm_example_com");
}

#[test]
fn cache_slug_localhost_with_port() {
    let p = OpenAiProvider::new(
        "k".into(),
        "http://localhost:8080/v1".into(),
        "model".into(),
        1024,
        None,
        None,
    );
    assert_eq!(p.cache_slug(), "localhost");
}

// Tests for list_models_remote JSON response parsing (inline, no HTTP server needed).
#[test]
fn list_models_response_parses_data_array() {
    let page = serde_json::json!({
        "data": [
            {"id": "gpt-4o", "created": 1_700_000_000i64},
            {"id": "gpt-3.5-turbo", "created": 1_600_000_000i64}
        ]
    });
    let models: Vec<crate::model_cache::RemoteModelInfo> = page
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_str()?.to_string();
                    let created_at = item.get("created").and_then(serde_json::Value::as_i64);
                    Some(crate::model_cache::RemoteModelInfo {
                        display_name: id.clone(),
                        id,
                        context_window: None,
                        created_at,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "gpt-4o");
    assert_eq!(models[0].created_at, Some(1_700_000_000));
    assert_eq!(models[1].id, "gpt-3.5-turbo");
}

#[test]
fn list_models_response_empty_data_array() {
    let page = serde_json::json!({"data": []});
    let models: Vec<crate::model_cache::RemoteModelInfo> = page
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_str()?.to_string();
                    Some(crate::model_cache::RemoteModelInfo {
                        display_name: id.clone(),
                        id,
                        context_window: None,
                        created_at: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(models.is_empty());
}

#[test]
fn list_models_response_missing_data_key_returns_empty() {
    let page = serde_json::json!({"error": "some error"});
    let models: Vec<crate::model_cache::RemoteModelInfo> = page
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_str()?.to_string();
                    Some(crate::model_cache::RemoteModelInfo {
                        display_name: id.clone(),
                        id,
                        context_window: None,
                        created_at: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(models.is_empty());
}

#[tokio::test]
async fn list_models_remote_http_error_propagates() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(
        "key".into(),
        server.uri(),
        "gpt-4o".into(),
        1024,
        None,
        None,
    );
    let result = p.list_models_remote().await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("500"));
}

// ------------------------------------------------------------------
// Wiremock HTTP-level tests using fixture helpers from testing module
// ------------------------------------------------------------------

#[tokio::test]
async fn chat_happy_path_wiremock() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use crate::testing::openai_chat_response;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_chat_response("hello from mock"))
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(
        "sk-test".into(),
        server.uri(),
        "gpt-4o".into(),
        256,
        None,
        None,
    );
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let result = p.chat(&messages).await.unwrap();
    assert_eq!(result, "hello from mock");
}

#[tokio::test]
async fn chat_with_tools_handles_null_assistant_content() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let response = serde_json::json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": "{\"command\":\"ls\"}"
                    }
                }]
            }
        }]
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(
        "sk-test".into(),
        server.uri(),
        "gpt-4o".into(),
        256,
        None,
        None,
    );
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "Execute a shell command".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        }),
    }];

    let result = p.chat_with_tools(&messages, &tools).await.unwrap();
    match result {
        ChatResponse::ToolUse {
            text, tool_calls, ..
        } => {
            assert_eq!(text, None);
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call_123");
            assert_eq!(tool_calls[0].name, "bash");
            assert_eq!(tool_calls[0].input, serde_json::json!({"command": "ls"}));
        }
        other @ ChatResponse::Text(_) => panic!("expected ToolUse response, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_429_rate_limit_propagates() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use crate::testing::openai_rate_limit_response;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_rate_limit_response())
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(
        "sk-test".into(),
        server.uri(),
        "gpt-4o".into(),
        256,
        None,
        None,
    );
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let result = p.chat(&messages).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn chat_401_auth_error_propagates() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use crate::testing::openai_auth_error_response;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_auth_error_response())
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(
        "bad-key".into(),
        server.uri(),
        "gpt-4o".into(),
        256,
        None,
        None,
    );
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let result = p.chat(&messages).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn chat_500_server_error_propagates() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use crate::testing::openai_server_error_response;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_server_error_response())
        .mount(&server)
        .await;

    let p = OpenAiProvider::new(
        "sk-test".into(),
        server.uri(),
        "gpt-4o".into(),
        256,
        None,
        None,
    );
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let result = p.chat(&messages).await;
    assert!(result.is_err());
}

#[test]
fn with_generation_overrides_stores_overrides() {
    let provider = OpenAiProvider::new(
        "sk-test".into(),
        "http://localhost".into(),
        "gpt-4o".into(),
        256,
        None,
        None,
    );
    assert!(provider.generation_overrides.is_none());
    let overrides = GenerationOverrides {
        temperature: Some(0.7),
        top_p: Some(0.95),
        top_k: None,
        frequency_penalty: Some(0.1),
        presence_penalty: Some(0.2),
    };
    let patched = provider.with_generation_overrides(overrides);
    let ov = patched
        .generation_overrides
        .as_ref()
        .expect("overrides set");
    assert_eq!(ov.temperature, Some(0.7));
    assert_eq!(ov.top_p, Some(0.95));
    assert_eq!(ov.frequency_penalty, Some(0.1));
    assert_eq!(ov.presence_penalty, Some(0.2));
}

#[test]
fn normalize_for_openai_strict_adds_additional_properties() {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "age": {"type": "integer"}
        }
    });
    normalize_for_openai_strict(&mut schema, 8);
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema["required"].as_array().is_some());
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "name"));
    assert!(required.iter().any(|v| v == "age"));
}

#[test]
fn normalize_for_openai_strict_preserves_anyof_for_option() {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "value": {"type": "string"},
            "opt": {
                "anyOf": [{"type": "string"}, {"type": "null"}]
            }
        }
    });
    normalize_for_openai_strict(&mut schema, 8);
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "opt"));
    assert!(schema["properties"]["opt"].get("anyOf").is_some());
}

#[test]
fn inline_refs_openai_resolves_defs() {
    let mut schema = serde_json::json!({
        "$defs": {
            "Foo": {"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"]}
        },
        "type": "object",
        "properties": {
            "foo": {"$ref": "#/$defs/Foo"}
        }
    });
    inline_refs_openai(&mut schema, 8);
    assert!(schema.get("$defs").is_none());
    assert!(schema["properties"]["foo"].get("$ref").is_none());
    assert_eq!(schema["properties"]["foo"]["type"], "object");
}

#[test]
fn normalize_nested_objects_get_additional_properties() {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "inner": {
                "type": "object",
                "properties": {
                    "x": {"type": "string"}
                }
            }
        }
    });
    normalize_for_openai_strict(&mut schema, 16);
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["inner"]["additionalProperties"], false);
}

#[test]
fn convert_messages_structured_preserves_internal_variants() {
    // Recall/CodeContext/Summary/CrossSession in tool-use messages must not be silently dropped.
    // Previously, the wildcard `_ => {}` in convert_messages_structured() dropped these variants.
    let messages = vec![
        // Assistant message with Recall + ToolUse: Recall text must appear in content
        Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::Recall {
                    text: "past context".into(),
                },
                MessagePart::ToolUse {
                    id: "call_1".into(),
                    name: "search".into(),
                    input: serde_json::json!({}),
                },
            ],
        ),
        // User message with ToolResult + CodeContext: CodeContext must not be dropped
        Message::from_parts(
            Role::User,
            vec![
                MessagePart::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "result".into(),
                    is_error: false,
                },
                MessagePart::CodeContext {
                    text: "fn main() {}".into(),
                },
            ],
        ),
    ];
    let result = convert_messages_structured(&messages);
    // Assistant message: content must include the Recall text
    assert_eq!(result[0].role, "assistant");
    assert_eq!(result[0].content.as_deref(), Some("past context"));
    assert!(result[0].tool_calls.is_some());
    // User messages: ToolResult becomes role "tool", CodeContext becomes role "user"
    let tool_msg = result
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool message");
    assert_eq!(tool_msg.content.as_deref(), Some("result"));
    let user_msg = result
        .iter()
        .find(|m| m.role == "user")
        .expect("user message");
    assert_eq!(user_msg.content.as_deref(), Some("fn main() {}"));
}
