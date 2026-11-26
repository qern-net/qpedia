//! OpenAI-compatible HTTP implementation for vLLM, Ollama, LM Studio, llama.cpp's
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

#[cfg(test)]
mod privacy_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PRIVATE_RESPONSE: &str = "private-matter-contents-never-log";

    async fn provider(status: u16, body: String) -> OpenAICompatible {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 { return; }
                request.extend_from_slice(&chunk[..read]);
                assert!(request.len() <= 65536);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap_or("0").trim().parse().unwrap();
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        OpenAICompatible::new(format!("http://{address}/v1"), Some("fixture-only".into()), "fixture-model")
    }

    #[test]
    fn luna_uses_completion_budget_and_preserves_tools() {
        let original = serde_json::json!({"model":"gpt-6-luna", "max_tokens":81, "temperature":0, "tools":[{"type":"function"}]});
        let body = api_parameters(original.clone());
        assert_eq!(body["max_completion_tokens"], 81);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["store"], false);
        assert!(body.get("temperature").is_none() && body.get("max_tokens").is_none());
        assert_eq!(body["tools"], original["tools"]);
        let legacy = serde_json::json!({"model":"other-model", "max_tokens":5, "temperature":0});
        assert_eq!(api_parameters(legacy.clone()), legacy);
    }

    #[tokio::test]
    async fn truncated_vision_transcription_is_rejected() {
        let body = serde_json::json!({"choices":[{"finish_reason":"length","message":{"content":"partial"}}]});
        assert!(provider(200, body.to_string()).await.vision(vision_request()).await.is_err());
    }

    fn vision_request() -> VisionReq {
        VisionReq { model: "fixture-model".into(), prompt: "Read fixture".into(),
            image_mime: "image/png".into(), image_bytes: vec![0, 1], max_tokens: 10 }
    }

    #[tokio::test]
    async fn provider_errors_do_not_expose_response_bodies() {
        for status in [400, 401, 500] {
            let complete = provider(status, PRIVATE_RESPONSE.into()).await
                .complete(CompleteReq::new("fixture-model").user("fixture")).await.err().unwrap();
            let vision = provider(status, PRIVATE_RESPONSE.into()).await
                .vision(vision_request()).await.err().unwrap();
            let stream = provider(status, PRIVATE_RESPONSE.into()).await
                .stream(CompleteReq::new("fixture-model").user("fixture")).await.err().unwrap();
            for error in [complete, vision, stream] {
                let message = error.to_string();
                assert!(!message.contains(PRIVATE_RESPONSE), "response body escaped in an error");
                assert!(message.contains(&status.to_string()));
            }
        }
    }

    #[tokio::test]
    async fn invalid_success_responses_do_not_expose_response_bodies() {
        let complete = provider(200, PRIVATE_RESPONSE.into()).await
            .complete(CompleteReq::new("fixture-model").user("fixture")).await.err().unwrap();
        let vision = provider(200, PRIVATE_RESPONSE.into()).await
            .vision(vision_request()).await.err().unwrap();
        let missing = provider(200, format!("{{\"private\":\"{PRIVATE_RESPONSE}\"}}")).await
            .vision(vision_request()).await.err().unwrap();
        for error in [complete, vision, missing] {
            assert!(!error.to_string().contains(PRIVATE_RESPONSE), "response body escaped in a decode error");
        }
        assert!(matches!(provider(429, PRIVATE_RESPONSE.into()).await
            .complete(CompleteReq::new("fixture-model").user("fixture")).await,
            Err(LlmError::RateLimited)));
    }
}

fn api_parameters(mut body: serde_json::Value) -> serde_json::Value {
    if body["model"].as_str().is_some_and(|m| m.starts_with("gpt-6")) {
        if let Some(fields) = body.as_object_mut() {
            if let Some(tokens) = fields.remove("max_tokens") {
                fields.insert("max_completion_tokens".into(), tokens);
            }
            fields.remove("temperature");
            fields.insert("reasoning_effort".into(), "none".into());
            fields.insert("store".into(), false.into());
        }
    }
    body
}

impl OpenAICompatible {
    fn completion_body(&self, req: &CompleteReq) -> Result<serde_json::Value, LlmError> {
        serde_json::to_value(OpenAIRequest::from_complete(req, &self.default_model))
            .map(api_parameters).map_err(|_| LlmError::Provider("Could not encode completion request".into()))
    }

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
        let body = self.completion_body(&req)?;
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
            return Err(LlmError::Provider(format!("OpenAI-compatible provider returned HTTP {status}")));
        }
        let parsed: OpenAIResponse = serde_json::from_str(&text)
            .map_err(|_| LlmError::Provider("OpenAI-compatible provider returned an invalid response".into()))?;
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
        let body = api_parameters(serde_json::json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "temperature": 0.0,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": req.prompt },
                    { "type": "image_url", "image_url": { "url": data_url, "detail": "high" } }
                ]
            }]
        }));
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
            return Err(LlmError::Provider(format!("OpenAI-compatible provider returned HTTP {status}")));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| LlmError::Provider("OpenAI-compatible provider returned an invalid vision response".into()))?;
        if parsed["choices"][0]["finish_reason"].as_str().is_some_and(|reason| reason != "stop") {
            return Err(LlmError::Provider("OpenAI-compatible vision response was incomplete".into()));
        }
        let content = parsed["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| LlmError::Provider("OpenAI-compatible vision response contained no content".into()))?
            .to_string();
        debug!(model = %req.model, chars = content.len(), "openai vision");
        Ok(content)
    }

    async fn stream(&self, req: CompleteReq) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError> {
        let mut body = self.completion_body(&req)?;
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
            if status.as_u16() == 429 { return Err(LlmError::RateLimited); }
            return Err(LlmError::Provider(format!("OpenAI-compatible provider returned HTTP {status}")));
        }

        let bytes_stream = resp.bytes_stream();
        let token_stream = openai_sse_to_tokens(bytes_stream);
        Ok(Box::pin(token_stream))
    }
}
