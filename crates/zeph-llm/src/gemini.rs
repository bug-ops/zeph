// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::error::LlmError;
use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, MessagePart, Role,
    StatusTx, ToolDefinition, ToolUseRequest,
};
use crate::retry::send_with_retry;
use crate::sse::gemini_sse_to_stream;

const MAX_RETRIES: u32 = 3;
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    max_output_tokens: u32,
    embedding_model: Option<String>,
    last_usage: std::sync::Mutex<Option<(u64, u64)>>,
    generation_overrides: Option<GenerationOverrides>,
    status_tx: Option<StatusTx>,
}

impl fmt::Debug for GeminiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiProvider")
            .field("client", &"<reqwest::Client>")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("embedding_model", &self.embedding_model)
            .field("last_usage", &self.last_usage.lock().ok().and_then(|g| *g))
            .field("generation_overrides", &self.generation_overrides)
            .field("status_tx", &self.status_tx.is_some())
            .finish()
    }
}

impl Clone for GeminiProvider {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            max_output_tokens: self.max_output_tokens,
            embedding_model: self.embedding_model.clone(),
            last_usage: std::sync::Mutex::new(None),
            generation_overrides: self.generation_overrides.clone(),
            status_tx: self.status_tx.clone(),
        }
    }
}

impl GeminiProvider {
    #[must_use]
    pub fn new(api_key: String, model: String, max_output_tokens: u32) -> Self {
        Self {
            client: crate::http::llm_client(600),
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model,
            max_output_tokens,
            embedding_model: None,
            last_usage: std::sync::Mutex::new(None),
            generation_overrides: None,
            status_tx: None,
        }
    }

    #[must_use]
    pub fn with_embedding_model(mut self, model: impl Into<String>) -> Self {
        self.embedding_model = Some(model.into()).filter(|s| !s.is_empty());
        self
    }

