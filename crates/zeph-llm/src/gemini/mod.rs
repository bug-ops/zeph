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
use crate::usage::UsageTracker;

const MAX_RETRIES: u32 = 3;
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Thinking level for Gemini models that support extended reasoning.
///
/// Maps to `generationConfig.thinkingConfig.thinkingLevel` in the Gemini API.
/// Valid for Gemini 3+ models. For Gemini 2.5, use `thinking_budget` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    max_output_tokens: u32,
    embedding_model: Option<String>,
    usage: UsageTracker,
    generation_overrides: Option<GenerationOverrides>,
    status_tx: Option<StatusTx>,
    thinking_level: Option<ThinkingLevel>,
    thinking_budget: Option<i32>,
    include_thoughts: Option<bool>,
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
            .field("usage", &self.usage)
            .field("generation_overrides", &self.generation_overrides)
            .field("status_tx", &self.status_tx.is_some())
            .field("thinking_level", &self.thinking_level)
            .field("thinking_budget", &self.thinking_budget)
            .field("include_thoughts", &self.include_thoughts)
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
            usage: UsageTracker::default(),
            generation_overrides: self.generation_overrides.clone(),
            status_tx: self.status_tx.clone(),
            thinking_level: self.thinking_level,
            thinking_budget: self.thinking_budget,
            include_thoughts: self.include_thoughts,
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
            usage: UsageTracker::default(),
            generation_overrides: None,
            status_tx: None,
            thinking_level: None,
            thinking_budget: None,
            include_thoughts: None,
        }
    }

    #[must_use]
    pub fn with_thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = Some(level);
        self
    }

    /// Set the thinking budget (tokens) for Gemini 2.5 models.
    ///
    /// Valid values: `-1` (dynamic), `0` (disable), or `1–32768`.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Other`] if `budget` is outside the valid range.
    pub fn with_thinking_budget(mut self, budget: i32) -> Result<Self, LlmError> {
        if budget != -1 && !(0..=32768).contains(&budget) {
            return Err(LlmError::Other(format!(
                "thinking_budget {budget} is out of range; valid: -1 (dynamic), 0 (disable), 1-32768"
            )));
        }
        self.thinking_budget = Some(budget);
        Ok(self)
    }

    #[must_use]
    pub fn with_include_thoughts(mut self, include: bool) -> Self {
        self.include_thoughts = Some(include);
        self
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
        let thinking_config = if self.thinking_level.is_some()
            || self.thinking_budget.is_some()
            || self.include_thoughts.is_some()
        {
            if self.thinking_level.is_some() || self.thinking_budget.is_some() {
                tracing::debug!(
                    model = %self.model,
                    "thinking_config is set; ensure your model supports it \
                     (thinkingLevel for Gemini 3+, thinkingBudget for Gemini 2.5)"
                );
            }
            Some(GeminiThinkingConfig {
                thinking_level: self.thinking_level,
                thinking_budget: self.thinking_budget,
                include_thoughts: self.include_thoughts,
            })
        } else {
            None
        };
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
            thinking_config,
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

        if let Some(ref u) = resp.usage_metadata {
            self.usage
                .record_usage(u.prompt_token_count, u.candidates_token_count);
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

        if let Some(ref u) = resp.usage_metadata {
            self.usage
                .record_usage(u.prompt_token_count, u.candidates_token_count);
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
struct GeminiNormalizeVisitor;

impl crate::schema::SchemaVisitor for GeminiNormalizeVisitor {
    fn visit(&mut self, schema: &mut serde_json::Value) -> bool {
        let Some(obj) = schema.as_object_mut() else {
            return false;
        };

        // Handle anyOf/oneOf: detect Option<T> pattern [{type: T}, {type: "null"}]
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
                    // Option<T> pattern: replace node with non-null variant + nullable: true
                    let mut replacement = non_null[0].clone();
                    if let Some(r) = replacement.as_object_mut() {
                        r.remove("anyOf");
                        r.remove("oneOf");
                        r.insert("nullable".to_owned(), serde_json::Value::Bool(true));
                    }
                    *schema = replacement;
                    // walker will recurse into the replacement node
                    return true;
                }
            }
            // Cannot simplify — drop the anyOf/oneOf entirely (Gemini rejects it)
            obj.remove("anyOf");
            obj.remove("oneOf");
        }

        // Re-borrow after potential anyOf removal.
        let Some(obj) = schema.as_object_mut() else {
            return true;
        };

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
        true
    }
}

fn normalize_schema(schema: &mut serde_json::Value, depth: u8) {
    crate::schema::walk_schema(schema, &mut GeminiNormalizeVisitor, depth);
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
    if let MessagePart::ToolOutput {
        tool_name, body, ..
    } = part
    {
        return Some(format!("[tool output: {tool_name}]\n{body}"));
    }
    // Exclude Text (handled separately) and non-text-like parts.
    match part {
        MessagePart::Recall { .. }
        | MessagePart::Summary { .. }
        | MessagePart::CodeContext { .. }
        | MessagePart::CrossSession { .. } => part
            .as_plain_text()
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
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
struct GeminiThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<ThinkingLevel>,
    /// Token budget for thinking (Gemini 2.5): 0 = disable, -1 = dynamic, 0–32768.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_thoughts: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
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

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        self.send_tool_request(messages, tools).await
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        use crate::embed::truncate_for_embed;

        let model = self
            .embedding_model
            .as_deref()
            .ok_or_else(|| LlmError::EmbedUnsupported {
                provider: "gemini".into(),
            })?;

        let url = format!("{}/v1beta/models/{}:embedContent", self.base_url, model);

        let text = truncate_for_embed(text);
        let body = EmbedContentRequest {
            model: format!("models/{model}"),
            content: EmbedContent {
                parts: vec![EmbedPart { text: &text }],
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
            // Check for 400 before delegating to parse_gemini_error, which maps all
            // non-rate-limited errors to LlmError::Other. A 400 means the input itself
            // is invalid; retrying on another provider would fail identically.
            if status == reqwest::StatusCode::BAD_REQUEST {
                return Err(LlmError::InvalidInput {
                    provider: "gemini".into(),
                    message: body_text,
                });
            }
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
        self.usage.last_usage()
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
mod tests;
