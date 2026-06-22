//! LLM provider abstraction. See DESIGN.md §4.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tracing::{info, warn};

pub mod anthropic;
pub mod models;
pub mod openai_compatible;
pub mod openrouter;

pub use anthropic::AnthropicDirect;
pub use models::{
    all_models, approved_models, default_model, is_approved, ApprovedModel, Provider, Status,
};
pub use openai_compatible::OpenAICompatible;
pub use openrouter::OpenRouter;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(&self, req: CompleteReq) -> Result<CompleteResp, LlmError>;
    async fn stream(&self, req: CompleteReq) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError>;

    /// One-shot multimodal request: OCR/describe an image, returning plain
    /// text. Default reports no vision support so callers fall back to a
    /// metadata-only path. Implemented for OpenAI-compatible providers.
    async fn vision(&self, _req: VisionReq) -> Result<String, LlmError> {
        Err(LlmError::Provider("vision not supported by this provider".into()))
    }
}

/// A single image + instruction for a `vision` call.
#[derive(Debug, Clone)]
pub struct VisionReq {
    pub model: String,
    pub prompt: String,
    pub image_mime: String,
    pub image_bytes: Vec<u8>,
    pub max_tokens: u32,
}

/// Per-provider default model. Override with `QPEDIA_LLM_MODEL`.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4-mini";
const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-haiku-4-5";

/// Build an LLM provider from env. Returns Ok(None) if no provider can be
/// configured (e.g. no API key). Returns Err if the configuration is malformed.
///
/// Detection order:
///   1. `QPEDIA_LLM_PROVIDER` if set explicitly
///   2. else: ANTHROPIC_API_KEY → anthropic; OPENAI_API_KEY → openai;
///      OPENROUTER_API_KEY → openrouter; QPEDIA_LLM_BASE_URL → openai-compatible
///
/// Env reads (per provider):
///   anthropic         : ANTHROPIC_API_KEY, optional ANTHROPIC_BASE_URL
///   openai            : OPENAI_API_KEY,   optional OPENAI_BASE_URL
///   openrouter        : OPENROUTER_API_KEY
///   openai-compatible : QPEDIA_LLM_BASE_URL, optional QPEDIA_LLM_API_KEY
///
/// Model selection: `QPEDIA_LLM_MODEL` overrides; otherwise per-provider default.
pub fn provider_from_env() -> Result<Option<Arc<dyn LlmProvider>>> {
    let kind = detect_provider_kind();
    let model_override = std::env::var("QPEDIA_LLM_MODEL").ok().filter(|s| !s.trim().is_empty());

    let provider: Option<Arc<dyn LlmProvider>> = match kind.as_str() {
        "anthropic" => match nonempty_env("ANTHROPIC_API_KEY") {
            Some(key) => {
                let base = anthropic_base_url();
                info!(provider = "anthropic-direct", base = %base, "LLM configured");
                Some(Arc::new(AnthropicDirect::with_base_url(key, base)) as Arc<dyn LlmProvider>)
            }
            None => {
                warn!("provider=anthropic but ANTHROPIC_API_KEY missing — LLM disabled");
                None
            }
        },
        "openai" => match nonempty_env("OPENAI_API_KEY") {
            Some(key) => {
                let base = nonempty_env("OPENAI_BASE_URL")
                    .unwrap_or_else(|| "https://api.openai.com/v1".into());
                let model = model_override.unwrap_or_else(|| DEFAULT_OPENAI_MODEL.into());
                info!(provider = "openai", base = %base, model = %model, "LLM configured");
                Some(Arc::new(OpenAICompatible::new(base, Some(key), model)) as Arc<dyn LlmProvider>)
            }
            None => {
                warn!("provider=openai but OPENAI_API_KEY missing — LLM disabled");
                None
            }
        },
        "openrouter" => match nonempty_env("OPENROUTER_API_KEY") {
            Some(key) => {
                let model = model_override.unwrap_or_else(|| DEFAULT_OPENROUTER_MODEL.into());
                info!(provider = "openrouter", model = %model, "LLM configured");
                Some(Arc::new(OpenRouter::new(key, model)) as Arc<dyn LlmProvider>)
            }
            None => {
                warn!("provider=openrouter but OPENROUTER_API_KEY missing — LLM disabled");
                None
            }
        },
        "openai-compatible" | "vllm" | "ollama" => {
            let base = nonempty_env("QPEDIA_LLM_BASE_URL").ok_or_else(|| {
                anyhow!("QPEDIA_LLM_BASE_URL required for openai-compatible provider")
            })?;
            let api_key = nonempty_env("QPEDIA_LLM_API_KEY");
            let model = model_override.unwrap_or_else(|| "default".into());
            info!(provider = "openai-compatible", base = %base, model = %model, "LLM configured");
            Some(Arc::new(OpenAICompatible::new(base, api_key, model)) as Arc<dyn LlmProvider>)
        }
        other => return Err(anyhow!("unknown QPEDIA_LLM_PROVIDER: {other}")),
    };

    Ok(provider)
}

