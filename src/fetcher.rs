use std::collections::HashSet;
use std::time::Duration;

use regex::Regex;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::SourceConfig;
use crate::error::{AppError, Result};
use crate::http::{send_with_retry, RetryPolicy};
use crate::store::{SeenSourceItem, Store};

const DEFAULT_MAX_CHARS: usize = 12_000;
const DEFAULT_RSS_LIMIT: usize = 15;

#[derive(Debug, Clone, Copy)]
pub struct FetchOptions {
    pub timeout: Duration,
    pub retry: RetryPolicy,
    pub fail_on_source_error: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            fail_on_source_error: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchedSource {
    pub name: String,
    pub url: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct FetchOutput {
    pub sources: Vec<FetchedSource>,
    pub new_rss_items: Vec<SeenSourceItem>,
    pub tracked_rss_sources: usize,
}

/// Fetch all configured pre-fetcher sources (`url` and `rss`) for a feed.
pub async fn fetch_sources(
    client: &Client,
    feed_id: &str,
    sources: &[SourceConfig],
    store: &Store,
    options: FetchOptions,
) -> Result<FetchOutput> {
    let mut output = FetchOutput {
        sources: Vec::with_capacity(sources.len()),
        new_rss_items: Vec::new(),
        tracked_rss_sources: 0,
    };

    for src in sources {
        match src {
            SourceConfig::Url {
                name,
                url,
                max_chars,
                headers,
            } => {
                match fetch_url_source(
                    client,
                    url,
                    max_chars.unwrap_or(DEFAULT_MAX_CHARS),
                    headers,
                    options,
                )
                .await
                {
                    Ok(content) => output.sources.push(FetchedSource {
                        name: name.clone(),
                        url: url.clone(),
                        content,
                    }),
                    Err(err) if !options.fail_on_source_error => {
                        warn!(
                            feed_id = %feed_id,
                            source = %name,
                            url = %url,
                            error = %err,
                            "URL source fetch failed; continuing"
                        );
                        output.sources.push(FetchedSource {
                            name: name.clone(),
                            url: url.clone(),
                            content: format!("[Source unavailable: {err}]"),
                        });
                    }
                    Err(err) => return Err(err),
                }
            }
            SourceConfig::Rss {
                name,
                url,
                limit,
                only_new,
            } => {
                let seen_keys = if *only_new {
                    Some(store.seen_item_keys(feed_id, url)?)
                } else {
                    None
                };

                match fetch_rss_source(
                    client,
                    url,
                    limit.unwrap_or(DEFAULT_RSS_LIMIT),
                    seen_keys.as_ref(),
                    options,
                )
                .await
                {
                    Ok((content, new_keys)) => {
                        if *only_new {
                            output.tracked_rss_sources += 1;
                            for key in new_keys {
                                output.new_rss_items.push(SeenSourceItem {
                                    source_url: url.clone(),
                                    item_key: key,
                                });
                            }
                        }
                        output.sources.push(FetchedSource {
                            name: name.clone(),
                            url: url.clone(),
                            content,
                        });
                    }
                    Err(err) if !options.fail_on_source_error => {
                        warn!(
                            feed_id = %feed_id,
                            source = %name,
                            url = %url,
                            error = %err,
                            "RSS source fetch failed; continuing"
                        );
                        output.sources.push(FetchedSource {
                            name: name.clone(),
                            url: url.clone(),
                            content: format!("[Source unavailable: {err}]"),
                        });
                    }
                    Err(err) => return Err(err),
                }
            }
        }
    }

    Ok(output)
}

async fn fetch_url_source(
    client: &Client,
    url: &str,
    max_chars: usize,
    headers: &std::collections::BTreeMap<String, String>,
    options: FetchOptions,
) -> Result<String> {
    let resp = send_with_retry(url, options.retry, || {
        let mut req = client
            .get(url)
            .timeout(options.timeout)
            .header("User-Agent", "llm-to-rss/0.1 (+https://github.com)");
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

async fn fetch_rss_source(
    client: &Client,
    url: &str,
    limit: usize,
    seen_keys: Option<&HashSet<String>>,
    options: FetchOptions,
) -> Result<(String, Vec<String>)> {
    let resp = send_with_retry(url, options.retry, || {
        client
            .get(url)
            .timeout(options.timeout)
            .header("User-Agent", "llm-to-rss/0.1 (+https://github.com)")
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

    let bytes = resp.bytes().await.map_err(|e| AppError::Fetch {
        url: url.to_string(),
        message: e.to_string(),
    })?;

    parse_syndication_bytes(&bytes, url, limit, seen_keys)
}

fn parse_syndication_bytes(
    bytes: &[u8],
    url: &str,
    limit: usize,
    seen_keys: Option<&HashSet<String>>,
) -> Result<(String, Vec<String>)> {
    if let Ok(channel) = rss::Channel::read_from(bytes) {
        let mut lines = Vec::new();
        let mut new_keys = Vec::new();
        for item in channel.items() {
            if lines.len() >= limit {
                break;
            }
            let key = rss_item_key(item);
            if seen_keys.is_some_and(|seen| seen.contains(&key)) {
                continue;
            }
            let title = item.title().unwrap_or("(untitled)").trim();
            let link = item.link().unwrap_or("");
            let date = item.pub_date().unwrap_or("");
            let summary = item
                .description()
                .map(html_to_text)
                .map(|s| truncate_chars(&s, 400))
                .unwrap_or_default();
            lines.push(format!("- **{title}** ({date}) {link}\n  {summary}"));
            new_keys.push(key);
        }
        let content = if lines.is_empty() && seen_keys.is_some() {
            "(No new items since last run.)".to_string()
        } else {
            lines.join("\n")
        };
        return Ok((content, new_keys));
    }

    if let Ok(feed) = atom_syndication::Feed::read_from(bytes) {
        let mut lines = Vec::new();
        let mut new_keys = Vec::new();
        for entry in feed.entries() {
            if lines.len() >= limit {
                break;
            }
            let key = atom_entry_key(entry);
            if seen_keys.is_some_and(|seen| seen.contains(&key)) {
                continue;
            }
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
            new_keys.push(key);
        }
        let content = if lines.is_empty() && seen_keys.is_some() {
            "(No new items since last run.)".to_string()
        } else {
            lines.join("\n")
        };
        return Ok((content, new_keys));
    }

    Err(AppError::Fetch {
        url: url.to_string(),
        message: "response was neither valid RSS 2.0 nor Atom 1.0".to_string(),
    })
}

fn rss_item_key(item: &rss::Item) -> String {
    if let Some(guid) = item
        .guid()
        .map(|g| g.value().trim())
        .filter(|s| !s.is_empty())
    {
        return guid.to_string();
    }
    if let Some(link) = item.link().map(str::trim).filter(|s| !s.is_empty()) {
        return link.to_string();
    }
    fallback_item_hash(item.title().unwrap_or(""), item.pub_date().unwrap_or(""))
}

fn atom_entry_key(entry: &atom_syndication::Entry) -> String {
    let id = entry.id().trim();
    if !id.is_empty() {
        return id.to_string();
    }
    if let Some(href) = entry
        .links()
        .first()
        .map(|l| l.href().trim())
        .filter(|s| !s.is_empty())
    {
        return href.to_string();
    }
    fallback_item_hash(entry.title().as_str(), &entry.updated().to_rfc3339())
}

fn fallback_item_hash(title: &str, date: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.trim().as_bytes());
    hasher.update(b"\0");
    hasher.update(date.trim().as_bytes());
    format!("sha256:{:x}", hasher.finalize())
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

    #[test]
    fn filters_seen_rss_items_before_applying_limit() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Feed</title><link>https://example.com</link><description>d</description>
  <item><guid>id-1</guid><title>Old Story</title><link>https://example.com/1</link></item>
  <item><guid>id-2</guid><title>New Story 1</title><link>https://example.com/2</link></item>
  <item><guid>id-3</guid><title>New Story 2</title><link>https://example.com/3</link></item>
</channel></rss>"#;

        let mut seen = HashSet::new();
        seen.insert("id-1".to_string());

        let (content, new_keys) =
            parse_syndication_bytes(xml.as_bytes(), "https://example.com/rss", 1, Some(&seen))
                .unwrap();
        assert!(!content.contains("Old Story"));
        assert!(content.contains("New Story 1"));
        assert!(!content.contains("New Story 2"));
        assert_eq!(new_keys, vec!["id-2".to_string()]);
    }
}
