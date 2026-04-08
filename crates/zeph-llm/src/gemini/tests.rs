// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::provider::{MessageMetadata, Role};

fn msg(role: Role, content: &str) -> Message {
    Message {
        role,
        content: content.to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }
}

#[test]
fn gemini_name() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    assert_eq!(p.name(), "gemini");
}

#[test]
fn gemini_supports_streaming_true() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    assert!(p.supports_streaming());
}

#[test]
fn gemini_supports_embeddings_false() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    assert!(!p.supports_embeddings());
}

#[test]
fn gemini_supports_vision_true() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    assert!(p.supports_vision());
}

#[test]
fn gemini_context_window_1_5_pro() {
    let p = GeminiProvider::new("key".into(), "gemini-1.5-pro".into(), 1024);
    assert_eq!(p.context_window(), Some(2_097_152));
}

#[test]
fn gemini_context_window_2_0_flash() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    assert_eq!(p.context_window(), Some(1_048_576));
}

#[test]
fn gemini_context_window_default() {
    let p = GeminiProvider::new("key".into(), "gemini-unknown-model".into(), 1024);
    assert_eq!(p.context_window(), Some(1_048_576));
}

#[test]
fn test_system_instruction_extraction() {
    let messages = vec![
        msg(Role::System, "You are a helpful assistant."),
        msg(Role::User, "Hello"),
    ];
    let (system, contents) = convert_messages(&messages);
    let sys = system.expect("system instruction should be Some");
    assert_eq!(
        sys.parts[0].text.as_deref(),
        Some("You are a helpful assistant.")
    );
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].role.as_deref(), Some("user"));
}

#[test]
fn test_empty_system_omitted() {
    let messages = vec![msg(Role::System, ""), msg(Role::User, "Hello")];
    let (system, _) = convert_messages(&messages);
    assert!(system.is_none(), "empty system prompt must yield None");
}

#[test]
fn test_consecutive_role_merging() {
    let messages = vec![
        msg(Role::User, "First"),
        msg(Role::User, "Second"),
        msg(Role::Assistant, "Reply"),
    ];
    let (_, contents) = convert_messages(&messages);
    assert_eq!(
        contents.len(),
        2,
        "consecutive user messages must be merged"
    );
    assert_eq!(contents[0].role.as_deref(), Some("user"));
    assert_eq!(contents[0].parts.len(), 2);
    assert_eq!(contents[1].role.as_deref(), Some("model"));
}

#[test]
fn test_consecutive_assistant_merging() {
    let messages = vec![
        msg(Role::User, "Q"),
        msg(Role::Assistant, "A1"),
        msg(Role::Assistant, "A2"),
    ];
    let (_, contents) = convert_messages(&messages);
    assert_eq!(
        contents.len(),
        2,
        "consecutive assistant messages must be merged"
    );
    assert_eq!(contents[1].role.as_deref(), Some("model"));
    assert_eq!(contents[1].parts.len(), 2);
}

#[test]
fn test_request_serialization() {
    let messages = vec![msg(Role::System, "Be helpful"), msg(Role::User, "Say hi")];
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 2048);
    let json = p.debug_request_json(&messages, &[], false);
    assert!(json.get("systemInstruction").is_some());
    assert!(json.get("contents").is_some());
    assert!(json.get("generationConfig").is_some());
}

#[test]
fn test_request_no_system_instruction_when_empty() {
    let messages = vec![msg(Role::User, "Hello")];
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 2048);
    let json = p.debug_request_json(&messages, &[], false);
    assert!(
        json.get("systemInstruction").is_none() || json["systemInstruction"].is_null(),
        "systemInstruction must be absent when no system messages"
    );
}

#[test]
fn test_error_response_parsing() {
    let json = r#"{
            "error": {
                "code": 403,
                "message": "API key not valid.",
                "status": "PERMISSION_DENIED"
            }
        }"#;
    let err: GeminiErrorResponse = serde_json::from_str(json).unwrap();
    assert_eq!(err.error.code, 403);
    assert_eq!(err.error.status, "PERMISSION_DENIED");
    assert!(err.error.message.contains("API key"));
}

#[test]
fn test_resource_exhausted_error_parsing() {
    let json = r#"{
            "error": {
                "code": 429,
                "message": "Quota exceeded.",
                "status": "RESOURCE_EXHAUSTED"
            }
        }"#;
    let err: GeminiErrorResponse = serde_json::from_str(json).unwrap();
    assert_eq!(err.error.status, "RESOURCE_EXHAUSTED");
}

#[test]
fn gemini_list_models_non_empty() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    let models = p.list_models();
    assert!(!models.is_empty());
    assert!(models.iter().any(|m| m.contains("gemini")));
}

#[test]
fn gemini_debug_redacts_api_key() {
    let p = GeminiProvider::new("super-secret-key".into(), "gemini-2.0-flash".into(), 1024);
    let debug = format!("{p:?}");
    assert!(!debug.contains("super-secret-key"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn gemini_clone_resets_usage() {
    // Both original and clone start with no usage (UsageTracker::default).
    // Clone must not carry over any accumulated state — verified by checking
    // that a freshly-cloned provider reports no usage.
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    assert!(p.last_usage().is_none());
    let cloned = p.clone();
    assert!(cloned.last_usage().is_none(), "clone must reset last_usage");
}

#[tokio::test]
async fn gemini_embed_returns_unsupported() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    let result = p.embed("test").await;
    assert!(matches!(result, Err(LlmError::EmbedUnsupported { .. })));
}

#[tokio::test]
async fn gemini_chat_stream_error_on_failed_request() {
    let body =
        r#"{"error":{"code":403,"message":"Permission denied.","status":"PERMISSION_DENIED"}}"#;
    let http_resp = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let messages = vec![msg(Role::User, "hello")];
    let result = p.chat_stream(&messages).await;
    assert!(result.is_err());
    let err = result.err().unwrap().to_string();
    assert!(
        err.contains("PERMISSION_DENIED"),
        "error must include API status: {err}"
    );
}

#[tokio::test]
async fn gemini_chat_stream_yields_chunks_from_sse() {
    use tokio_stream::StreamExt as _;

    let event1 = r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}"#;
    let event2 = r#"{"candidates":[{"content":{"parts":[{"text":" world","thought":false}]}}]}"#;
    let event3 = r#"{"candidates":[{"content":{"parts":[{"text":"thinking","thought":true}]}}]}"#;
    let sse_body = format!("data: {event1}\r\n\r\ndata: {event2}\r\n\r\ndata: {event3}\r\n\r\n");
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        sse_body.len(),
        sse_body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let messages = vec![msg(Role::User, "hi")];
    let stream = p.chat_stream(&messages).await.expect("stream must open");
    let chunks: Vec<_> = stream.collect().await;
    assert!(!chunks.is_empty(), "stream must yield at least one chunk");
}

#[test]
fn test_first_message_guard_prepends_user() {
    let messages = vec![
        msg(Role::Assistant, "I am the assistant"),
        msg(Role::User, "Hello"),
    ];
    let (_, contents) = convert_messages(&messages);
    assert_eq!(
        contents[0].role.as_deref(),
        Some("user"),
        "contents must always start with user role"
    );
    assert_eq!(contents.len(), 3); // synthetic user + model + user
}

// ---------------------------------------------------------------------------
// Schema conversion tests
// ---------------------------------------------------------------------------

#[test]
fn test_uppercase_types_simple() {
    let mut schema = serde_json::json!({"type": "string"});
    uppercase_types(&mut schema, 32);
    assert_eq!(schema["type"], "STRING");
}

#[test]
fn test_uppercase_types_nested() {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "count": {"type": "integer"}
        }
    });
    uppercase_types(&mut schema, 32);
    assert_eq!(schema["type"], "OBJECT");
    assert_eq!(schema["properties"]["name"]["type"], "STRING");
    assert_eq!(schema["properties"]["count"]["type"], "INTEGER");
}