/// An explicit, per-tenant LLM configuration (resolved from the `llm_config`
/// table). Any field left `None` falls back to the deployment env, so a tenant
/// can override just the model, just the credentials, or both — and a tenant
/// with no row behaves exactly like `provider_from_env`.
#[derive(Debug, Clone, Default)]
pub struct LlmConfig {
    pub provider: String,         // anthropic | openai | openrouter | openai-compatible
    pub model: Option<String>,    // approved model id; None ⇒ per-provider default
    pub api_key: Option<String>,  // decrypted BYO key; None ⇒ deployment env key
    pub base_url: Option<String>, // openai-compatible / proxy override
}

/// Build a provider from an explicit [`LlmConfig`] rather than the process
/// environment. Mirrors [`provider_from_env`] construction so behaviour is
/// identical; used by per-request/per-tenant resolution. Returns `Ok(None)`
/// when the chosen provider has no usable credential (BYOL not configured).
pub fn provider_from_config(cfg: &LlmConfig) -> Result<Option<Arc<dyn LlmProvider>>> {
    let model = cfg.model.clone().filter(|s| !s.trim().is_empty());
    let provider: Option<Arc<dyn LlmProvider>> = match cfg.provider.as_str() {
        "anthropic" => match cfg.api_key.clone().or_else(|| nonempty_env("ANTHROPIC_API_KEY")) {
            // Anthropic carries the model per-request (CompleteReq.model); the
            // caller sets the resolved model there. Provider holds creds only.
            Some(key) => {
                let base = cfg.base_url.clone().unwrap_or_else(anthropic_base_url);
                Some(Arc::new(AnthropicDirect::with_base_url(key, base)) as Arc<dyn LlmProvider>)
            }
            None => None,
        },
        "openai" => match cfg.api_key.clone().or_else(|| nonempty_env("OPENAI_API_KEY")) {
            Some(key) => {
                let base = cfg
                    .base_url
                    .clone()
                    .or_else(|| nonempty_env("OPENAI_BASE_URL"))
                    .unwrap_or_else(|| "https://api.openai.com/v1".into());
                let model = model.unwrap_or_else(|| DEFAULT_OPENAI_MODEL.into());
                Some(Arc::new(OpenAICompatible::new(base, Some(key), model)) as Arc<dyn LlmProvider>)
            }
            None => None,
        },
        "openrouter" => match cfg.api_key.clone().or_else(|| nonempty_env("OPENROUTER_API_KEY")) {
            Some(key) => {
                let model = model.unwrap_or_else(|| DEFAULT_OPENROUTER_MODEL.into());
                Some(Arc::new(OpenRouter::new(key, model)) as Arc<dyn LlmProvider>)
            }
            None => None,
        },
        "openai-compatible" | "vllm" | "ollama" => {
            let base = cfg
                .base_url
                .clone()
                .or_else(|| nonempty_env("QPEDIA_LLM_BASE_URL"))
                .ok_or_else(|| anyhow!("base_url required for openai-compatible provider"))?;
            let api_key = cfg.api_key.clone().or_else(|| nonempty_env("QPEDIA_LLM_API_KEY"));
            let model = model.unwrap_or_else(|| "default".into());
            Some(Arc::new(OpenAICompatible::new(base, api_key, model)) as Arc<dyn LlmProvider>)
        }
        other => return Err(anyhow!("unknown provider: {other}")),
    };
    Ok(provider)
}

