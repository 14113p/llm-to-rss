pub mod anthropic;
pub mod gemini;
pub mod openai_compatible;
pub mod openai_responses;

use std::time::Duration;

use reqwest::Client;

use crate::config::{ProviderConfig, ProviderKind};
use crate::error::{AppError, Result};
use crate::store::Citation;

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub system_prompt: Option<String>,
    pub user_prompt: String,
    pub web_search: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
    pub model: String,
    pub citations: Vec<Citation>,
}

/// Dispatch an LLM request to the configured provider backend.
pub async fn generate(
    client: &Client,
    provider_name: &str,
    provider: &ProviderConfig,
    req: &LlmRequest,
    timeout: Duration,
) -> Result<LlmResponse> {
    match provider.kind {
        ProviderKind::OpenaiCompatible => {
            openai_compatible::call(client, provider_name, provider, req, timeout).await
        }
        ProviderKind::OpenaiResponses => {
            openai_responses::call(client, provider_name, provider, req, timeout).await
        }
        ProviderKind::Gemini => gemini::call(client, provider_name, provider, req, timeout).await,
        ProviderKind::Anthropic => {
            anthropic::call(client, provider_name, provider, req, timeout).await
        }
    }
}

pub(crate) fn require_api_key<'a>(
    provider_name: &str,
    provider: &'a ProviderConfig,
) -> Result<&'a str> {
    provider
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Provider {
            provider: provider_name.to_string(),
            message: "api_key is missing or empty (check your environment variables)".to_string(),
        })
}

pub(crate) fn dedupe_citations(citations: Vec<Citation>) -> Vec<Citation> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in citations {
        if c.url.trim().is_empty() {
            continue;
        }
        if seen.insert(c.url.clone()) {
            out.push(c);
        }
    }
    out
}
