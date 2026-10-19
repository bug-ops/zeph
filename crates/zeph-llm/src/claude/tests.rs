// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::cache::cache_min_tokens;
use std::assert_matches;

use super::types::{
    ApiMessage, ApiResponse, ApiUsage, CACHE_MARKER_STABLE, CACHE_MARKER_TOOLS,
    CACHE_MARKER_VOLATILE, CacheControl, CacheType, ContentBlock, ImageSource, StructuredContent,
    ThinkingParam,
};
use super::*;
use crate::CacheTtl;
use crate::provider::{ImageData, MessageMetadata, Role, ThinkingBlock};
use crate::testing::TEST_CLAUDE_MODEL;
use tokio_stream::StreamExt;

#[test]
fn context_window_known_models() {
    let sonnet = ClaudeProvider::new("k".into(), "claude-sonnet-4-5-20250929".into(), 1024);
    assert_eq!(sonnet.context_window(), Some(200_000));

    let opus = ClaudeProvider::new("k".into(), "claude-opus-4-8".into(), 1024);
    assert_eq!(opus.context_window(), Some(200_000));

    let haiku = ClaudeProvider::new("k".into(), "claude-haiku-4-5".into(), 1024);
    assert_eq!(haiku.context_window(), Some(200_000));
}

#[test]
fn context_window_unknown_model() {
    let provider = ClaudeProvider::new("k".into(), "unknown-model".into(), 1024);
    assert!(provider.context_window().is_none());
}

#[test]
fn stale_model_suggestion_old_gen_below_floor_returns_some() {
    assert_eq!(
        stale_model_suggestion("claude-sonnet-4-6"),
        Some("claude-sonnet-5")
    );
    assert_eq!(
        stale_model_suggestion("claude-opus-4-6"),
        Some("claude-opus-4-8")
    );
    assert_eq!(
        stale_model_suggestion("claude-opus-4-7"),
        Some("claude-opus-4-8")
    );
}

#[test]
fn stale_model_suggestion_current_gen_returns_none() {
    assert_eq!(stale_model_suggestion("claude-sonnet-5"), None);
    assert_eq!(stale_model_suggestion("claude-opus-4-8"), None);
    assert_eq!(stale_model_suggestion("claude-haiku-4-5-20251001"), None);
}

#[test]
fn stale_model_suggestion_future_version_fails_open() {
    assert_eq!(stale_model_suggestion("claude-opus-4-9"), None);
    assert_eq!(stale_model_suggestion("claude-sonnet-6"), None);
}

#[test]
fn stale_model_suggestion_legacy_claude_3_returns_some() {
    assert_eq!(
        stale_model_suggestion("claude-3-opus"),
        Some("claude-sonnet-5 or claude-haiku-4-5-20251001")
    );
    assert_eq!(
        stale_model_suggestion("claude-3-5-sonnet"),
        Some("claude-sonnet-5 or claude-haiku-4-5-20251001")
    );
}

#[test]
fn stale_model_suggestion_non_claude_or_alias_fails_open() {
    assert_eq!(stale_model_suggestion("gpt-4o"), None);
    assert_eq!(stale_model_suggestion("claude-opus-latest"), None);
    assert_eq!(stale_model_suggestion("claude-unknownfamily-9"), None);
}

#[test]
fn name_defaults_to_claude() {
    let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-5-20250929".into(), 1024);
    assert_eq!(provider.name(), "claude");
}

#[test]
fn with_provider_name_overrides_name() {
    let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-5-20250929".into(), 1024)
        .with_provider_name("quality-claude");
    assert_eq!(provider.name(), "quality-claude");
}

#[test]
fn supports_streaming_returns_true() {
    let provider =
        ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
    assert!(provider.supports_streaming());
}

#[test]
fn debug_redacts_api_key() {
    let provider = ClaudeProvider::new(
        "sk-secret-key".into(),
        "claude-sonnet-4-5-20250929".into(),
        1024,
    );
    let debug_output = format!("{provider:?}");
    assert!(!debug_output.contains("sk-secret-key"));
    assert!(debug_output.contains("<redacted>"));
    assert!(debug_output.contains("claude-sonnet-4-5-20250929"));
}

#[test]
fn claude_supports_embeddings_returns_false() {
    let provider =
        ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
    assert!(!provider.supports_embeddings());
}

#[tokio::test]
async fn claude_embed_returns_error() {
    let provider =
        ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
    let result = provider.embed("test").await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string()
            .contains("embedding not supported by claude")
    );
}

#[test]
fn name_returns_claude() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
    assert_eq!(provider.name(), "claude");
}

#[test]
fn clone_preserves_fields() {
    let provider = ClaudeProvider::new(
        "test-api-key".into(),
        "claude-sonnet-4-5-20250929".into(),
        2048,
    );
    let cloned = provider.clone();
    assert_eq!(cloned.model, provider.model);
    assert_eq!(cloned.api_key, provider.api_key);
    assert_eq!(cloned.max_tokens, provider.max_tokens);
}

#[test]
fn new_stores_fields_correctly() {
    let provider = ClaudeProvider::new("my-key".into(), "claude-haiku-35".into(), 4096);
    assert_eq!(provider.api_key, "my-key");
    assert_eq!(provider.model, "claude-haiku-35");
    assert_eq!(provider.max_tokens, 4096);
}

#[test]
fn debug_includes_model_and_max_tokens() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 512);
    let debug = format!("{provider:?}");
    assert!(debug.contains("ClaudeProvider"));
    assert!(debug.contains("512"));
    assert!(debug.contains("<reqwest::Client>"));
}

#[test]
fn request_body_serializes_without_system() {
    let history = GatedPlainHistory::new(vec![ApiMessage {
        role: "user",
        content: "hello".into(),
    }]);
    let body = RequestBody {
        model: "claude-sonnet-4-5-20250929",
        max_tokens: 1024,
        system: None,
        messages: &history,
        stream: false,
        thinking: None,
        output_config: None,
        temperature: None,
        context_management: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(!json.contains("system"));
    assert!(!json.contains("stream"));
    assert!(json.contains("\"model\":\"claude-sonnet-4-5-20250929\""));
    assert!(json.contains("\"max_tokens\":1024"));
}

#[test]
fn request_body_serializes_with_system_blocks() {
    let history = GatedPlainHistory::new(vec![]);
    let body = RequestBody {
        model: "claude-sonnet-4-5-20250929",
        max_tokens: 1024,
        system: Some(vec![SystemContentBlock {
            block_type: "text",
            text: "You are helpful.".into(),
            cache_control: Some(CacheControl {
                cache_type: CacheType::Ephemeral,
                ttl: None,
            }),
        }]),
        messages: &history,
        stream: false,
        thinking: None,
        output_config: None,
        temperature: None,
        context_management: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"system\""));
    assert!(json.contains("You are helpful."));
    assert!(json.contains("\"cache_control\""));
}

#[test]
fn request_body_serializes_stream_true() {
    let history = GatedPlainHistory::new(vec![]);
    let body = RequestBody {
        model: "test",
        max_tokens: 100,
        system: None,
        messages: &history,
        stream: true,
        thinking: None,
        output_config: None,
        temperature: None,
        context_management: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(json.contains("\"stream\":true"));
}

#[test]
fn api_message_serializes() {
    let msg = ApiMessage {
        role: "user",
        content: "hello world".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"role\":\"user\""));
    assert!(json.contains("\"content\":\"hello world\""));
}

#[test]
fn content_block_deserializes() {
    let json = r#"{"text":"response text"}"#;
    let block: ContentBlock = serde_json::from_str(json).unwrap();
    assert_eq!(block.text, "response text");
}

#[test]
fn api_response_multiple_content_blocks() {
    let json = r#"{"content":[{"text":"first"},{"text":"second"}]}"#;
    let resp: ApiResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.content.len(), 2);
    assert_eq!(resp.content[0].text, "first");
    assert_eq!(resp.content[1].text, "second");
}

#[tokio::test]
async fn chat_with_unreachable_endpoint_errors() {
    let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
    let messages = vec![Message {
        role: Role::User,
        content: "test".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let result = provider.chat(&messages).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn chat_stream_with_unreachable_endpoint_errors() {
    let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
    let messages = vec![Message {
        role: Role::User,
        content: "test".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let result = provider.chat_stream(&messages).await;
    assert!(result.is_err());
}

#[test]
fn request_body_serializes_with_stream_false_omits_stream() {
    let history = GatedPlainHistory::new(vec![]);
    let body = RequestBody {
        model: "test",
        max_tokens: 100,
        system: None,
        messages: &history,
        stream: false,
        thinking: None,
        output_config: None,
        temperature: None,
        context_management: None,
    };
    let json = serde_json::to_string(&body).unwrap();
    assert!(!json.contains("stream"));
}

#[test]
fn split_system_no_markers_caches_entire_block() {
    // Text must meet the 2048-token threshold for sonnet (≈ 8192 chars).
    let long_text = format!("You are Zeph, an AI assistant. {}", "x".repeat(8200));
    let blocks = split_system_into_blocks(&long_text, TEST_CLAUDE_MODEL, None);
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].cache_control.is_some());
    assert!(blocks[0].text.contains("Zeph"));
}

#[test]
fn split_system_no_markers_short_text_skips_cache() {
    let blocks =
        split_system_into_blocks("You are Zeph, an AI assistant.", TEST_CLAUDE_MODEL, None);
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].cache_control.is_none());
}

#[test]
fn split_system_no_markers_exact_threshold_sonnet_caches() {
    // Exactly 8192 chars => 8192 / 4 = 2048 tokens == sonnet threshold: should cache.
    let exact_text = "A".repeat(8192);
    let blocks = split_system_into_blocks(&exact_text, TEST_CLAUDE_MODEL, None);
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].cache_control.is_some());
}

#[test]
fn split_system_no_markers_opus_skips_short_text() {
    // 8192 chars = 2048 tokens < 4096 opus minimum — no cache.
    let medium_text = "A".repeat(8192);
    let blocks = split_system_into_blocks(&medium_text, "claude-opus-4-8", None);
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].cache_control.is_none());
}

#[test]
fn split_system_no_markers_opus_caches_long_text() {
    // 16384 chars = 4096 tokens >= 4096 opus minimum — should cache.
    let long_text = "A".repeat(16384);
    let blocks = split_system_into_blocks(&long_text, "claude-opus-4-8", None);
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].cache_control.is_some());
}

#[test]
fn split_system_with_all_markers() {
    // Each block must exceed 2048 tokens (≈ 8192 chars) for sonnet threshold
    let padding = "x".repeat(8200);
    let system = format!(
        "base prompt {padding}\n{CACHE_MARKER_STABLE}\nskills here {padding}\n\
         {CACHE_MARKER_TOOLS}\ntool catalog {padding}\n\
         {CACHE_MARKER_VOLATILE}\nvolatile stuff"
    );
    let blocks = split_system_into_blocks(&system, TEST_CLAUDE_MODEL, None);
    assert_eq!(blocks.len(), 4);
    assert!(blocks[0].cache_control.is_some());
    assert!(blocks[0].text.contains("base prompt"));
    assert!(blocks[1].cache_control.is_some());
    assert!(blocks[1].text.contains("skills here"));
    assert!(blocks[2].cache_control.is_some());
    assert!(blocks[2].text.contains("tool catalog"));
    assert!(blocks[3].cache_control.is_none());
    assert!(blocks[3].text.contains("volatile stuff"));
}

#[test]
fn split_system_partial_markers() {
    let padding = "x".repeat(8200);
    let system = format!("base prompt {padding}\n{CACHE_MARKER_VOLATILE}\nvolatile only");
    let blocks = split_system_into_blocks(&system, TEST_CLAUDE_MODEL, None);
    assert_eq!(blocks.len(), 2);
    assert!(blocks[0].cache_control.is_some());
    assert!(blocks[1].cache_control.is_none());
}

#[test]
fn split_system_block1_padded_when_below_threshold() {
    // Block 1 is below 2048 tokens but gets padded with AGENT_IDENTITY_PREAMBLE,
    // so it must receive cache_control after padding.
    let system = format!("short text\n{CACHE_MARKER_STABLE}\nmore content");
    let blocks = split_system_into_blocks(&system, TEST_CLAUDE_MODEL, None);
    // Block 1 must be padded and cached
    assert!(blocks[0].cache_control.is_some());
    assert!(blocks[0].text.contains("short text"));
    assert!(blocks[0].text.contains("Agent Identity"));
}

#[test]
fn split_system_block2_not_padded_when_below_threshold() {
    // Only Block 1 (first cacheable block) gets the identity preamble padding.
    // Subsequent blocks below threshold should NOT be padded.
    let padding = "x".repeat(8200);
    let system =
        format!("base {padding}\n{CACHE_MARKER_STABLE}\nshort\n{CACHE_MARKER_TOOLS}\nmore");
    let blocks = split_system_into_blocks(&system, TEST_CLAUDE_MODEL, None);
    // Block 2 ("short") is below threshold and must NOT contain identity preamble
    assert!(!blocks[1].text.contains("Agent Identity"));
}

#[test]
fn api_usage_deserialization() {
    let json = r#"{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":1000,"cache_read_input_tokens":900}"#;
    let usage: ApiUsage = serde_json::from_str(json).unwrap();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.cache_creation_input_tokens, 1000);
    assert_eq!(usage.cache_read_input_tokens, 900);
}

#[test]
fn api_response_with_usage() {
    let json = r#"{"content":[{"text":"Hello"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#;
    let resp: ApiResponse = serde_json::from_str(json).unwrap();
    assert!(resp.usage.is_some());
    assert_eq!(resp.usage.unwrap().input_tokens, 10);
}

#[test]
fn api_response_deserializes() {
    let json = r#"{"content":[{"text":"Hello world"}]}"#;
    let resp: ApiResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.content.len(), 1);
    assert_eq!(resp.content[0].text, "Hello world");
}

#[test]
fn api_response_empty_content() {
    let json = r#"{"content":[]}"#;
    let resp: ApiResponse = serde_json::from_str(json).unwrap();
    assert!(resp.content.is_empty());
}