fn detect_provider_kind() -> String {
    if let Some(v) = nonempty_env("QPEDIA_LLM_PROVIDER") {
        return v.trim().to_lowercase();
    }
    if nonempty_env("ANTHROPIC_API_KEY").is_some()  { return "anthropic".into(); }
    if nonempty_env("OPENAI_API_KEY").is_some()     { return "openai".into(); }
    if nonempty_env("OPENROUTER_API_KEY").is_some() { return "openrouter".into(); }
    if nonempty_env("QPEDIA_LLM_BASE_URL").is_some() { return "openai-compatible".into(); }
    "anthropic".into()  // emit the helpful "missing key" warning
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

fn anthropic_base_url() -> String {
    let raw = nonempty_env("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|| "https://api.anthropic.com".into());
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{}/v1", trimmed)
    }
}

/// Returns the configured model for ad-hoc callers (e.g. classifier).
/// Falls back to the anthropic default if `QPEDIA_LLM_MODEL` isn't set —
/// override per-provider via `QPEDIA_LLM_MODEL` to keep the right kind in sync.
pub fn current_model() -> String {
    if let Some(m) = nonempty_env("QPEDIA_LLM_MODEL") { return m; }
    match detect_provider_kind().as_str() {
        "openai" => DEFAULT_OPENAI_MODEL.into(),
        "openrouter" => DEFAULT_OPENROUTER_MODEL.into(),
        _ => DEFAULT_ANTHROPIC_MODEL.into(),
    }
}

/// The model to use for image OCR/description (Band 6.1), or `None` if image
/// vision is disabled/unavailable. `QPEDIA_VISION=0` disables it;
/// `QPEDIA_VISION_MODEL` sets an explicit model; otherwise the current model
/// is used for vision-capable providers (OpenAI / OpenRouter — `gpt-5.4-mini`
/// and friends are multimodal). Anthropic-direct and bare local endpoints
/// require an explicit `QPEDIA_VISION_MODEL` to opt in.
pub fn vision_model() -> Option<String> {
    if let Some(v) = nonempty_env("QPEDIA_VISION") {
        if matches!(v.trim().to_lowercase().as_str(), "0" | "false" | "off" | "no") {
            return None;
        }
    }
    if let Some(m) = nonempty_env("QPEDIA_VISION_MODEL") {
        return Some(m);
    }
    match detect_provider_kind().as_str() {
        "openai" | "openrouter" => Some(current_model()),
        _ => None,
    }
}

// ---------- shared wire types ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteReq {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl CompleteReq {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 1024,
            temperature: 0.0,
        }
    }
    pub fn system(mut self, s: impl Into<String>) -> Self { self.system = Some(s.into()); self }
    pub fn user(mut self, content: impl Into<String>) -> Self {
        self.messages.push(Message::user(content));
        self
    }
    pub fn max_tokens(mut self, n: u32) -> Self { self.max_tokens = n; self }
    pub fn temperature(mut self, t: f32) -> Self { self.temperature = t; self }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Set on Tool-role messages to identify which `tool_use` they answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Set on Tool-role messages to flag failed tool execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), tool_calls: Vec::new(), tool_call_id: None, is_error: None }
    }
    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, content: content.into(), tool_calls, tool_call_id: None, is_error: None }
    }
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            is_error: Some(is_error),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role { System, User, Assistant, Tool }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResp {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason { EndTurn, ToolUse, MaxTokens, StopSequence }

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub text: String,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http: {0}")]
    Http(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("rate limited")]
    RateLimited,
    #[error("context window exceeded")]
    ContextWindow,
}