    #[must_use]
    pub fn with_status_tx(mut self, tx: StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        self.status_tx = Some(tx);
    }

    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.generation_overrides = Some(overrides);
        self
    }

    fn make_gen_config(&self) -> GenerationConfig {
        GenerationConfig {
            max_output_tokens: Some(self.max_output_tokens),
            temperature: self
                .generation_overrides
                .as_ref()
                .and_then(|o| o.temperature),
            top_p: self.generation_overrides.as_ref().and_then(|o| o.top_p),
            top_k: self
                .generation_overrides
                .as_ref()
                .and_then(|o| o.top_k.and_then(|k| u32::try_from(k).ok())),
        }
    }

    fn build_request(&self, messages: &[Message]) -> GenerateContentRequest {
        let (system_instruction, contents) = convert_messages(messages);
        GenerateContentRequest {
            system_instruction,
            contents,
            generation_config: Some(self.make_gen_config()),
            tools: None,
            tool_config: None,
        }
    }

    fn build_tool_request(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> GenerateContentRequest {
        let (system_instruction, contents) = convert_messages(messages);
        let declarations = convert_tool_definitions(tools);
        let (tools_field, tool_config) = if declarations.is_empty() {
            (None, None)
        } else {
            (
                Some(vec![GeminiTools {
                    function_declarations: declarations,
                }]),
                Some(GeminiToolConfig {
                    function_calling_config: FunctionCallingConfig {
                        mode: "AUTO".to_owned(),
                    },
                }),
            )
        };
        GenerateContentRequest {
            system_instruction,
            contents,
            generation_config: Some(self.make_gen_config()),
            tools: tools_field,
            tool_config,
        }
    }

    async fn send_request(&self, messages: &[Message]) -> Result<String, LlmError> {
        let request = self.build_request(messages);
        // Serialize once before the retry loop — avoids re-serializing on each attempt.
        let body_bytes = serde_json::to_vec(&request)?;
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, self.model
        );

        let response = send_with_retry("gemini", MAX_RETRIES, self.status_tx.as_ref(), || {
            let req = self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .header("Content-Type", "application/json")
                .body(body_bytes.clone());
            async move { req.send().await }
        })
        .await?;

        let status = response.status();
        let body = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            return Err(parse_gemini_error(&body, status));
        }

        let resp: GenerateContentResponse = serde_json::from_str(&body)?;

        if let Some(ref usage) = resp.usage_metadata
            && let Ok(mut guard) = self.last_usage.lock()
        {
            *guard = Some((usage.prompt_token_count, usage.candidates_token_count));
        }

        resp.candidates
            .first()
            .and_then(|c| c.content.parts.first())
            .and_then(|p| p.text.as_deref())
            .map(str::to_owned)
            .ok_or_else(|| LlmError::EmptyResponse {
                provider: "gemini".into(),
            })
    }

    async fn send_tool_request(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        // If tools are empty or all filtered, fall back to plain chat.
        if tools.is_empty() {
            return Ok(ChatResponse::Text(self.send_request(messages).await?));
        }

        let request = self.build_tool_request(messages, tools);

        // If declarations were all filtered out (e.g. unsupported schemas), fall back.
        let has_tools = request
            .tools
            .as_ref()
            .is_some_and(|t| !t.is_empty() && !t[0].function_declarations.is_empty());
        if !has_tools {
            return Ok(ChatResponse::Text(self.send_request(messages).await?));
        }

        let body_bytes = serde_json::to_vec(&request)?;
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, self.model
        );

        let response = send_with_retry("gemini", MAX_RETRIES, self.status_tx.as_ref(), || {
            let req = self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .header("Content-Type", "application/json")
                .body(body_bytes.clone());
            async move { req.send().await }
        })
        .await?;

        let status = response.status();
        let body = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            return Err(parse_gemini_error(&body, status));
        }

        let resp: GenerateContentResponse = serde_json::from_str(&body)?;

        if let Some(ref usage) = resp.usage_metadata
            && let Ok(mut guard) = self.last_usage.lock()
        {
            *guard = Some((usage.prompt_token_count, usage.candidates_token_count));
        }

        parse_tool_response(resp)
    }

    async fn send_stream_request(
        &self,
        messages: &[Message],
    ) -> Result<reqwest::Response, LlmError> {
        let request = self.build_request(messages);
        let body_bytes = serde_json::to_vec(&request)?;
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.model
        );

        let response = send_with_retry("gemini", MAX_RETRIES, self.status_tx.as_ref(), || {
            let req = self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .header("Content-Type", "application/json")
                .body(body_bytes.clone());
            async move { req.send().await }
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.map_err(LlmError::Http)?;
            return Err(parse_gemini_error(&body, status));
        }

        Ok(response)
    }

    /// Fetch available models from the Gemini API and update the disk cache.
    ///
    /// Only models supporting `generateContent` are included (embedding-only
    /// models like `text-embedding-004` are filtered out).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or returns a non-success status.
    // TODO: pagination via nextPageToken can be added if the model count grows significantly.
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let url = format!("{}/v1beta/models", self.base_url);

        let response = send_with_retry("gemini", MAX_RETRIES, self.status_tx.as_ref(), || {
            let req = self
                .client
                .get(&url)
                .header("x-goog-api-key", &self.api_key);
            async move { req.send().await }
        })
        .await?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(LlmError::Other(format!(
                "Gemini API auth error listing models: {status}"
            )));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::debug!(status = %status, body = %body, "Gemini list_models_remote error");
            return Err(LlmError::Other(format!(
                "Gemini list models failed: {status}"
            )));
        }

        let list: GeminiModelList = response.json().await?;
        let models: Vec<crate::model_cache::RemoteModelInfo> = list
            .models
            .into_iter()
            .filter(|m| {
                m.supported_generation_methods
                    .iter()
                    .any(|s| s == "generateContent")
            })
            .map(|m| {
                let id = m.name.strip_prefix("models/").unwrap_or(&m.name).to_owned();
                crate::model_cache::RemoteModelInfo {
                    display_name: m.display_name,
                    id,
                    context_window: m.input_token_limit.map(|n| n as usize),
                    created_at: None,
                }
            })
            .collect();

        let cache = crate::model_cache::ModelCache::for_slug("gemini");
        cache.save(&models)?;
        Ok(models)
    }
}