#[tokio::test]
#[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
async fn integration_claude_chat() {
    let api_key = std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
    let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

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
#[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
async fn integration_claude_chat_stream() {
    let api_key = std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
    let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

    let messages = vec![Message {
        role: Role::User,
        content: "Reply with exactly: pong".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];

    let mut stream = provider.chat_stream(&messages).await.unwrap();
    let mut full_response = String::new();
    let mut chunk_count = 0;

    while let Some(result) = stream.next().await {
        if let crate::StreamChunk::Content(text) = result.unwrap() {
            full_response.push_str(&text);
        }
        chunk_count += 1;
    }

    assert!(!full_response.is_empty());
    assert!(full_response.to_lowercase().contains("pong"));
    assert!(chunk_count >= 1);
}

#[tokio::test]
#[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
async fn integration_claude_stream_matches_chat() {
    let api_key = std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
    let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

    let messages = vec![Message {
        role: Role::User,
        content: "What is 2+2? Reply with just the number.".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];

    let chat_response = provider.chat(&messages).await.unwrap();

    let mut stream = provider.chat_stream(&messages).await.unwrap();
    let mut stream_response = String::new();
    while let Some(result) = stream.next().await {
        if let crate::StreamChunk::Content(text) = result.unwrap() {
            stream_response.push_str(&text);
        }
    }

    assert!(chat_response.contains('4'));
    assert!(stream_response.contains('4'));
}

#[test]
fn anthropic_tool_serialization() {
    let tool = AnthropicTool {
        name: "bash",
        description: "Execute a shell command",
        input_schema: &serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"}
            },
            "required": ["command"]
        }),
    };
    let json = serde_json::to_string(&tool).unwrap();
    assert!(json.contains("\"name\":\"bash\""));
    assert!(json.contains("\"input_schema\""));
}

#[test]
fn parse_tool_response_text_only() {
    let resp = ToolApiResponse {
        content: vec![AnthropicContentBlock::Text {
            text: "Hello".into(),
            cache_control: None,
        }],
        stop_reason: None,
        usage: None,
        stop_details: None,
        input_transformations: Vec::new(),
    };
    let mut result = parse_tool_response(resp);
    let compaction = result.take_compaction();
    assert_matches!(result, ChatResponse::Text { text: s, .. } if s == "Hello");
    assert!(compaction.is_none());
}

#[test]
fn parse_tool_response_with_tool_use() {
    let resp = ToolApiResponse {
        content: vec![
            AnthropicContentBlock::Text {
                text: "I'll run that".into(),
                cache_control: None,
            },
            AnthropicContentBlock::ToolUse {
                id: "toolu_123".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ],
        stop_reason: None,
        usage: None,
        stop_details: None,
        input_transformations: Vec::new(),
    };
    let mut result = parse_tool_response(resp);
    let compaction = result.take_compaction();
    if let ChatResponse::ToolUse { content, .. } = result {
        let text = content.text();
        let tool_calls: Vec<_> = content.tool_calls().cloned().collect();
        assert_eq!(text.unwrap(), "I'll run that");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "bash");
        assert_eq!(tool_calls[0].id, "toolu_123");
    } else {
        panic!("expected ToolUse");
    }
    assert!(compaction.is_none());
}

#[test]
fn parse_tool_response_tool_use_only() {
    let resp = ToolApiResponse {
        content: vec![AnthropicContentBlock::ToolUse {
            id: "toolu_456".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "/tmp/file.txt"}),
        }],
        stop_reason: None,
        usage: None,
        stop_details: None,
        input_transformations: Vec::new(),
    };
    let mut result = parse_tool_response(resp);
    let compaction = result.take_compaction();
    if let ChatResponse::ToolUse { content, .. } = result {
        let text = content.text();
        let tool_calls: Vec<_> = content.tool_calls().cloned().collect();
        assert!(text.is_none());
        assert_eq!(tool_calls.len(), 1);
    } else {
        panic!("expected ToolUse");
    }
    assert!(compaction.is_none());
}

#[test]
fn parse_tool_response_json_deserialization() {
    let json = r#"{"content":[{"type":"text","text":"Let me check"},{"type":"tool_use","id":"toolu_abc","name":"bash","input":{"command":"ls"}}]}"#;
    let resp: ToolApiResponse = serde_json::from_str(json).unwrap();
    let result = parse_tool_response(resp);
    assert_matches!(result, ChatResponse::ToolUse { .. });
}

#[test]
fn parse_tool_response_with_compaction() {
    let resp = ToolApiResponse {
        content: vec![AnthropicContentBlock::Compaction {
            content: Some("Context was summarized for efficiency.".into()),
        }],
        stop_reason: None,
        usage: None,
        stop_details: None,
        input_transformations: Vec::new(),
    };
    let mut result = parse_tool_response(resp);
    let compaction = result.take_compaction();
    assert_matches!(result, ChatResponse::Text { text: ref s, .. } if s.is_empty());
    assert_eq!(
        compaction.as_deref(),
        Some("Context was summarized for efficiency.")
    );
}

#[test]
fn supports_tool_use_returns_true() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
    assert!(provider.supports_tool_use());
}

#[test]
fn anthropic_content_block_image_serializes_correctly() {
    let block = AnthropicContentBlock::Image {
        source: ImageSource {
            source_type: "base64".to_owned(),
            media_type: "image/jpeg".to_owned(),
            data: "abc123".to_owned(),
        },
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "image");
    assert_eq!(json["source"]["type"], "base64");
    assert_eq!(json["source"]["media_type"], "image/jpeg");
    assert_eq!(json["source"]["data"], "abc123");
}

#[test]
fn tool_cache_returns_same_values_on_second_call() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
    let tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "Run shell commands".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        output_schema: None,
    }];
    let first = provider.get_or_build_api_tools(&tools);
    let second = provider.get_or_build_api_tools(&tools);
    assert_eq!(first, second);
    assert_eq!(first[0]["name"], "bash");
    assert_eq!(first[0]["description"], "Run shell commands");
}

#[test]
fn tool_cache_invalidates_when_tools_change() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
    let tools_a = vec![ToolDefinition {
        name: "bash".into(),
        description: "Run shell commands".into(),
        parameters: serde_json::json!({}),
        output_schema: None,
    }];
    let tools_b = vec![ToolDefinition {
        name: "read".into(),
        description: "Read files".into(),
        parameters: serde_json::json!({}),
        output_schema: None,
    }];
    let first = provider.get_or_build_api_tools(&tools_a);
    let second = provider.get_or_build_api_tools(&tools_b);
    assert_eq!(first[0]["name"], "bash");
    assert_eq!(second[0]["name"], "read");
}

#[test]
fn tool_cache_serialized_shape_snapshot() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
    let tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "Run a shell command".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run"}
            },
            "required": ["command"]
        }),
        output_schema: None,
    }];
    let cached = provider.get_or_build_api_tools(&tools);
    let pretty = serde_json::to_string_pretty(&cached).unwrap();
    insta::assert_snapshot!(pretty);
}

#[tokio::test]
async fn chat_with_tools_sends_correct_tool_fields() {
    use crate::provider::ToolDefinition;
    use crate::testing::claude_tool_use_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_response(
            "read_file",
            "toolu_1",
            &serde_json::json!({"path": "/tmp/f"}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("test-key".into(), "claude-test".into(), 256)
        .with_api_url(format!("{}/v1/messages", server.uri()));

    let tools = vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file from disk".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        output_schema: None,
    }];
    let messages = vec![Message::from_legacy(Role::User, "read /tmp/f")];

    let result = provider.chat_with_tools(&messages, &tools).await;
    assert!(result.is_ok(), "request must succeed: {result:?}");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let sent_tools = body["tools"].as_array().expect("tools array present");
    assert_eq!(sent_tools.len(), 1);
    assert_eq!(sent_tools[0]["name"], "read_file");
    assert_eq!(sent_tools[0]["description"], "Read a file from disk");
    assert!(sent_tools[0]["input_schema"].is_object());
    assert_eq!(sent_tools[0]["input_schema"]["type"], "object");
}

// ── #6156: HTTP-level regression coverage — drive chat_with_tools_stream,
// chat_with_tools, and chat_typed through a mock server and assert the no-prefill
// gate strips a trailing assistant message in the request actually sent over the
// wire, not just in the body-construction functions exercised via debug_request_json ──

#[tokio::test]
async fn chat_with_tools_no_prefill_strips_trailing_assistant_over_wire() {
    use crate::provider::ToolDefinition;
    use crate::testing::claude_tool_use_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_response(
            "read_file",
            "toolu_1",
            &serde_json::json!({"path": "a.rs"}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    // claude-sonnet-5 unconditionally rejects assistant prefill (rejects_prefill).
    let provider = ClaudeProvider::new("test-key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));

    let tools = vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        output_schema: None,
    }];
    let messages = vec![
        Message::from_legacy(Role::User, "hello"),
        Message::from_legacy(Role::Assistant, "trailing"),
    ];

    let result = provider.chat_with_tools(&messages, &tools).await;
    assert!(result.is_ok(), "request must succeed: {result:?}");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.last().and_then(|m| m["role"].as_str()),
        Some("user"),
        "no-prefill gate must strip the trailing assistant message from the wire request: {body}"
    );
}

#[tokio::test]
async fn chat_with_tools_stream_no_prefill_strips_trailing_assistant_over_wire() {
    use crate::provider::ToolDefinition;
    use crate::testing::claude_tool_use_sse_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_sse_response(
            "toolu_1",
            "read_file",
            r#"{"path":"a.rs"}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("test-key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));

    let tools = vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        output_schema: None,
    }];
    let messages = vec![
        Message::from_legacy(Role::User, "hello"),
        Message::from_legacy(Role::Assistant, "trailing"),
    ];

    let result = provider.chat_with_tools_stream(&messages, &tools).await;
    assert!(result.is_ok(), "request must succeed: {:?}", result.err());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.last().and_then(|m| m["role"].as_str()),
        Some("user"),
        "no-prefill gate must strip the trailing assistant message from the wire request: {body}"
    );
}

#[tokio::test]
async fn chat_typed_no_prefill_strips_trailing_assistant_over_wire() {
    use crate::testing::claude_tool_use_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
    struct TypedOut {
        value: String,
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_response(
            "submit_TypedOut",
            "toolu_1",
            &serde_json::json!({"value": "ok"}),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("test-key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));

    let messages = vec![
        Message::from_legacy(Role::User, "hello"),
        Message::from_legacy(Role::Assistant, "trailing"),
    ];

    let result: Result<TypedOut, _> = provider.chat_typed(&messages).await;
    assert!(result.is_ok(), "request must succeed: {result:?}");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.last().and_then(|m| m["role"].as_str()),
        Some("user"),
        "no-prefill gate must strip the trailing assistant message from the wire request: {body}"
    );
}

#[tokio::test]
async fn chat_with_tools_cache_hit_does_not_re_serialize() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 512);
    let tools = vec![
        ToolDefinition {
            name: "tool_a".into(),
            description: "First tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        },
        ToolDefinition {
            name: "tool_b".into(),
            description: "Second tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        },
    ];

    let first = provider.get_or_build_api_tools(&tools);
    let second = provider.get_or_build_api_tools(&tools);
    let third = provider.get_or_build_api_tools(&tools);

    // All calls return identical values
    assert_eq!(first, second);
    assert_eq!(second, third);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["name"], "tool_a");
    assert_eq!(first[1]["name"], "tool_b");

    // Verify cache is populated
    let guard = provider.tool_cache.lock();
    let (hash, values) = guard.as_ref().unwrap();
    assert_ne!(*hash, 0);
    assert_eq!(values.len(), 2);
}

#[tokio::test]
async fn chat_with_tools_cache_partial_tool_set_change_invalidates() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 512);

    let tools_v1 = vec![ToolDefinition {
        name: "search".into(),
        description: "Search the web".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        output_schema: None,
    }];
    let tools_v2 = vec![
        ToolDefinition {
            name: "search".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        },
        ToolDefinition {
            name: "browse".into(),
            description: "Browse a URL".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        },
    ];

    let v1 = provider.get_or_build_api_tools(&tools_v1);
    assert_eq!(v1.len(), 1);

    let v2 = provider.get_or_build_api_tools(&tools_v2);
    assert_eq!(v2.len(), 2);
    assert_eq!(v2[1]["name"], "browse");

    // Cache now reflects v2
    let guard = provider.tool_cache.lock();
    let (hash, values) = guard.as_ref().unwrap();
    assert_ne!(*hash, 0);
    assert_eq!(values.len(), 2);
}

#[test]
fn has_image_parts_detects_image_in_messages() {
    let with_image = Message::from_parts(
        Role::User,
        vec![MessagePart::Image(Box::new(ImageData {
            data: vec![1],
            mime_type: "image/png".into(),
        }))],
    );
    let without_image = Message::from_legacy(Role::User, "plain text");
    assert!(ClaudeProvider::has_image_parts(&[with_image]));
    assert!(!ClaudeProvider::has_image_parts(&[without_image]));
}

// Test that the pagination response JSON structure is correctly parsed inline.
// list_models_remote uses serde_json::Value for page parsing; test the same logic here.
#[test]
fn pagination_response_has_more_true_extracts_last_id() {
    let page = serde_json::json!({
        "data": [{"id": "model-a", "type": "model", "display_name": "Model A"}],
        "has_more": true,
        "last_id": "model-a"
    });
    let has_more = page
        .get("has_more")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let last_id = page
        .get("last_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    assert!(has_more);
    assert_eq!(last_id, Some("model-a".to_string()));
}

#[test]
fn pagination_response_has_more_false_stops_loop() {
    let page = serde_json::json!({
        "data": [{"id": "model-b", "type": "model", "display_name": "Model B"}],
        "has_more": false
    });
    let has_more = page
        .get("has_more")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    assert!(!has_more);
}

#[test]
fn model_item_filters_non_model_type() {
    let page = serde_json::json!({
        "data": [
            {"id": "model-ok", "type": "model", "display_name": "OK"},
            {"id": "skip-me", "type": "other", "display_name": "Skip"}
        ],
        "has_more": false
    });
    let models: Vec<crate::model_cache::RemoteModelInfo> = page
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let type_field = item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if type_field != "model" {
                        return None;
                    }
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let display_name = item
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&id)
                        .to_string();
                    Some(crate::model_cache::RemoteModelInfo {
                        id,
                        display_name,
                        context_window: None,
                        created_at: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "model-ok");
}

#[test]
fn model_item_uses_id_as_display_name_when_missing() {
    let item = serde_json::json!({"id": "claude-x", "type": "model"});
    let id = item
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let display_name = item
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(&id)
        .to_string();
    assert_eq!(display_name, "claude-x");
}

// ------------------------------------------------------------------
// Wiremock HTTP-level tests using fixture helpers from testing module
// ------------------------------------------------------------------

#[test]
fn messages_response_deserialization() {
    let raw = serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": TEST_CLAUDE_MODEL,
        "content": [{"type": "text", "text": "hello claude"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    });
    let resp: ApiResponse = serde_json::from_value(raw).unwrap();
    let text: String = resp.content.iter().map(|b| b.text.as_str()).collect();
    assert_eq!(text, "hello claude");
}

#[tokio::test]
async fn messages_429_overload_propagates() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use crate::testing::claude_overload_response;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_overload_response(429))
        .mount(&server)
        .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", server.uri()))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 429);
}