#[test]
fn test_uppercase_types_number() {
    let mut schema = serde_json::json!({"type": "number"});
    uppercase_types(&mut schema, 32);
    assert_eq!(schema["type"], "NUMBER");
}

#[test]
fn test_uppercase_types_boolean() {
    let mut schema = serde_json::json!({"type": "boolean"});
    uppercase_types(&mut schema, 32);
    assert_eq!(schema["type"], "BOOLEAN");
}

#[test]
fn test_uppercase_types_array() {
    let mut schema = serde_json::json!({"type": "array", "items": {"type": "string"}});
    uppercase_types(&mut schema, 32);
    assert_eq!(schema["type"], "ARRAY");
    assert_eq!(schema["items"]["type"], "STRING");
}

#[test]
fn test_uppercase_types_null() {
    let mut schema = serde_json::json!({"type": "null"});
    uppercase_types(&mut schema, 32);
    assert_eq!(schema["type"], "NULL");
}

#[test]
fn test_inline_refs_simple() {
    let mut schema = serde_json::json!({
        "$defs": {
            "MyType": {"type": "string", "description": "a string"}
        },
        "type": "object",
        "properties": {
            "field": {"$ref": "#/$defs/MyType"}
        }
    });
    inline_refs(&mut schema, 8);
    assert!(schema.get("$defs").is_none(), "$defs must be removed");
    assert_eq!(schema["properties"]["field"]["type"], "string");
    assert_eq!(schema["properties"]["field"]["description"], "a string");
}

#[test]
fn test_inline_refs_no_defs() {
    let mut schema = serde_json::json!({"type": "object", "properties": {"x": {"type": "number"}}});
    let before = schema.clone();
    inline_refs(&mut schema, 8);
    assert_eq!(schema, before);
}

#[test]
fn test_inline_refs_depth_limit() {
    // Circular: A -> B -> A (can't actually serialize, simulate with self-ref string)
    // We simulate a depth-exceeded path: a schema with deeply nested $refs
    let mut schema = serde_json::json!({
        "$defs": {
            "A": {"$ref": "#/$defs/A"}
        },
        "$ref": "#/$defs/A"
    });
    // Should not stack overflow, should produce a fallback object
    inline_refs(&mut schema, 8);
    // After inlining, the result should be an OBJECT or something Gemini-acceptable
    assert!(schema.is_object());
}

#[test]
fn test_inline_refs_deep_plain_nesting() {
    // Regression for: depth counter was decremented on every structural recursion step,
    // causing schemas with 9+ levels of plain nesting to hit the depth-8 limit prematurely
    // even when no $ref is present.
    let mut schema = serde_json::json!({
        "$defs": {
            "Leaf": {"type": "string"}
        },
        "type": "object",
        "properties": {
            "l1": {"type": "object", "properties": {
                "l2": {"type": "object", "properties": {
                    "l3": {"type": "object", "properties": {
                        "l4": {"type": "object", "properties": {
                            "l5": {"type": "object", "properties": {
                                "l6": {"type": "object", "properties": {
                                    "l7": {"type": "object", "properties": {
                                        "l8": {"type": "object", "properties": {
                                            "l9": {"$ref": "#/$defs/Leaf"}
                                        }}
                                    }}
                                }}
                            }}
                        }}
                    }}
                }}
            }}
        }
    });
    inline_refs(&mut schema, 8);
    // The $ref at level 9 must be resolved to the Leaf type, not replaced with a fallback.
    assert_eq!(
        schema["properties"]["l1"]["properties"]["l2"]["properties"]["l3"]["properties"]["l4"]["properties"]
            ["l5"]["properties"]["l6"]["properties"]["l7"]["properties"]["l8"]["properties"]["l9"]
            ["type"],
        "string",
        "$ref at deep nesting level must be resolved, not replaced with fallback"
    );
}

#[test]
fn test_normalize_schema_allowlist() {
    let mut schema = serde_json::json!({
        "type": "object",
        "title": "MyObj",
        "$schema": "http://json-schema.org/draft-07/schema#",
        "additionalProperties": false,
        "format": "uri",
        "description": "A test object",
        "properties": {
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": 100,
                "title": "Name"
            }
        },
        "required": ["name"]
    });
    normalize_schema(&mut schema, 16);
    assert!(schema.get("title").is_none());
    assert!(schema.get("$schema").is_none());
    assert!(schema.get("additionalProperties").is_none());
    assert!(schema.get("format").is_none());
    assert_eq!(schema["description"], "A test object");
    assert_eq!(schema["type"], "object");
    // Nested cleanup
    assert!(schema["properties"]["name"].get("minLength").is_none());
    assert!(schema["properties"]["name"].get("title").is_none());
    assert_eq!(schema["properties"]["name"]["type"], "string");
}

#[test]
fn test_normalize_schema_anyof_option_pattern() {
    // schemars generates anyOf: [{type: T}, {type: "null"}] for Option<T>
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "optional_field": {
                "anyOf": [
                    {"type": "string", "description": "a string"},
                    {"type": "null"}
                ]
            }
        }
    });
    normalize_schema(&mut schema, 16);
    let field = &schema["properties"]["optional_field"];
    assert!(field.get("anyOf").is_none(), "anyOf must be removed");
    assert_eq!(field["type"], "string");
    assert_eq!(field["nullable"], true);
    assert_eq!(field["description"], "a string");
}

#[test]
fn test_normalize_schema_anyof_complex_dropped() {
    // anyOf with more than one non-null variant — can't simplify, must drop
    let mut schema = serde_json::json!({
        "anyOf": [
            {"type": "string"},
            {"type": "integer"},
            {"type": "null"}
        ]
    });
    normalize_schema(&mut schema, 16);
    assert!(schema.get("anyOf").is_none());
}

