use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::config::ProviderConfig;
use crate::error::{AppError, Result};
use crate::llm::{dedupe_citations, LlmRequest, LlmResponse};
use crate::store::Citation;

pub async fn call(
    client: &Client,
    provider_name: &str,
    provider: &ProviderConfig,
    req: &LlmRequest,
    timeout: Duration,
) -> Result<LlmResponse> {
    let endpoint = format!("{}/chat/completions", provider.effective_base_url());

    let mut messages = Vec::new();
    if let Some(sys) = &req.system_prompt {
        if !sys.trim().is_empty() {
            messages.push(json!({ "role": "system", "content": sys }));
        }
    }
    messages.push(json!({ "role": "user", "content": req.user_prompt }));

    let mut body = json!({
        "model": req.model,
        "messages": messages,
    });
    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = req.max_tokens {
        body["max_tokens"] = json!(max);
    }

    let mut http_req = client.post(&endpoint).timeout(timeout).json(&body);
    if let Some(key) = provider
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        http_req = http_req.bearer_auth(key);
    }
    for (k, v) in &provider.extra_headers {
        http_req = http_req.header(k, v);
    }

    let resp = http_req.send().await.map_err(|e| AppError::Provider {
        provider: provider_name.to_string(),
        message: e.to_string(),
    })?;
    let status = resp.status();
    let payload: Value = resp.json().await.map_err(|e| AppError::Provider {
        provider: provider_name.to_string(),
        message: format!("invalid JSON response ({status}): {e}"),
    })?;

    if !status.is_success() {
        return Err(AppError::Provider {
            provider: provider_name.to_string(),
            message: format!("HTTP {status}: {payload}"),
        });
    }

    let text = payload["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| AppError::Provider {
            provider: provider_name.to_string(),
            message: format!("missing choices[0].message.content in response: {payload}"),
        })?
        .to_string();

    let mut citations = Vec::new();
    if let Some(arr) = payload["citations"].as_array() {
        for item in arr {
            if let Some(url) = item.as_str() {
                citations.push(Citation {
                    title: url.to_string(),
                    url: url.to_string(),
                });
            }
        }
    }

    Ok(LlmResponse {
        text,
        model: payload["model"].as_str().unwrap_or(&req.model).to_string(),
        citations: dedupe_citations(citations),
    })
}