#[tokio::test]
async fn messages_529_overload_propagates() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use crate::testing::claude_overload_response;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_overload_response(529))
        .mount(&server)
        .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", server.uri()))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 529);
}

#[tokio::test]
async fn claude_sse_fixture_contains_expected_events() {
    use crate::testing::claude_sse_stream_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/stream"))
        .respond_with(claude_sse_stream_response(&["Hello", " world"]))
        .mount(&server)
        .await;
    let raw = reqwest::Client::new()
        .post(format!("{}/stream", server.uri()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(raw.contains("Hello"));
    assert!(raw.contains(" world"));
    assert!(raw.contains("message_stop"));
    assert!(raw.contains("content_block_delta"));
}

#[test]
fn thinking_config_extended_serializes() {
    let cfg = ThinkingConfig::Extended {
        budget_tokens: 10_000,
    };
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json["mode"], "extended");
    assert_eq!(json["budget_tokens"], 10_000);
}

#[test]
fn thinking_config_adaptive_serializes_without_effort() {
    let cfg = ThinkingConfig::Adaptive { effort: None };
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json["mode"], "adaptive");
    assert!(json.get("effort").is_none());
}

#[test]
fn thinking_config_adaptive_serializes_with_effort() {
    let cfg = ThinkingConfig::Adaptive {
        effort: Some(ThinkingEffort::High),
    };
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json["mode"], "adaptive");
    assert_eq!(json["effort"], "high");
}

#[test]
fn thinking_config_extended_deserializes() {
    let json = r#"{"mode":"extended","budget_tokens":8000}"#;
    let cfg: ThinkingConfig = serde_json::from_str(json).unwrap();
    assert_eq!(
        cfg,
        ThinkingConfig::Extended {
            budget_tokens: 8000
        }
    );
}

#[test]
fn thinking_config_adaptive_deserializes() {
    let json = r#"{"mode":"adaptive","effort":"low"}"#;
    let cfg: ThinkingConfig = serde_json::from_str(json).unwrap();
    assert_eq!(
        cfg,
        ThinkingConfig::Adaptive {
            effort: Some(ThinkingEffort::Low)
        }
    );
}

fn thinking_capability(model: &str) -> ModelCapabilities {
    ClaudeModel::from_model_id(model).capabilities()
}

#[test]
fn thinking_capability_sonnet_4_6_needs_interleaved_beta() {
    let cap = thinking_capability("claude-sonnet-4-6-20250514");
    assert!(cap.needs_interleaved_beta);
}

#[test]
fn thinking_capability_opus_4_6_no_interleaved_beta() {
    let cap = thinking_capability("claude-opus-4-6");
    assert!(!cap.needs_interleaved_beta);
}

#[test]
fn thinking_capability_unknown_model_no_beta() {
    let cap = thinking_capability("gpt-4o");
    assert!(!cap.needs_interleaved_beta);
}

#[test]
fn thinking_capability_opus_4_6_prefers_effort() {
    let cap = thinking_capability("claude-opus-4-6");
    assert!(cap.prefers_effort);
}

#[test]
fn thinking_capability_sonnet_4_6_no_prefers_effort() {
    let cap = thinking_capability("claude-sonnet-4-6-20250514");
    assert!(!cap.prefers_effort);
}

#[test]
fn thinking_capability_opus_4_8_prefers_effort() {
    let cap = thinking_capability("claude-opus-4-8");
    assert!(cap.prefers_effort);
    assert!(!cap.needs_interleaved_beta);
}

#[test]
fn thinking_capability_opus_4_7_prefers_effort() {
    let cap = thinking_capability("claude-opus-4-7");
    assert!(cap.prefers_effort);
}

#[test]
fn thinking_capability_sonnet_5_prefers_effort_no_interleaved_beta() {
    let cap = thinking_capability("claude-sonnet-5");
    assert!(cap.prefers_effort);
    assert!(!cap.needs_interleaved_beta);
}

#[test]
fn budget_to_effort_boundaries() {
    assert_eq!(budget_to_effort(4_999), ThinkingEffort::Low);
    assert_eq!(budget_to_effort(5_000), ThinkingEffort::Medium);
    assert_eq!(budget_to_effort(14_999), ThinkingEffort::Medium);
    assert_eq!(budget_to_effort(15_000), ThinkingEffort::High);
    assert_eq!(budget_to_effort(1_024), ThinkingEffort::Low);
    assert_eq!(budget_to_effort(20_000), ThinkingEffort::High);
}

#[test]
fn build_thinking_param_opus_extended_converts_to_adaptive() {
    let p = ClaudeProvider::new("k".into(), "claude-opus-4-8".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5_000,
        })
        .unwrap();
    let (param, _temp, effort) = p.build_thinking_param();
    let param = param.unwrap();
    assert_eq!(param.thinking_type, ThinkingType::Adaptive);
    assert!(param.budget_tokens.is_none());
    assert_eq!(effort, Some(ThinkingEffort::Medium));
}

#[test]
fn build_thinking_param_opus_adaptive_unchanged() {
    let p = ClaudeProvider::new("k".into(), "claude-opus-4-8".into(), 32_000)
        .with_thinking(ThinkingConfig::Adaptive {
            effort: Some(ThinkingEffort::High),
        })
        .unwrap();
    let (param, _temp, effort) = p.build_thinking_param();
    let param = param.unwrap();
    assert_eq!(param.thinking_type, ThinkingType::Adaptive);
    assert!(param.budget_tokens.is_none());
    assert_eq!(effort, Some(ThinkingEffort::High));
}

#[test]
fn build_thinking_param_sonnet_extended_unchanged() {
    let p = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5_000,
        })
        .unwrap();
    let (param, _temp, effort) = p.build_thinking_param();
    let param = param.unwrap();
    assert_eq!(param.thinking_type, ThinkingType::Enabled);
    assert_eq!(param.budget_tokens, Some(5_000));
    assert!(effort.is_none());
}

#[test]
fn with_thinking_rejects_budget_below_minimum() {
    let err = ClaudeProvider::new("k".into(), "m".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended { budget_tokens: 0 })
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");

    let err = ClaudeProvider::new("k".into(), "m".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 1023,
        })
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
fn with_thinking_accepts_minimum_budget() {
    ClaudeProvider::new("k".into(), "m".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 1024,
        })
        .unwrap();
}

#[test]
fn with_thinking_accepts_maximum_budget() {
    ClaudeProvider::new("k".into(), "m".into(), 256_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 128_000,
        })
        .unwrap();
}

#[test]
fn with_thinking_rejects_budget_above_maximum() {
    let err = ClaudeProvider::new("k".into(), "m".into(), 256_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 128_001,
        })
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
fn with_thinking_rejects_budget_not_less_than_max_tokens() {
    // After auto-bump max_tokens = 16_000, budget_tokens = 16_000 is not < max_tokens
    let err = ClaudeProvider::new("k".into(), "m".into(), 1024)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 16_000,
        })
        .unwrap_err();
    assert!(err.to_string().contains("less than max_tokens"), "{err}");
}

#[test]
fn with_thinking_bumps_max_tokens_when_too_low() {
    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 8000,
        })
        .unwrap();
    assert!(provider.max_tokens >= MIN_MAX_TOKENS_WITH_THINKING);
}

#[test]
fn with_thinking_keeps_max_tokens_when_already_high() {
    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 8000,
        })
        .unwrap();
    assert_eq!(provider.max_tokens, 32_000);
}

#[test]
fn set_thinking_none_restores_base_max_tokens_after_enable() {
    // S1 regression: enable (bumps max_tokens to the 16k floor) → off must restore the
    // originally configured max_tokens, not leave it stuck at the floor.
    let mut provider = ClaudeProvider::new("k".into(), "claude-haiku-4-5-20251001".into(), 1024);
    provider
        .set_thinking(Some(ThinkingConfig::Extended {
            budget_tokens: 8000,
        }))
        .unwrap();
    assert!(provider.max_tokens >= MIN_MAX_TOKENS_WITH_THINKING);

    provider.set_thinking(None).unwrap();
    assert_eq!(
        provider.max_tokens, 1024,
        "max_tokens must be restored to the construction-time baseline, not left at the floor"
    );
    assert!(provider.thinking.is_none());
}

#[test]
fn set_thinking_enable_disable_enable_is_idempotent() {
    // S1: repeated enable/disable cycles must never ratchet max_tokens upward permanently —
    // every `Some(_)` step recomputes from the immutable base_max_tokens baseline.
    let mut provider = ClaudeProvider::new("k".into(), "claude-haiku-4-5-20251001".into(), 1024);
    for budget in [8000, 12_000, 4000] {
        provider
            .set_thinking(Some(ThinkingConfig::Extended {
                budget_tokens: budget,
            }))
            .unwrap();
        assert_eq!(provider.max_tokens, MIN_MAX_TOKENS_WITH_THINKING);

        provider.set_thinking(None).unwrap();
        assert_eq!(provider.max_tokens, 1024);
    }
}

#[test]
fn set_thinking_out_of_range_budget_rejected() {
    let mut provider = ClaudeProvider::new("k".into(), "m".into(), 32_000);
    let err = provider
        .set_thinking(Some(ThinkingConfig::Extended { budget_tokens: 0 }))
        .unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
fn set_thinking_construction_parity_with_with_thinking() {
    // Construction-time behavior via `with_thinking` must remain byte-for-byte identical to
    // calling `set_thinking` directly after `new` (delegation must not change semantics).
    let via_with_thinking = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 8000,
        })
        .unwrap();

    let mut via_set_thinking = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024);
    via_set_thinking
        .set_thinking(Some(ThinkingConfig::Extended {
            budget_tokens: 8000,
        }))
        .unwrap();

    assert_eq!(via_with_thinking.max_tokens, via_set_thinking.max_tokens);
    assert_eq!(via_with_thinking.thinking, via_set_thinking.thinking);
}

#[test]
fn current_thinking_budget_reflects_extended_config() {
    let mut provider = ClaudeProvider::new("k".into(), "m".into(), 32_000);
    assert!(provider.current_thinking_budget().is_none());
    provider
        .set_thinking(Some(ThinkingConfig::Extended {
            budget_tokens: 8000,
        }))
        .unwrap();
    assert_eq!(provider.current_thinking_budget(), Some(8000));
    assert!(provider.current_reasoning_effort().is_none());
}

#[test]
fn current_reasoning_effort_reflects_adaptive_config() {
    let mut provider = ClaudeProvider::new("k".into(), "m".into(), 32_000);
    provider
        .set_thinking(Some(ThinkingConfig::Adaptive {
            effort: Some(ThinkingEffort::High),
        }))
        .unwrap();
    assert_eq!(provider.current_reasoning_effort().as_deref(), Some("high"));
    assert!(provider.current_thinking_budget().is_none());
}

#[test]
fn current_thinking_budget_and_effort_none_when_disabled() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 32_000);
    assert!(provider.current_thinking_budget().is_none());
    assert!(provider.current_reasoning_effort().is_none());
}

#[test]
fn build_thinking_param_extended_returns_enabled_with_budget() {
    let provider = ClaudeProvider::new("k".into(), "claude-haiku-4-5-20251001".into(), 16_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5000,
        })
        .unwrap();
    let (param, temp, effort) = provider.build_thinking_param();
    let param = param.unwrap();
    assert_eq!(param.thinking_type, ThinkingType::Enabled);
    assert_eq!(param.budget_tokens, Some(5000));
    assert!(temp.is_none());
    assert!(effort.is_none());
}

#[test]
fn build_thinking_param_adaptive_returns_adaptive_type() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 16_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let (param, temp, effort) = provider.build_thinking_param();
    let param = param.unwrap();
    assert_eq!(param.thinking_type, ThinkingType::Adaptive);
    assert!(param.budget_tokens.is_none());
    assert!(temp.is_none());
    assert!(effort.is_none());
}

#[test]
fn build_thinking_param_adaptive_with_effort_returns_effort() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 16_000)
        .with_thinking(ThinkingConfig::Adaptive {
            effort: Some(ThinkingEffort::High),
        })
        .unwrap();
    let (param, temp, effort) = provider.build_thinking_param();
    let param = param.unwrap();
    assert_eq!(param.thinking_type, ThinkingType::Adaptive);
    assert!(param.budget_tokens.is_none());
    assert!(temp.is_none());
    assert_eq!(effort, Some(ThinkingEffort::High));
}

#[test]
fn build_thinking_param_adaptive_serializes_correctly() {
    let param = ThinkingParam {
        thinking_type: ThinkingType::Adaptive,
        budget_tokens: None,
        block_binding: None,
    };
    let json = serde_json::to_value(&param).unwrap();
    assert_eq!(json, serde_json::json!({"type": "adaptive"}));
    assert!(json.get("budget_tokens").is_none());
}

#[test]
fn build_thinking_param_no_thinking_returns_none() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 1024);
    let (param, temp, effort) = provider.build_thinking_param();
    assert!(param.is_none());
    assert!(temp.is_none());
    assert!(effort.is_none());
}

#[test]
fn beta_header_without_thinking_returns_none() {
    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024);
    let beta = provider.beta_header(true);
    assert!(beta.is_none());
}

#[test]
fn beta_header_sonnet_4_6_extended_with_tools_includes_interleaved() {
    let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 16_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5000,
        })
        .unwrap();
    let beta = provider.beta_header(true);
    assert!(
        beta.as_deref()
            .is_some_and(|b| b.contains(ANTHROPIC_BETA_INTERLEAVED_THINKING))
    );
}

#[test]
fn beta_header_sonnet_4_6_extended_no_tools_excludes_interleaved() {
    let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 16_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5000,
        })
        .unwrap();
    let beta = provider.beta_header(false);
    assert!(beta.is_none());
}

#[test]
fn beta_header_adaptive_mode_excludes_interleaved() {
    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 16_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let beta = provider.beta_header(true);
    assert!(beta.is_none());
}

#[test]
fn extended_context_disabled_no_beta_header() {
    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024);
    let beta = provider.beta_header(true);
    assert!(beta.is_none());
}