#[test]
fn test_convert_tool_definitions_single() {
    let tool = ToolDefinition {
        name: "get_weather".to_owned(),
        description: "Get current weather".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City name"}
            },
            "required": ["location"],
            "additionalProperties": false
        }),
    };
    let decls = convert_tool_definitions(&[tool]);
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0].name, "get_weather");
    assert_eq!(decls[0].description, "Get current weather");
    let params = decls[0].parameters.as_ref().unwrap();
    assert_eq!(params["type"], "OBJECT");
    assert_eq!(params["properties"]["location"]["type"], "STRING");
    assert!(params.get("additionalProperties").is_none());
}

#[test]
fn test_convert_tool_definitions_empty() {
    let decls = convert_tool_definitions(&[]);
    assert!(decls.is_empty());
}

#[test]
fn test_convert_tool_definitions_multiple() {
    let tools = vec![
        ToolDefinition {
            name: "tool_a".to_owned(),
            description: "Tool A".to_owned(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
        ToolDefinition {
            name: "tool_b".to_owned(),
            description: "Tool B".to_owned(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
    ];
    let decls = convert_tool_definitions(&tools);
    assert_eq!(decls.len(), 2);
    assert_eq!(decls[0].name, "tool_a");
    assert!(decls[0].parameters.is_none());
    assert_eq!(decls[1].name, "tool_b");
    assert!(decls[1].parameters.is_none());
}

#[test]
fn test_convert_tool_no_parameters() {
    let tool = ToolDefinition {
        name: "no_params".to_owned(),
        description: "A tool with no parameters".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    };
    let decls = convert_tool_definitions(&[tool]);
    assert_eq!(decls.len(), 1);
    assert!(decls[0].parameters.is_none());

    // Serialization must omit the parameters key entirely
    let json = serde_json::to_value(&decls[0]).unwrap();
    assert!(json.get("parameters").is_none());
}

#[test]
fn test_is_empty_object_schema() {
    // Empty properties map -> true
    assert!(is_empty_object_schema(
        &serde_json::json!({"type": "OBJECT", "properties": {}})
    ));
    // Missing properties key -> true
    assert!(is_empty_object_schema(
        &serde_json::json!({"type": "OBJECT"})
    ));
    // Non-empty properties -> false
    assert!(!is_empty_object_schema(&serde_json::json!({
        "type": "OBJECT",
        "properties": {"name": {"type": "STRING"}}
    })));
    // Non-object type -> false
    assert!(!is_empty_object_schema(
        &serde_json::json!({"type": "STRING"})
    ));
}

#[test]
fn test_normalize_schema_oneof_option_pattern() {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "optional_field": {
                "oneOf": [
                    {"type": "string", "description": "a name"},
                    {"type": "null"}
                ]
            }
        }
    });
    normalize_schema(&mut schema, 16);
    let field = &schema["properties"]["optional_field"];
    assert!(field.get("oneOf").is_none(), "oneOf must be removed");
    assert_eq!(field["type"], "string");
    assert_eq!(field["nullable"], true);
    assert_eq!(field["description"], "a name");
}

#[test]
fn test_normalize_schema_anyof_null_first_order() {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "field": {
                "anyOf": [
                    {"type": "null"},
                    {"type": "integer", "description": "count"}
                ]
            }
        }
    });
    normalize_schema(&mut schema, 16);
    let field = &schema["properties"]["field"];
    assert!(field.get("anyOf").is_none(), "anyOf must be removed");
    assert_eq!(field["type"], "integer");
    assert_eq!(field["nullable"], true);
    assert_eq!(field["description"], "count");
}

#[test]
fn test_inline_refs_unknown_ref_fallback() {
    let mut schema = serde_json::json!({
        "$defs": {
            "Known": {"type": "string"}
        },
        "type": "object",
        "properties": {
            "good": {"$ref": "#/$defs/Known"},
            "bad": {"$ref": "#/$defs/DoesNotExist"}
        }
    });
    inline_refs(&mut schema, 8);
    assert_eq!(schema["properties"]["good"]["type"], "string");
    assert_eq!(schema["properties"]["bad"]["type"], "OBJECT");
    assert_eq!(
        schema["properties"]["bad"]["description"],
        "unresolved reference"
    );
}

#[test]
fn test_inline_refs_nested_multi_level() {
    let mut schema = serde_json::json!({
        "$defs": {
            "C": {"type": "number", "description": "leaf"},
            "B": {"$ref": "#/$defs/C"},
            "A": {"$ref": "#/$defs/B"}
        },
        "type": "object",
        "properties": {
            "value": {"$ref": "#/$defs/A"}
        }
    });
    inline_refs(&mut schema, 8);
    assert_eq!(schema["properties"]["value"]["type"], "number");
    assert_eq!(schema["properties"]["value"]["description"], "leaf");
}

#[test]
fn test_build_tool_request_parameterless_tools_still_includes_tools_field() {
    let tools = vec![
        ToolDefinition {
            name: "ping".to_owned(),
            description: "Ping".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        },
        ToolDefinition {
            name: "pong".to_owned(),
            description: "Pong".to_owned(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
    ];
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    let messages = vec![msg(Role::User, "test")];
    let req = p.build_tool_request(&messages, &tools);
    let tools_field = req
        .tools
        .expect("tools field must be Some for non-empty tool list");
    assert!(!tools_field.is_empty());
    assert_eq!(tools_field[0].function_declarations.len(), 2);
    assert!(tools_field[0].function_declarations[0].parameters.is_none());
    assert!(tools_field[0].function_declarations[1].parameters.is_none());
}

// ---------------------------------------------------------------------------
// Message conversion tests
// ---------------------------------------------------------------------------

#[test]
fn test_tool_use_part_to_function_call() {
    let messages = vec![
        msg(Role::User, "What's the weather in Paris?"),
        Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: "call-1".to_owned(),
                name: "get_weather".to_owned(),
                input: serde_json::json!({"location": "Paris"}),
            }],
            metadata: MessageMetadata::default(),
        },
    ];
    let (_, contents) = convert_messages(&messages);
    // contents: user[0] + model[1]
    assert_eq!(contents.len(), 2);
    let part = &contents[1].parts[0];
    assert!(part.function_call.is_some());
    let fc = part.function_call.as_ref().unwrap();
    assert_eq!(fc.name, "get_weather");
    assert_eq!(fc.args.as_ref().unwrap()["location"], "Paris");
}