// ---------------------------------------------------------------------------
// Error handling helper
// ---------------------------------------------------------------------------

fn parse_gemini_error(body: &str, status: reqwest::StatusCode) -> LlmError {
    if let Ok(err_resp) = serde_json::from_str::<GeminiErrorResponse>(body) {
        if err_resp.error.status == "RESOURCE_EXHAUSTED" {
            return LlmError::RateLimited;
        }
        tracing::error!(
            code = err_resp.error.code,
            status = %err_resp.error.status,
            "Gemini API error: {}", err_resp.error.message
        );
        LlmError::Other(format!(
            "Gemini API error ({}): {}",
            err_resp.error.status, err_resp.error.message
        ))
    } else {
        LlmError::Other(format!("Gemini API request failed (status {status})"))
    }
}

// ---------------------------------------------------------------------------
// Tool response parsing
// ---------------------------------------------------------------------------

fn parse_tool_response(resp: GenerateContentResponse) -> Result<ChatResponse, LlmError> {
    // Known limitation: only the first candidate is processed (issue #1640).
    // Gemini supports `candidateCount > 1`, but Zeph never requests multiple
    // candidates, so the remaining entries are unreachable in normal operation.
    if resp.candidates.len() > 1 {
        tracing::debug!(
            count = resp.candidates.len(),
            "Gemini returned multiple candidates; only the first will be used"
        );
    }
    let candidate = resp
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::EmptyResponse {
            provider: "gemini".into(),
        })?;

    // Log non-STOP finish reasons.
    if let Some(ref reason) = candidate.finish_reason
        && reason != "STOP"
        && reason != "TOOL_CALLS"
    {
        tracing::warn!(finish_reason = %reason, "Gemini returned non-STOP finish reason");
    }

    let mut tool_calls: Vec<ToolUseRequest> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();

    for part in candidate.content.parts {
        if let Some(fc) = part.function_call {
            tool_calls.push(ToolUseRequest {
                id: uuid::Uuid::new_v4().to_string(),
                name: fc.name,
                input: fc
                    .args
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::default())),
            });
        } else if let Some(text) = part.text
            && !text.is_empty()
        {
            text_parts.push(text);
        }
    }

    if tool_calls.is_empty() {
        let text = text_parts.join("");
        if text.is_empty() {
            return Err(LlmError::EmptyResponse {
                provider: "gemini".into(),
            });
        }
        return Ok(ChatResponse::Text(text));
    }

    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };

    Ok(ChatResponse::ToolUse {
        text,
        tool_calls,
        thinking_blocks: vec![],
    })
}

// ---------------------------------------------------------------------------
// Schema conversion pipeline
// ---------------------------------------------------------------------------

/// Resolve all `$ref` pointers against `$defs`/`definitions` in the schema.
/// Operates in-place. `depth` guards against circular references.
fn inline_refs(schema: &mut serde_json::Value, depth: u8) {
    if depth == 0 {
        // Depth limit exceeded — replace unresolvable $ref with a generic OBJECT
        // so Gemini at least accepts the schema.
        if schema.get("$ref").is_some() {
            *schema = serde_json::json!({"type": "OBJECT", "description": "recursive reference (depth exceeded)"});
        }
        return;
    }

    // Collect $defs at this level.
    let defs: HashMap<String, serde_json::Value> = {
        let mut map = HashMap::new();
        for key in &["$defs", "definitions"] {
            if let Some(serde_json::Value::Object(d)) = schema.get(*key) {
                for (k, v) in d {
                    map.insert(k.clone(), v.clone());
                }
            }
        }
        map
    };

    inline_refs_inner(schema, &defs, depth);

    // Remove $defs / definitions after inlining.
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("$defs");
        obj.remove("definitions");
    }
}