#[test]
fn extended_context_enabled_includes_beta_header() {
    let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-5-20250929".into(), 1024)
        .with_extended_context(true);
    let beta = provider.beta_header(true);
    assert!(
        beta.as_deref()
            .is_some_and(|b| b.contains(ANTHROPIC_BETA_EXTENDED_CONTEXT))
    );
}

#[test]
fn extended_context_with_interleaved_thinking_combines_headers() {
    let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 16_000)
        .with_server_compaction(true)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5000,
        })
        .unwrap();
    let beta = provider.beta_header(true);
    let beta_str = beta.expect("beta header should be present");
    assert!(beta_str.contains(BetaFeature::Compact.header()));
    assert!(beta_str.contains(ANTHROPIC_BETA_INTERLEAVED_THINKING));
    // Both betas must be comma-separated in a single header value
    assert!(beta_str.contains(','));
}

#[test]
fn extended_context_enabled_returns_1m_context_window() {
    let provider =
        ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024).with_extended_context(true);
    assert_eq!(provider.context_window(), Some(1_000_000));
}

#[test]
fn extended_context_disabled_returns_200k_context_window() {
    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024);
    assert_eq!(provider.context_window(), Some(200_000));
}

#[test]
fn extended_context_enabled_haiku_returns_200k_context_window() {
    // Haiku does not support the 1M context window; flag must be ignored.
    let provider = ClaudeProvider::new("k".into(), "claude-haiku-4-5-20251001".into(), 1024)
        .with_extended_context(true);
    assert_eq!(provider.context_window(), Some(200_000));
}

#[test]
fn parse_tool_response_with_thinking_blocks() {
    let resp = ToolApiResponse {
        content: vec![
            AnthropicContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: "sig123".into(),
            },
            AnthropicContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ],
        stop_reason: None,
        usage: None,
        stop_details: None,
        input_transformations: Vec::new(),
    };
    let result = parse_tool_response(resp);
    if let ChatResponse::ToolUse { content, .. } = result {
        let tool_calls: Vec<_> = content.tool_calls().cloned().collect();
        let thinking_blocks: Vec<_> = content.thinking_blocks().cloned().collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(thinking_blocks.len(), 1);
        if let ThinkingBlock::Thinking {
            thinking,
            signature,
        } = &thinking_blocks[0]
        {
            assert_eq!(thinking, "let me think");
            assert_eq!(signature, "sig123");
        } else {
            panic!("expected Thinking variant");
        }
    } else {
        panic!("expected ToolUse");
    }
}

#[test]
fn parse_tool_response_with_redacted_thinking() {
    let resp = ToolApiResponse {
        content: vec![
            AnthropicContentBlock::RedactedThinking {
                data: "redacted".into(),
            },
            AnthropicContentBlock::Text {
                text: "result".into(),
                cache_control: None,
            },
        ],
        stop_reason: None,
        usage: None,
        stop_details: None,
        input_transformations: Vec::new(),
    };
    let result = parse_tool_response(resp);
    // No tool calls, so returns Text; thinking is dropped for text-only responses
    assert_matches!(result, ChatResponse::Text { .. });
}

#[test]
fn thinking_content_block_roundtrip() {
    let block = AnthropicContentBlock::Thinking {
        thinking: "internal reasoning".into(),
        signature: "signature-data".into(),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "thinking");
    let restored: AnthropicContentBlock = serde_json::from_value(json).unwrap();
    if let AnthropicContentBlock::Thinking {
        thinking,
        signature,
    } = restored
    {
        assert_eq!(thinking, "internal reasoning");
        assert_eq!(signature, "signature-data");
    } else {
        panic!("expected Thinking");
    }
}

#[test]
fn redacted_thinking_content_block_roundtrip() {
    let block = AnthropicContentBlock::RedactedThinking {
        data: "opaque-data".into(),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "redacted_thinking");
    let restored: AnthropicContentBlock = serde_json::from_value(json).unwrap();
    if let AnthropicContentBlock::RedactedThinking { data } = restored {
        assert_eq!(data, "opaque-data");
    } else {
        panic!("expected RedactedThinking");
    }
}

// ── #1085: anthropic-beta header removed ──────────────────────────────────

#[test]
fn build_request_does_not_include_anthropic_beta_header() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 256);
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let req = provider.build_request(&messages, false).build().unwrap();
    assert!(
        req.headers().get("anthropic-beta").is_none(),
        "anthropic-beta header must not be present"
    );
    assert!(req.headers().get("anthropic-version").is_some());
    assert!(req.headers().get("x-api-key").is_some());
}

#[test]
fn build_request_with_extended_context_includes_beta_header() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 256)
        .with_extended_context(true);
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let req = provider.build_request(&messages, false).build().unwrap();
    let header_value = req
        .headers()
        .get("anthropic-beta")
        .expect("anthropic-beta header must be present when extended context is enabled")
        .to_str()
        .expect("header must be valid UTF-8");
    assert!(
        header_value.contains(ANTHROPIC_BETA_EXTENDED_CONTEXT),
        "anthropic-beta header must contain '{ANTHROPIC_BETA_EXTENDED_CONTEXT}', got '{header_value}'"
    );
}

// ── #1084: cache_control only on last tool ────────────────────────────────

#[test]
fn get_or_build_api_tools_only_last_tool_has_cache_control() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 512);
    let tools = vec![
        ToolDefinition {
            name: "alpha".into(),
            description: "First".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        },
        ToolDefinition {
            name: "beta".into(),
            description: "Second".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        },
        ToolDefinition {
            name: "gamma".into(),
            description: "Third".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        },
    ];
    let result = provider.get_or_build_api_tools(&tools);
    assert_eq!(result.len(), 3);
    assert!(
        result[0].get("cache_control").is_none(),
        "first tool must not have cache_control"
    );
    assert!(
        result[1].get("cache_control").is_none(),
        "middle tool must not have cache_control"
    );
    assert!(
        result[2].get("cache_control").is_some(),
        "last tool must have cache_control"
    );
    assert_eq!(result[2]["cache_control"]["type"], "ephemeral");
}

#[test]
fn get_or_build_api_tools_single_tool_has_cache_control() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 512);
    let tools = vec![ToolDefinition {
        name: "only".into(),
        description: "Only tool".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        output_schema: None,
    }];
    let result = provider.get_or_build_api_tools(&tools);
    assert_eq!(result.len(), 1);
    assert!(result[0].get("cache_control").is_some());
    assert_eq!(result[0]["cache_control"]["type"], "ephemeral");
}

// ── #1083: model-aware token threshold ───────────────────────────────────

#[test]
fn cache_min_tokens_sonnet_returns_2048() {
    assert_eq!(cache_min_tokens(TEST_CLAUDE_MODEL), 2048);
    assert_eq!(cache_min_tokens("claude-sonnet-4-5-20250929"), 2048);
}

#[test]
fn cache_min_tokens_non_sonnet_returns_4096() {
    assert_eq!(cache_min_tokens("claude-opus-4-8"), 4096);
    assert_eq!(cache_min_tokens("claude-haiku-4-5"), 4096);
    assert_eq!(cache_min_tokens("unknown-model"), 4096);
}

#[test]
fn split_system_opus_block_above_threshold_gets_cache_control() {
    // opus threshold = 4096 tokens = 16384 chars
    let padding = "x".repeat(16400);
    let system = format!("{padding}\n{CACHE_MARKER_STABLE}\nmore");
    let blocks = split_system_into_blocks(&system, "claude-opus-4-8", None);
    assert!(
        blocks[0].cache_control.is_some(),
        "block above opus threshold must be cached"
    );
}

#[test]
fn split_system_opus_block_below_threshold_skips_cache_control() {
    // text under 16384 chars is below opus threshold (4096 tokens)
    let system = format!("short\n{CACHE_MARKER_STABLE}\nmore content");
    let blocks = split_system_into_blocks(&system, "claude-opus-4-8", None);
    assert!(
        blocks[0].cache_control.is_none(),
        "block below opus threshold must not be cached"
    );
}

// ── #1086/#1570: Claude requests avoid top-level cache_control ────────────

#[test]
fn build_request_single_message_no_top_level_cache_control() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 256);
    let messages = vec![Message {
        role: Role::User,
        content: "hello".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    assert!(
        body.get("cache_control").is_none(),
        "single-turn request must not have top-level cache_control"
    );
}

#[test]
fn build_request_multi_turn_no_top_level_cache_control() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 256);
    let messages = vec![
        Message {
            role: Role::User,
            content: "first".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "reply".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: "second".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    assert!(
        body.get("cache_control").is_none(),
        "multi-turn request must not have top-level cache_control"
    );
}

// ── #1087: message-level breakpoint at position max(0, total-20) ──────────

fn count_cache_control_occurrences(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map
                    .values()
                    .map(count_cache_control_occurrences)
                    .sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter().map(count_cache_control_occurrences).sum(),
        _ => 0,
    }
}

#[test]
fn debug_tool_request_caps_block_cache_controls_at_four() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 256);
    let padding = "x".repeat(8200);
    let system = format!(
        "base prompt {padding}\n{CACHE_MARKER_STABLE}\nskills here {padding}\n\
         {CACHE_MARKER_TOOLS}\ntool catalog {padding}\n\
         {CACHE_MARKER_VOLATILE}\nvolatile stuff"
    );
    let messages = vec![
        Message::from_legacy(Role::System, system),
        Message::from_legacy(Role::User, "diagnose ACP startup"),
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: "toolu_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "false"}),
            }],
        ),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "command failed".into(),
                is_error: true,
            }],
        ),
    ];
    let tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "Run shell commands".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"}
            },
            "required": ["command"]
        }),
        output_schema: None,
    }];

    let body = provider.debug_request_json(&messages, &tools, false);

    assert_eq!(
        count_cache_control_occurrences(&body["tools"]),
        1,
        "tool definitions should keep their cache breakpoint"
    );
    assert_eq!(
        count_cache_control_occurrences(&body["system"]),
        3,
        "system markers should keep all three cacheable blocks"
    );
    assert_eq!(
        count_cache_control_occurrences(&body["messages"]),
        0,
        "message-level cache breakpoint must be dropped when tools+system already consume the Anthropic budget"
    );
    assert_eq!(
        count_cache_control_occurrences(&body["tools"])
            + count_cache_control_occurrences(&body["system"])
            + count_cache_control_occurrences(&body["messages"]),
        4,
        "tool requests must never serialize more than four nested cache_control entries"
    );
    assert_eq!(
        count_cache_control_occurrences(&body),
        4,
        "tool requests must stay within Anthropic's total cache_control budget"
    );
    assert!(
        body.get("cache_control").is_none(),
        "top-level cache_control must be dropped when tools and system blocks already consume the budget"
    );
}

#[test]
fn debug_vision_request_caps_total_cache_controls_at_four() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 256);
    let padding = "x".repeat(8200);
    let system = format!(
        "base prompt {padding}\n{CACHE_MARKER_STABLE}\nskills here {padding}\n\
         {CACHE_MARKER_TOOLS}\ntool catalog {padding}\n\
         {CACHE_MARKER_VOLATILE}\nvolatile stuff"
    );
    let messages = vec![
        Message::from_legacy(Role::System, system),
        Message::from_legacy(Role::User, "describe this screenshot"),
        Message::from_legacy(Role::Assistant, "send it"),
        Message::from_parts(
            Role::User,
            vec![MessagePart::Image(Box::new(ImageData {
                data: vec![1, 2, 3, 4],
                mime_type: "image/png".into(),
            }))],
        ),
    ];

    let body = provider.debug_request_json(&messages, &[], false);

    assert_eq!(
        count_cache_control_occurrences(&body["system"]),
        3,
        "system markers should keep all three cacheable blocks"
    );
    assert_eq!(
        count_cache_control_occurrences(&body["messages"]),
        1,
        "vision requests may keep one message breakpoint when system blocks consume only three slots"
    );
    assert_eq!(
        count_cache_control_occurrences(&body),
        4,
        "vision requests must stay within Anthropic's total cache_control budget"
    );
    assert!(
        body.get("cache_control").is_none(),
        "vision requests must not serialize top-level cache_control"
    );
}

// --- #1094: tool schema hash in cache key ---

#[test]
fn tool_cache_invalidates_on_schema_change() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
    let tools_v1 = vec![ToolDefinition {
        name: "tool".into(),
        description: "desc".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        output_schema: None,
    }];
    let tools_v2 = vec![ToolDefinition {
        name: "tool".into(),
        description: "desc".into(),
        parameters: serde_json::json!({"type": "object", "properties": {"b": {"type": "number"}}}),
        output_schema: None,
    }];
    let first = provider.get_or_build_api_tools(&tools_v1);
    let second = provider.get_or_build_api_tools(&tools_v2);
    // Same names but different schemas — must return different serialized tools.
    assert_eq!(
        first[0]["input_schema"]["properties"]["a"]["type"],
        "string"
    );
    assert_eq!(
        second[0]["input_schema"]["properties"]["b"]["type"],
        "number"
    );
    // Hash-based invalidation contract: different schemas must produce different keys.
    assert_ne!(tool_cache_key(&tools_v1), tool_cache_key(&tools_v2));
}

#[test]
fn tool_cache_hits_on_same_tools() {
    use crate::provider::ToolDefinition;
    let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
    let tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "Run".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: None,
    }];
    let first = provider.get_or_build_api_tools(&tools);
    let second = provider.get_or_build_api_tools(&tools);
    assert_eq!(first, second);
    let expected = tool_cache_key(&tools);
    let cached_hash = provider.tool_cache.lock().as_ref().map(|(h, _)| *h);
    assert_eq!(cached_hash, Some(expected));
}

// --- #1093: cache_user_messages toggle ---

#[test]
fn with_cache_user_messages_builder() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 256).with_cache_user_messages(false);
    assert!(!provider.cache_user_messages);
    let provider2 = ClaudeProvider::new("k".into(), "m".into(), 256);
    assert!(provider2.cache_user_messages);
}

#[test]
fn clone_preserves_cache_user_messages() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 256).with_cache_user_messages(false);
    let cloned = provider.clone();
    assert!(!cloned.cache_user_messages);
}

#[test]
fn store_cache_usage_updates_last_usage() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 256);
    assert!(provider.last_usage().is_none());

    let usage = ApiUsage {
        input_tokens: 42,
        output_tokens: 17,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        iterations: Vec::new(),
    };
    provider.store_cache_usage(&usage);

    assert_eq!(provider.last_usage(), Some((42, 17)));
}

