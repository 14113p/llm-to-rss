pub mod html;
pub mod rss;
pub mod url;

use std::time::Duration;

use reqwest::Client;
use tokio::task::JoinSet;
use tracing::warn;

use crate::config::SourceConfig;
use crate::error::{AppError, Result};
use crate::http::RetryPolicy;
use crate::store::{SeenSourceItem, Store};

pub use html::{html_to_text, truncate_chars};

pub(crate) const USER_AGENT: &str = "llm-to-rss/0.1 (+https://github.com)";
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

struct SingleSourceResult {
    source: FetchedSource,
    new_rss_items: Vec<SeenSourceItem>,
    tracked_rss_source: bool,
}

/// Fetch all configured pre-fetcher sources (`url`, `rss`, etc.) concurrently while preserving
/// their declared YAML order in the rendered prompt.
pub async fn fetch_sources(
    client: &Client,
    feed_id: &str,
    sources: &[SourceConfig],
    store: &Store,
    options: FetchOptions,
) -> Result<FetchOutput> {
    if sources.is_empty() {
        return Ok(FetchOutput::default());
    }

    let mut set = JoinSet::new();
    for (idx, src) in sources.iter().cloned().enumerate() {
        let client = client.clone();
        let feed_id = feed_id.to_string();
        let store = store.clone();
        set.spawn(async move {
            let res = fetch_one(&client, &feed_id, src, &store, options).await;
            (idx, res)
        });
    }

    let mut ordered: Vec<Option<SingleSourceResult>> = (0..sources.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        let (idx, res) = joined.map_err(|e| AppError::Fetch {
            url: feed_id.to_string(),
            message: format!("source fetch task failed: {e}"),
        })?;
        match res {
            Ok(item) => ordered[idx] = Some(item),
            Err(err) => {
                set.abort_all();
                return Err(err);
            }
        }
    }

    let mut output = FetchOutput {
        sources: Vec::with_capacity(sources.len()),
        new_rss_items: Vec::new(),
        tracked_rss_sources: 0,
    };
    for item in ordered.into_iter().flatten() {
        output.sources.push(item.source);
        output.new_rss_items.extend(item.new_rss_items);
        if item.tracked_rss_source {
            output.tracked_rss_sources += 1;
        }
    }

    Ok(output)
}

async fn fetch_one(
    client: &Client,
    feed_id: &str,
    src: SourceConfig,
    store: &Store,
    options: FetchOptions,
) -> Result<SingleSourceResult> {
    match src {
        SourceConfig::Url {
            name,
            url: target_url,
            max_chars,
            headers,
        } => {
            match url::fetch(
                client,
                &target_url,
                max_chars.unwrap_or(DEFAULT_MAX_CHARS),
                &headers,
                options,
            )
            .await
            {
                Ok(content) => Ok(SingleSourceResult {
                    source: FetchedSource {
                        name,
                        url: target_url,
                        content,
                    },
                    new_rss_items: Vec::new(),
                    tracked_rss_source: false,
                }),
                Err(err) if !options.fail_on_source_error => {
                    warn!(
                        feed_id = %feed_id,
                        source = %name,
                        url = %target_url,
                        error = %err,
                        "URL source fetch failed; continuing"
                    );
                    Ok(SingleSourceResult {
                        source: FetchedSource {
                            name,
                            url: target_url,
                            content: format!("[Source unavailable: {err}]"),
                        },
                        new_rss_items: Vec::new(),
                        tracked_rss_source: false,
                    })
                }
                Err(err) => Err(err),
            }
        }
        SourceConfig::Rss {
            name,
            url: target_url,
            limit,
            only_new,
        } => {
            let seen_keys = if only_new {
                Some(store.seen_item_keys(feed_id, &target_url)?)
            } else {
                None
            };

            match rss::fetch(
                client,
                &target_url,
                limit.unwrap_or(DEFAULT_RSS_LIMIT),
                seen_keys.as_ref(),
                options,
            )
            .await
            {
                Ok((content, new_keys)) => {
                    let new_rss_items = if only_new {
                        new_keys
                            .into_iter()
                            .map(|item_key| SeenSourceItem {
                                source_url: target_url.clone(),
                                item_key,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    Ok(SingleSourceResult {
                        source: FetchedSource {
                            name,
                            url: target_url,
                            content,
                        },
                        new_rss_items,
                        tracked_rss_source: only_new,
                    })
                }
                Err(err) if !options.fail_on_source_error => {
                    warn!(
                        feed_id = %feed_id,
                        source = %name,
                        url = %target_url,
                        error = %err,
                        "RSS source fetch failed; continuing"
                    );
                    Ok(SingleSourceResult {
                        source: FetchedSource {
                            name,
                            url: target_url,
                            content: format!("[Source unavailable: {err}]"),
                        },
                        new_rss_items: Vec::new(),
                        tracked_rss_source: false,
                    })
                }
                Err(err) => Err(err),
            }
        }
    }
}