#[test]
fn test_tool_result_part_to_function_response_with_name_lookup() {
    // The tool use message must come before the result for name lookup to work.
    let messages = vec![
        msg(Role::User, "What's the weather?"),
        Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: "call-1".to_owned(),
                name: "get_weather".to_owned(),
                input: serde_json::json!({}),
            }],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "call-1".to_owned(),
                content: "Sunny, 20°C".to_owned(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        },
    ];
    let (_, contents) = convert_messages(&messages);
    // contents: user[0] + model[1] (tool call) + user[2] (tool result)
    assert_eq!(contents.len(), 3);
    let result_part = &contents[2].parts[0];
    assert!(result_part.function_response.is_some());
    let fr = result_part.function_response.as_ref().unwrap();
    assert_eq!(fr.name, "get_weather");
    assert_eq!(fr.response["result"], "Sunny, 20°C");
}

#[test]
fn test_tool_result_is_error_wrapping() {
    let messages = vec![
        msg(Role::User, "Run something."),
        Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: "call-err".to_owned(),
                name: "run_shell".to_owned(),
                input: serde_json::json!({}),
            }],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "call-err".to_owned(),
                content: "Command not found".to_owned(),
                is_error: true,
            }],
            metadata: MessageMetadata::default(),
        },
    ];
    let (_, contents) = convert_messages(&messages);
    // user[0] + model[1] + user[2]
    let fr = contents[2].parts[0].function_response.as_ref().unwrap();
    assert_eq!(fr.response["error"], "Command not found");
    assert!(fr.response.get("result").is_none());
}

#[test]
fn test_multiple_tool_results_merged_into_one_user_content() {
    let messages = vec![
        msg(Role::User, "Do both things."),
        Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![
                MessagePart::ToolUse {
                    id: "call-1".to_owned(),
                    name: "tool_a".to_owned(),
                    input: serde_json::json!({}),
                },
                MessagePart::ToolUse {
                    id: "call-2".to_owned(),
                    name: "tool_b".to_owned(),
                    input: serde_json::json!({}),
                },
            ],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: String::new(),
            parts: vec![
                MessagePart::ToolResult {
                    tool_use_id: "call-1".to_owned(),
                    content: "result A".to_owned(),
                    is_error: false,
                },
                MessagePart::ToolResult {
                    tool_use_id: "call-2".to_owned(),
                    content: "result B".to_owned(),
                    is_error: false,
                },
            ],
            metadata: MessageMetadata::default(),
        },
    ];
    let (_, contents) = convert_messages(&messages);
    // user[0] + model[1] with two tool calls + user[2] with two tool results
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[2].role.as_deref(), Some("user"));
    assert_eq!(contents[2].parts.len(), 2);
    assert_eq!(
        contents[2].parts[0]
            .function_response
            .as_ref()
            .unwrap()
            .name,
        "tool_a"
    );
    assert_eq!(
        contents[2].parts[1]
            .function_response
            .as_ref()
            .unwrap()
            .name,
        "tool_b"
    );
}

#[test]
fn test_mixed_text_and_tool_use() {
    // Prepend a user message so the assistant message is not first (avoids synthetic prepend)
    let messages = vec![
        msg(Role::User, "Check the weather in London."),
        Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![
                MessagePart::Text {
                    text: "Let me check the weather.".to_owned(),
                },
                MessagePart::ToolUse {
                    id: "call-1".to_owned(),
                    name: "get_weather".to_owned(),
                    input: serde_json::json!({"location": "London"}),
                },
            ],
            metadata: MessageMetadata::default(),
        },
    ];
    let (_, contents) = convert_messages(&messages);
    // contents: user + model (2 parts)
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[1].role.as_deref(), Some("model"));
    assert_eq!(contents[1].parts.len(), 2);
    assert!(contents[1].parts[0].text.is_some());
    assert!(contents[1].parts[1].function_call.is_some());
}

// ---------------------------------------------------------------------------
// Response parsing tests
// ---------------------------------------------------------------------------

#[test]
fn test_parse_single_function_call() {
    let resp = GenerateContentResponse {
        candidates: vec![GeminiCandidate {
            content: GeminiContent {
                role: Some("model".to_owned()),
                parts: vec![GeminiPart {
                    text: None,
                    inline_data: None,
                    function_call: Some(GeminiFunctionCall {
                        name: "get_weather".to_owned(),
                        args: Some(serde_json::json!({"location": "Tokyo"})),
                    }),
                    function_response: None,
                }],
            },
            finish_reason: Some("TOOL_CALLS".to_owned()),
        }],
        usage_metadata: None,
    };
    let result = parse_tool_response(resp).unwrap();
    assert!(matches!(result, ChatResponse::ToolUse { .. }));
    if let ChatResponse::ToolUse {
        tool_calls, text, ..
    } = result
    {
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "get_weather");
        assert_eq!(tool_calls[0].input["location"], "Tokyo");
        assert!(text.is_none());
    }
}

#[test]
fn test_parse_multiple_function_calls() {
    let resp = GenerateContentResponse {
        candidates: vec![GeminiCandidate {
            content: GeminiContent {
                role: Some("model".to_owned()),
                parts: vec![
                    GeminiPart {
                        text: None,
                        inline_data: None,
                        function_call: Some(GeminiFunctionCall {
                            name: "tool_a".to_owned(),
                            args: Some(serde_json::json!({"x": 1})),
                        }),
                        function_response: None,
                    },
                    GeminiPart {
                        text: None,
                        inline_data: None,
                        function_call: Some(GeminiFunctionCall {
                            name: "tool_b".to_owned(),
                            args: Some(serde_json::json!({"y": 2})),
                        }),
                        function_response: None,
                    },
                ],
            },
            finish_reason: Some("TOOL_CALLS".to_owned()),
        }],
        usage_metadata: None,
    };
    let result = parse_tool_response(resp).unwrap();
    if let ChatResponse::ToolUse { tool_calls, .. } = result {
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].name, "tool_a");
        assert_eq!(tool_calls[1].name, "tool_b");
    } else {
        panic!("expected ToolUse");
    }
}

#[test]
fn test_parse_mixed_text_and_function_call() {
    let resp = GenerateContentResponse {
        candidates: vec![GeminiCandidate {
            content: GeminiContent {
                role: Some("model".to_owned()),
                parts: vec![
                    GeminiPart {
                        text: Some("I'll look that up.".to_owned()),
                        inline_data: None,
                        function_call: None,
                        function_response: None,
                    },
                    GeminiPart {
                        text: None,
                        inline_data: None,
                        function_call: Some(GeminiFunctionCall {
                            name: "search".to_owned(),
                            args: Some(serde_json::json!({"query": "rust"})),
                        }),
                        function_response: None,
                    },
                ],
            },
            finish_reason: Some("TOOL_CALLS".to_owned()),
        }],
        usage_metadata: None,
    };
    let result = parse_tool_response(resp).unwrap();
    if let ChatResponse::ToolUse {
        tool_calls, text, ..
    } = result
    {
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(text.as_deref(), Some("I'll look that up."));
    } else {
        panic!("expected ToolUse");
    }
}

