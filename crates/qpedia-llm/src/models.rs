//! Approved-models registry — the machine-readable source of truth the API
//! serves and that validates tenant model selections.
//!
//! Mirrors `APPROVED-MODELS.md` (the human/changelog doc). Keep the two in
//! lockstep: when the quarterly review changes the approved set, update both
//! this table and the doc's changelog in the same PR (see
//! `FEATURE-LANDING.md`). Only `Status::Approved` models are offered to
//! tenants and to the managed tier.

use serde::{Deserialize, Serialize};

/// A provider Qpedia can talk to. String values match `QPEDIA_LLM_PROVIDER`
/// and the `llm_config.provider` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Anthropic,
    Openai,
    Openrouter,
    Gemini,
    OpenaiCompatible,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
            Provider::Openrouter => "openrouter",
            Provider::Gemini => "gemini",
            Provider::OpenaiCompatible => "openai-compatible",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Provider::Anthropic),
            "openai" => Some(Provider::Openai),
            "openrouter" => Some(Provider::Openrouter),
            "gemini" => Some(Provider::Gemini),
            "openai-compatible" => Some(Provider::OpenaiCompatible),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Validated + supported; offered to tenants and the managed tier.
    Approved,
    /// Under evaluation; visible to operators but not offered by default.
    Trial,
}

/// One entry in the approved list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedModel {
    /// API model id (what goes in `llm_config.model` / `QPEDIA_LLM_MODEL`).
    pub id: &'static str,
    pub provider: Provider,
    /// Short role label for UI (e.g. "Production sweet spot").
    pub role: &'static str,
    pub status: Status,
    /// License for open-weight models reached via `openai-compatible`; None for
    /// hosted cloud APIs.
    pub license: Option<&'static str>,
    /// True for open-weight models run on a self-hosted OpenAI-compatible
    /// endpoint (the `id` is then informational — the endpoint serves it).
    pub open_weight: bool,
}

/// The Q2 2026 approved list. Mirrors `APPROVED-MODELS.md`. Newest review wins;
/// bump together with the doc's changelog.
pub const APPROVED_MODELS: &[ApprovedModel] = &[
    // ---- cloud ----
    ApprovedModel { id: "claude-opus-4-8",   provider: Provider::Anthropic,  role: "Heavy distillation",        status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "claude-sonnet-4-6", provider: Provider::Anthropic,  role: "Production sweet spot",      status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "claude-haiku-4-5",  provider: Provider::Anthropic,  role: "Fast / cheap (default)",     status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "gpt-5.5",           provider: Provider::Openai,     role: "Heavy reasoning",            status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "gpt-5.4-mini",      provider: Provider::Openai,     role: "Cost-efficient default",     status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "gpt-5.4-nano",      provider: Provider::Openai,     role: "High-volume / low-latency",  status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "gpt-5.5-pro",       provider: Provider::Openai,     role: "Premium reasoning",          status: Status::Trial,    license: None, open_weight: false },
    ApprovedModel { id: "gemini-3-pro",          provider: Provider::Gemini, role: "Heavy reasoning, long ctx", status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "gemini-3.5-flash",      provider: Provider::Gemini, role: "Fast default-class",        status: Status::Approved, license: None, open_weight: false },
    ApprovedModel { id: "gemini-3.1-flash-lite", provider: Provider::Gemini, role: "High-volume / low-cost",    status: Status::Trial,    license: None, open_weight: false },
    // ---- open-weight (run via openai-compatible) ----
    ApprovedModel { id: "qwen-3.5",        provider: Provider::OpenaiCompatible, role: "General distill + chat",   status: Status::Approved, license: Some("Apache-2.0"), open_weight: true },
    ApprovedModel { id: "deepseek-v4",     provider: Provider::OpenaiCompatible, role: "Heavy reasoning",          status: Status::Approved, license: Some("MIT"),        open_weight: true },
    ApprovedModel { id: "llama-4-maverick", provider: Provider::OpenaiCompatible, role: "General-purpose",         status: Status::Approved, license: Some("Llama Community"), open_weight: true },
    ApprovedModel { id: "llama-4-scout",   provider: Provider::OpenaiCompatible, role: "Very-long-context",        status: Status::Approved, license: Some("Llama Community"), open_weight: true },
    ApprovedModel { id: "mistral-small-4", provider: Provider::OpenaiCompatible, role: "Single-GPU self-host",     status: Status::Approved, license: Some("Apache-2.0"), open_weight: true },
    ApprovedModel { id: "mistral-large-3", provider: Provider::OpenaiCompatible, role: "Multilingual, heavier",    status: Status::Approved, license: Some("Apache-2.0"), open_weight: true },
    ApprovedModel { id: "glm-5.1",         provider: Provider::OpenaiCompatible, role: "Agentic / structured",     status: Status::Trial,    license: Some("MIT"),        open_weight: true },
    ApprovedModel { id: "gemma-4",         provider: Provider::OpenaiCompatible, role: "Lightweight on-prem",      status: Status::Trial,    license: Some("Gemma terms"), open_weight: true },
];