/// C1 (issue #6549 review): `AnyProvider::last_ttft_ms()` must delegate to the wrapped
/// provider's own `last_ttft_ms()` — the enum dispatch previously fell through to the
/// `LlmProvider` trait default (`None`) for every variant, silently defeating the
/// per-message usage ledger's `ttft_ms`/`tokens_per_sec` columns.
#[test]
fn any_provider_claude_surfaces_last_ttft_ms() {
    let provider = ClaudeProvider::new("k".into(), "m".into(), 256);
    assert!(provider.last_ttft_ms().is_none());
    provider.usage.record_ttft(123);
    assert_eq!(provider.last_ttft_ms(), Some(123));

    let any = crate::any::AnyProvider::Claude(provider);
    assert_eq!(
        LlmProvider::last_ttft_ms(&any),
        Some(123),
        "AnyProvider must delegate last_ttft_ms to the inner provider"
    );
}

/// C1 (issue #6549 review): `MaskedProvider` had the identical omission as `AnyProvider` —
/// `last_usage`/`last_cache_usage` were delegated but `last_ttft_ms` was not.
#[test]
fn masked_provider_surfaces_last_ttft_ms() {
    #[derive(Debug)]
    struct NoopMasker;
    impl crate::masking::OutboundMasker for NoopMasker {
        fn mask(&self, _text: &str) -> Option<String> {
            None
        }
    }

    let provider = ClaudeProvider::new("k".into(), "m".into(), 256);
    provider.usage.record_ttft(456);
    let any = crate::any::AnyProvider::Claude(provider);
    let masked = crate::masking::MaskedProvider::new(any, std::sync::Arc::new(NoopMasker));

    assert_eq!(
        LlmProvider::last_ttft_ms(&masked),
        Some(456),
        "MaskedProvider must delegate last_ttft_ms to the inner provider"
    );
}

// ── Opus no-prefill: trailing assistant messages must be stripped ──────────

#[test]
fn build_request_opus_thinking_strips_trailing_assistant() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-8".into(), 32_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let messages = vec![
        Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert!(
        msgs.last()
            .and_then(|m| m["role"].as_str())
            .is_none_or(|r| r != "assistant"),
        "trailing assistant message must be stripped for Opus with thinking"
    );
}

#[test]
fn build_request_opus_4_8_no_thinking_strips_trailing_assistant() {
    // #5903: Opus 4.8 rejects prefill unconditionally, even with thinking disabled.
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-8".into(), 32_000);
    let messages = vec![
        Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert!(
        msgs.last()
            .and_then(|m| m["role"].as_str())
            .is_none_or(|r| r != "assistant"),
        "trailing assistant message must be stripped for Opus 4.8 regardless of thinking state"
    );
}

#[test]
fn build_request_sonnet_5_no_thinking_strips_trailing_assistant() {
    // #5903: Sonnet 5 rejects prefill unconditionally, even with thinking disabled.
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5".into(), 32_000);
    let messages = vec![
        Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert!(
        msgs.last()
            .and_then(|m| m["role"].as_str())
            .is_none_or(|r| r != "assistant"),
        "trailing assistant message must be stripped for Sonnet 5 regardless of thinking state"
    );
}

#[test]
fn build_request_sonnet_4_6_thinking_strips_trailing_assistant() {
    // #5903: legacy Sonnet 4.6 rejects prefill unconditionally, even though it
    // does not prefer effort-based thinking (`prefers_effort` is false for it).
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5_000,
        })
        .unwrap();
    let messages = vec![
        Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert!(
        msgs.last()
            .and_then(|m| m["role"].as_str())
            .is_none_or(|r| r != "assistant"),
        "Sonnet 4.6 must strip trailing assistant messages regardless of thinking state"
    );
}

#[test]
fn build_request_sonnet_4_6_no_thinking_strips_trailing_assistant() {
    // #5903: rejection is independent of thinking being configured at all.
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 32_000);
    let messages = vec![
        Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert!(
        msgs.last()
            .and_then(|m| m["role"].as_str())
            .is_none_or(|r| r != "assistant"),
        "Sonnet 4.6 must strip trailing assistant messages even without thinking configured"
    );
}

#[test]
fn build_request_opus_4_6_no_thinking_keeps_trailing_assistant() {
    // Opus 4.6 is intentionally excluded from unconditional `rejects_prefill`: it
    // only rejects prefill while thinking is active (existing `prefers_effort` gate).
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-6".into(), 32_000);
    let messages = vec![
        Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.last().and_then(|m| m["role"].as_str()),
        Some("assistant"),
        "Opus 4.6 keeps trailing assistant message when thinking is disabled"
    );
}

// ── #6146: no-prefill gate must apply on every request-construction path,
// not just build_request() ─────────────────────────────────────────────────

#[test]
fn no_prefill_true_for_opus_4_6_with_thinking() {
    // Opus 4.6 is excluded from unconditional `rejects_prefill` (#6145) — it only rejects
    // prefill while thinking is active, so this isolates the `prefers_effort` branch.
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-6".into(), 32_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let (thinking_param, _, _) = provider.build_thinking_param();
    assert!(provider.no_prefill(thinking_param.as_ref()));
}

#[test]
fn no_prefill_false_for_opus_4_6_without_thinking() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-6".into(), 32_000);
    let (thinking_param, _, _) = provider.build_thinking_param();
    assert!(!provider.no_prefill(thinking_param.as_ref()));
}

#[test]
fn no_prefill_true_for_sonnet_4_6_with_thinking() {
    // Sonnet 4.6 rejects prefill unconditionally since #6145 (`rejects_prefill`), regardless
    // of whether thinking is configured.
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5_000,
        })
        .unwrap();
    let (thinking_param, _, _) = provider.build_thinking_param();
    assert!(provider.no_prefill(thinking_param.as_ref()));
}

#[test]
fn no_prefill_true_for_sonnet_4_6_without_thinking() {
    // Companion to the case above: `rejects_prefill` is unconditional, so this must also be
    // true with no thinking configured at all — the case #6145 added and this branch's
    // pre-rebase `no_prefill()` would have missed.
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 32_000);
    let (thinking_param, _, _) = provider.build_thinking_param();
    assert!(provider.no_prefill(thinking_param.as_ref()));
}

#[test]
fn strip_trailing_assistant_structured_removes_only_trailing_run() {
    let mut wire = request::WireHistory::from_messages(vec![
        StructuredApiMessage {
            role: "user".into(),
            content: StructuredContent::Text("hi".into()),
        },
        StructuredApiMessage {
            role: "assistant".into(),
            content: StructuredContent::Text("mid".into()),
        },
        StructuredApiMessage {
            role: "user".into(),
            content: StructuredContent::Text("follow-up".into()),
        },
        StructuredApiMessage {
            role: "assistant".into(),
            content: StructuredContent::Text("trailing".into()),
        },
    ]);
    ClaudeProvider::strip_trailing_assistant_structured(true, &mut wire);
    assert_eq!(
        wire.messages().len(),
        3,
        "only the trailing assistant run is stripped"
    );
    assert_eq!(wire.messages().last().unwrap().role, "user");
    assert_eq!(wire.history_indices(), [0, 1, 2]);
}

#[test]
fn strip_trailing_assistant_structured_noop_when_gate_false() {
    let mut wire = request::WireHistory::from_messages(vec![StructuredApiMessage {
        role: "assistant".into(),
        content: StructuredContent::Text("trailing".into()),
    }]);
    ClaudeProvider::strip_trailing_assistant_structured(false, &mut wire);
    assert_eq!(
        wire.messages().len(),
        1,
        "no_prefill=false must never strip messages"
    );
}

#[test]
fn strip_trailing_assistant_plain_removes_only_trailing_run() {
    let mut messages = vec![
        ApiMessage {
            role: "user",
            content: "hi".into(),
        },
        ApiMessage {
            role: "assistant",
            content: "trailing".into(),
        },
    ];
    ClaudeProvider::strip_trailing_assistant_plain(true, &mut messages);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages.last().unwrap().role, "user");
}

#[test]
fn debug_request_json_tools_branch_strips_trailing_assistant_for_opus_thinking() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-8".into(), 32_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let messages = vec![
        Message::from_legacy(Role::User, "hello"),
        Message::from_legacy(Role::Assistant, "world"),
    ];
    let tools = vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        output_schema: None,
    }];

    let body = provider.debug_request_json(&messages, &tools, false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "only the trailing assistant message should be stripped, not the whole history"
    );
    assert_eq!(
        msgs[0]["role"].as_str(),
        Some("user"),
        "tool-use requests must strip trailing assistant messages for Opus with thinking"
    );
}

#[test]
fn debug_request_json_tools_branch_keeps_trailing_assistant_without_thinking() {
    // Opus 4.6 is the only current model whose prefill rejection is thinking-gated
    // (`prefers_effort`) rather than unconditional (`rejects_prefill`, #6145) — Opus 4.8
    // would strip here regardless of thinking state.
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-6".into(), 32_000);
    let messages = vec![
        Message::from_legacy(Role::User, "hello"),
        Message::from_legacy(Role::Assistant, "world"),
    ];
    let tools = vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        output_schema: None,
    }];

    let body = provider.debug_request_json(&messages, &tools, false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.last().and_then(|m| m["role"].as_str()),
        Some("assistant"),
        "trailing assistant message must be preserved when thinking is disabled"
    );
}

#[test]
fn debug_request_json_image_branch_strips_trailing_assistant_for_opus_thinking() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-8".into(), 32_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let messages = vec![
        Message::from_parts(
            Role::User,
            vec![MessagePart::Image(Box::new(ImageData {
                data: vec![1, 2, 3],
                mime_type: "image/png".into(),
            }))],
        ),
        Message::from_legacy(Role::Assistant, "world"),
    ];

    let body = provider.debug_request_json(&messages, &[], false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "only the trailing assistant message should be stripped, not the whole history"
    );
    assert_eq!(
        msgs[0]["role"].as_str(),
        Some("user"),
        "vision requests must strip trailing assistant messages for Opus with thinking"
    );
}

#[test]
fn debug_request_json_plain_branch_strips_trailing_assistant_for_opus_thinking() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-8".into(), 32_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let messages = vec![
        Message::from_legacy(Role::User, "hello"),
        Message::from_legacy(Role::Assistant, "world"),
    ];

    let body = provider.debug_request_json(&messages, &[], false);
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "only the trailing assistant message should be stripped, not the whole history"
    );
    assert_eq!(
        msgs[0]["role"].as_str(),
        Some("user"),
        "plain (no tools, no image) requests must strip trailing assistant messages too"
    );
}

#[test]
fn build_request_opus_4_6_thinking_strips_trailing_assistant() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-4-6".into(), 32_000)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .unwrap();
    let messages = vec![
        Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::Assistant,
            content: "world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];
    let req = provider.build_request(&messages, false).build().unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert!(
        msgs.last()
            .and_then(|m| m["role"].as_str())
            .is_none_or(|r| r != "assistant"),
        "Opus 4.6 must still strip trailing assistant messages when thinking is enabled"
    );
}

#[test]
fn thinking_capability_sonnet_4_6_rejects_prefill() {
    let cap = thinking_capability("claude-sonnet-4-6-20250514");
    assert!(cap.rejects_prefill);
}

#[test]
fn thinking_capability_opus_4_6_does_not_reject_prefill_unconditionally() {
    let cap = thinking_capability("claude-opus-4-6");
    assert!(!cap.rejects_prefill);
}

#[test]
fn thinking_capability_opus_4_7_rejects_prefill() {
    let cap = thinking_capability("claude-opus-4-7");
    assert!(cap.rejects_prefill);
}

#[test]
fn thinking_capability_opus_4_8_rejects_prefill() {
    let cap = thinking_capability("claude-opus-4-8");
    assert!(cap.rejects_prefill);
}

#[test]
fn thinking_capability_sonnet_5_rejects_prefill() {
    let cap = thinking_capability("claude-sonnet-5");
    assert!(cap.rejects_prefill);
}

#[test]
fn server_compaction_disabled_by_default() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024);
    assert!(!provider.server_compaction_enabled());
}

#[test]
fn with_server_compaction_enables_flag() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    assert!(provider.server_compaction_enabled());
}

#[test]
fn with_server_compaction_haiku_stays_disabled() {
    // Haiku does not support the compact-2026-01-12 beta; flag must be ignored.
    let provider = ClaudeProvider::new("key".into(), "claude-haiku-4-5-20251001".into(), 1024)
        .with_server_compaction(true);
    assert!(!provider.server_compaction_enabled());
}

#[test]
fn context_management_absent_when_disabled() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024);
    assert!(provider.context_management().is_none());
}

fn compaction_trigger_value(provider: &ClaudeProvider) -> u64 {
    let json = serde_json::to_value(provider.context_management().unwrap()).unwrap();
    json["edits"][0]["trigger"]["value"].as_u64().unwrap()
}

#[test]
fn compaction_default_trigger_is_seventy_percent_of_window() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    assert_eq!(compaction_trigger_value(&provider), 140_000);
    let extended = provider.with_extended_context(true);
    assert_eq!(compaction_trigger_value(&extended), 700_000);
}

#[test]
fn compaction_explicit_trigger_overrides_default() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true)
        .with_server_compaction_trigger(Some(CompactionTrigger::new(60_000).unwrap()));
    assert_eq!(compaction_trigger_value(&provider), 60_000);
}

#[test]
fn context_management_wire_snapshot() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    let json = serde_json::to_string_pretty(&provider.context_management().unwrap()).unwrap();
    insta::assert_snapshot!(json);
}

#[test]
fn thinking_binding_wire_snapshot() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 4096)
        .with_thinking(ThinkingConfig::Adaptive {
            effort: Some(ThinkingEffort::High),
        })
        .unwrap();
    let (param, _, _) = provider.build_thinking_param();
    let json = serde_json::to_string_pretty(&param.unwrap()).unwrap();
    insta::assert_snapshot!(json);
}

#[test]
fn beta_header_includes_compact_when_server_compaction_enabled() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    let header = provider.beta_header(false).unwrap_or_default();
    assert!(
        header.contains("compact-2026-01-12"),
        "beta header must include compact beta when server_compaction is on"
    );
}

#[test]
fn beta_header_excludes_compact_when_disabled() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024);
    let header = provider.beta_header(false).unwrap_or_default();
    assert!(
        !header.contains("compact-2026-01-12"),
        "beta header must not include compact beta when server_compaction is off"
    );
}

#[test]
fn compaction_content_block_deserialized() {
    let json = r#"{"type":"compaction","content":"Context summary here"}"#;
    let block: AnthropicContentBlock = serde_json::from_str(json).unwrap();
    assert!(matches!(
        block,
        AnthropicContentBlock::Compaction { content: Some(c) } if c == "Context summary here"
    ));
}