#[test]
fn test_parse_text_only_response() {
    let resp = GenerateContentResponse {
        candidates: vec![GeminiCandidate {
            content: GeminiContent {
                role: Some("model".to_owned()),
                parts: vec![GeminiPart {
                    text: Some("Hello, world!".to_owned()),
                    inline_data: None,
                    function_call: None,
                    function_response: None,
                }],
            },
            finish_reason: Some("STOP".to_owned()),
        }],
        usage_metadata: None,
    };
    let result = parse_tool_response(resp).unwrap();
    assert!(matches!(result, ChatResponse::Text(s) if s == "Hello, world!"));
}

#[test]
fn test_parse_null_args_uses_empty_object() {
    let resp = GenerateContentResponse {
        candidates: vec![GeminiCandidate {
            content: GeminiContent {
                role: Some("model".to_owned()),
                parts: vec![GeminiPart {
                    text: None,
                    inline_data: None,
                    function_call: Some(GeminiFunctionCall {
                        name: "no_args_tool".to_owned(),
                        args: None,
                    }),
                    function_response: None,
                }],
            },
            finish_reason: Some("TOOL_CALLS".to_owned()),
        }],
        usage_metadata: None,
    };
    let result = parse_tool_response(resp).unwrap();
    if let ChatResponse::ToolUse { tool_calls, .. } = result {
        assert_eq!(
            tool_calls[0].input,
            serde_json::Value::Object(serde_json::Map::default())
        );
    } else {
        panic!("expected ToolUse");
    }
}

#[test]
fn test_debug_request_json_with_tools_includes_function_declarations() {
    let messages = vec![msg(Role::User, "What is the weather?")];
    let tools = vec![ToolDefinition {
        name: "get_weather".to_owned(),
        description: "Get weather".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"location": {"type": "string"}},
            "required": ["location"]
        }),
    }];
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    let json = p.debug_request_json(&messages, &tools, false);
    assert!(json.get("tools").is_some());
    let tools_arr = json["tools"].as_array().unwrap();
    assert!(!tools_arr.is_empty());
    let decls = &tools_arr[0]["functionDeclarations"];
    assert!(decls.is_array());
    assert_eq!(decls[0]["name"], "get_weather");
}

#[test]
fn test_debug_request_json_no_tools_no_tools_field() {
    let messages = vec![msg(Role::User, "Hi")];
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    let json = p.debug_request_json(&messages, &[], false);
    assert!(json.get("tools").is_none());
}

// ---------------------------------------------------------------------------
// HTTP integration tests using a local mock TCP server.
// ---------------------------------------------------------------------------

async fn spawn_mock_server(responses: Vec<&'static str>) -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        for resp in responses {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (reader, mut writer) = stream.split();
                let mut buf_reader = BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    buf_reader.read_line(&mut line).await.unwrap_or(0);
                    if line == "\r\n" || line == "\n" || line.is_empty() {
                        break;
                    }
                }
                writer.write_all(resp.as_bytes()).await.ok();
            });
        }
    });

    (port, handle)
}

#[tokio::test]
async fn gap1_http_error_response_maps_to_llm_error_other() {
    let body =
        r#"{"error":{"code":403,"message":"API key not valid.","status":"PERMISSION_DENIED"}}"#;
    let http_resp = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let messages = vec![msg(Role::User, "hello")];
    let result = p.chat(&messages).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("PERMISSION_DENIED"),
        "error must include API status: {err}"
    );
}

#[tokio::test]
async fn gap2_resource_exhausted_maps_to_rate_limited() {
    let body =
        r#"{"error":{"code":429,"message":"Quota exceeded.","status":"RESOURCE_EXHAUSTED"}}"#;
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let messages = vec![msg(Role::User, "hello")];
    let result = p.chat(&messages).await;
    drop(result);

    let rate_limit =
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
    let responses: Vec<&'static str> = vec![rate_limit; MAX_RETRIES as usize + 1];
    let (port2, _handle2) = spawn_mock_server(responses).await;

    let p2 = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port2}"));
    let result2 = p2.chat(&messages).await;
    assert!(
        matches!(result2, Err(LlmError::RateLimited)),
        "429 exhausted must return RateLimited, got: {result2:?}"
    );
}

#[tokio::test]
async fn gap3_successful_response_populates_last_usage() {
    let body = r#"{
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hello!"}]}}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}
        }"#;
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let messages = vec![msg(Role::User, "hi")];
    let result = p.chat(&messages).await;

    assert!(result.is_ok(), "chat must succeed: {result:?}");
    assert_eq!(result.unwrap(), "Hello!");

    let usage = p
        .last_usage()
        .expect("last_usage must be populated after successful call");
    assert_eq!(usage.0, 10, "prompt_token_count");
    assert_eq!(usage.1, 5, "candidates_token_count");
}

#[tokio::test]
async fn test_chat_with_tools_returns_tool_use() {
    let body = r#"{
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "get_weather", "args": {"location": "Berlin"}}}]
                },
                "finishReason": "TOOL_CALLS"
            }],
            "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 10}
        }"#;
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let messages = vec![msg(Role::User, "What's the weather in Berlin?")];
    let tools = vec![ToolDefinition {
        name: "get_weather".to_owned(),
        description: "Get weather".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {"location": {"type": "string"}}}),
    }];

    let result = p.chat_with_tools(&messages, &tools).await.unwrap();
    assert!(matches!(result, ChatResponse::ToolUse { .. }));
    if let ChatResponse::ToolUse { tool_calls, .. } = result {
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "get_weather");
        assert_eq!(tool_calls[0].input["location"], "Berlin");
    }
}

// ---------------------------------------------------------------------------
// Vision / inlineData tests
// ---------------------------------------------------------------------------

#[test]
fn test_image_part_converted_to_inline_data() {
    use crate::provider::{ImageData, MessageMetadata};

    let messages = vec![Message {
        role: Role::User,
        content: String::new(),
        parts: vec![MessagePart::Image(Box::new(ImageData {
            data: vec![0xFF, 0xD8, 0xFF],
            mime_type: "image/jpeg".to_owned(),
        }))],
        metadata: MessageMetadata::default(),
    }];
    let (_, contents) = convert_messages(&messages);
    assert_eq!(contents.len(), 1);
    let part = &contents[0].parts[0];
    assert!(part.text.is_none());
    assert!(part.function_call.is_none());
    let inline = part.inline_data.as_ref().expect("inline_data must be set");
    assert_eq!(inline.mime_type, "image/jpeg");
    assert_eq!(
        inline.data,
        base64::engine::general_purpose::STANDARD.encode([0xFF, 0xD8, 0xFF])
    );
}

