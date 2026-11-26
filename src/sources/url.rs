use std::collections::BTreeMap;

use reqwest::Client;

use crate::error::{AppError, Result};
use crate::http::send_with_retry;

use super::html::{html_to_text, truncate_chars};
use super::{FetchOptions, USER_AGENT};

pub async fn fetch(
    client: &Client,
    url: &str,
    max_chars: usize,
    headers: &BTreeMap<String, String>,
    options: FetchOptions,
) -> Result<String> {
    let resp = send_with_retry(url, options.retry, || {
        let mut req = client
            .get(url)
            .timeout(options.timeout)
            .header("User-Agent", USER_AGENT);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req
    })
    .await
    .map_err(|e| AppError::Fetch {
        url: url.to_string(),
        message: e.to_string(),
    })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(AppError::Fetch {
            url: url.to_string(),
            message: format!("HTTP {status}"),
        });
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let body = resp.text().await.map_err(|e| AppError::Fetch {
        url: url.to_string(),
        message: e.to_string(),
    })?;

    let cleaned = if content_type.contains("html") || body.trim_start().starts_with('<') {
        html_to_text(&body)
    } else {
        body
    };

    Ok(truncate_chars(&cleaned, max_chars))
}