fn inline_refs_inner(
    schema: &mut serde_json::Value,
    defs: &HashMap<String, serde_json::Value>,
    depth: u8,
) {
    if depth == 0 {
        if schema.get("$ref").is_some() {
            *schema = serde_json::json!({"type": "OBJECT", "description": "recursive reference (depth exceeded)"});
        }
        return;
    }

    // If this node is a $ref, resolve it.
    if let Some(ref_val) = schema
        .get("$ref")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
    {
        let name = ref_val
            .trim_start_matches("#/$defs/")
            .trim_start_matches("#/definitions/");
        if let Some(resolved) = defs.get(name) {
            let mut resolved = resolved.clone();
            inline_refs_inner(&mut resolved, defs, depth - 1);
            *schema = resolved;
            return;
        }
        // Unknown ref — replace with generic object.
        *schema = serde_json::json!({"type": "OBJECT", "description": "unresolved reference"});
        return;
    }

    // Recurse into object values. Depth is only decremented on $ref resolution,
    // not on structural nesting, so schemas with deep plain nesting are handled correctly.
    if let Some(obj) = schema.as_object_mut() {
        for v in obj.values_mut() {
            inline_refs_inner(v, defs, depth);
        }
    } else if let Some(arr) = schema.as_array_mut() {
        for v in arr.iter_mut() {
            inline_refs_inner(v, defs, depth);
        }
    }
}

/// Normalize a schema using an allowlist approach.
///
/// Keeps only: `type`, `description`, `properties`, `required`, `items`, `enum`, `nullable`.
/// Handles `anyOf`/`oneOf` Option<T> pattern: extracts the non-null variant and adds `nullable: true`.
fn normalize_schema(schema: &mut serde_json::Value, depth: u8) {
    if depth == 0 {
        return;
    }

    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    // Handle anyOf/oneOf: detect Option<T> pattern [{type: T}, {type: "null"}] or [{type: "null"}, {type: T}]
    let any_of_key = if obj.contains_key("anyOf") {
        Some("anyOf")
    } else if obj.contains_key("oneOf") {
        Some("oneOf")
    } else {
        None
    };

    if let Some(key) = any_of_key {
        if let Some(serde_json::Value::Array(variants)) = obj.get(key) {
            let variants = variants.clone();
            let non_null: Vec<&serde_json::Value> = variants
                .iter()
                .filter(|v| v.get("type").and_then(|t| t.as_str()) != Some("null"))
                .collect();
            if non_null.len() == 1 {
                // Option<T> pattern: replace node with the non-null variant + nullable: true
                let mut replacement = non_null[0].clone();
                if let Some(r) = replacement.as_object_mut() {
                    r.remove("anyOf");
                    r.remove("oneOf");
                    r.insert("nullable".to_owned(), serde_json::Value::Bool(true));
                }
                *schema = replacement;
                normalize_schema(schema, depth - 1);
                return;
            }
        }
        // Cannot simplify — drop the anyOf/oneOf entirely (Gemini rejects it)
        obj.remove("anyOf");
        obj.remove("oneOf");
    }

    // Allowlist: keep only known-good keys
    let allowed: &[&str] = &[
        "type",
        "description",
        "properties",
        "required",
        "items",
        "enum",
        "nullable",
    ];
    let keys_to_remove: Vec<String> = obj
        .keys()
        .filter(|k| !allowed.contains(&k.as_str()))
        .cloned()
        .collect();
    for k in keys_to_remove {
        obj.remove(&k);
    }

    // Recurse into properties
    if let Some(serde_json::Value::Object(props)) = obj.get_mut("properties") {
        for v in props.values_mut() {
            normalize_schema(v, depth - 1);
        }
    }

    // Recurse into items (arrays)
    if let Some(items) = obj.get_mut("items") {
        normalize_schema(items, depth - 1);
    }
}

/// Uppercase all `type` string values in a JSON Schema tree.
fn uppercase_types(schema: &mut serde_json::Value, depth: u8) {
    if depth == 0 {
        return;
    }
    match schema {
        serde_json::Value::Object(obj) => {
            if let Some(serde_json::Value::String(t)) = obj.get_mut("type") {
                *t = t.to_uppercase();
            }
            for v in obj.values_mut() {
                uppercase_types(v, depth - 1);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                uppercase_types(v, depth - 1);
            }
        }
        _ => {}
    }
}