#[test]
fn test_multiple_images_in_single_message() {
    use crate::provider::{ImageData, MessageMetadata};

    let messages = vec![Message {
        role: Role::User,
        content: String::new(),
        parts: vec![
            MessagePart::Image(Box::new(ImageData {
                data: vec![1, 2, 3],
                mime_type: "image/png".to_owned(),
            })),
            MessagePart::Image(Box::new(ImageData {
                data: vec![4, 5, 6],
                mime_type: "image/webp".to_owned(),
            })),
        ],
        metadata: MessageMetadata::default(),
    }];
    let (_, contents) = convert_messages(&messages);
    assert_eq!(contents[0].parts.len(), 2);
    assert_eq!(
        contents[0].parts[0].inline_data.as_ref().unwrap().mime_type,
        "image/png"
    );
    assert_eq!(
        contents[0].parts[1].inline_data.as_ref().unwrap().mime_type,
        "image/webp"
    );
}

#[test]
fn test_mixed_text_and_image_parts() {
    use crate::provider::{ImageData, MessageMetadata};

    let messages = vec![Message {
        role: Role::User,
        content: String::new(),
        parts: vec![
            MessagePart::Text {
                text: "Describe this image:".to_owned(),
            },
            MessagePart::Image(Box::new(ImageData {
                data: vec![10, 20, 30],
                mime_type: "image/jpeg".to_owned(),
            })),
            MessagePart::Text {
                text: "Be detailed.".to_owned(),
            },
        ],
        metadata: MessageMetadata::default(),
    }];
    let (_, contents) = convert_messages(&messages);
    let parts = &contents[0].parts;
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].text.as_deref(), Some("Describe this image:"));
    assert!(parts[0].inline_data.is_none());
    assert!(parts[1].inline_data.is_some());
    assert!(parts[1].text.is_none());
    assert_eq!(parts[2].text.as_deref(), Some("Be detailed."));
    assert!(parts[2].inline_data.is_none());
}

#[test]
fn test_inline_data_serializes_to_camel_case() {
    let part = GeminiPart {
        text: None,
        inline_data: Some(GeminiInlineData {
            mime_type: "image/jpeg".to_owned(),
            data: "abc".to_owned(),
        }),
        function_call: None,
        function_response: None,
    };
    let json = serde_json::to_value(&part).unwrap();
    assert!(
        json.get("inlineData").is_some(),
        "must serialize as inlineData"
    );
    assert!(json.get("inline_data").is_none(), "must not use snake_case");
    let inline = &json["inlineData"];
    assert_eq!(inline["mimeType"], "image/jpeg");
    assert_eq!(inline["data"], "abc");
}

#[tokio::test]
async fn test_chat_with_tools_empty_tools_falls_back_to_chat() {
    let body = r#"{
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hello!"}]}}]
        }"#;
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let messages = vec![msg(Role::User, "hi")];

    // Empty tools — should fall back to chat()
    let result = p.chat_with_tools(&messages, &[]).await.unwrap();
    assert!(matches!(result, ChatResponse::Text(s) if s == "Hello!"));
}

// ---------------------------------------------------------------------------
// Embedding tests
// ---------------------------------------------------------------------------

#[test]
fn gemini_supports_embeddings_without_model() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    assert!(!p.supports_embeddings());
}

#[test]
fn gemini_supports_embeddings_with_model() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004");
    assert!(p.supports_embeddings());
}

#[test]
fn gemini_with_embedding_model_empty_string_is_none() {
    let p =
        GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024).with_embedding_model("");
    assert!(
        !p.supports_embeddings(),
        "empty string must not enable embeddings"
    );
}

#[test]
fn embed_content_request_serialization() {
    let req = EmbedContentRequest {
        model: "models/text-embedding-004".to_owned(),
        content: EmbedContent {
            parts: vec![EmbedPart {
                text: "hello world",
            }],
        },
        task_type: "RETRIEVAL_QUERY",
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["model"], "models/text-embedding-004");
    assert_eq!(json["taskType"], "RETRIEVAL_QUERY");
    assert_eq!(json["content"]["parts"][0]["text"], "hello world");
    assert!(
        json.get("task_type").is_none(),
        "must use camelCase taskType"
    );
}

#[test]
fn embed_content_response_deserialization() {
    let json = r#"{"embedding":{"values":[0.1,0.2,0.3]}}"#;
    let resp: EmbedContentResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.embedding.values, vec![0.1_f32, 0.2, 0.3]);
}

#[test]
fn embed_content_response_empty_values() {
    let json = r#"{"embedding":{"values":[]}}"#;
    let resp: EmbedContentResponse = serde_json::from_str(json).unwrap();
    assert!(resp.embedding.values.is_empty());
}

#[tokio::test]
async fn gemini_embed_no_model_returns_unsupported() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    let result = p.embed("test text").await;
    assert!(
        matches!(result, Err(LlmError::EmbedUnsupported { .. })),
        "embed without embedding_model must return EmbedUnsupported"
    );
}

#[tokio::test]
async fn gemini_embed_success() {
    let body = r#"{"embedding":{"values":[0.1,0.2,0.3,0.4]}}"#;
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004")
        .with_base_url(format!("http://127.0.0.1:{port}"));

    let result = p.embed("hello world").await.unwrap();
    assert_eq!(result.len(), 4);
    assert!((result[0] - 0.1_f32).abs() < 1e-6);
}

#[tokio::test]
async fn gemini_embed_api_error_403() {
    let body =
        r#"{"error":{"code":403,"message":"API key not valid.","status":"PERMISSION_DENIED"}}"#;
    let http_resp = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004")
        .with_base_url(format!("http://127.0.0.1:{port}"));

    let err = p.embed("test").await.unwrap_err().to_string();
    assert!(
        err.contains("PERMISSION_DENIED"),
        "error must contain status: {err}"
    );
}

#[tokio::test]
async fn gemini_embed_api_error_429() {
    // send_with_retry retries up to MAX_RETRIES times on 429 — need MAX_RETRIES+1 responses.
    // Use Retry-After: 0 to avoid sleep delays in tests.
    let rate_limit =
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
    let responses: Vec<&'static str> = vec![rate_limit; MAX_RETRIES as usize + 1];
    let (port, _handle) = spawn_mock_server(responses).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004")
        .with_base_url(format!("http://127.0.0.1:{port}"));

    let result = p.embed("test").await;
    assert!(
        matches!(result, Err(LlmError::RateLimited)),
        "429 RESOURCE_EXHAUSTED must return RateLimited, got: {result:?}"
    );
}

#[tokio::test]
async fn gemini_embed_api_error_500() {
    let body = "Internal Server Error";
    let http_resp = format!(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004")
        .with_base_url(format!("http://127.0.0.1:{port}"));

    let result = p.embed("test").await;
    assert!(result.is_err(), "500 must return error");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("500"), "error must mention status code: {err}");
}

