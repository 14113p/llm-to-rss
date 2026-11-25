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
    let endpoint = format!("{}/responses", provider.effective_base_url());

    let mut body = json!({
        "model": req.model,
        "input": req.user_prompt,
    });
    if let Some(sys) = &req.system_prompt {
        if !sys.trim().is_empty() {
            body["instructions"] = json!(sys);
        }
    }
    if req.web_search {
        body["tools"] = json!([{ "type": "web_search" }]);
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = req.max_tokens {
        body["max_output_tokens"] = json!(max);
    }

    let mut http_req = client
        .post(&endpoint)
        .timeout(timeout)
        .bearer_auth(api_key)
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

    let mut parts = Vec::new();
    let mut citations = Vec::new();

    if let Some(output) = payload["output"].as_array() {
        for item in output {
            if item["type"].as_str() == Some("message") {
                if let Some(contents) = item["content"].as_array() {
                    for c in contents {
                        if let Some(text) = c["text"].as_str() {
                            parts.push(text.to_string());
                        }
                        if let Some(annotations) = c["annotations"].as_array() {
                            for ann in annotations {
                                if ann["type"].as_str() == Some("url_citation") {
                                    if let Some(url) = ann["url"].as_str() {
                                        let title =
                                            ann["title"].as_str().unwrap_or(url).to_string();
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
            }
        }
    }

    let text = if !parts.is_empty() {
        parts.join("\n\n")
    } else if let Some(top) = payload["output_text"].as_str() {
        top.to_string()
    } else {
        return Err(AppError::Provider {
            provider: provider_name.to_string(),
            message: format!("missing message output in Responses API payload: {payload}"),
        });
    };

    Ok(LlmResponse {
        text,
        model: payload["model"].as_str().unwrap_or(&req.model).to_string(),
        citations: dedupe_citations(citations),
    })
}