/// Apply full schema conversion pipeline: `inline_refs` → normalize → `uppercase_types`.
fn prepare_schema(schema: &serde_json::Value) -> serde_json::Value {
    let mut s = schema.clone();
    inline_refs(&mut s, 8);
    normalize_schema(&mut s, 16);
    uppercase_types(&mut s, 32);
    s
}

// ---------------------------------------------------------------------------
// Tool definition conversion
// ---------------------------------------------------------------------------

/// Returns `true` when the schema represents an empty object (no parameters).
///
/// Matches `{"type": "OBJECT"}` with either an absent or empty `properties` map.
fn is_empty_object_schema(schema: &serde_json::Value) -> bool {
    schema["type"] == "OBJECT"
        && schema
            .get("properties")
            .is_none_or(|p| p.as_object().is_some_and(serde_json::Map::is_empty))
}

fn convert_tool_definitions(tools: &[ToolDefinition]) -> Vec<GeminiFunctionDeclaration> {
    tools
        .iter()
        .map(|t| {
            let prepared = prepare_schema(&t.parameters);
            let parameters = if is_empty_object_schema(&prepared) {
                None
            } else {
                Some(prepared)
            };
            GeminiFunctionDeclaration {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// Build a lookup map from `tool_use_id` to `tool_name` by scanning all messages.
fn build_tool_name_lookup(messages: &[Message]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for msg in messages {
        for part in &msg.parts {
            if let MessagePart::ToolUse { id, name, .. } = part {
                map.insert(id.clone(), name.clone());
            }
        }
    }
    map
}

/// Convert Zeph messages to Gemini `(system_instruction, contents)`.
///
/// System messages are extracted and concatenated into `system_instruction`.
/// Consecutive same-role messages are merged (Gemini requires strict alternation).
/// Handles `ToolUse` and `ToolResult` `MessagePart`s.
fn convert_messages(messages: &[Message]) -> (Option<GeminiContent>, Vec<GeminiContent>) {
    let tool_names = build_tool_name_lookup(messages);
    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                let text = extract_text(msg);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            Role::User | Role::Assistant => {
                let role_str = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "model",
                    Role::System => unreachable!(),
                };

                let parts = convert_message_parts(msg, &tool_names);
                if parts.is_empty() {
                    continue;
                }

                // Merge consecutive same-role messages to satisfy Gemini alternation requirement.
                if let Some(last) = contents.last_mut()
                    && last.role.as_deref() == Some(role_str)
                {
                    last.parts.extend(parts);
                    continue;
                }
                contents.push(GeminiContent {
                    role: Some(role_str.to_owned()),
                    parts,
                });
            }
        }
    }

    // Gemini requires contents to start with a "user" message.
    if contents.first().and_then(|c| c.role.as_deref()) == Some("model") {
        contents.insert(
            0,
            GeminiContent {
                role: Some("user".to_owned()),
                parts: vec![GeminiPart {
                    text: Some(String::new()),
                    inline_data: None,
                    function_call: None,
                    function_response: None,
                }],
            },
        );
    }

    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        let combined = system_parts.join("\n\n");
        Some(GeminiContent {
            role: None,
            parts: vec![GeminiPart {
                text: Some(combined),
                inline_data: None,
                function_call: None,
                function_response: None,
            }],
        })
    };

    (system_instruction, contents)
}