#[tokio::test]
async fn gemini_embed_malformed_response() {
    let body = r#"{"not_embedding": true}"#;
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004")
        .with_base_url(format!("http://127.0.0.1:{port}"));

    let result = p.embed("test").await;
    assert!(result.is_err(), "malformed response must return error");
}

#[test]
fn gemini_list_models_includes_embedding_model_when_configured() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004");
    let models = p.list_models();
    assert!(
        models.contains(&"text-embedding-004".to_owned()),
        "configured embedding model must appear in list_models"
    );
}

#[test]
fn gemini_list_models_excludes_embedding_model_when_not_configured() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
    let models = p.list_models();
    assert!(
        !models.contains(&"text-embedding-004".to_owned()),
        "embedding model must not appear when not configured"
    );
}

#[tokio::test]
#[ignore = "requires live Gemini API key"]
async fn integration_gemini_embed() {
    let api_key = std::env::var("ZEPH_GEMINI_API_KEY").expect("ZEPH_GEMINI_API_KEY required");
    let p = GeminiProvider::new(api_key, "gemini-2.0-flash".into(), 1024)
        .with_embedding_model("text-embedding-004");
    let result = p.embed("Hello, world!").await.expect("embed must succeed");
    assert!(!result.is_empty(), "embedding must be non-empty");
    // text-embedding-004 returns 768 dimensions
    assert_eq!(
        result.len(),
        768,
        "text-embedding-004 returns 768 dimensions"
    );
}

// ---------------------------------------------------------------------------
// list_models_remote tests
// ---------------------------------------------------------------------------

#[test]
fn list_models_response_filters_generate_content() {
    let json = r#"{
            "models": [
                {
                    "name": "models/gemini-2.0-flash",
                    "displayName": "Gemini 2.0 Flash",
                    "inputTokenLimit": 1048576,
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "displayName": "Text Embedding 004",
                    "inputTokenLimit": 2048,
                    "supportedGenerationMethods": ["embedContent"]
                }
            ]
        }"#;
    let list: GeminiModelList = serde_json::from_str(json).unwrap();
    let models: Vec<_> = list
        .models
        .into_iter()
        .filter(|m| {
            m.supported_generation_methods
                .iter()
                .any(|s| s == "generateContent")
        })
        .collect();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "models/gemini-2.0-flash");
}

#[test]
fn list_models_response_strips_models_prefix() {
    let json = r#"{
            "models": [{
                "name": "models/gemini-2.0-flash",
                "displayName": "Gemini 2.0 Flash",
                "supportedGenerationMethods": ["generateContent"]
            }]
        }"#;
    let list: GeminiModelList = serde_json::from_str(json).unwrap();
    let entry = &list.models[0];
    let id = entry
        .name
        .strip_prefix("models/")
        .unwrap_or(&entry.name)
        .to_owned();
    assert_eq!(id, "gemini-2.0-flash");
}

#[test]
fn list_models_response_empty_models() {
    let json = r#"{"models": []}"#;
    let list: GeminiModelList = serde_json::from_str(json).unwrap();
    assert!(list.models.is_empty());
}

#[test]
fn list_models_response_missing_models_field() {
    let json = r"{}";
    let list: GeminiModelList = serde_json::from_str(json).unwrap();
    assert!(
        list.models.is_empty(),
        "#[serde(default)] must yield empty vec"
    );
}

#[test]
fn list_models_response_missing_input_token_limit() {
    let json = r#"{
            "models": [{
                "name": "models/gemini-2.0-flash",
                "displayName": "Gemini 2.0 Flash",
                "supportedGenerationMethods": ["generateContent"]
            }]
        }"#;
    let list: GeminiModelList = serde_json::from_str(json).unwrap();
    assert!(
        list.models[0].input_token_limit.is_none(),
        "missing inputTokenLimit must deserialize as None"
    );
}

#[test]
fn gemini_model_entry_camel_case_deser() {
    let json = r#"{
            "name": "models/gemini-1.5-pro",
            "displayName": "Gemini 1.5 Pro",
            "inputTokenLimit": 2097152,
            "supportedGenerationMethods": ["generateContent"]
        }"#;
    let entry: GeminiModelEntry = serde_json::from_str(json).unwrap();
    assert_eq!(entry.name, "models/gemini-1.5-pro");
    assert_eq!(entry.display_name, "Gemini 1.5 Pro");
    assert_eq!(entry.input_token_limit, Some(2_097_152));
    assert_eq!(entry.supported_generation_methods, ["generateContent"]);
}

#[test]
fn list_models_response_extra_unknown_fields_ignored() {
    let json = r#"{
            "models": [{
                "name": "models/gemini-2.0-flash",
                "displayName": "Gemini 2.0 Flash",
                "supportedGenerationMethods": ["generateContent"],
                "outputTokenLimit": 8192,
                "unknownFutureField": "value"
            }],
            "nextPageToken": "abc123"
        }"#;
    let list: GeminiModelList = serde_json::from_str(json).unwrap();
    assert_eq!(
        list.models.len(),
        1,
        "unknown fields must be silently ignored"
    );
}

#[tokio::test]
async fn list_models_remote_success() {
    let body = r#"{
            "models": [
                {
                    "name": "models/gemini-2.0-flash",
                    "displayName": "Gemini 2.0 Flash",
                    "inputTokenLimit": 1048576,
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "displayName": "Text Embedding 004",
                    "inputTokenLimit": 2048,
                    "supportedGenerationMethods": ["embedContent"]
                }
            ]
        }"#;
    let http_resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let models = p.list_models_remote().await.unwrap();

    assert_eq!(
        models.len(),
        1,
        "only generateContent models must be returned"
    );
    assert_eq!(models[0].id, "gemini-2.0-flash");
    assert_eq!(models[0].display_name, "Gemini 2.0 Flash");
    assert_eq!(models[0].context_window, Some(1_048_576));
    assert!(models[0].created_at.is_none());
}

#[tokio::test]
async fn list_models_remote_http_error() {
    let body = "Internal Server Error";
    let http_resp = format!(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let result = p.list_models_remote().await;
    assert!(result.is_err(), "500 must return error");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("500"), "error must mention status code: {err}");
}

#[tokio::test]
async fn list_models_remote_auth_error() {
    let body = r#"{"error":{"code":401,"message":"Request had invalid authentication credentials.","status":"UNAUTHENTICATED"}}"#;
    let http_resp = format!(
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let (port, _handle) = spawn_mock_server(vec![Box::leak(http_resp.into_boxed_str())]).await;

    let p = GeminiProvider::new("bad-key".into(), "gemini-2.0-flash".into(), 1024)
        .with_base_url(format!("http://127.0.0.1:{port}"));
    let result = p.list_models_remote().await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("auth error"),
        "error must mention auth error: {err}"
    );
}

