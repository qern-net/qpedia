//! OpenAI-compatible HTTP impl — works with vLLM, Ollama, LM Studio, llama.cpp's
//! server, and any other endpoint that speaks `/v1/chat/completions`.
//! Used for on-prem / air-gapped deployments.

use crate::openrouter::{OpenAIRequest, OpenAIResponse};
use crate::{CompleteReq, CompleteResp, LlmError, LlmProvider, Token};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::time::Duration;

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
        Ok(parsed.into())
    }

    async fn stream(&self, _req: CompleteReq) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError> {
        Err(LlmError::Provider("streaming not yet implemented".into()))
    }
}