fn convert_message_parts(msg: &Message, tool_names: &HashMap<String, String>) -> Vec<GeminiPart> {
    if msg.parts.is_empty() {
        // Legacy message: use content field as plain text.
        let text = msg.content.clone();
        if text.is_empty() {
            return vec![];
        }
        return vec![GeminiPart {
            text: Some(text),
            inline_data: None,
            function_call: None,
            function_response: None,
        }];
    }

    let mut result = Vec::new();
    for part in &msg.parts {
        match part {
            MessagePart::Text { text } => {
                if !text.is_empty() {
                    result.push(GeminiPart {
                        text: Some(text.clone()),
                        inline_data: None,
                        function_call: None,
                        function_response: None,
                    });
                }
            }
            MessagePart::ToolUse { name, input, .. } => {
                result.push(GeminiPart {
                    text: None,
                    inline_data: None,
                    function_call: Some(GeminiFunctionCall {
                        name: name.clone(),
                        args: Some(input.clone()),
                    }),
                    function_response: None,
                });
            }
            MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let name = tool_names.get(tool_use_id).cloned().unwrap_or_else(|| {
                    tracing::warn!(
                        tool_use_id = %tool_use_id,
                        "ToolResult name lookup miss — using raw ID as function name"
                    );
                    tool_use_id.clone()
                });
                let response = if *is_error {
                    serde_json::json!({"error": content})
                } else {
                    serde_json::json!({"result": content})
                };
                result.push(GeminiPart {
                    text: None,
                    inline_data: None,
                    function_call: None,
                    function_response: Some(GeminiFunctionResponse { name, response }),
                });
            }
            MessagePart::Image(img) => {
                result.push(GeminiPart {
                    text: None,
                    inline_data: Some(GeminiInlineData {
                        mime_type: img.mime_type.clone(),
                        data: STANDARD.encode(&img.data),
                    }),
                    function_call: None,
                    function_response: None,
                });
            }
            // Other parts (ToolOutput, Recall, etc.) fall through to text extraction.
            other => {
                // Extract text if available (e.g. Recall, Summary, CodeContext, ToolOutput).
                let text = extract_part_text(other);
                if let Some(t) = text {
                    result.push(GeminiPart {
                        text: Some(t),
                        inline_data: None,
                        function_call: None,
                        function_response: None,
                    });
                }
            }
        }
    }
    result
}

fn extract_part_text(part: &MessagePart) -> Option<String> {
    match part {
        MessagePart::Recall { text }
        | MessagePart::Summary { text }
        | MessagePart::CodeContext { text }
        | MessagePart::CrossSession { text } => {
            if text.is_empty() {
                None
            } else {
                Some(text.clone())
            }
        }
        MessagePart::ToolOutput {
            tool_name, body, ..
        } => Some(format!("[tool output: {tool_name}]\n{body}")),
        _ => None,
    }
}

/// Extract plain text from a message, preferring parts over the legacy `content` field.
fn extract_text(msg: &Message) -> String {
    if !msg.parts.is_empty() {
        let mut pieces: Vec<&str> = Vec::new();
        for part in &msg.parts {
            if let MessagePart::Text { text } = part {
                pieces.push(text.as_str());
            }
            // Image / tool parts silently skipped in system extraction.
        }
        if !pieces.is_empty() {
            return pieces.join("\n");
        }
    }
    msg.content.clone()
}

// ---------------------------------------------------------------------------
// Gemini API types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTools>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
}

#[derive(Serialize, Deserialize)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<GeminiInlineData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTools {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig {
    function_calling_config: FunctionCallingConfig,
}

#[derive(Serialize)]
struct FunctionCallingConfig {
    mode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: GeminiContent,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(default)]
    candidates_token_count: u64,
}

#[derive(Deserialize)]
struct GeminiErrorResponse {
    error: GeminiErrorDetail,
}

#[derive(Deserialize)]
struct GeminiErrorDetail {
    code: u16,
    message: String,
    status: String,
}

// ---------------------------------------------------------------------------
// Model list API types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GeminiModelList {
    #[serde(default)]
    models: Vec<GeminiModelEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiModelEntry {
    /// e.g. "models/gemini-2.0-flash"
    name: String,
    display_name: String,
    #[serde(default)]
    input_token_limit: Option<u32>,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

// ---------------------------------------------------------------------------
// Embedding API types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmbedContentRequest<'a> {
    model: String,
    content: EmbedContent<'a>,
    task_type: &'static str,
}

#[derive(Serialize)]
struct EmbedContent<'a> {
    parts: Vec<EmbedPart<'a>>,
}

#[derive(Serialize)]
struct EmbedPart<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct EmbedContentResponse {
    embedding: EmbedValues,
}