// ---------------------------------------------------------------------------
// ThinkingLevel / ThinkingConfig tests
// ---------------------------------------------------------------------------

#[test]
fn thinking_level_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::Minimal).unwrap(),
        "\"minimal\""
    );
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::Low).unwrap(),
        "\"low\""
    );
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::Medium).unwrap(),
        "\"medium\""
    );
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::High).unwrap(),
        "\"high\""
    );
}

#[test]
fn thinking_level_deserializes_from_lowercase() {
    let level: ThinkingLevel = serde_json::from_str("\"medium\"").unwrap();
    assert_eq!(level, ThinkingLevel::Medium);
}

#[test]
fn thinking_config_serializes_camelcase() {
    let cfg = GeminiThinkingConfig {
        thinking_level: Some(ThinkingLevel::Medium),
        thinking_budget: None,
        include_thoughts: None,
    };
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json["thinkingLevel"], "medium");
    assert!(json.get("thinkingBudget").is_none());
    assert!(json.get("includeThoughts").is_none());
}

#[test]
fn thinking_config_with_budget_serializes() {
    let cfg = GeminiThinkingConfig {
        thinking_level: None,
        thinking_budget: Some(1024),
        include_thoughts: Some(true),
    };
    let json = serde_json::to_value(&cfg).unwrap();
    assert!(json.get("thinkingLevel").is_none());
    assert_eq!(json["thinkingBudget"], 1024);
    assert_eq!(json["includeThoughts"], true);
}

#[test]
fn generation_config_without_thinking_omits_field() {
    let cfg = GenerationConfig {
        max_output_tokens: Some(8192),
        temperature: None,
        top_p: None,
        top_k: None,
        thinking_config: None,
    };
    let json = serde_json::to_value(&cfg).unwrap();
    assert!(json.get("thinkingConfig").is_none());
    assert_eq!(json["maxOutputTokens"], 8192);
}

#[test]
fn generation_config_with_thinking_includes_nested_field() {
    let cfg = GenerationConfig {
        max_output_tokens: Some(8192),
        temperature: None,
        top_p: None,
        top_k: None,
        thinking_config: Some(GeminiThinkingConfig {
            thinking_level: Some(ThinkingLevel::High),
            thinking_budget: None,
            include_thoughts: None,
        }),
    };
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json["thinkingConfig"]["thinkingLevel"], "high");
}

#[test]
fn provider_with_thinking_level_included_in_gen_config() {
    let p = GeminiProvider::new("key".into(), "gemini-3.0-flash".into(), 2048)
        .with_thinking_level(ThinkingLevel::Medium);
    let gcfg = p.make_gen_config();
    let json = serde_json::to_value(&gcfg).unwrap();
    assert_eq!(json["thinkingConfig"]["thinkingLevel"], "medium");
}

#[test]
fn provider_without_thinking_no_thinking_config() {
    let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 2048);
    let gcfg = p.make_gen_config();
    let json = serde_json::to_value(&gcfg).unwrap();
    assert!(json.get("thinkingConfig").is_none());
}

#[test]
fn provider_with_thinking_budget_included_in_gen_config() {
    let p = GeminiProvider::new("key".into(), "gemini-2.5-flash".into(), 2048)
        .with_thinking_budget(1024)
        .unwrap();
    let gcfg = p.make_gen_config();
    let json = serde_json::to_value(&gcfg).unwrap();
    assert_eq!(json["thinkingConfig"]["thinkingBudget"], 1024);
}

#[test]
fn provider_clone_preserves_thinking_level() {
    let p = GeminiProvider::new("key".into(), "gemini-3.0-flash".into(), 2048)
        .with_thinking_level(ThinkingLevel::High);
    let cloned = p.clone();
    assert_eq!(cloned.thinking_level, Some(ThinkingLevel::High));
}

#[test]
fn provider_debug_includes_thinking_level() {
    let p = GeminiProvider::new("key".into(), "gemini-3.0-flash".into(), 2048)
        .with_thinking_level(ThinkingLevel::Low);
    let debug = format!("{p:?}");
    assert!(debug.contains("thinking_level"));
}

// MT-1: deser round-trip for all four variants
#[test]
fn thinking_level_roundtrip_all_variants() {
    for (s, expected) in [
        ("\"minimal\"", ThinkingLevel::Minimal),
        ("\"low\"", ThinkingLevel::Low),
        ("\"medium\"", ThinkingLevel::Medium),
        ("\"high\"", ThinkingLevel::High),
    ] {
        let level: ThinkingLevel = serde_json::from_str(s).unwrap();
        assert_eq!(level, expected);
        assert_eq!(serde_json::to_string(&level).unwrap(), s);
    }
}

// MT-2: with_include_thoughts end-to-end
#[test]
fn provider_with_include_thoughts_in_gen_config() {
    let p = GeminiProvider::new("key".into(), "gemini-3.0-flash".into(), 2048)
        .with_include_thoughts(true);
    let gcfg = p.make_gen_config();
    let json = serde_json::to_value(&gcfg).unwrap();
    assert_eq!(json["thinkingConfig"]["includeThoughts"], true);
}

// MT-3: budget edge values
#[test]
fn thinking_budget_edge_values() {
    // 0 = disable
    let p = GeminiProvider::new("key".into(), "gemini-2.5-flash".into(), 2048)
        .with_thinking_budget(0)
        .unwrap();
    let json = serde_json::to_value(p.make_gen_config()).unwrap();
    assert_eq!(json["thinkingConfig"]["thinkingBudget"], 0);

    // -1 = dynamic
    let p = GeminiProvider::new("key".into(), "gemini-2.5-flash".into(), 2048)
        .with_thinking_budget(-1)
        .unwrap();
    let json = serde_json::to_value(p.make_gen_config()).unwrap();
    assert_eq!(json["thinkingConfig"]["thinkingBudget"], -1);

    // max = 32768
    let p = GeminiProvider::new("key".into(), "gemini-2.5-flash".into(), 2048)
        .with_thinking_budget(32768)
        .unwrap();
    let json = serde_json::to_value(p.make_gen_config()).unwrap();
    assert_eq!(json["thinkingConfig"]["thinkingBudget"], 32768);
}

#[test]
fn thinking_budget_invalid_values_rejected() {
    assert!(
        GeminiProvider::new("key".into(), "gemini-2.5-flash".into(), 2048)
            .with_thinking_budget(-2)
            .is_err()
    );
    assert!(
        GeminiProvider::new("key".into(), "gemini-2.5-flash".into(), 2048)
            .with_thinking_budget(32769)
            .is_err()
    );
}