/// All entries (any status). The API filters to `Approved` for tenant-facing use.
pub fn all_models() -> &'static [ApprovedModel] {
    APPROVED_MODELS
}

/// Models offered to tenants / the managed tier (status = Approved).
pub fn approved_models() -> impl Iterator<Item = &'static ApprovedModel> {
    APPROVED_MODELS.iter().filter(|m| m.status == Status::Approved)
}

/// Is `model` approved (✅) for `provider`? For `openai-compatible` the operator
/// runs an arbitrary endpoint, so any non-empty model id is accepted (BYOL) —
/// the registry is advisory there, gating only the hosted providers.
pub fn is_approved(provider: Provider, model: &str) -> bool {
    if provider == Provider::OpenaiCompatible {
        return !model.trim().is_empty();
    }
    approved_models().any(|m| m.provider == provider && m.id == model)
}

/// The per-provider default model (kept in sync with the constants in lib.rs
/// and the "Defaults" section of APPROVED-MODELS.md).
pub fn default_model(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "claude-haiku-4-5",
        Provider::Openai => "gpt-5.4-mini",
        Provider::Openrouter => "anthropic/claude-haiku-4-5",
        Provider::Gemini => "gemini-3.5-flash",
        Provider::OpenaiCompatible => "default",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_hosted_models_validate() {
        assert!(is_approved(Provider::Anthropic, "claude-sonnet-4-6"));
        assert!(is_approved(Provider::Openai, "gpt-5.5"));
        assert!(is_approved(Provider::Gemini, "gemini-3-pro"));
    }

    #[test]
    fn wrong_provider_or_dropped_model_is_rejected() {
        // right id, wrong provider
        assert!(!is_approved(Provider::Openai, "claude-sonnet-4-6"));
        // a dropped legacy default
        assert!(!is_approved(Provider::Openai, "gpt-4.1-mini"));
        // unknown id
        assert!(!is_approved(Provider::Anthropic, "claude-nonexistent"));
    }

    #[test]
    fn trial_models_are_not_offered_as_approved() {
        // gpt-5.5-pro is Trial in the Q2 2026 list
        assert!(!is_approved(Provider::Openai, "gpt-5.5-pro"));
        assert!(approved_models().all(|m| m.status == Status::Approved));
    }

    #[test]
    fn openai_compatible_accepts_any_nonempty_id_byol() {
        assert!(is_approved(Provider::OpenaiCompatible, "some-local-model"));
        assert!(!is_approved(Provider::OpenaiCompatible, ""));
        assert!(!is_approved(Provider::OpenaiCompatible, "   "));
    }

    #[test]
    fn defaults_track_the_approved_list() {
        assert_eq!(default_model(Provider::Openai), "gpt-5.4-mini");
        assert_eq!(default_model(Provider::Anthropic), "claude-haiku-4-5");
    }

    #[test]
    fn provider_string_roundtrip() {
        for p in [
            Provider::Anthropic,
            Provider::Openai,
            Provider::Openrouter,
            Provider::Gemini,
            Provider::OpenaiCompatible,
        ] {
            assert_eq!(Provider::parse(p.as_str()), Some(p));
        }
        assert_eq!(Provider::parse("openai-compatible"), Some(Provider::OpenaiCompatible));
        assert_eq!(Provider::parse("nope"), None);
    }
}
