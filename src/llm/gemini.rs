use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::config::ProviderConfig;
use crate::error::{AppError, Result};
use crate::llm::{dedupe_citations, require_api_key, LlmRequest, LlmResponse};
use crate::store::Citation;

pub async fn call(
    client: &Client,
    provider_name: &str,
    provider: &ProviderConfig,
    req: &LlmRequest,
    timeout: Duration,
) -> Result<LlmResponse> {
    let api_key = require_api_key(provider_name, provider)?;
    let endpoint = format!(
        "{}/models/{}:generateContent",
        provider.effective_base_url(),
        req.model
    );

    let mut body = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": req.user_prompt }]
        }]
    });

    if let Some(sys) = &req.system_prompt {
        if !sys.trim().is_empty() {
            body["systemInstruction"] = json!({
                "parts": [{ "text": sys }]
            });
        }
    }

    if req.web_search {
        body["tools"] = json!([{ "google_search": {} }]);
    }

    let mut gen_cfg = serde_json::Map::new();
    if let Some(temp) = req.temperature {
        gen_cfg.insert("temperature".into(), json!(temp));
    }
    if let Some(max) = req.max_tokens {
        gen_cfg.insert("maxOutputTokens".into(), json!(max));
    }
    if !gen_cfg.is_empty() {
        body["generationConfig"] = Value::Object(gen_cfg);
    }

    let mut http_req = client
        .post(&endpoint)
        .timeout(timeout)
        .header("x-goog-api-key", api_key)
        .json(&body);
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

    let candidate = &payload["candidates"][0];
    let mut parts = Vec::new();
    if let Some(arr) = candidate["content"]["parts"].as_array() {
        for part in arr {
            if let Some(text) = part["text"].as_str() {
                parts.push(text.to_string());
            }
        }
    }

    if parts.is_empty() {
        return Err(AppError::Provider {
            provider: provider_name.to_string(),
            message: format!("missing candidates[0].content.parts in Gemini payload: {payload}"),
        });
    }

    let mut citations = Vec::new();
    if let Some(chunks) = candidate["groundingMetadata"]["groundingChunks"].as_array() {
        for chunk in chunks {
            if let Some(uri) = chunk["web"]["uri"].as_str() {
                let title = chunk["web"]["title"].as_str().unwrap_or(uri).to_string();
                citations.push(Citation {
                    title,
                    url: uri.to_string(),
                });
            }
        }
    }

    Ok(LlmResponse {
        text: parts.join("\n"),
        model: req.model.clone(),
        citations: dedupe_citations(citations),
    })
}