#[test]
fn compaction_content_block_null_content_yields_no_summary() {
    let json = r#"{"content":[{"type":"compaction","content":null}],"stop_reason":"end_turn"}"#;
    let resp: ToolApiResponse = serde_json::from_str(json).unwrap();
    let compaction = parse_tool_response(resp).take_compaction();
    assert!(compaction.is_none());
}

#[test]
fn compaction_content_block_serializes_with_content_field() {
    let block = AnthropicContentBlock::Compaction {
        content: Some("sum".into()),
    };
    assert_eq!(
        serde_json::to_value(&block).unwrap(),
        serde_json::json!({"type": "compaction", "content": "sum"})
    );
}

#[test]
fn tool_response_reports_dropped_thinking() {
    let json = r#"{"content":[],"input_transformations":[
        {"type":"thinking_dropped","path":"messages.4.content.0","reason":"prefix_binding_mismatch"}
    ]}"#;
    let resp: ToolApiResponse = serde_json::from_str(json).unwrap();
    assert_eq!(
        resp.thinking_replay(),
        crate::ThinkingReplay::Dropped {
            first_message_index: 4
        }
    );
}

// --- graceful degradation tests (SEC-COMPACT-03) ---

#[test]
fn server_compaction_not_rejected_by_default() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    assert!(!provider.is_server_compaction_rejected());
}

#[test]
fn beta_header_excluded_after_rejection() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    provider.beta_latches.reject(BetaFeature::Compact);
    let header = provider.beta_header(false).unwrap_or_default();
    assert!(
        !header.contains("compact-2026-01-12"),
        "compact beta must be excluded once rejected"
    );
}

#[test]
fn context_management_absent_after_rejection() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    provider.beta_latches.reject(BetaFeature::Compact);
    assert!(
        provider.context_management().is_none(),
        "context_management must be None after beta header rejection"
    );
}

#[test]
fn clone_shares_rejection_flag() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    let clone = provider.clone();
    provider.beta_latches.reject(BetaFeature::Compact);
    assert!(
        clone.is_server_compaction_rejected(),
        "clone must share the rejection Arc"
    );
}

#[test]
fn handle_beta_rejection_latches_named_beta_once() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    let body = "Unexpected value(s) `compact-2026-01-12` for the `anthropic-beta` header.";
    assert!(provider.handle_beta_rejection(reqwest::StatusCode::BAD_REQUEST, body, "typed"));
    assert!(provider.is_server_compaction_rejected());
    assert!(
        !provider.handle_beta_rejection(reqwest::StatusCode::BAD_REQUEST, body, "typed"),
        "a latched beta is no longer sent, so a repeat must not retry"
    );
}

#[test]
fn handle_beta_rejection_ignores_validation_errors_that_name_no_header() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    for body in [
        r#"{"error":{"message":"context_management.edits.0.trigger.value: must be at least 50000"}}"#,
        r#"{"error":{"message":"unknown beta"}}"#,
        r#"{"error":{"message":"max_tokens: field required"}}"#,
    ] {
        assert!(!provider.handle_beta_rejection(reqwest::StatusCode::BAD_REQUEST, body, ""));
    }
    assert!(!provider.is_server_compaction_rejected());
}

#[test]
fn handle_beta_rejection_ignores_non_400() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_server_compaction(true);
    assert!(!provider.handle_beta_rejection(
        reqwest::StatusCode::UNAUTHORIZED,
        "compact-2026-01-12",
        ""
    ));
}

#[test]
fn binding_mismatch_is_not_read_as_binding_header_rejection() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 4096);
    let body = "messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound \
                to a different conversation. That setting requires the \
                `thinking-binding-controls-2026-08-01` value in the `anthropic-beta` header.";
    assert!(!provider.handle_beta_rejection(reqwest::StatusCode::BAD_REQUEST, body, ""));
    assert!(
        !provider
            .beta_latches
            .is_rejected(BetaFeature::ThinkingBinding)
    );
}

#[test]
fn binding_header_rejection_latches_and_drops_block_binding() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 4096);
    assert!(
        provider
            .beta_header(false)
            .unwrap()
            .contains("thinking-binding-controls-2026-08-01")
    );
    let body = "Unexpected value(s) `thinking-binding-controls-2026-08-01` for the \
                `anthropic-beta` header.";
    assert!(provider.handle_beta_rejection(reqwest::StatusCode::BAD_REQUEST, body, ""));
    assert!(provider.beta_header(false).is_none());
    assert!(provider.build_thinking_param().0.is_none());
}

#[test]
fn with_server_compaction_requires_model_support() {
    for (model, expected) in [
        ("claude-haiku-4-5-20251001", false),
        ("claude-sonnet-4-5-20250929", false),
        ("claude-opus-9", false),
        ("claude-haiku-5-5", true),
        ("claude-opus-4-6", true),
    ] {
        let provider =
            ClaudeProvider::new("key".into(), model.into(), 1024).with_server_compaction(true);
        assert_eq!(provider.server_compaction_enabled(), expected, "{model}");
    }
}

// --- thinking binding, sampling, headroom ---

#[test]
fn preserved_thinking_model_sends_drop_block_with_explicit_adaptive() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5-5".into(), 4096);
    let (param, _, effort) = provider.build_thinking_param();
    let json = serde_json::to_value(param.unwrap()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "type": "adaptive",
            "block_binding": {"prefix_mismatch_behavior": "drop_block"}
        })
    );
    assert!(effort.is_none());
    assert!(
        provider
            .beta_header(true)
            .unwrap()
            .contains("thinking-binding-controls-2026-08-01")
    );
}

#[test]
fn non_preserved_models_never_send_block_binding() {
    for model in [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-mythos-5-1",
        "claude-sonnet-4-6",
        "claude-future-9",
    ] {
        let provider = ClaudeProvider::new("key".into(), model.into(), 4096)
            .with_thinking(ThinkingConfig::Adaptive { effort: None })
            .unwrap();
        let json = serde_json::to_value(provider.build_thinking_param().0.unwrap()).unwrap();
        assert!(json.get("block_binding").is_none(), "{model}");
        assert!(provider.beta_header(true).is_none(), "{model}");
    }
}

#[test]
fn unknown_model_gets_no_implicit_thinking_param() {
    let provider = ClaudeProvider::new("key".into(), "claude-future-9".into(), 4096);
    assert!(provider.build_thinking_param().0.is_none());
}

#[test]
fn extended_thinking_on_opus_5_5_converts_to_effort_with_binding() {
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 32_000)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 20_000,
        })
        .unwrap();
    let (param, _, effort) = provider.build_thinking_param();
    let param = param.unwrap();
    assert_eq!(param.thinking_type, ThinkingType::Adaptive);
    assert!(param.budget_tokens.is_none());
    assert!(param.block_binding.is_some());
    assert_eq!(effort, Some(ThinkingEffort::High));
}

#[test]
fn sampling_models_never_send_temperature() {
    let overrides = GenerationOverrides {
        temperature: Some(0.2),
        ..GenerationOverrides::default()
    };
    for (model, sent) in [
        ("claude-opus-5-5", false),
        ("claude-sonnet-5", false),
        ("claude-haiku-5-5", false),
        ("claude-future-9", false),
        ("claude-sonnet-4-6", true),
        ("claude-haiku-4-5-20251001", true),
    ] {
        let provider = ClaudeProvider::new("key".into(), model.into(), 1024)
            .with_generation_overrides(overrides.clone());
        let body =
            provider.debug_request_json(&[Message::from_legacy(Role::User, "hi")], &[], false);
        assert_eq!(body.get("temperature").is_some(), sent, "{model}");
    }
}

#[test]
fn always_on_thinking_models_get_max_tokens_headroom() {
    let always_on = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024);
    assert_eq!(always_on.max_tokens, MIN_MAX_TOKENS_WITH_THINKING);
    let mut explicit = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024);
    explicit.set_thinking(None).unwrap();
    assert_eq!(explicit.max_tokens, MIN_MAX_TOKENS_WITH_THINKING);

    let optional = ClaudeProvider::new("key".into(), "claude-opus-4-6".into(), 1024);
    assert_eq!(optional.max_tokens, 1024);
}

// --- context window ceiling ---

#[test]
fn context_window_is_capped_at_200k_unless_extended() {
    for (model, default_window, extended_window) in [
        ("claude-opus-5-5", Some(200_000), Some(1_000_000)),
        ("claude-sonnet-4-6", Some(200_000), Some(1_000_000)),
        ("claude-fable-5-1", Some(200_000), Some(1_000_000)),
        ("claude-haiku-5-5", Some(200_000), Some(1_000_000)),
        ("claude-haiku-4-5-20251001", Some(200_000), Some(200_000)),
        ("claude-sonnet-4-5-20250929", Some(200_000), Some(1_000_000)),
        ("claude-future-9", None, None),
        ("gpt-4o", None, None),
    ] {
        let provider = ClaudeProvider::new("key".into(), model.into(), 1024);
        assert_eq!(provider.context_window(), default_window, "{model}");
        let provider = provider.with_extended_context(true);
        assert_eq!(provider.context_window(), extended_window, "{model}");
    }
}

#[test]
fn extended_context_beta_header_only_for_models_that_need_it() {
    let legacy = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024)
        .with_extended_context(true);
    assert!(
        legacy
            .beta_header(false)
            .unwrap()
            .contains("context-1m-2025-08-07")
    );
    let current = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_extended_context(true);
    assert!(
        !current
            .beta_header(false)
            .unwrap_or_default()
            .contains("context-1m-2025-08-07")
    );
}

// --- wire-level: beta latch retry and structured output ---

fn error_400(message: &str) -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message}
    }))
}

#[tokio::test]
async fn rejected_compact_beta_retries_once_without_it() {
    use crate::testing::claude_tool_use_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(error_400(
            "Unexpected value(s) `compact-2026-01-12` for the `anthropic-beta` header.",
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_response(
            "t",
            "toolu_1",
            &serde_json::json!({}),
        ))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()))
        .with_server_compaction(true);
    let messages = vec![Message::from_legacy(Role::User, "hi")];
    let result = provider.chat_with_tools(&messages, &[]).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(provider.is_server_compaction_rejected());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(
        first["context_management"]["edits"][0]["type"],
        "compact_20260112"
    );
    assert!(second.get("context_management").is_none());
    assert!(reqs[1].headers.get("anthropic-beta").is_none());
}

#[tokio::test]
async fn context_management_validation_400_surfaces_without_retry() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(error_400(
            "context_management.edits.0.trigger.value: must be at least 50000",
        ))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()))
        .with_server_compaction(true);
    let messages = vec![Message::from_legacy(Role::User, "hi")];
    let result = provider.chat_with_tools(&messages, &[]).await;
    assert_matches!(result, Err(LlmError::ApiError { status: 400, .. }));
    assert!(!provider.is_server_compaction_rejected());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn binding_mismatch_400_maps_to_typed_error() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(error_400(
            "messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound \
             to a different conversation. Remove the block.",
        ))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let messages = vec![Message::from_legacy(Role::User, "hi")];
    let result = provider.chat_with_tools(&messages, &[]).await;
    assert_matches!(result, Err(LlmError::ThinkingBindingMismatch));
    assert!(
        !provider
            .beta_latches
            .is_rejected(BetaFeature::ThinkingBinding)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
struct TypedOut {
    value: String,
}

#[tokio::test]
async fn chat_typed_forces_tool_choice_on_models_that_accept_it() {
    use crate::testing::claude_tool_use_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_response(
            "submit_TypedOut",
            "toolu_1",
            &serde_json::json!({"value": "ok"}),
        ))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("key".into(), "claude-opus-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let out: TypedOut = provider
        .chat_typed(&[Message::from_legacy(Role::User, "go")])
        .await
        .unwrap();
    assert_eq!(out.value, "ok");

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        body["tool_choice"],
        serde_json::json!({"type": "tool", "name": "submit_TypedOut"})
    );
}

#[tokio::test]
async fn chat_typed_uses_auto_and_retries_once_when_forced_choice_is_forbidden() {
    use crate::testing::{claude_messages_response, claude_tool_use_response};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_messages_response("plain text, no tool"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_response(
            "submit_TypedOut",
            "toolu_1",
            &serde_json::json!({"value": "second"}),
        ))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("key".into(), "claude-future-9".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let out: TypedOut = provider
        .chat_typed(&[Message::from_legacy(Role::User, "go")])
        .await
        .unwrap();
    assert_eq!(out.value, "second");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert_eq!(first["tool_choice"], serde_json::json!({"type": "auto"}));
    assert!(
        first["system"]
            .to_string()
            .contains("calling the `submit_TypedOut` tool")
    );
    let nudge = second["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(nudge["role"], "user");
    assert!(nudge.to_string().contains("submit_TypedOut"));
}

#[tokio::test]
async fn chat_typed_second_miss_returns_structured_parse() {
    use crate::testing::claude_messages_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_messages_response("still no tool"))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("key".into(), "claude-future-9".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let result: Result<TypedOut, _> = provider
        .chat_typed(&[Message::from_legacy(Role::User, "go")])
        .await;
    assert_matches!(result, Err(LlmError::StructuredParse(_)));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[test]
fn output_schema_forwarding_enabled_appends_hint() {
    use zeph_common::types::ToolDefinition;
    let tool = ToolDefinition {
        name: "my_tool".into(),
        description: "Base description".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: Some(serde_json::json!({"type": "string"})),
    };
    let provider = crate::claude::ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1000)
        .with_output_schema_forwarding(true, 512, usize::MAX);
    let api_tools = provider.get_or_build_api_tools(&[tool]);
    let desc = api_tools[0]["description"].as_str().unwrap();
    assert!(
        desc.contains("Expected output schema (JSON):"),
        "hint must appear in description when forward_output_schema=true"
    );
    assert!(
        !desc.contains("too large"),
        "small schema must not trigger stub"
    );
}

#[test]
fn output_schema_forwarding_disabled_no_hint() {
    use zeph_common::types::ToolDefinition;
    let tool = ToolDefinition {
        name: "my_tool".into(),
        description: "Base description".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: Some(serde_json::json!({"type": "string"})),
    };
    let provider = crate::claude::ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1000);
    let api_tools = provider.get_or_build_api_tools(&[tool]);
    let desc = api_tools[0]["description"].as_str().unwrap();
    assert!(
        !desc.contains("Expected output schema"),
        "hint must NOT appear when forward_output_schema=false"
    );
}

#[test]
fn output_schema_forwarding_truncates_large_schema() {
    use zeph_common::types::ToolDefinition;
    let large_schema = serde_json::json!({"description": "x".repeat(600)});
    let tool = ToolDefinition {
        name: "big_tool".into(),
        description: "Base".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: Some(large_schema),
    };
    let provider = crate::claude::ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1000)
        .with_output_schema_forwarding(true, 64, usize::MAX);
    let api_tools = provider.get_or_build_api_tools(&[tool]);
    let desc = api_tools[0]["description"].as_str().unwrap();
    assert!(
        desc.contains("too large"),
        "oversized schema must use stub message"
    );
}

