use std::collections::HashSet;

use reqwest::Client;
use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};
use crate::http::send_with_retry;

use super::html::{html_to_text, truncate_chars};
use super::{FetchOptions, USER_AGENT};

pub async fn fetch(
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
            .header("User-Agent", USER_AGENT)
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

pub(crate) fn parse_syndication_bytes(
    bytes: &[u8],
    url: &str,
    limit: usize,
    seen_keys: Option<&HashSet<String>>,
) -> Result<(String, Vec<String>)> {
    if let Ok(channel) = ::rss::Channel::read_from(bytes) {
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

fn rss_item_key(item: &::rss::Item) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
