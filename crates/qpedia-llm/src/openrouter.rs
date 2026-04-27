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
            .timeout(Duration::from_secs(120))
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

// Shared with OpenAICompatible.

#[derive(Debug, Serialize)]
pub(crate) struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    max_tokens: u32,
    temperature: f32,
}

impl OpenAIRequest {
    pub(crate) fn from_complete(req: &CompleteReq, fallback_model: &str) -> Self {
        let mut messages = Vec::new();
        if let Some(sys) = &req.system {
            messages.push(OpenAIMessage { role: "system".into(), content: sys.clone() });
        }
        for m in &req.messages {
            messages.push(OpenAIMessage {
                role: match m.role {
                    crate::Role::User => "user",
                    crate::Role::Assistant => "assistant",
                    crate::Role::System => "system",
                    crate::Role::Tool => "tool",
                }.into(),
                content: m.content.clone(),
            });
        }
        OpenAIRequest {
            model: if req.model.is_empty() { fallback_model.into() } else { req.model.clone() },
            messages,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    content: String,
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
    content: String,
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
        let (content, finish) = r.choices.into_iter().next()
            .map(|c| (c.message.content, c.finish_reason))
            .unwrap_or_default();
        let usage = r.usage.unwrap_or(OpenAIUsage { prompt_tokens: 0, completion_tokens: 0 });
        CompleteResp {
            content,
            tool_calls: Vec::<ToolCall>::new(),
            usage: Usage { input_tokens: usage.prompt_tokens, output_tokens: usage.completion_tokens },
            stop_reason: match finish.as_deref() {
                Some("length") => StopReason::MaxTokens,
                Some("tool_calls") => StopReason::ToolUse,
                _ => StopReason::EndTurn,
            },
        }
    }
}