#[test]
fn tool_cache_key_sensitive_to_output_schema() {
    use crate::claude::cache::tool_cache_key;
    use zeph_common::types::ToolDefinition;
    let base = ToolDefinition {
        name: "t".into(),
        description: "d".into(),
        parameters: serde_json::json!({}),
        output_schema: None,
    };
    let with_schema = ToolDefinition {
        output_schema: Some(serde_json::json!({"type": "string"})),
        ..base.clone()
    };
    assert_ne!(
        tool_cache_key(&[base]),
        tool_cache_key(&[with_schema]),
        "cache key must differ when output_schema changes"
    );
}

#[test]
fn tool_cache_key_sensitive_to_description() {
    use crate::claude::cache::tool_cache_key;
    use zeph_common::types::ToolDefinition;
    let base = ToolDefinition {
        name: "t".into(),
        description: "original".into(),
        parameters: serde_json::json!({}),
        output_schema: None,
    };
    let changed = ToolDefinition {
        description: "changed".into(),
        ..base.clone()
    };
    assert_ne!(
        tool_cache_key(&[base]),
        tool_cache_key(&[changed]),
        "cache key must differ when description changes"
    );
}

#[test]
fn test_claude_default_output_schema_hint_bytes_is_1024() {
    use zeph_common::types::ToolDefinition;
    let schema =
        serde_json::json!({"type": "object", "properties": {"result": {"type": "string"}}});
    let compact = serde_json::to_string(&schema).unwrap();
    assert!(
        compact.len() < 1024,
        "test schema must be under 1024 bytes for this assertion to be meaningful"
    );
    let tool = ToolDefinition {
        name: "do_something".into(),
        description: "Do something".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: Some(schema),
    };
    let provider = crate::claude::ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1000)
        .with_output_schema_forwarding(true, 1024, usize::MAX);
    let api_tools = provider.get_or_build_api_tools(&[tool]);
    let desc = api_tools[0]["description"].as_str().unwrap();
    assert!(
        desc.contains("Expected output schema"),
        "schema under 1024 bytes must be forwarded with default budget"
    );
    assert!(
        !desc.contains("too large"),
        "schema under 1024 bytes must not trigger stub with default budget"
    );
}

#[test]
fn test_claude_stub_used_when_schema_exceeds_1024_bytes() {
    use zeph_common::types::ToolDefinition;
    let large_schema = serde_json::json!({"description": "y".repeat(1100)});
    let tool = ToolDefinition {
        name: "large_tool".into(),
        description: "Base".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: Some(large_schema),
    };
    let provider = crate::claude::ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 1000)
        .with_output_schema_forwarding(true, 1024, usize::MAX);
    let api_tools = provider.get_or_build_api_tools(&[tool]);
    let desc = api_tools[0]["description"].as_str().unwrap();
    assert!(
        desc.contains("too large"),
        "schema exceeding 1024 bytes must use stub with default budget"
    );
}

// ─── CacheTtl tests ──────────────────────────────────────────────────────────

#[test]
fn cache_ttl_ephemeral_serializes_without_ttl_field() {
    use super::cache::build_cache_control;
    let cc = build_cache_control(None);
    let v = serde_json::to_value(&cc).unwrap();
    assert_eq!(v, serde_json::json!({"type": "ephemeral"}));
}

#[test]
fn cache_ttl_one_hour_serializes_with_ttl_field() {
    use super::cache::build_cache_control;
    let cc = build_cache_control(Some(CacheTtl::OneHour));
    let v = serde_json::to_value(&cc).unwrap();
    assert_eq!(v, serde_json::json!({"type": "ephemeral", "ttl": "1h"}));
}

#[test]
fn cache_ttl_deserialize_rejects_unknown_string() {
    let result = serde_json::from_str::<CacheTtl>("\"30m\"");
    assert!(
        result.is_err(),
        "unknown TTL string must fail deserialization"
    );
}

#[test]
fn cache_ttl_one_hour_toml_round_trip() {
    use serde::Deserialize;
    let v = toml::Value::String("1h".into());
    let ttl = CacheTtl::deserialize(v).unwrap();
    assert_eq!(ttl, CacheTtl::OneHour);
}

#[test]
fn cache_ttl_ephemeral_toml_round_trip() {
    use serde::Deserialize;
    let v = toml::Value::String("ephemeral".into());
    let ttl = CacheTtl::deserialize(v).unwrap();
    assert_eq!(ttl, CacheTtl::Ephemeral);
}

#[test]
fn beta_header_includes_extended_cache_ttl_for_one_hour_on_legacy_models() {
    for model in ["claude-sonnet-4-5-20250929", "claude-haiku-4-5-20251001"] {
        let p = ClaudeProvider::new("k".into(), model.into(), 1024)
            .with_prompt_cache_ttl(Some(CacheTtl::OneHour));
        let header = p.beta_header(false).unwrap_or_default();
        assert!(
            header.contains("extended-cache-ttl-2025-04-25"),
            "{model}: beta header must include extended-cache-ttl-2025-04-25 for OneHour TTL"
        );
    }
}

#[test]
fn beta_header_omits_extended_cache_ttl_for_current_and_unknown_models() {
    for model in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-unknown-9"] {
        let p = ClaudeProvider::new("k".into(), model.into(), 1024)
            .with_prompt_cache_ttl(Some(CacheTtl::OneHour));
        let header = p.beta_header(false).unwrap_or_default();
        assert!(
            !header.contains("extended-cache-ttl-2025-04-25"),
            "{model}: no extended-cache-ttl beta header expected"
        );
    }
}

#[test]
fn beta_header_omits_extended_cache_ttl_for_ephemeral() {
    let p = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024);
    let header = p.beta_header(false).unwrap_or_default();
    assert!(
        !header.contains("extended-cache-ttl-2025-04-25"),
        "beta header must not include extended-cache-ttl for default ephemeral TTL"
    );
}

#[test]
fn tool_cache_control_uses_typed_path_for_one_hour() {
    use zeph_common::types::ToolDefinition;
    let tool = ToolDefinition {
        name: "my_tool".into(),
        description: "desc".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: None,
    };
    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 1024)
        .with_prompt_cache_ttl(Some(CacheTtl::OneHour));
    let api_tools = provider.get_or_build_api_tools(&[tool]);
    let cc = &api_tools[0]["cache_control"];
    assert_eq!(cc["type"], "ephemeral");
    assert_eq!(cc["ttl"], "1h");
}

#[test]
fn split_system_into_blocks_propagates_one_hour_ttl() {
    use super::cache::split_system_into_blocks;
    // Use a long enough string to exceed the cache threshold for any model
    let system = "A".repeat(20_000);
    let blocks = split_system_into_blocks(&system, TEST_CLAUDE_MODEL, Some(CacheTtl::OneHour));
    let cached = blocks
        .iter()
        .find(|b| b.cache_control.is_some())
        .expect("at least one cached block");
    let cc = cached.cache_control.as_ref().unwrap();
    assert_eq!(cc.ttl, Some(CacheTtl::OneHour));
}

#[test]
fn apply_cache_breakpoint_propagates_one_hour_ttl() {
    use super::cache::apply_cache_breakpoint;
    use super::types::{AnthropicContentBlock, StructuredContent};
    let mut chat = vec![
        StructuredApiMessage {
            role: "user".into(),
            content: StructuredContent::Text("first".into()),
        },
        StructuredApiMessage {
            role: "assistant".into(),
            content: StructuredContent::Text("reply".into()),
        },
        StructuredApiMessage {
            role: "user".into(),
            content: StructuredContent::Text("second".into()),
        },
    ];
    apply_cache_breakpoint(&mut chat, Some(CacheTtl::OneHour));
    let breakpoint_msg = chat.iter().find(|m| {
        if let StructuredContent::Blocks(blocks) = &m.content {
            blocks.iter().any(|b| {
                if let AnthropicContentBlock::Text { cache_control, .. } = b {
                    cache_control
                        .as_ref()
                        .is_some_and(|cc| cc.ttl == Some(CacheTtl::OneHour))
                } else {
                    false
                }
            })
        } else {
            false
        }
    });
    assert!(
        breakpoint_msg.is_some(),
        "breakpoint message must carry 1h TTL"
    );
}

fn anchor_provider() -> ClaudeProvider {
    ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 512)
}

fn history_items(history: &GatedStructuredHistory) -> Vec<serde_json::Value> {
    match serde_json::to_value(history).unwrap() {
        serde_json::Value::Array(items) => items,
        other => panic!("history must serialize to an array, got {other}"),
    }
}

#[test]
fn leading_assistant_after_skipped_thinking_only_user_gets_anchor() {
    let messages = vec![
        Message::from_parts(
            Role::User,
            vec![MessagePart::ThinkingBlock {
                thinking: "stray".into(),
                signature: "sig".into(),
            }],
        ),
        Message::from_legacy(Role::Assistant, "hello"),
        Message::from_legacy(Role::User, "next"),
    ];
    let (_, history) =
        anchor_provider().structured_history(&messages, None, 0, DirectiveRendering::UserText);
    let items = history_items(&history);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["content"], LEADING_USER_ANCHOR);
    assert_eq!(items[1]["role"], "assistant");
}

#[test]
fn server_compaction_shape_gets_anchor_and_keeps_compaction_block() {
    let messages = vec![
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::Compaction {
                summary: "summary".into(),
            }],
        ),
        Message::from_legacy(Role::User, "continue"),
    ];
    let (_, history) =
        anchor_provider().structured_history(&messages, None, 0, DirectiveRendering::UserText);
    let items = history_items(&history);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["content"], LEADING_USER_ANCHOR);
    assert_eq!(items[1]["role"], "assistant");
    assert_eq!(items[1]["content"][0]["type"], "compaction");
    assert_eq!(items[2]["role"], "user");
}

#[test]
fn leading_tool_pair_keeps_native_blocks_behind_anchor() {
    let messages = vec![
        Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::ThinkingBlock {
                    thinking: "t".into(),
                    signature: "sig".into(),
                },
                MessagePart::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
            ],
        ),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: false,
            }],
        ),
    ];
    let (_, history) =
        anchor_provider().structured_history(&messages, None, 0, DirectiveRendering::UserText);
    let items = history_items(&history);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["content"], LEADING_USER_ANCHOR);
    assert_eq!(items[1]["content"][0]["type"], "thinking");
    assert_eq!(items[1]["content"][1]["type"], "tool_use");
    assert_eq!(items[2]["content"][0]["type"], "tool_result");
}

#[test]
fn no_prefill_all_assistant_history_stays_empty() {
    let messages = vec![Message::from_legacy(Role::Assistant, "only")];
    let provider = anchor_provider();
    assert!(
        provider.no_prefill(None),
        "fixture model must reject prefill"
    );
    let (_, history) =
        provider.structured_history(&messages, None, 0, DirectiveRendering::UserText);
    assert_eq!(history_items(&history), Vec::<serde_json::Value>::new());
}

#[test]
fn leading_user_anchor_carries_no_cache_control() {
    let messages = vec![
        Message::from_legacy(Role::Assistant, "hello"),
        Message::from_legacy(Role::User, "next"),
    ];
    let (_, history) =
        anchor_provider().structured_history(&messages, None, 4, DirectiveRendering::UserText);
    let items = history_items(&history);
    assert_eq!(items[0]["content"], LEADING_USER_ANCHOR);
    assert!(!items[0].to_string().contains("cache_control"));
}

#[test]
fn leading_assistant_gets_user_anchor_plain() {
    let history = GatedPlainHistory::new(vec![
        ApiMessage {
            role: "assistant",
            content: "hello".into(),
        },
        ApiMessage {
            role: "user",
            content: "next".into(),
        },
    ]);
    let json = serde_json::to_value(&history).unwrap();
    assert_eq!(json[0]["role"], "user");
    assert_eq!(json[0]["content"], LEADING_USER_ANCHOR);
    assert_eq!(json[1]["role"], "assistant");
}

#[test]
fn leading_user_history_is_unchanged() {
    let history = GatedPlainHistory::new(vec![ApiMessage {
        role: "user",
        content: "hi".into(),
    }]);
    let json = serde_json::to_value(&history).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
}

#[test]
fn debug_request_json_omits_user_only_messages() {
    let provider = ClaudeProvider::new("key".into(), TEST_CLAUDE_MODEL.into(), 256);
    let messages = vec![
        Message::from_legacy(Role::System, "sys"),
        Message::from_legacy(Role::User, "visible question"),
        Message {
            metadata: crate::provider::MessageMetadata::user_only(),
            ..Message::from_legacy(Role::Assistant, "SECRET-HIDDEN-ORIGINAL")
        },
    ];
    for tools_json in [
        provider.debug_request_json(&messages, &[], false),
        provider.debug_request_json(&messages, &[], true),
    ] {
        let json = tools_json.to_string();
        assert!(json.contains("visible question"));
        assert!(!json.contains("SECRET-HIDDEN-ORIGINAL"));
    }
}

// --- append-only tool loop requests, ordered thinking replay ---

fn turn_input(text: &str) -> Message {
    Message {
        metadata: MessageMetadata {
            origin: Some(crate::provider::MessageOrigin::TurnInput),
            ..MessageMetadata::default()
        },
        ..Message::from_legacy(Role::User, text)
    }
}

fn assistant_tool_step(id: &str, thinking: &str) -> [Message; 2] {
    [
        Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::ThinkingBlock {
                    thinking: thinking.into(),
                    signature: format!("sig-{thinking}"),
                },
                MessagePart::ToolUse {
                    id: id.into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls", "all": true}),
                },
            ],
        ),
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }],
        ),
    ]
}

fn bash_tool() -> ToolDefinition {
    ToolDefinition {
        name: "bash".into(),
        description: "Run shell commands".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: None,
    }
}

fn loop_history(steps: usize) -> Vec<Message> {
    let mut messages = vec![
        Message::from_legacy(Role::System, "system"),
        turn_input("go"),
    ];
    for i in 0..steps {
        messages.extend(assistant_tool_step(
            &format!("toolu_{i}"),
            &format!("think {i}"),
        ));
    }
    messages
}

