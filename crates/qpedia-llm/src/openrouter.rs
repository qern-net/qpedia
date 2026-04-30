//! OpenRouter provider — multi-model aggregator with one key/bill.
//! Wire-compatible with OpenAI's chat-completions API plus a few headers.

use crate::{CompleteReq, CompleteResp, LlmError, LlmProvider, StopReason, Token, ToolCall, Usage};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouter {
    api_key: String,
    base_url: String,
    default_model: String,
    client: reqwest::Client,
}

impl OpenRouter {
    pub fn new(api_key: impl Into<String>, default_model: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .expect("reqwest client");
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.into(),
            default_model: default_model.into(),
            client,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenRouter {
    fn name(&self) -> &str { "openrouter" }

    async fn complete(&self, req: CompleteReq) -> Result<CompleteResp, LlmError> {
        let body = OpenAIRequest::from_complete(&req, &self.default_model);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let resp = self.client.post(&url)
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "https://qpedia.local")
            .header("X-Title", "Qpedia")
            .json(&body)
            .send().await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;
        if !status.is_success() {
            if status.as_u16() == 429 { return Err(LlmError::RateLimited); }
            return Err(LlmError::Provider(format!("{status}: {text}")));
        }
        let parsed: OpenAIResponse = serde_json::from_str(&text)
            .map_err(|e| LlmError::Provider(format!("decode: {e}\nbody: {text}")))?;
        Ok(parsed.into())
    }

    async fn stream(&self, _req: CompleteReq) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError> {
        Err(LlmError::Provider("streaming not yet implemented".into()))
    }
}

// Shared OpenAI-shape wire types — also used by OpenAICompatible.

#[derive(Debug, Serialize)]
pub(crate) struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAIToolDef>,
}

#[derive(Debug, Serialize)]
struct OpenAIToolDef {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAIToolFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenAIToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAIToolCallFn,
}

#[derive(Debug, Serialize)]
struct OpenAIToolCallFn {
    name: String,
    /// JSON-encoded string per OpenAI spec.
    arguments: String,
}

impl OpenAIRequest {
    pub(crate) fn from_complete(req: &CompleteReq, fallback_model: &str) -> Self {
        let mut messages = Vec::new();
        if let Some(sys) = &req.system {
            messages.push(OpenAIMessage {
                role: "system".into(),
                content: Some(sys.clone()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        }
        for m in &req.messages {
            let (role, content, tool_calls, tool_call_id) = match m.role {
                crate::Role::User => ("user", Some(m.content.clone()), Vec::new(), None),
                crate::Role::Assistant => {
                    let tcs = m.tool_calls.iter().map(|tc| OpenAIToolCall {
                        id: tc.id.clone(),
                        kind: "function",
                        function: OpenAIToolCallFn {
                            name: tc.name.clone(),
                            arguments: tc.arguments.to_string(),
                        },
                    }).collect();
                    let content = if m.content.is_empty() { None } else { Some(m.content.clone()) };
                    ("assistant", content, tcs, None)
                }
                crate::Role::Tool => (
                    "tool",
                    Some(m.content.clone()),
                    Vec::new(),
                    m.tool_call_id.clone(),
                ),
                crate::Role::System => ("system", Some(m.content.clone()), Vec::new(), None),
            };
            messages.push(OpenAIMessage {
                role: role.into(),
                content,
                tool_calls,
                tool_call_id,
            });
        }

        let tools = req.tools.iter().map(|t| OpenAIToolDef {
            kind: "function",
            function: OpenAIToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        }).collect();

        OpenAIRequest {
            model: if req.model.is_empty() { fallback_model.into() } else { req.model.clone() },
            messages,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            tools,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIRespMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIRespMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAIRespToolCall>,
}

#[derive(Debug, Deserialize)]
struct OpenAIRespToolCall {
    id: String,
    #[serde(rename = "type")]
    #[serde(default)]
    _kind: Option<String>,
    function: OpenAIRespToolCallFn,
}

#[derive(Debug, Deserialize)]
struct OpenAIRespToolCallFn {
    name: String,
    /// JSON-encoded string per spec.
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl From<OpenAIResponse> for CompleteResp {
    fn from(r: OpenAIResponse) -> Self {
        let (content, finish, raw_calls) = r.choices.into_iter().next()
            .map(|c| (c.message.content.unwrap_or_default(), c.finish_reason, c.message.tool_calls))
            .unwrap_or_default();

        let tool_calls: Vec<ToolCall> = raw_calls.into_iter().map(|tc| ToolCall {
            id: tc.id,
            name: tc.function.name,
            // OpenAI sends arguments as a JSON-encoded string. Try parse;
            // fall back to a string-valued JSON object.
            arguments: serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments)),
        }).collect();

        let usage = r.usage.unwrap_or(OpenAIUsage { prompt_tokens: 0, completion_tokens: 0 });
        CompleteResp {
            content,
            tool_calls,
            usage: Usage { input_tokens: usage.prompt_tokens, output_tokens: usage.completion_tokens },
            stop_reason: match finish.as_deref() {
                Some("length") => StopReason::MaxTokens,
                Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
                _ => StopReason::EndTurn,
            },
        }
    }
}
