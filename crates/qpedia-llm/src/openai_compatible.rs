//! OpenAI-compatible HTTP impl — works with vLLM, Ollama, LM Studio, llama.cpp's
//! server, and any other endpoint that speaks `/v1/chat/completions`.
//! Used for on-prem / air-gapped deployments.

use crate::openrouter::{openai_sse_to_tokens, OpenAIRequest, OpenAIResponse};
use crate::{CompleteReq, CompleteResp, LlmError, LlmProvider, Token};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::time::Duration;
use tracing::debug;

pub struct OpenAICompatible {
    base_url: String,
    api_key: Option<String>,
    default_model: String,
    client: reqwest::Client,
}

impl OpenAICompatible {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>, default_model: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .expect("reqwest client");
        Self {
            base_url: base_url.into(),
            api_key,
            default_model: default_model.into(),
            client,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAICompatible {
    fn name(&self) -> &str { "openai-compatible" }

    async fn complete(&self, req: CompleteReq) -> Result<CompleteResp, LlmError> {
        let body = OpenAIRequest::from_complete(&req, &self.default_model);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut builder = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let resp = builder.send().await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;
        if !status.is_success() {
            if status.as_u16() == 429 { return Err(LlmError::RateLimited); }
            return Err(LlmError::Provider(format!("{status}: {text}")));
        }
        let parsed: OpenAIResponse = serde_json::from_str(&text)
            .map_err(|e| LlmError::Provider(format!("decode: {e}\nbody: {text}")))?;
        let resp: CompleteResp = parsed.into();
        tracing::info!(
            model = %req.model,
            input_tokens = resp.usage.input_tokens,
            output_tokens = resp.usage.output_tokens,
            total_tokens = resp.usage.input_tokens + resp.usage.output_tokens,
            msgs = req.messages.len(),
            tools = req.tools.len(),
            "openai complete"
        );
        Ok(resp)
    }

    async fn stream(&self, req: CompleteReq) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError> {
        let mut body = serde_json::to_value(OpenAIRequest::from_complete(&req, &self.default_model))
            .map_err(|e| LlmError::Provider(format!("encode req: {e}")))?;
        body.as_object_mut().unwrap().insert("stream".into(), serde_json::Value::Bool(true));

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        debug!(model = %req.model, "openai-compatible stream");

        let mut builder = self.client.post(&url).header("accept", "text/event-stream").json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let resp = builder.send().await.map_err(|e| LlmError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 { return Err(LlmError::RateLimited); }
            return Err(LlmError::Provider(format!("{status}: {text}")));
        }

        let bytes_stream = resp.bytes_stream();
        let token_stream = openai_sse_to_tokens(bytes_stream);
        Ok(Box::pin(token_stream))
    }
}