#[test]
fn next_iteration_request_only_appends_to_the_previous_one() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5-5".into(), 4096)
        .with_cache_user_messages(false);
    let tools = [bash_tool()];
    let k = provider.debug_request_json(&loop_history(2), &tools, false);
    let k1 = provider.debug_request_json(&loop_history(3), &tools, false);

    assert_eq!(k["tools"], k1["tools"]);
    assert_eq!(k["system"], k1["system"]);
    let (old, new) = (
        k["messages"].as_array().unwrap(),
        k1["messages"].as_array().unwrap(),
    );
    assert_eq!(new.len(), old.len() + 2);
    assert_eq!(
        &new[..old.len()],
        old.as_slice(),
        "earlier messages must not change"
    );
}

#[test]
fn cache_breakpoint_moves_to_the_new_tail() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5-5".into(), 4096)
        .with_cache_user_messages(true);
    let tools = [bash_tool()];
    for steps in [2, 3] {
        let body = provider.debug_request_json(&loop_history(steps), &tools, false);
        let messages = body["messages"].as_array().unwrap();
        let (last, rest) = messages.split_last().unwrap();
        assert!(
            last.to_string().contains("cache_control"),
            "tail must be marked"
        );
        assert!(
            rest.iter()
                .all(|m| !m.to_string().contains("cache_control")),
            "no older message may be marked"
        );
    }
}

fn thinking_texts(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "thinking")
        .map(|b| b["thinking"].as_str().unwrap().to_owned())
        .collect()
}

fn two_turn_history() -> Vec<Message> {
    let mut messages = vec![
        Message::from_legacy(Role::System, "system"),
        turn_input("one"),
    ];
    messages.extend(assistant_tool_step("toolu_a", "old turn"));
    messages.push(Message::from_legacy(Role::Assistant, "answer one"));
    messages.push(turn_input("two"));
    messages.extend(assistant_tool_step("toolu_b", "current turn"));
    messages
}

#[test]
fn preserved_thinking_models_send_only_current_turn_thinking() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5-5".into(), 4096);
    let body = provider.debug_request_json(&two_turn_history(), &[bash_tool()], false);
    assert_eq!(thinking_texts(&body), ["current turn"]);
}

#[test]
fn other_models_forward_thinking_from_every_turn() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 4096);
    let body = provider.debug_request_json(&two_turn_history(), &[bash_tool()], false);
    assert_eq!(thinking_texts(&body), ["old turn", "current turn"]);
}

#[test]
fn tool_response_keeps_blocks_in_response_order() {
    let json = r#"{"content":[
        {"type":"thinking","thinking":"a","signature":"s1"},
        {"type":"text","text":"plan"},
        {"type":"thinking","thinking":"b","signature":"s2"},
        {"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"ls"}}
    ],"input_transformations":[
        {"type":"thinking_dropped","path":"messages.2.content.0","reason":"prefix_binding_mismatch"}
    ]}"#;
    let resp: ToolApiResponse = serde_json::from_str(json).unwrap();
    let parsed = parse_tool_response(resp);
    let ChatResponse::ToolUse {
        content, replay, ..
    } = parsed
    else {
        panic!("expected ToolUse");
    };
    assert!(matches!(
        content.blocks(),
        [
            crate::provider::AssistantBlock::Thinking(ThinkingBlock::Thinking { thinking: a, .. }),
            crate::provider::AssistantBlock::Text(t),
            crate::provider::AssistantBlock::Thinking(ThinkingBlock::Thinking { thinking: b, .. }),
            crate::provider::AssistantBlock::ToolUse(_),
        ] if a == "a" && t == "plan" && b == "b"
    ));
    assert_eq!(
        replay,
        crate::ThinkingReplay::Dropped {
            first_message_index: 2
        }
    );
}

fn messages_with_marker_checkpoint() -> Vec<Message> {
    vec![
        Message::from_legacy(Role::System, "system prompt"),
        Message::from_legacy(Role::User, "hello"),
        Message {
            role: Role::System,
            content: "[focus checkpoint: MARKER-ONLY-TEXT]".into(),
            parts: vec![],
            metadata: MessageMetadata::marker(),
        },
    ]
}

#[test]
fn marker_checkpoint_never_reaches_the_claude_request() {
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5-5".into(), 4096);
    let messages = messages_with_marker_checkpoint();
    for tools in [vec![], vec![bash_tool()]] {
        let json = provider
            .debug_request_json(&messages, &tools, false)
            .to_string();
        assert!(!json.contains("MARKER-ONLY-TEXT"), "{json}");
        assert!(json.contains("hello"));
    }
}

// ─── Audit follow-ups: usage iterations, structured outputs, refusals, compaction ─────────────

fn refusal_body() -> serde_json::Value {
    serde_json::json!({
        "id": "msg_refusal",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-5-5",
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"type": "refusal", "category": "cyber", "explanation": "no"},
        "usage": {"input_tokens": 3, "output_tokens": 0}
    })
}

async fn mock_messages(body: serde_json::Value) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

fn refused_with_cyber(err: &LlmError) -> bool {
    matches!(
        err,
        LlmError::Refused { category: Some(c), .. } if c.as_str() == "cyber"
    ) && err.forbids_provider_fallback()
}

#[test]
fn usage_iterations_sum_only_compaction_and_leave_top_level_alone() {
    let json = r#"{
        "input_tokens": 10, "output_tokens": 5,
        "cache_creation_input_tokens": 1, "cache_read_input_tokens": 2,
        "iterations": [
            {"type": "compaction", "input_tokens": 100, "output_tokens": 40,
             "cache_creation_input_tokens": 7, "cache_read_input_tokens": 9},
            {"type": "message", "input_tokens": 10, "output_tokens": 5},
            {"type": "future_kind", "input_tokens": 999, "output_tokens": 999}
        ]
    }"#;
    let usage: ApiUsage = serde_json::from_str(json).unwrap();
    let aux = usage.auxiliary().expect("compaction iteration is billed");
    assert_eq!(
        (aux.input, aux.output, aux.cache_creation, aux.cache_read),
        (100, 40, 7, 9)
    );

    let provider = ClaudeProvider::new("k".into(), TEST_CLAUDE_MODEL.into(), 256);
    provider.store_cache_usage(&usage);
    assert_eq!(provider.last_usage(), Some((10, 5)));
    assert_eq!(provider.last_auxiliary_usage(), Some(aux));

    let plain: ApiUsage =
        serde_json::from_str(r#"{"input_tokens": 1, "output_tokens": 1}"#).unwrap();
    provider.store_cache_usage(&plain);
    assert_eq!(
        provider.last_auxiliary_usage(),
        None,
        "reset on the next response"
    );
}

#[tokio::test]
async fn compaction_on_a_text_only_response_is_returned() {
    let server = mock_messages(serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [
            {"type": "compaction", "content": "summary of history"},
            {"type": "text", "text": "answer"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let mut resp = provider
        .chat_with_tools(&[Message::from_legacy(Role::User, "hi")], &[])
        .await
        .unwrap();
    assert_eq!(
        resp.take_compaction().as_deref(),
        Some("summary of history")
    );
    assert_matches!(resp, ChatResponse::Text { text, .. } if text == "answer");
}

#[test]
fn adjacent_text_blocks_stay_separate_in_non_stream_parse() {
    let resp = ToolApiResponse {
        content: vec![
            AnthropicContentBlock::Text {
                text: "first".into(),
                cache_control: None,
            },
            AnthropicContentBlock::Text {
                text: "second".into(),
                cache_control: None,
            },
            AnthropicContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
        ],
        stop_reason: None,
        stop_details: None,
        usage: None,
        input_transformations: Vec::new(),
    };
    let ChatResponse::ToolUse { content, .. } = parse_tool_response(resp) else {
        panic!("expected ToolUse");
    };
    assert_matches!(
        content.blocks(),
        [
            crate::AssistantBlock::Text(a),
            crate::AssistantBlock::Text(b),
            crate::AssistantBlock::ToolUse(_)
        ] if a == "first" && b == "second"
    );
}

#[tokio::test]
async fn chat_typed_uses_structured_outputs_on_models_that_forbid_forced_choice() {
    let server = mock_messages(serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": "{\"value\":\"ok\"}"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let out: TypedOut = provider
        .chat_typed(&[Message::from_legacy(Role::User, "go")])
        .await
        .unwrap();
    assert_eq!(out.value, "ok");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "no retry needed");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(
        body["output_config"]["format"]["schema"]["additionalProperties"],
        false
    );
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("tools").is_none());
}

#[derive(Debug)]
struct SecretMasker;

impl crate::masking::OutboundMasker for SecretMasker {
    fn mask(&self, text: &str) -> Option<String> {
        text.contains("hunter2")
            .then(|| text.replace("hunter2", "[MASKED]"))
    }
}

fn assert_send<T: Send>(_: &T) {}

#[tokio::test]
async fn masked_claude_chat_typed_reaches_structured_outputs_with_masked_content() {
    let server = mock_messages(serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": "{\"value\":\"ok\"}"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let provider = crate::any::AnyProvider::Claude(
        ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
            .with_api_url(format!("{}/v1/messages", server.uri())),
    )
    .masked(std::sync::Arc::new(SecretMasker));

    let messages = [Message::from_legacy(Role::User, "password is hunter2")];
    let fut = provider.chat_typed::<TypedOut>(&messages);
    assert_send(&fut);
    let out = fut.await.unwrap();
    assert_eq!(out.value, "ok");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    let sent = body["messages"].to_string();
    assert!(sent.contains("[MASKED]"), "masked content expected: {sent}");
    assert!(!sent.contains("hunter2"), "secret leaked: {sent}");
}

#[tokio::test]
async fn any_claude_chat_typed_reaches_native_override() {
    let server = mock_messages(serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": "{\"value\":\"ok\"}"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let provider = crate::any::AnyProvider::Claude(
        ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
            .with_api_url(format!("{}/v1/messages", server.uri())),
    );
    let out: TypedOut = provider
        .chat_typed(&[Message::from_legacy(Role::User, "go")])
        .await
        .unwrap();
    assert_eq!(out.value, "ok");
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RecursiveNode {
    #[allow(dead_code)]
    next: Option<Box<RecursiveNode>>,
}

#[tokio::test]
async fn chat_typed_recursive_schema_falls_back_to_the_tool_path() {
    use crate::testing::claude_tool_use_response;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(claude_tool_use_response(
            "submit_RecursiveNode",
            "toolu_1",
            &serde_json::json!({"next": null}),
        ))
        .mount(&server)
        .await;
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let _: RecursiveNode = provider
        .chat_typed(&[Message::from_legacy(Role::User, "go")])
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(
        body["tools"].is_array(),
        "tool fallback sends the submit tool"
    );
    assert!(body["output_config"].get("format").is_none());
}

#[tokio::test]
async fn refusal_surfaces_on_chat_with_tools() {
    let server = mock_messages(refusal_body()).await;
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let err = provider
        .chat_with_tools(&[Message::from_legacy(Role::User, "hi")], &[])
        .await
        .unwrap_err();
    assert!(refused_with_cyber(&err), "{err:?}");
}

#[tokio::test]
async fn refusal_surfaces_on_plain_chat() {
    let server = mock_messages(refusal_body()).await;
    let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let err = provider
        .chat(&[Message::from_legacy(Role::User, "hi")])
        .await
        .unwrap_err();
    assert!(refused_with_cyber(&err), "{err:?}");
}

#[tokio::test]
async fn refusal_surfaces_on_chat_typed_both_paths() {
    for model in ["claude-opus-5-5", "claude-opus-5"] {
        let server = mock_messages(refusal_body()).await;
        let provider = ClaudeProvider::new("key".into(), model.into(), 1024)
            .with_api_url(format!("{}/v1/messages", server.uri()));
        let result: Result<TypedOut, _> = provider
            .chat_typed(&[Message::from_legacy(Role::User, "go")])
            .await;
        let err = result.unwrap_err();
        assert!(refused_with_cyber(&err), "{model}: {err:?}");
    }
}

#[tokio::test]
async fn plain_chat_and_typed_requests_carry_no_context_management() {
    let server = mock_messages(serde_json::json!({
        "id": "m", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": "{\"value\":\"ok\"}"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_server_compaction(true)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let msgs = [Message::from_legacy(Role::User, "go")];
    provider.chat(&msgs).await.unwrap();
    let _: TypedOut = provider.chat_typed(&msgs).await.unwrap();
    provider.chat_with_tools(&msgs, &[]).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let has_cm = |i: usize| {
        let body: serde_json::Value = serde_json::from_slice(&reqs[i].body).unwrap();
        body.get("context_management").is_some()
    };
    assert!(!has_cm(0), "plain chat");
    assert!(!has_cm(1), "typed");
    assert!(has_cm(2), "chat_with_tools");
}

#[test]
fn thinking_binding_follows_the_preserved_thinking_capability() {
    use crate::provider::ThinkingBinding;

    let bound = [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5-1",
        "claude-haiku-5-5",
    ];
    let unbound = [
        "claude-sonnet-4-5-20250929",
        "claude-haiku-4-5-20251001",
        "claude-sonnet-4-6",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-5-1",
        "claude-unknown-9",
    ];
    for model in bound {
        let p = ClaudeProvider::new("k".into(), model.into(), 1024);
        assert_eq!(
            p.thinking_binding(),
            ThinkingBinding::PrefixBound,
            "{model}"
        );
    }
    for model in unbound {
        let p = ClaudeProvider::new("k".into(), model.into(), 1024);
        assert_eq!(p.thinking_binding(), ThinkingBinding::Unbound, "{model}");
    }
}

#[tokio::test]
async fn auxiliary_usage_is_not_rebilled_on_later_requests() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "m", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10, "output_tokens": 5,
                "iterations": [{"type": "compaction", "input_tokens": 100, "output_tokens": 40}]
            }
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{}"))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()));
    let msgs = [Message::from_legacy(Role::User, "hi")];

    provider.chat_with_tools(&msgs, &[]).await.unwrap();
    let aux = provider
        .last_auxiliary_usage()
        .expect("compaction billed once");
    assert_eq!((aux.input, aux.output), (100, 40));

    assert!(provider.chat_with_tools_stream(&msgs, &[]).await.is_err());
    assert_eq!(
        provider.last_auxiliary_usage(),
        None,
        "a later (failed) stream request must not expose the previous compaction"
    );
}