#[derive(Deserialize)]
struct EmbedValues {
    values: Vec<f32>,
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

impl LlmProvider for GeminiProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "gemini"
    }

    fn context_window(&self) -> Option<usize> {
        // Gemini 1.5 Pro has 2M token context; all other Gemini models default to 1M.
        if self.model.contains("1.5-pro") || self.model.contains("gemini-1.5-pro") {
            Some(2_097_152)
        } else {
            Some(1_048_576)
        }
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.send_request(messages).await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let response = self.send_stream_request(messages).await?;
        Ok(gemini_sse_to_stream(response))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        self.send_tool_request(messages, tools).await
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let model = self
            .embedding_model
            .as_deref()
            .ok_or_else(|| LlmError::EmbedUnsupported {
                provider: "gemini".into(),
            })?;

        let url = format!("{}/v1beta/models/{}:embedContent", self.base_url, model);

        let body = EmbedContentRequest {
            model: format!("models/{model}"),
            content: EmbedContent {
                parts: vec![EmbedPart { text }],
            },
            // TODO(#1597): use RETRIEVAL_DOCUMENT for storage paths once
            // LlmProvider::embed() supports taskType parameter.
            task_type: "RETRIEVAL_QUERY",
        };

        let body_bytes = serde_json::to_vec(&body)?;

        let response = send_with_retry("gemini", MAX_RETRIES, self.status_tx.as_ref(), || {
            let req = self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .header("Content-Type", "application/json")
                .body(body_bytes.clone());
            async move { req.send().await }
        })
        .await?;

        let status = response.status();
        let body_text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            return Err(parse_gemini_error(&body_text, status));
        }

        let resp: EmbedContentResponse = serde_json::from_str(&body_text)?;
        if resp.embedding.values.is_empty() {
            return Err(LlmError::EmptyResponse {
                provider: "gemini".into(),
            });
        }
        Ok(resp.embedding.values)
    }

    fn supports_embeddings(&self) -> bool {
        self.embedding_model.is_some()
    }

    fn supports_vision(&self) -> bool {
        true
    }

    fn list_models(&self) -> Vec<String> {
        let mut models = vec![
            "gemini-2.5-pro".to_owned(),
            "gemini-2.0-flash".to_owned(),
            "gemini-1.5-pro".to_owned(),
            "gemini-1.5-flash".to_owned(),
        ];
        if let Some(ref em) = self.embedding_model {
            models.push(em.clone());
        }
        models
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.last_usage.lock().ok().and_then(|g| *g)
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        _stream: bool,
    ) -> serde_json::Value {
        if tools.is_empty() {
            let request = self.build_request(messages);
            serde_json::to_value(&request).unwrap_or(serde_json::Value::Null)
        } else {
            let request = self.build_tool_request(messages, tools);
            serde_json::to_value(&request).unwrap_or(serde_json::Value::Null)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
    fn gemini_supports_tool_use_true() {
        let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
        assert!(p.supports_tool_use());
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
        let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024);
        if let Ok(mut guard) = p.last_usage.lock() {
            *guard = Some((100, 200));
        }
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
        let event2 =
            r#"{"candidates":[{"content":{"parts":[{"text":" world","thought":false}]}}]}"#;
        let event3 =
            r#"{"candidates":[{"content":{"parts":[{"text":"thinking","thought":true}]}}]}"#;
        let sse_body =
            format!("data: {event1}\r\n\r\ndata: {event2}\r\n\r\ndata: {event3}\r\n\r\n");
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
        let mut schema =
            serde_json::json!({"type": "object", "properties": {"x": {"type": "number"}}});
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
            schema["properties"]["l1"]["properties"]["l2"]["properties"]["l3"]["properties"]["l4"]
                ["properties"]["l5"]["properties"]["l6"]["properties"]["l7"]["properties"]["l8"]["properties"]
                ["l9"]["type"],
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
                serde_json::Value::Object(Default::default())
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
        let p = GeminiProvider::new("key".into(), "gemini-2.0-flash".into(), 1024)
            .with_embedding_model("");
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
    #[ignore]
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
        let json = r#"{}"#;
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
}
