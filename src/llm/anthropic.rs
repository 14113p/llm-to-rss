use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::config::ProviderConfig;
use crate::error::{AppError, Result};
use crate::http::{send_with_retry, RetryPolicy};
use crate::llm::{
    decode_json_response, dedupe_citations, require_api_key, LlmRequest, LlmResponse,
};
use crate::store::Citation;

pub async fn call(
    client: &Client,
    provider_name: &str,
    provider: &ProviderConfig,
    req: &LlmRequest,
    timeout: Duration,
    retry: RetryPolicy,
) -> Result<LlmResponse> {
    let api_key = require_api_key(provider_name, provider)?;
    let endpoint = format!("{}/messages", provider.effective_base_url());

    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens.unwrap_or(4096),
        "messages": [{
            "role": "user",
            "content": req.user_prompt
        }]
    });

    if let Some(sys) = &req.system_prompt {
        if !sys.trim().is_empty() {
            body["system"] = json!(sys);
        }
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }
    if req.web_search {
        body["tools"] = json!([{
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 5
        }]);
    }

    let resp = send_with_retry(provider_name, retry, || {
        let mut http_req = client
            .post(&endpoint)
            .timeout(timeout)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        for (k, v) in &provider.extra_headers {
            http_req = http_req.header(k, v);
        }
        http_req
    })
    .await
    .map_err(|e| AppError::Provider {
        provider: provider_name.to_string(),
        message: e.to_string(),
    })?;
    let payload: Value = decode_json_response(provider_name, resp).await?;

    let mut parts = Vec::new();
    let mut citations = Vec::new();

    if let Some(blocks) = payload["content"].as_array() {
        for block in blocks {
            if block["type"].as_str() == Some("text") {
                if let Some(text) = block["text"].as_str() {
                    parts.push(text.to_string());
                }
                if let Some(cits) = block["citations"].as_array() {
                    for c in cits {
                        if let Some(url) = c["url"].as_str() {
                            let title = c["title"].as_str().unwrap_or(url).to_string();
                            citations.push(Citation {
                                title,
                                url: url.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        return Err(AppError::Provider {
            provider: provider_name.to_string(),
            message: format!("missing text content in Anthropic payload: {payload}"),
        });
    }

    Ok(LlmResponse {
        text: parts.join("\n\n"),
        model: payload["model"].as_str().unwrap_or(&req.model).to_string(),
        citations: dedupe_citations(citations),
    })
}
