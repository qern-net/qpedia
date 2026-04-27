//! Direct Anthropic Messages API impl. See https://docs.anthropic.com/api/messages
//!
//! Translates between our generic `CompleteReq`/`CompleteResp` and Anthropic's
//! native shape. Streaming is not yet implemented (returns Provider error);
//! the ingest agent and chat retriever will need it later.

use crate::{CompleteReq, CompleteResp, LlmError, LlmProvider, Message, Role, StopReason, ToolCall, Token, Usage};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicDirect {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl AnthropicDirect {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, "https://api.anthropic.com/v1")
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client");
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            client,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicDirect {
    fn name(&self) -> &str { "anthropic-direct" }

    async fn complete(&self, req: CompleteReq) -> Result<CompleteResp, LlmError> {
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        let body = AnthropicRequest::from(&req);

        debug!(model = %req.model, msgs = req.messages.len(), "anthropic complete");

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;

        if !status.is_success() {
            if status.as_u16() == 429 {
                return Err(LlmError::RateLimited);
            }
            return Err(LlmError::Provider(format!("{status}: {text}")));
        }

        let parsed: AnthropicResponse = serde_json::from_str(&text)
            .map_err(|e| LlmError::Provider(format!("decode response: {e}\nbody: {text}")))?;

        Ok(parsed.into())
    }

    async fn stream(&self, _req: CompleteReq) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError> {
        Err(LlmError::Provider("streaming not yet implemented".into()))
    }
}

// ---------- Anthropic wire types ----------

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

impl From<&CompleteReq> for AnthropicRequest {
    fn from(r: &CompleteReq) -> Self {
        AnthropicRequest {
            model: r.model.clone(),
            max_tokens: r.max_tokens,
            system: r.system.clone(),
            messages: r.messages.iter().map(AnthropicMessage::from).collect(),
            tools: r.tools.iter().map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            }).collect(),
            temperature: Some(r.temperature),
        }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContent>,
}

impl From<&Message> for AnthropicMessage {
    fn from(m: &Message) -> Self {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "user",     // Anthropic encodes tool results as user turn
            Role::System => "user",   // System lives in `system` field, not messages
        }.to_string();
        AnthropicMessage {
            role,
            content: vec![AnthropicContent::Text { text: m.content.clone() }],
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text { text: String },
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<AnthropicResponseContent>,
    #[serde(default)]
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseContent {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

impl From<AnthropicResponse> for CompleteResp {
    fn from(r: AnthropicResponse) -> Self {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in r.content {
            match block {
                AnthropicResponseContent::Text { text: t } => {
                    if !text.is_empty() { text.push('\n'); }
                    text.push_str(&t);
                }
                AnthropicResponseContent::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall { id, name, arguments: input });
                }
            }
        }

        let stop_reason = match r.stop_reason.as_deref() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        };

        CompleteResp {
            content: text,
            tool_calls,
            usage: Usage {
                input_tokens: r.usage.input_tokens,
                output_tokens: r.usage.output_tokens,
            },
            stop_reason,
        }
    }
}
