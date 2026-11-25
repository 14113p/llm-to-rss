use std::time::Duration;

use regex::Regex;
use reqwest::Client;

use crate::config::SourceConfig;
use crate::error::{AppError, Result};

const DEFAULT_MAX_CHARS: usize = 12_000;
const DEFAULT_RSS_LIMIT: usize = 15;

#[derive(Debug, Clone)]
pub struct FetchedSource {
    pub name: String,
    pub url: String,
    pub content: String,
}

/// Fetch all configured pre-fetcher sources (`url` and `rss`) for a feed.
pub async fn fetch_sources(
    client: &Client,
    sources: &[SourceConfig],
) -> Result<Vec<FetchedSource>> {
    let mut results = Vec::with_capacity(sources.len());
    for src in sources {
        match src {
            SourceConfig::Url {
                name,
                url,
                max_chars,
                headers,
            } => {
                let content =
                    fetch_url_source(client, url, max_chars.unwrap_or(DEFAULT_MAX_CHARS), headers)
                        .await?;
                results.push(FetchedSource {
                    name: name.clone(),
                    url: url.clone(),
                    content,
                });
            }
            SourceConfig::Rss { name, url, limit } => {
                let content =
                    fetch_rss_source(client, url, limit.unwrap_or(DEFAULT_RSS_LIMIT)).await?;
                results.push(FetchedSource {
                    name: name.clone(),
                    url: url.clone(),
                    content,
                });
            }
        }
    }
    Ok(results)
}

async fn fetch_url_source(
    client: &Client,
    url: &str,
    max_chars: usize,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<String> {
    let mut req = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .header("User-Agent", "llm-to-rss/0.1 (+https://github.com)");
    for (k, v) in headers {
        req = req.header(k, v);
    }

    let resp = req.send().await.map_err(|e| AppError::Fetch {
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

async fn fetch_rss_source(client: &Client, url: &str, limit: usize) -> Result<String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .header("User-Agent", "llm-to-rss/0.1 (+https://github.com)")
        .send()
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

    let bytes = resp.bytes().await.map_err(|e| AppError::Fetch {
        url: url.to_string(),
        message: e.to_string(),
    })?;

    if let Ok(channel) = rss::Channel::read_from(&bytes[..]) {
        let mut lines = Vec::new();
        for item in channel.items().iter().take(limit) {
            let title = item.title().unwrap_or("(untitled)").trim();
            let link = item.link().unwrap_or("");
            let date = item.pub_date().unwrap_or("");
            let summary = item
                .description()
                .map(html_to_text)
                .map(|s| truncate_chars(&s, 400))
                .unwrap_or_default();
            lines.push(format!("- **{title}** ({date}) {link}\n  {summary}"));
        }
        return Ok(lines.join("\n"));
    }

    if let Ok(feed) = atom_syndication::Feed::read_from(&bytes[..]) {
        let mut lines = Vec::new();
        for entry in feed.entries().iter().take(limit) {
            let title = entry.title().as_str().trim();
            let link = entry.links().first().map(|l| l.href()).unwrap_or("");
            let date = entry
                .published()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| entry.updated().to_rfc3339());
            let summary = entry
                .summary()
                .map(|s| html_to_text(s.as_str()))
                .or_else(|| entry.content().and_then(|c| c.value()).map(html_to_text))
                .map(|s| truncate_chars(&s, 400))
                .unwrap_or_default();
            lines.push(format!("- **{title}** ({date}) {link}\n  {summary}"));
        }
        return Ok(lines.join("\n"));
    }

    Err(AppError::Fetch {
        url: url.to_string(),
        message: "response was neither valid RSS 2.0 nor Atom 1.0".to_string(),
    })
}

/// Strip scripts/styles/tags and decode common HTML entities into readable plain text.
pub fn html_to_text(html: &str) -> String {
    let script_re =
        Regex::new(r"(?is)<(script|style|noscript|svg)[^>]*>.*?</(script|style|noscript|svg)>")
            .expect("valid regex");
    let without_scripts = script_re.replace_all(html, " ");

    let tag_re = Regex::new(r"(?s)<[^>]+>").expect("valid regex");
    let without_tags = tag_re.replace_all(&without_scripts, " ");

    let decoded = without_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    let ws_re = Regex::new(r"\s+").expect("valid regex");
    ws_re.replace_all(decoded.trim(), " ").into_owned()
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let collected: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{collected}… [truncated]")
    } else {
        collected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_scripts_and_tags() {
        let html = "<html><head><style>body{}</style><script>alert(1)</script></head><body><h1>PS5 &amp; Pro</h1><p>Price: <b>$449</b></p></body></html>";
        assert_eq!(html_to_text(html), "PS5 & Pro Price: $449");
    }

    #[test]
    fn truncates_cleanly() {
        assert_eq!(truncate_chars("abcdef", 4), "abcd… [truncated]");
        assert_eq!(truncate_chars("abc", 4), "abc");
    }
}
