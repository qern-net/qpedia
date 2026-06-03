//! OpenAI-compatible HTTP impl — works with vLLM, Ollama, LM Studio, llama.cpp's
//! server, and any other endpoint that speaks `/v1/chat/completions`.
//! Used for on-prem / air-gapped deployments.

use crate::openrouter::{openai_sse_to_tokens, OpenAIRequest, OpenAIResponse};
use crate::{CompleteReq, CompleteResp, LlmError, LlmProvider, Token, VisionReq};
use async_trait::async_trait;
use base64::Engine;
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

    async fn vision(&self, req: VisionReq) -> Result<String, LlmError> {
        // Chat-completions multimodal shape: a user message whose content is a
        // [text, image_url] array. The image rides inline as a base64 data URL.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&req.image_bytes);
        let data_url = format!("data:{};base64,{}", req.image_mime, b64);
        let body = serde_json::json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "temperature": 0.0,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": req.prompt },
                    { "type": "image_url", "image_url": { "url": data_url, "detail": "auto" } }
                ]
            }]
        });
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let mut builder = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder.send().await.map_err(|e| LlmError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;
        if !status.is_success() {
            if status.as_u16() == 429 {
                return Err(LlmError::RateLimited);
            }
            return Err(LlmError::Provider(format!("{status}: {text}")));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Provider(format!("decode vision: {e}\nbody: {text}")))?;
        let content = parsed["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| LlmError::Provider(format!("no content in vision response: {text}")))?
            .to_string();
        debug!(model = %req.model, chars = content.len(), "openai vision");
        Ok(content)
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
